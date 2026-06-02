//! Block-builder backrun engine.
//!
//! # Integration modes
//!
//! **In-process** (same binary): construct a [`Backrunner`], create a tokio mpsc/watch channel
//! pair, and spawn [`Backrunner::run`] as a task alongside your builder loop.
//!
//! ```ignore
//! use backrunner::{Backrunner, BackrunnerConfig};
//! use builder_types::{BackrunCandidate, BuildEvent};
//! use tokio::sync::{mpsc, watch};
//!
//! let config = BackrunnerConfig { ... };
//! let backrunner = Backrunner::build(config).await?;
//!
//! let (event_tx, event_rx) = mpsc::channel::<BuildEvent>(1024);
//! let (candidate_tx, candidate_rx) = watch::channel(None::<BackrunCandidate>);
//!
//! tokio::spawn(backrunner.run(event_rx, candidate_tx));
//! event_tx.send(BuildEvent::IterationStart { uuid, block }).await?;
//! ```

pub mod abi;
mod client;
mod order;
#[cfg(test)]
mod encode_test;

use std::{collections::HashMap, sync::Arc, time::Duration};

use alloy::primitives::Address as AlloyAddress;

use abi::{build_settle_calldata, RawOrderFields, SettleParams};
use builder_types::{BackrunCandidate, BackrunTx, BlockEnv, BuildEvent, ExecutedTx, PostState, RawTx};
use client::OneinchClient;
use fynd_core::{
    feed::market_data::MarketData, EncodingOptions, FyndBuilder, MarketEvent, Order, OrderQuote,
    OrderSide, PendingBlockProcessor, PendingError, QuoteOptions, QuoteRequest, Solver, SolveError,
    SolverBuildError,
};
use num_bigint::BigUint;
use order::{amount_at_timestamp, is_gtc_order, FusionOrder};
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, error, warn};
use tycho_simulation::tycho_client::feed::BlockHeader;
use tycho_simulation::tycho_common::{
    models::{blockchain::{LogInput, TxInput}, token::Token, Address},
    Bytes as TychoBytes,
};
use uuid::Uuid;

/// Configuration for a [`Backrunner`] instance.
#[derive(Debug, Clone)]
pub struct BackrunnerConfig {
    /// Chain slug: `"ethereum"`, `"base"`, `"arbitrum"`, `"bsc"`, `"zksync"`, `"unichain"`.
    pub chain: String,
    /// Tycho WebSocket host (e.g. `"app.propellerheads.xyz"`).
    pub tycho_url: String,
    /// Ethereum JSON-RPC URL used for gas price fetching.
    pub rpc_url: String,
    /// Tycho API key, if required.
    pub tycho_api_key: Option<String>,
    /// Protocol slugs to subscribe to (e.g. `"uniswap_v3_ethereum"`).
    pub protocols: Vec<String>,
    /// Minimum pool TVL in USD.
    pub min_tvl: f64,
    /// Address that will send and receive backrun transactions.
    pub wallet_address: String,
    /// How long to wait for the initial market-data snapshot before failing.
    pub ready_timeout: Duration,
    /// 1inch Fusion chain ID (1 = Ethereum mainnet).
    pub chain_id: u64,
    /// Address of the deployed `BackrunResolver` contract.
    pub resolver_address: AlloyAddress,
    /// Slippage tolerance for Fynd quotes (0.005 = 0.5%).
    pub slippage: f64,
}

/// Error returned by [`Backrunner::build`].
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("unsupported chain: {0}")]
    UnsupportedChain(String),
    #[error(transparent)]
    Solver(#[from] SolverBuildError),
    #[error("timed out waiting for market data to be ready")]
    MarketDataTimeout,
    #[error("failed to build 1inch client: {0}")]
    OneinchClient(String),
}

