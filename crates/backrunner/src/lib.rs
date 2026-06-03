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

use alloy::primitives::{Address as AlloyAddress, U256};

use abi::{build_settle_calldata, RawOrderFields, SettleParams};
use builder_types::{BackrunCandidate, BackrunTx, BlockEnv, BuildEvent, ExecutedTx, PostState, RawTx};
use client::OneinchClient;
use fynd_core::{
    feed::market_data::MarketData, EncodingOptions, FyndBuilder, MarketEvent, Order, OrderQuote,
    OrderSide, PendingBlockProcessor, PendingError, QuoteOptions, QuoteRequest, Solver, SolveError,
    SolverBuildError,
};
use uniswap_v3_core::processor::UniswapV3Processor;
use num_bigint::BigUint;
use order::{amount_at_timestamp, compute_gas_bump, is_gtc_order, onchain_taking_amount, FusionOrder};
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, error, warn};
use tycho_simulation::tycho_client::feed::BlockHeader;
use tycho_simulation::tycho_common::{
    models::{blockchain::{LogInput, TxInput}, token::Token, Address},
    Bytes as TychoBytes,
};
use uuid::Uuid;

// ABI for querying the Fusion Dutch-auction extension via LOP.simulate().
// All Order address fields are `uint256` to match 1inch's packed `Address` type.
// The extension's getTakingAmount reads transient storage set by the LOP before
// calling it, so it cannot be called directly — we route through LOP.simulate()
// which calls the extension with msg.sender == LOP.
alloy::sol! {
    struct FusionExtOrder {
        uint256 salt;
        uint256 maker;
        uint256 receiver;
        uint256 makerAsset;
        uint256 takerAsset;
        uint256 makingAmount;
        uint256 takingAmount;
        uint256 makerTraits;
    }

    interface IAmountGetter {
        function getTakingAmount(
            FusionExtOrder order,
            bytes extension,
            bytes32 orderHash,
            address taker,
            uint256 makingAmount,
            uint256 remainingMakingAmount,
            bytes extraData
        ) external view returns (uint256);
    }

    interface IOrderMixin {
        /// Calls `target.call(data)` and reverts with `SimulationResults(success, result)`.
        function simulate(address target, bytes data) external;
        error SimulationResults(bool success, bytes result);
    }
}

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
/// 1inch LOP v4 on Ethereum mainnet.
const LOP_V4: AlloyAddress =
    alloy::primitives::address!("111111125421cA6dc452d289314280a0f8842A65");

/// `remainingInvalidatorForOrder(address,bytes32)` selector on LOP v4.
/// Returns the remaining making amount for partially-fillable orders.
/// Returns 0 if fully filled/cancelled, `U256::MAX` if never partially filled.
const REMAINING_SELECTOR: [u8; 4] = [0x10, 0xad, 0x2c, 0x8b];

/// Converts a [`BigUint`] to [`U256`], clamping to [`U256::MAX`] for values that don't fit.
pub(crate) fn biguint_to_u256(b: &BigUint) -> U256 {
    let bytes = b.to_bytes_be();
    if bytes.is_empty() {
        U256::ZERO
    } else if bytes.len() > 32 {
        U256::MAX
    } else {
        U256::from_be_slice(&bytes)
    }
}