#[derive(Debug, thiserror::Error)]
enum EvaluateError {
    #[error("pending update failed: {0}")]
    Pending(#[from] PendingError),
    #[error("solve failed: {0}")]
    Solve(#[from] SolveError),
}

struct PendingIteration {
    block: BlockEnv,
    txs: Vec<ExecutedTx>,
}

/// Consumes [`BuildEvent`]s from a block builder and publishes [`BackrunCandidate`]s.
pub struct Backrunner {
    solver: Solver,
    pending: tokio::sync::Mutex<PendingBlockProcessor>,
    /// Receiver for the current set of live Fusion orders (refreshed ~every 12s).
    orders_rx: watch::Receiver<Arc<Vec<FusionOrder>>>,
    pub(crate) resolver_address: AlloyAddress,
    pub(crate) slippage: f64,
}

impl Backrunner {
    /// Builds a [`Backrunner`] and waits for the market-data snapshot to arrive.
    pub async fn build(config: BackrunnerConfig) -> Result<Self, BuildError> {
        let chain = parse_chain(&config.chain)?;

        let oneinch = OneinchClient::new(config.chain_id)
            .map_err(|e| BuildError::OneinchClient(e.to_string()))?;

        let builder = FyndBuilder::new(
            chain,
            config.tycho_url,
            config.rpc_url,
            config.protocols,
            config.min_tvl,
        )
        .algorithm("bellman_ford");

        let builder = match config.tycho_api_key {
            Some(key) => builder.tycho_api_key(key),
            None => builder,
        };

        let (solver, pending) = builder.build_with_pending().await?;

        solver
            .wait_until_ready(config.ready_timeout)
            .await
            .map_err(|_| BuildError::MarketDataTimeout)?;

        // Seed the watch channel with an empty orderbook; the background task
        // will populate it on the first poll (within 12 seconds).
        let (orders_tx, orders_rx) = watch::channel(Arc::new(Vec::new()));

        let market_data_for_orders = solver.market_data();
        tokio::spawn(run_orderbook(Arc::new(oneinch), orders_tx, market_data_for_orders));

        Ok(Self {
            solver,
            pending: tokio::sync::Mutex::new(pending),
            orders_rx,
            resolver_address: config.resolver_address,
            slippage: config.slippage,
        })
    }

    /// Returns the number of live Fusion orders currently held by the orderbook poller.
    ///
    /// Zero until the first poll completes (~12 s after `build()`).
    pub fn active_order_count(&self) -> usize {
        self.orders_rx.borrow().len()
    }

    /// Subscribes to market update events — one `MarketUpdated` per Ethereum block.
    ///
    /// Must be called before [`Backrunner::run`] since that method moves `self`.
    pub fn subscribe_market_events(&self) -> broadcast::Receiver<MarketEvent> {
        self.solver.subscribe_market_events()
    }

    /// Returns the current confirmed block number, or `None` before the first block arrives.
    pub fn current_block_number(&self) -> Option<u64> {
        let md = self.solver.market_data();
        let view = md.try_read_blocking()?;
        Some(view.last_updated()?.number())
    }

    /// Returns a handle to the underlying market data store.
    ///
    /// Must be called before [`Backrunner::run`] since that method moves `self`.
    pub fn market_data(&self) -> MarketData {
        self.solver.market_data()
    }

    /// Runs the backrun event loop until `events` is closed.
    pub async fn run(
        self,
        mut events: mpsc::Receiver<BuildEvent>,
        candidates: watch::Sender<Option<BackrunCandidate>>,
    ) {
        tracing::info!("backrunner started");
        let mut pending: HashMap<Uuid, PendingIteration> = HashMap::new();

        while let Some(event) = events.recv().await {
            handle_event(&self, &mut pending, &candidates, event).await;
        }

        tracing::info!("event channel closed, backrunner shutting down");
    }
}

/// Background task: polls 1inch Fusion for active orders every 12 seconds.
///
/// After each fetch, token decimals are patched from Tycho's registry (which has exact
/// on-chain values for every indexed token) to fix any fallback-18 from the 1inch API.
async fn run_orderbook(
    client: Arc<OneinchClient>,
    orders_tx: watch::Sender<Arc<Vec<FusionOrder>>>,
    market_data: MarketData,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(12));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;
        match client.fetch_active_orders().await {
            Ok(orders) => {
                let mut filtered: Vec<FusionOrder> =
                    orders.into_iter().filter(|o| !is_gtc_order(o)).collect();
                if let Some(view) = market_data.try_read_blocking() {
                    patch_decimals_from_registry(&mut filtered, view.token_registry_ref());
                }
                debug!(order_count = filtered.len(), "orderbook refreshed");
                if orders_tx.send(Arc::new(filtered)).is_err() {
                    break;
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to fetch active orders; retrying next tick");
            }
        }
    }
}

/// Overwrites decimal fields on orders whose token addresses appear in the Tycho registry.
fn patch_decimals_from_registry(orders: &mut Vec<FusionOrder>, registry: &HashMap<Address, Token>) {
    for order in orders {
        if let Ok(addr) = parse_address(&order.from_token) {
            if let Some(token) = registry.get(&addr) {
                order.from_token_decimals = u8::try_from(token.decimals).unwrap_or(18);
            }
        }
        if let Ok(addr) = parse_address(&order.to_token) {
            if let Some(token) = registry.get(&addr) {
                order.to_token_decimals = u8::try_from(token.decimals).unwrap_or(18);
            }
        }
    }
}

fn parse_chain(s: &str) -> Result<tycho_simulation::tycho_common::models::Chain, BuildError> {
    use tycho_simulation::tycho_common::models::Chain;
    match s {
        "ethereum" => Ok(Chain::Ethereum),
        "base" => Ok(Chain::Base),
        "arbitrum" => Ok(Chain::Arbitrum),
        "bsc" => Ok(Chain::Bsc),
        "zksync" => Ok(Chain::ZkSync),
        "unichain" => Ok(Chain::Unichain),
        "starknet" => Ok(Chain::Starknet),
        other => Err(BuildError::UnsupportedChain(other.to_string())),
    }
}

async fn handle_event(
    backrunner: &Backrunner,
    pending: &mut HashMap<Uuid, PendingIteration>,
    candidates: &watch::Sender<Option<BackrunCandidate>>,
    event: BuildEvent,
) {
    match event {
        BuildEvent::IterationStart { uuid, block } => {
            debug!(%uuid, block_number = block.block_number, "iteration started");
            pending.insert(uuid, PendingIteration { block, txs: vec![] });
        }
        BuildEvent::TxExecuted { uuid, tx } => {
            if let Some(iter) = pending.get_mut(&uuid) {
                iter.txs.push(tx);
            } else {
                warn!(%uuid, "TxExecuted for unknown iteration");
            }
        }
        BuildEvent::IterationComplete { uuid, state } => {
            let Some(iter) = pending.remove(&uuid) else {
                warn!(%uuid, "IterationComplete for unknown iteration");
                return;
            };
            let candidate = evaluate_backrun(backrunner, uuid, iter, state).await;
            if let Err(e) = candidates.send(candidate) {
                error!(error = %e, "candidate watch channel has no receivers");
            }
        }
        BuildEvent::IterationAborted { uuid } => {
            debug!(%uuid, "iteration aborted");
            pending.remove(&uuid);
        }
    }
}

async fn evaluate_backrun(
    backrunner: &Backrunner,
    uuid: Uuid,
    iter: PendingIteration,
    _state: PostState,
) -> Option<BackrunCandidate> {
    debug!(
        %uuid,
        block_number = iter.block.block_number,
        tx_count = iter.txs.len(),
        "evaluating backrun opportunity",
    );

    let orders = backrunner.orders_rx.borrow().clone();
    let block_ts = iter.block.block_timestamp;

    let active: Vec<&FusionOrder> =
        orders.iter().filter(|o| amount_at_timestamp(o, block_ts).is_some()).collect();

    if active.is_empty() {
        debug!(%uuid, "no active Fusion orders at block timestamp");
        return None;
    }

    let quote = match try_evaluate(backrunner, uuid, &iter, &active).await {
        Ok(Some(q)) => q,
        Ok(None) => {
            debug!(%uuid, "parent block not yet confirmed, skipping");
            return None;
        }
        Err(e) => {
            warn!(%uuid, error = %e, "evaluate_backrun failed");
            return None;
        }
    };

    // Build a map from order_id → FusionOrder for fast lookup.
    let order_map: HashMap<&str, &FusionOrder> =
        active.iter().map(|o| (o.order_id.as_str(), *o)).collect();

    let ctx = BackrunContext {
        uuid,
        block_ts,
        base_fee: iter.block.base_fee_per_gas,
        block_number: iter.block.block_number,
        solve_time_ms: quote.solve_time_ms(),
        orders_quoted: active.len(),
        backrunner,
    };

    let mut backrun_txs: Vec<BackrunTx> = Vec::new();

    for order_quote in quote.orders() {
        let Some(fynd_tx) = order_quote.transaction() else { continue };
        let Some(&fusion_order) = order_map.get(order_quote.order_id()) else { continue };

        if let Some(backrun_tx) =
            build_backrun_tx(&ctx, fusion_order, order_quote, fynd_tx).await
        {
            backrun_txs.push(backrun_tx);
        }
    }

    if backrun_txs.is_empty() {
        return None;
    }

    Some(BackrunCandidate {
        uuid,
        block_number: iter.block.block_number,
        txs: backrun_txs,
    })
}

/// Iteration-level context shared across all per-order calls inside [`evaluate_backrun`].
struct BackrunContext<'a> {
    uuid: Uuid,
    block_ts: u64,
    base_fee: u64,
    block_number: u64,
    solve_time_ms: u64,
    orders_quoted: usize,
    backrunner: &'a Backrunner,
}

/// Builds a single [`BackrunTx`] for one matched Fusion order quote.
///
/// Returns `None` when the swap output is below the auction price or order fields are invalid.
async fn build_backrun_tx(
    ctx: &BackrunContext<'_>,
    fusion_order: &FusionOrder,
    order_quote: &OrderQuote,
    fynd_tx: &fynd_core::Transaction,
) -> Option<BackrunTx> {
    let BackrunContext { uuid, block_ts, base_fee, block_number, solve_time_ms, orders_quoted, backrunner } = ctx;
    // Two sources of systematic under-estimation of the on-chain Dutch-auction price:
    //
    // 1. GAS-BUMP: The extension computes totalBump = auctionBump + gasBump where
    //    gasBump = gasBumpEstimate × block.basefee / gasPriceEstimate. The API does not
    //    expose these params so we ignore gas_bump. At real basefees this adds ~1-2% to the
    //    taking amount. The smoke-test eth_call uses baseFeePerGas=0 to neutralise this.
    //
    // 2. DURATION MISMATCH: The API's auctionEndDate reports a shorter auction window (~84s)
    //    than the on-chain extension actually encodes (~180s). Late in the API window our
    //    amount_at_timestamp returns floor while on-chain the auction still carries ~0.5% bump.
    //
    // MIN_PROFIT_MARGIN_BPS covers mismatch (2): require the Fynd output to exceed our
    // API-derived estimate by at least 75 bps so the residual ~0.5% on-chain bump is absorbed.
    // Fix: decode the actual `duration` field from extension bytes (bytes 62-64 of the extension
    // data section, after the 32-byte LOP header + 20-byte extension address).
    let taking_amount = amount_at_timestamp(fusion_order, *block_ts)?;

    // Minimum headroom above the API-derived price estimate (covers duration-mismatch error).
    const MIN_PROFIT_MARGIN_BPS: u128 = 75; // 0.75%

    // Check that the Fynd swap output is above the auction price with required margin.
    let biguint_to_u128 = |b: &BigUint| {
        let digits = b.to_u64_digits();
        if digits.is_empty() {
            0u128
        } else if digits.len() > 2 {
            u128::MAX
        } else {
            digits.iter().enumerate().fold(0u128, |acc, (i, &d)| acc | (u128::from(d) << (i * 64)))
        }
    };

    let amount_out_gross = biguint_to_u128(order_quote.amount_out());
    if amount_out_gross == 0 {
        return None;
    }

    // TODO: re-enable once the Fynd team sets our resolver's client fee to 0 on-chain.
    // let amount_out_u128 = if let Some(fb) = order_quote.fee_breakdown() {
    //     let router_fee = biguint_to_u128(fb.router_fee());
    //     amount_out_gross.saturating_sub(router_fee)
    // } else {
    //     amount_out_gross
    // };
    let amount_out_u128 = amount_out_gross;

    let taking_amount_with_margin = taking_amount
        .saturating_add(taking_amount.saturating_mul(MIN_PROFIT_MARGIN_BPS) / 10_000);

    if amount_out_u128 < taking_amount_with_margin {
        debug!(%uuid, order_id = %fusion_order.order_id,
            amount_out_gross, amount_out_net = amount_out_u128,
            taking_amount, taking_amount_with_margin,
            "swap output below auction price + margin, skipping");
        return None;
    }

    // Get secondary Fynd quote for surplus → WETH (async, best-effort).
    let surplus_amount = amount_out_u128.saturating_sub(taking_amount);
    let surplus_state_label = format!("surplus-{uuid}");
    let surplus_quote = if surplus_amount > 0 {
        quote_surplus_swap(
            &backrunner.solver,
            &fusion_order.to_token,
            surplus_amount,
            backrunner.resolver_address,
            surplus_state_label,
        )
        .await
    } else {
        None
    };

    // Extract surplus router and calldata from the surplus quote transaction.
    // The surplus router comes from the transaction's `to` address — same pattern as primary.
    let (surplus_router, surplus_calldata) =
        match surplus_quote.as_ref().and_then(|q| q.transaction()) {
            Some(tx) => (AlloyAddress::from_slice(tx.to().as_ref()), tx.data().to_vec()),
            None => (AlloyAddress::ZERO, vec![]),
        };

    // Build RawOrderFields for fillContractOrder.
    let raw_order = match build_raw_order_fields(fusion_order) {
        Ok(f) => f,
        Err(e) => {
            warn!(%uuid, order_id = %fusion_order.order_id, "bad order fields: {e}");
            return None;
        }
    };

    let signature = match hex_to_bytes(&fusion_order.signature) {
        Ok(b) => b,
        Err(e) => {
            warn!(%uuid, "bad signature: {e}");
            return None;
        }
    };
    let extension = match hex_to_bytes(&fusion_order.extension) {
        Ok(b) => b,
        Err(e) => {
            warn!(%uuid, "bad extension: {e}");
            return None;
        }
    };

    // Use the router address from the actual Fynd quote transaction (fynd_tx.to()).
    let fynd_router = AlloyAddress::from_slice(fynd_tx.to().as_ref());
    // Use Fynd output as the takerTraits threshold (max we'll pay).
    // The LOP computes the actual on-chain auction price and fills at that amount;
    // we keep amount_out_u128 - actual_price as surplus. This avoids us having to
    // replicate the gas-bump calculation that the contract applies to the auction rate.
    // TODO: query LOP.remainingInvalidatorForOrder(maker, orderHash) to get the remaining
    // making amount. For partially-filled orders, use remaining as fill_amount AND requote
    // Fynd for only the remaining tokens. Currently we fill the full making_amount which
    // causes swap calldata mismatch when the order is only partially fillable.
    let params = SettleParams {
        order_fields: &raw_order,
        signature: &signature,
        extension: &extension,
        taking_amount: amount_out_u128,
        fill_amount: raw_order.making_amount,
        router: fynd_router,
        primary_swap_calldata: fynd_tx.data(),
        surplus_calldata: &surplus_calldata,
        resolver_address: backrunner.resolver_address,
    };
    let _ = surplus_router; // surplus always uses the same Fynd router as primary
    let settle_data = build_settle_calldata(&params);

    let raw_tx = RawTx {
        to: Some(backrunner.resolver_address),
        value: alloy::primitives::U256::ZERO,
        data: settle_data,
        gas_limit: 500_000,
        max_fee_per_gas: u128::from(*base_fee) * 2 + 1_000_000_000,
        max_priority_fee_per_gas: 100_000_000, // 0.1 gwei tip
    };

    let expected_profit = alloy::primitives::I256::try_from(
        i128::try_from(surplus_amount).unwrap_or(i128::MAX),
    )
    .unwrap_or_default();

    debug!(
        %uuid,
        block_number,
        solve_time_ms,
        orders_quoted,
        taking_amount,
        amount_out_gross,
        amount_out_net = amount_out_u128,
        surplus = surplus_amount,
        "backrun candidate built",
    );

    Some(BackrunTx { tx: raw_tx, expected_profit_wei: expected_profit, expected_gas: 300_000 })
}