pub struct Backrunner {
    solver: Solver,
    pending: tokio::sync::Mutex<PendingBlockProcessor>,
    /// Receiver for the current set of live Fusion orders (refreshed ~every 12s).
    orders_rx: watch::Receiver<Arc<Vec<FusionOrder>>>,
    pub(crate) resolver_address: AlloyAddress,
    pub(crate) slippage: f64,
    /// RPC URL for direct on-chain queries (remaining amount, etc.).
    rpc_url: String,
    /// Shared HTTP client for all RPC calls — reuses connection pool across requests.
    rpc_client: reqwest::Client,
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
            config.rpc_url.clone(),
            config.protocols,
            config.min_tvl,
        )
        .with_pending_indexer(
            "uniswap_v3",
            Box::new(UniswapV3Processor::new(chain, "uniswap_v3".to_string())),
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

        let rpc_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| BuildError::OneinchClient(format!("building RPC client: {e}")))?;

        Ok(Self {
            solver,
            pending: tokio::sync::Mutex::new(pending),
            orders_rx,
            resolver_address: config.resolver_address,
            slippage: config.slippage,
            rpc_url: config.rpc_url.clone(),
            rpc_client,
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
                let max_bump = filtered.iter()
                    .map(|o| {
                        let end = o.auction_end_amount.max(U256::ONE);
                        o.auction_start_amount.saturating_sub(o.auction_end_amount)
                            .saturating_mul(U256::from(10_000u64)) / end
                    })
                    .max()
                    .unwrap_or(U256::ZERO);
                debug!(order_count = filtered.len(), max_init_bump_bps = %max_bump, "orderbook refreshed");
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

    // Query on-chain remaining making amount for each active order (concurrent).
    // This detects partially-filled orders so we quote Fynd for the correct amount.
    let rpc_client = backrunner.rpc_client.clone();
    let rpc_url = backrunner.rpc_url.clone();
    let remaining_amounts: Vec<U256> =
        futures::future::join_all(active.iter().map(|o| {
            let client = rpc_client.clone();
            let url = rpc_url.clone();
            async move { query_remaining_making_amount(&client, &url, o).await }
        }))
        .await;

    // Build an adjusted FusionOrder per active order using the on-chain remaining amount.
    // Orders where remaining = 0 (fully filled/cancelled) are dropped.
    let adjusted: Vec<FusionOrder> = active
        .iter()
        .zip(remaining_amounts.iter())
        .filter_map(|(&order, &remaining)| {
            if remaining.is_zero() {
                debug!(order_id = %order.order_id, "order fully filled on-chain, skipping");
                return None;
            }
            if remaining < order.making_amount {
                debug!(
                    order_id = %order.order_id,
                    making_amount = %order.making_amount,
                    remaining = %remaining,
                    "partial fill detected — quoting Fynd for remaining amount",
                );
            }
            // Use min(remaining, making_amount) as the fill amount for Fynd.
            Some(FusionOrder { making_amount: remaining.min(order.making_amount), ..order.clone() })
        })
        .collect();

    if adjusted.is_empty() {
        debug!(%uuid, "all active orders are fully filled on-chain");
        return None;
    }

    let adjusted_refs: Vec<&FusionOrder> = adjusted.iter().collect();

    // Build a map from order_id → remaining amount for use during settlement.
    let remaining_map: HashMap<&str, U256> = active
        .iter()
        .zip(remaining_amounts.iter())
        .map(|(&o, &r)| (o.order_id.as_str(), r.min(o.making_amount)))
        .collect();

    let quote = match try_evaluate(backrunner, uuid, &iter, &adjusted_refs).await {
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

    // Build a map from order_id → ORIGINAL FusionOrder for taking_amount computation.
    let order_map: HashMap<&str, &FusionOrder> =
        active.iter().map(|o| (o.order_id.as_str(), *o)).collect();

    let ctx = BackrunContext {
        uuid,
        block_ts,
        base_fee: iter.block.base_fee_per_gas,
        block_number: iter.block.block_number,
        solve_time_ms: quote.solve_time_ms(),
        orders_quoted: adjusted.len(),
        backrunner,
    };

    let mut backrun_txs: Vec<BackrunTx> = Vec::new();

    for order_quote in quote.orders() {
        let Some(fynd_tx) = order_quote.transaction() else { continue };
        let Some(&fusion_order) = order_map.get(order_quote.order_id()) else { continue };
        let fill_amount: U256 = remaining_map
            .get(order_quote.order_id())
            .copied()
            .unwrap_or(fusion_order.making_amount);

        if let Some(backrun_tx) =
            build_backrun_tx(&ctx, fusion_order, order_quote, fynd_tx, fill_amount).await
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

/// Gates on profitability, then delegates settlement construction to [`assemble_backrun_tx`].
async fn build_backrun_tx(
    ctx: &BackrunContext<'_>,
    fusion_order: &FusionOrder,
    order_quote: &OrderQuote,
    fynd_tx: &fynd_core::Transaction,
    fill_amount: U256,
) -> Option<BackrunTx> {
    let BackrunContext { uuid, block_ts, base_fee, backrunner, .. } = ctx;

    // Exact on-chain taking amount estimate:
    //   rateBump = max(0, auctionBump − gasBump)      ← gas subtracts from rate
    //   withFees = ceil(floor × (1e5 + totalFees) / 1e5)
    //   taking   = ceil(withFees × (1e7 + rateBump) / 1e7)
    let taking_amount_full = onchain_taking_amount(fusion_order, *block_ts, *base_fee)?;
    let taking_amount = if fill_amount < fusion_order.making_amount && !fusion_order.making_amount.is_zero()
    {
        taking_amount_full.saturating_mul(fill_amount) / fusion_order.making_amount
    } else {
        taking_amount_full
    };

    let elapsed_secs = block_ts.saturating_sub(fusion_order.auction_start_time);
    let gas_bump = compute_gas_bump(fusion_order, *base_fee);
    debug!(%uuid, order_id = %fusion_order.order_id,
        floor = %fusion_order.auction_end_amount,
        start_amount = %fusion_order.auction_start_amount,
        elapsed_secs, base_fee, gas_bump,
        total_fees_1e5 = fusion_order.total_fees_1e5,
        points_count = fusion_order.points.len(),
        taking_estimate = %taking_amount,
        "auction price estimate");

    let amount_out = biguint_to_u256(order_quote.amount_out());
    if amount_out.is_zero() {
        return None;
    }

    if amount_out < taking_amount {
        debug!(%uuid, order_id = %fusion_order.order_id,
            amount_out = %amount_out,
            taking_amount = %taking_amount,
            "swap output below auction price, skipping");
        return None;
    }

    debug!(%uuid, order_id = %fusion_order.order_id,
        amount_out = %amount_out,
        taking_estimate = %taking_amount,
        margin_bps = (amount_out.saturating_sub(taking_amount) * U256::from(10_000u64)
            / taking_amount.max(U256::ONE)).saturating_to::<u64>(),
        "order passed profitability filter — querying on-chain taking amount");

    // Pre-flight: static-call extension.getTakingAmount to get the exact on-chain price.
    // Our Rust estimate can diverge from the contract due to arithmetic differences or
    // resolver fees added inside getTakingAmount. Checking here avoids failed fill simulations.
    //
    // We pass the pending block timestamp so the extension sees the same elapsed time as our
    // off-chain estimate — without it the eth_call runs at the confirmed block timestamp
    // (12 s earlier), which can land before the auction start and return the full start price.
    let onchain_taking = query_onchain_taking_amount(
        &backrunner.rpc_client,
        &backrunner.rpc_url,
        fusion_order,
        fill_amount,
        fill_amount,
        backrunner.resolver_address,
        *block_ts,
    )
    .await;

    match onchain_taking {
        Some(onchain) if amount_out < onchain => {
            debug!(%uuid, order_id = %fusion_order.order_id,
                amount_out = %amount_out,
                onchain_taking = %onchain,
                shortfall = %onchain.saturating_sub(amount_out),
                taking_estimate = %taking_amount,
                extension_hex = %fusion_order.extension,
                block_ts, base_fee,
                "fynd output below on-chain taking amount, skipping");
            return None;
        }
        Some(onchain) => {
            debug!(%uuid, order_id = %fusion_order.order_id,
                amount_out = %amount_out,
                onchain_taking = %onchain,
                surplus = %amount_out.saturating_sub(onchain),
                taking_estimate = %taking_amount,
                "on-chain taking amount verified, proceeding to fill");
        }
        None => debug!(%uuid, order_id = %fusion_order.order_id,
            "on-chain taking amount query failed, using estimate"),
    }

    assemble_backrun_tx(ctx, fusion_order, fynd_tx, fill_amount, amount_out, taking_amount).await
}

/// Builds the settlement transaction once profitability is confirmed.
///
/// Fetches a surplus→WETH quote, encodes the LOP fill calldata, and wraps everything
/// in a [`BackrunTx`].
async fn assemble_backrun_tx(
    ctx: &BackrunContext<'_>,
    fusion_order: &FusionOrder,
    fynd_tx: &fynd_core::Transaction,
    fill_amount: U256,
    amount_out: U256,
    taking_estimate: U256,
) -> Option<BackrunTx> {
    let BackrunContext { uuid, base_fee, block_number, solve_time_ms, orders_quoted, backrunner, .. } =
        ctx;

    let surplus_amount = amount_out.saturating_sub(taking_estimate);
    let surplus_quote = if !surplus_amount.is_zero() {
        quote_surplus_swap(
            &backrunner.solver,
            &fusion_order.to_token,
            surplus_amount,
            backrunner.resolver_address,
            format!("surplus-{uuid}"),
        )
        .await
    } else {
        None
    };

    let surplus_calldata = surplus_quote
        .as_ref()
        .and_then(|q| q.transaction())
        .map_or_else(Vec::new, |tx| tx.data().to_vec());

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

    // Fynd output is the takerTraits threshold (max we'll pay). LOP fills at the exact
    // auction price and the delta becomes surplus held by the resolver.
    let params = SettleParams {
        order_fields: &raw_order,
        signature: &signature,
        extension: &extension,
        taking_amount: amount_out,
        fill_amount,
        router: AlloyAddress::from_slice(fynd_tx.to().as_ref()),
        primary_swap_calldata: fynd_tx.data(),
        surplus_calldata: &surplus_calldata,
        resolver_address: backrunner.resolver_address,
    };
    let settle_data = build_settle_calldata(&params);

    let raw_tx = RawTx {
        to: Some(backrunner.resolver_address),
        value: U256::ZERO,
        data: settle_data,
        gas_limit: 500_000,
        max_fee_per_gas: u128::from(*base_fee) * 2 + 1_000_000_000,
        max_priority_fee_per_gas: 100_000_000,
    };

    let expected_profit = alloy::primitives::I256::try_from(
        i128::try_from(surplus_amount.saturating_to::<u128>()).unwrap_or(i128::MAX),
    )
    .unwrap_or_default();

    debug!(%uuid, block_number, solve_time_ms, orders_quoted,
        amount_out = %amount_out,
        taking_estimate = %taking_estimate,
        surplus = %surplus_amount,
        "backrun candidate built");

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
    surplus_amount: U256,
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
    let amount = {
        let bytes = surplus_amount.to_be_bytes::<32>();
        BigUint::from_bytes_be(&bytes)
    };
    let order = Order::new(
        from_bytes,
        to_bytes,
        amount,
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
    let amount = {
        let bytes = fusion.making_amount.to_be_bytes::<32>();
        BigUint::from_bytes_be(&bytes)
    };
    let sender = TychoBytes::from(resolver_address.as_slice().to_vec());
    Some(
        Order::new(from_bytes, to_bytes, amount, OrderSide::Sell, sender)
            .with_id(fusion.order_id.clone()),
    )
}

fn build_raw_order_fields(fusion: &FusionOrder) -> anyhow::Result<RawOrderFields> {
    use std::str::FromStr;
    Ok(RawOrderFields {
        salt: U256::from_str(&fusion.salt)
            .map_err(|e| anyhow::anyhow!("invalid order salt {:?}: {e}", fusion.salt))?,
        maker: address_str_to_u256(&fusion.maker_address)?,
        receiver: address_str_to_u256(&fusion.receiver_address)?,
        maker_asset: address_str_to_u256(&fusion.from_token)?,
        taker_asset: address_str_to_u256(&fusion.to_token)?,
        making_amount: fusion.making_amount,
        taking_amount: fusion.auction_end_amount,
        maker_traits: U256::from_str(&fusion.maker_traits)
            .map_err(|e| anyhow::anyhow!("invalid maker_traits {:?}: {e}", fusion.maker_traits))?,
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
pub(crate) fn parse_address(hex_str: &str) -> anyhow::Result<TychoBytes> {
    let stripped = hex_str
        .strip_prefix("0x")
        .ok_or_else(|| anyhow::anyhow!("missing 0x prefix: {hex_str}"))?;
    let raw = hex::decode(stripped)
        .map_err(|e| anyhow::anyhow!("hex-decode failed for {hex_str}: {e}"))?;
    anyhow::ensure!(raw.len() == 20, "expected 20 bytes, got {}: {hex_str}", raw.len());
    Ok(TychoBytes::from(raw))
}

/// Calls `LOP.remainingInvalidatorForOrder(maker, orderHash)` via a raw JSON-RPC
/// `eth_call` and returns the remaining making amount that can still be filled.
///
/// - Returns `full_making` when the order is fresh (LOP returns `U256::MAX`) or on error.
/// - Returns `0` when the order is fully filled or cancelled.
/// - Returns the actual remaining making amount otherwise (clamped to `full_making`).
async fn query_remaining_making_amount(
    client: &reqwest::Client,
    rpc_url: &str,
    order: &FusionOrder,
) -> U256 {
    let full = order.making_amount;

    // Parse maker address
    let maker: AlloyAddress = match order.maker_address.parse() {
        Ok(a) => a,
        Err(_) => return full,
    };

    // Parse order hash (bytes32)
    let hash_str = order.order_id.strip_prefix("0x").unwrap_or(&order.order_id);
    let hash_bytes = match hex::decode(hash_str) {
        Ok(b) if b.len() == 32 => b,
        _ => return full,
    };

    // ABI-encode: remainingInvalidatorForOrder(address maker, bytes32 orderHash)
    // = selector(4B) + address_padded(32B) + bytes32(32B)
    let mut calldata = Vec::with_capacity(68);
    calldata.extend_from_slice(&REMAINING_SELECTOR);
    calldata.extend_from_slice(&[0u8; 12]); // address left-padded to 32 bytes
    calldata.extend_from_slice(maker.as_slice());
    calldata.extend_from_slice(&hash_bytes);

    // Build raw JSON-RPC eth_call payload (no alloy provider needed)
    let lop_hex = format!("0x{}", hex::encode(LOP_V4.as_slice()));
    let data_hex = format!("0x{}", hex::encode(&calldata));
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_call",
        "params": [{"to": lop_hex, "data": data_hex}, "latest"],
        "id": 1
    });

    let Ok(resp) = client.post(rpc_url).json(&body).send().await else { return full };

    let json: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return full,
    };

    // Parse hex result → U256, clamped to full_making.
    let hex_result = match json["result"].as_str() {
        Some(s) => s.strip_prefix("0x").unwrap_or(s),
        None => return full,
    };

    if hex_result.len() < 64 {
        return full;
    }

    let Ok(raw) = hex::decode(&hex_result[hex_result.len() - 64..]) else { return full };

    let val = U256::from_be_slice(&raw);
    if val == U256::MAX {
        full // fresh order: never partially filled
    } else {
        val.min(full)
    }
}

/// Static-calls `extension.getTakingAmount(...)` to get the exact on-chain auction price.
///
/// Returns `None` on RPC error, malformed order data, or call revert (e.g. order expired).
/// Callers fall back to the off-chain estimate when `None` is returned.
async fn query_onchain_taking_amount(
    client: &reqwest::Client,
    rpc_url: &str,
    fusion_order: &FusionOrder,
    fill_making_amount: U256,
    remaining_making_amount: U256,
    resolver: AlloyAddress,
    pending_block_ts: u64,
) -> Option<U256> {
    use alloy::sol_types::{SolCall, SolError as _};
    use std::str::FromStr;

    // Extension contract address lives at hex chars [64:104] (bytes [32:52]).
    let ext_hex = fusion_order.extension.strip_prefix("0x").unwrap_or(&fusion_order.extension);
    if ext_hex.len() < 104 {
        return None;
    }
    let ext_addr_bytes = hex::decode(&ext_hex[64..104]).ok()?;
    let ext_addr = AlloyAddress::from_slice(&ext_addr_bytes);

    // Order hash as bytes32.
    let hash_str = fusion_order.order_id.strip_prefix("0x").unwrap_or(&fusion_order.order_id);
    let hash_bytes: [u8; 32] = hex::decode(hash_str).ok()?.try_into().ok()?;

    let extension_bytes = hex::decode(ext_hex).ok()?;

    // Extract the TakingAmountData section from the extension header.
    // Header is 32 bytes big-endian uint256; each section's end offset is packed in 32-bit chunks:
    //   bits [95:64]  = MakingAmountData end offset  (header bytes [20:24])
    //   bits [127:96] = TakingAmountData end offset  (header bytes [16:20])
    // Section begin = previous section's end.
    // The first 20 bytes of TakingAmountData are the getter address; the rest is extraData.
    let taking_extra_data: alloy::primitives::Bytes = (|| -> Option<_> {
        let hdr = extension_bytes.get(0..32)?;
        let making_end = u32::from_be_bytes(hdr[20..24].try_into().ok()?) as usize;
        let taking_end = u32::from_be_bytes(hdr[16..20].try_into().ok()?) as usize;
        let begin = 32 + making_end;
        let end   = 32 + taking_end;
        let section = extension_bytes.get(begin..end)?;
        // section[0:20] = getter address, section[20:] = extraData for getTakingAmount
        Some(alloy::primitives::Bytes::copy_from_slice(section.get(20..)?))
    })()
    .unwrap_or_default();

    // Build the inner getTakingAmount calldata.
    let inner = IAmountGetter::getTakingAmountCall {
        order: FusionExtOrder {
            salt: U256::from_str(&fusion_order.salt).unwrap_or_default(),
            maker: address_str_to_u256(&fusion_order.maker_address).ok()?,
            receiver: address_str_to_u256(&fusion_order.receiver_address).ok()?,
            makerAsset: address_str_to_u256(&fusion_order.from_token).ok()?,
            takerAsset: address_str_to_u256(&fusion_order.to_token).ok()?,
            makingAmount: fusion_order.making_amount,
            takingAmount: fusion_order.auction_end_amount,
            makerTraits: U256::from_str(&fusion_order.maker_traits).unwrap_or_default(),
        },
        extension: alloy::primitives::Bytes::from(extension_bytes),
        orderHash: alloy::primitives::FixedBytes::from(hash_bytes),
        taker: resolver,
        makingAmount: fill_making_amount,
        remainingMakingAmount: remaining_making_amount,
        extraData: taking_extra_data,
    };

    // Wrap in LOP.simulate(extension_addr, inner_calldata).
    // The LOP calls the extension with msg.sender == LOP and reverts with
    // SimulationResults(success, abi.encode(taking_amount)).
    let simulate_call = IOrderMixin::simulateCall {
        target: ext_addr,
        data: alloy::primitives::Bytes::from(inner.abi_encode()),
    };
    let lop_hex   = format!("0x{}", hex::encode(LOP_V4.as_slice()));
    let data_hex  = format!("0x{}", hex::encode(simulate_call.abi_encode()));

    // Pass the pending block timestamp so the extension sees the same elapsed time our
    // off-chain estimate used.  Without this, eth_call runs at the confirmed block
    // timestamp (≈12 s earlier), which can make the auction appear not yet started and
    // return the full start price — inflating the required taking amount.
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_call",
        "params": [
            {"to": lop_hex, "data": data_hex},
            "latest",
            {},
            {"time": format!("0x{:x}", pending_block_ts)}
        ],
        "id": 1
    });

    let resp = client.post(rpc_url).json(&body).send().await.ok()?;
    let json: serde_json::Value = resp.json().await.ok()?;

    // simulate() ALWAYS reverts — expected revert is SimulationResults(success, result).
    let err = json.get("error")?;
    let revert_data_str = err.get("data").and_then(|d| d.as_str()).unwrap_or("");
    let revert_hex = revert_data_str.strip_prefix("0x").unwrap_or(revert_data_str);
    let revert_bytes = hex::decode(revert_hex).ok()?;

    // Decode SimulationResults(bool success, bytes result).
    let sim = IOrderMixin::SimulationResults::abi_decode(&revert_bytes).ok()?;
    if !sim.success {
        debug!(
            order_id = %fusion_order.order_id,
            "getTakingAmount inner call reverted via simulate()",
        );
        return None;
    }

    // Inner return value is abi.encode(uint256) — taking amount.
    if sim.result.len() < 32 {
        return None;
    }
    Some(U256::from_be_slice(&sim.result[..32]))
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