async fn try_evaluate(
    backrunner: &Backrunner,
    uuid: Uuid,
    iter: &PendingIteration,
    active_orders: &[&FusionOrder],
) -> Result<Option<fynd_core::Quote>, EvaluateError> {
    let tx_inputs: Vec<TxInput> = iter
        .txs
        .iter()
        .enumerate()
        .map(|(i, tx)| to_tx_input(tx, i as u64))
        .collect();

    let target_header = build_block_header(&iter.block);
    let label = format!("backrun-{uuid}");

    let pending_update = {
        let mut guard = backrunner.pending.lock().await;
        match guard.generate_pending_update(&tx_inputs, target_header, label.clone()).await {
            Ok(update) => update,
            Err(PendingError::ParentNotYetConfirmed { needed, current }) => {
                debug!(needed, current, "parent block not yet confirmed");
                return Ok(None);
            }
            Err(e) => return Err(EvaluateError::Pending(e)),
        }
    };

    let states = pending_update.update.states;
    let valid_until = iter.block.block_number;

    backrunner
        .solver
        .market_data()
        .register_labeled_state(label.clone(), states, valid_until)
        .await;

    let fynd_orders: Vec<Order> = active_orders
        .iter()
        .filter_map(|o| fusion_order_to_fynd(o, backrunner.resolver_address))
        .collect();

    let options = QuoteOptions::default()
        .with_state_label(label.clone())
        .with_encoding_options(EncodingOptions::new(backrunner.slippage));
    let request = QuoteRequest::new(fynd_orders, options);
    let quote_result = backrunner.solver.quote(request).await;

    backrunner.solver.market_data().remove_labeled_state(&label).await;

    Ok(Some(quote_result.map_err(EvaluateError::Solve)?))
}

/// Quotes a surplus→WETH swap via Fynd. Returns the `OrderQuote` if a route exists.
///
/// Returns `None` if `token_in` is WETH (direct unwrap; no swap needed) or no route found.
/// Uses 100% slippage (minAmountOut = 0) so amountIn can be patched on-chain safely.
async fn quote_surplus_swap(
    solver: &Solver,
    token_in_hex: &str,
    surplus_amount: u128,
    resolver_address: AlloyAddress,
    state_label: fynd_core::StateLabel,
) -> Option<OrderQuote> {
    const WETH_HEX: &str = "0xC02aAA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
    if token_in_hex.eq_ignore_ascii_case(WETH_HEX) {
        return None; // surplus WETH unwrapped directly — no swap needed
    }
    let from_bytes = parse_address(token_in_hex).ok()?;
    let to_bytes = parse_address(WETH_HEX).ok()?;
    let sender = TychoBytes::from(resolver_address.as_slice().to_vec());
    let order = Order::new(
        from_bytes,
        to_bytes,
        BigUint::from(surplus_amount),
        OrderSide::Sell,
        sender,
    );
    // 100% slippage → minAmountOut = 0; resolver patches amountIn on-chain.
    let options = QuoteOptions::default()
        .with_state_label(state_label)
        .with_encoding_options(EncodingOptions::new(1.0));
    let request = QuoteRequest::new(vec![order], options);
    match solver.quote(request).await {
        Ok(quote) => quote.into_orders().into_iter().next(),
        Err(e) => {
            debug!("surplus swap quote failed: {e}");
            None
        }
    }
}

/// Converts a [`FusionOrder`] to a fynd [`Order`].
///
/// Returns `None` when the token address is malformed — these are silently skipped.
fn fusion_order_to_fynd(fusion: &FusionOrder, resolver_address: AlloyAddress) -> Option<Order> {
    let from_bytes = parse_address(&fusion.from_token)
        .map_err(|e| tracing::warn!(order_id = %fusion.order_id, "bad from_token: {e}"))
        .ok()?;
    let to_bytes = parse_address(&fusion.to_token)
        .map_err(|e| tracing::warn!(order_id = %fusion.order_id, "bad to_token: {e}"))
        .ok()?;
    let amount = BigUint::from(fusion.making_amount);
    let sender = TychoBytes::from(resolver_address.as_slice().to_vec());
    Some(
        Order::new(from_bytes, to_bytes, amount, OrderSide::Sell, sender)
            .with_id(fusion.order_id.clone()),
    )
}

fn build_raw_order_fields(fusion: &FusionOrder) -> anyhow::Result<RawOrderFields> {
    use alloy::primitives::U256;
    use std::str::FromStr;
    Ok(RawOrderFields {
        salt: U256::from_str(&fusion.salt).unwrap_or_default(),
        maker: address_str_to_u256(&fusion.maker_address)?,
        receiver: address_str_to_u256(&fusion.receiver_address)?,
        maker_asset: address_str_to_u256(&fusion.from_token)?,
        taker_asset: address_str_to_u256(&fusion.to_token)?,
        making_amount: U256::from(fusion.making_amount),
        taking_amount: U256::from(fusion.auction_end_amount),
        maker_traits: U256::from_str(&fusion.maker_traits).unwrap_or_default(),
    })
}

fn address_str_to_u256(hex: &str) -> anyhow::Result<alloy::primitives::U256> {
    let addr = parse_address(hex)?;
    Ok(alloy::primitives::U256::from_be_slice(addr.as_ref()))
}

fn hex_to_bytes(s: &str) -> anyhow::Result<Vec<u8>> {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    if stripped.is_empty() {
        return Ok(vec![]);
    }
    hex::decode(stripped).map_err(|e| anyhow::anyhow!("hex decode: {e}"))
}

/// Decodes a `0x`-prefixed 20-byte address hex string into `TychoBytes`.
fn parse_address(hex_str: &str) -> anyhow::Result<TychoBytes> {
    let stripped = hex_str
        .strip_prefix("0x")
        .ok_or_else(|| anyhow::anyhow!("missing 0x prefix: {hex_str}"))?;
    let raw = hex::decode(stripped)
        .map_err(|e| anyhow::anyhow!("hex-decode failed for {hex_str}: {e}"))?;
    anyhow::ensure!(raw.len() == 20, "expected 20 bytes, got {}: {hex_str}", raw.len());
    Ok(TychoBytes::from(raw))
}

fn to_tx_input(tx: &ExecutedTx, index: u64) -> TxInput {
    let logs: Vec<LogInput> = tx
        .logs
        .iter()
        .enumerate()
        .map(|(i, log)| {
            let address = TychoBytes::from(log.address.as_slice().to_vec());
            let topics: Vec<TychoBytes> = log
                .data
                .topics()
                .iter()
                .map(|t| TychoBytes::from(t.as_slice().to_vec()))
                .collect();
            let data = TychoBytes::from(log.data.data.as_ref().to_vec());
            LogInput::new(address, topics, data, u32::try_from(i).unwrap_or(u32::MAX))
        })
        .collect();

    TxInput::new(
        TychoBytes::from(tx.tx_hash.as_slice().to_vec()),
        TychoBytes::from(tx.from.as_slice().to_vec()),
        tx.to
            .map(|a| TychoBytes::from(a.as_slice().to_vec()))
            .unwrap_or_default(),
        index,
        logs,
        tx.status,
    )
}

fn build_block_header(block: &BlockEnv) -> BlockHeader {
    BlockHeader {
        number: block.block_number,
        hash: TychoBytes::default(),
        parent_hash: TychoBytes::default(),
        timestamp: block.block_timestamp,
        revert: false,
        partial_block_index: None,
    }
}
