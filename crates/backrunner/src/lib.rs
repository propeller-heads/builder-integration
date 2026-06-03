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

use alloy::primitives::{Address as AlloyAddress, I256, U256};

use abi::{build_settle_calldata, SettleParams};
use builder_types::{BackrunCandidate, BackrunTx, BlockEnv, BuildEvent, ExecutedTx, PostState, RawTx};
use client::{build_order_fields, hex_to_bytes, OneinchClient};
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

/// Configuration for a [`Backrunner`] instance.
#[derive(Debug, Clone)]
pub struct BackrunnerConfig {
    /// Chain slug: `"ethereum"`, `"base"`, `"arbitrum"`, `"bsc"`, `"zksync"`, `"unichain"`.
    pub chain: String,
    /// Tycho WebSocket host (e.g. `"app.propellerheads.xyz"`).
    pub tycho_url: String,
    /// Ethereum JSON-RPC URL used for gas price fetching and on-chain order queries.
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
    /// How often to refresh the 1inch Fusion orderbook.
    ///
    /// Shorter intervals are more reactive to new orders but increase API load.
    /// Default: 12 seconds (one Ethereum block).
    pub orderbook_interval: Duration,
    /// When `true`, issues a static `eth_call` to verify the exact on-chain taking amount
    /// before submitting a fill. Adds one RPC round-trip per profitable order candidate.
    ///
    /// Useful for debugging taking-amount discrepancies. Off by default for production
    /// throughput.
    pub verify_onchain_taking: bool,
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
    #[error("failed to initialize HTTP clients: {0}")]
    OneinchClient(#[from] anyhow::Error),
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
    /// Receiver for the current set of live Fusion orders (refreshed per `orderbook_interval`).
    orders_rx: watch::Receiver<Arc<Vec<FusionOrder>>>,
    pub(crate) resolver_address: AlloyAddress,
    pub(crate) slippage: f64,
    oneinch: Arc<OneinchClient>,
    verify_onchain_taking: bool,
}

impl Backrunner {
    /// Builds a [`Backrunner`] and waits for the market-data snapshot to arrive.
    pub async fn build(config: BackrunnerConfig) -> Result<Self, BuildError> {
        let chain = parse_chain(&config.chain)?;

        let oneinch = Arc::new(OneinchClient::new(config.chain_id, config.rpc_url.clone())?);

        let builder = FyndBuilder::new(
            chain,
            config.tycho_url,
            config.rpc_url,
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
        // will populate it on the first poll.
        let (orders_tx, orders_rx) = watch::channel(Arc::new(Vec::new()));
        let market_data_for_orders = solver.market_data();
        tokio::spawn(run_orderbook(
            Arc::clone(&oneinch),
            orders_tx,
            market_data_for_orders,
            config.orderbook_interval,
        ));

        Ok(Self {
            solver,
            pending: tokio::sync::Mutex::new(pending),
            orders_rx,
            resolver_address: config.resolver_address,
            slippage: config.slippage,
            oneinch,
            verify_onchain_taking: config.verify_onchain_taking,
        })
    }

    /// Returns the number of live Fusion orders currently held by the orderbook poller.
    ///
    /// Zero until the first poll completes.
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
            self.handle_event(&mut pending, &candidates, event).await;
        }

        tracing::info!("event channel closed, backrunner shutting down");
    }

    async fn handle_event(
        &self,
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
                let candidate = self.evaluate_backrun(uuid, iter, state).await;
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
        &self,
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

        let orders = self.orders_rx.borrow().clone();
        let block_ts = iter.block.block_timestamp;

        let active: Vec<&FusionOrder> =
            orders.iter().filter(|o| amount_at_timestamp(o, block_ts).is_some()).collect();

        if active.is_empty() {
            debug!(%uuid, "no active Fusion orders at block timestamp");
            return None;
        }

        // Query on-chain remaining making amount for each active order (concurrent).
        let remaining_amounts: Vec<U256> =
            futures::future::join_all(active.iter().map(|&order| {
                let oneinch = Arc::clone(&self.oneinch);
                async move { oneinch.query_remaining_making_amount(order).await }
            }))
            .await;

        // Build a map from order_id → adjusted FusionOrder (making_amount = min(remaining, original)).
        // Orders where remaining = 0 (fully filled/cancelled) are dropped.
        let adjusted: HashMap<String, FusionOrder> = active
            .iter()
            .zip(remaining_amounts.iter())
            .filter_map(|(&order, &remaining)| {
                if remaining.is_zero() {
                    debug!(order_id = %order.order_id, "order fully filled on-chain, skipping");
                    return None;
                }
                let fill_making = remaining.min(order.making_amount);
                if fill_making < order.making_amount {
                    debug!(
                        order_id = %order.order_id,
                        making_amount = %order.making_amount,
                        remaining = %remaining,
                        "partial fill detected — quoting Fynd for remaining amount",
                    );
                }
                Some((order.order_id.clone(), FusionOrder { making_amount: fill_making, ..order.clone() }))
            })
            .collect();

        if adjusted.is_empty() {
            debug!(%uuid, "all active orders are fully filled on-chain");
            return None;
        }

        let adjusted_refs: Vec<&FusionOrder> = adjusted.values().collect();

        let (quote, pending_label) =
            match self.try_evaluate(uuid, &iter, &adjusted_refs).await {
                Ok(Some(pair)) => pair,
                Ok(None) => {
                    debug!(%uuid, "parent block not yet confirmed, skipping");
                    return None;
                }
                Err(e) => {
                    warn!(%uuid, error = %e, "evaluate_backrun failed");
                    return None;
                }
            };

        // Map from order_id → ORIGINAL FusionOrder (needed for pro-rating the taking amount).
        let order_map: HashMap<&str, &FusionOrder> =
            active.iter().map(|o| (o.order_id.as_str(), *o)).collect();

        let ctx = BackrunContext {
            uuid,
            block_ts,
            base_fee: iter.block.base_fee_per_gas,
            block_number: iter.block.block_number,
            solve_time_ms: quote.solve_time_ms(),
            orders_quoted: adjusted.len(),
            backrunner: self,
            state_label: pending_label.clone(),
        };

        let mut backrun_txs: Vec<BackrunTx> = Vec::new();

        for order_quote in quote.orders() {
            let Some(&fusion_order) = order_map.get(order_quote.order_id()) else { continue };
            let fill_amount = adjusted
                .get(order_quote.order_id())
                .map(|o| o.making_amount)
                .unwrap_or(fusion_order.making_amount);

            if let Some(backrun_tx) =
                build_backrun_tx(&ctx, fusion_order, order_quote, fill_amount).await
            {
                backrun_txs.push(backrun_tx);
            }
        }

        // Always clean up the pending state label regardless of outcome.
        self.solver.market_data().remove_labeled_state(&pending_label).await;

        if backrun_txs.is_empty() {
            return None;
        }

        Some(BackrunCandidate {
            uuid,
            block_number: iter.block.block_number,
            txs: backrun_txs,
        })
    }

    async fn try_evaluate(
        &self,
        uuid: Uuid,
        iter: &PendingIteration,
        active_orders: &[&FusionOrder],
    ) -> Result<Option<(fynd_core::Quote, String)>, EvaluateError> {
        let tx_inputs: Vec<TxInput> = iter
            .txs
            .iter()
            .enumerate()
            .map(|(i, tx)| to_tx_input(tx, i as u64))
            .collect();

        let target_header = build_block_header(&iter.block);
        let label = format!("backrun-{uuid}");

        let pending_update = {
            let mut guard = self.pending.lock().await;
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

        self.solver
            .market_data()
            .register_labeled_state(label.clone(), states, valid_until)
            .await;

        let fynd_orders: Vec<Order> = active_orders
            .iter()
            .filter_map(|o| fusion_order_to_fynd(o, self.resolver_address))
            .collect();

        let options = QuoteOptions::default()
            .with_state_label(label.clone())
            .with_encoding_options(EncodingOptions::new(self.slippage));
        let request = QuoteRequest::new(fynd_orders, options);
        let quote_result = self.solver.quote(request).await;

        // On quote error, clean up the label and propagate — caller won't hold the label.
        match quote_result {
            Ok(quote) => Ok(Some((quote, label))),
            Err(e) => {
                self.solver.market_data().remove_labeled_state(&label).await;
                Err(EvaluateError::Solve(e))
            }
        }
    }
}

/// Background task: polls 1inch Fusion for active orders at the configured interval.
///
/// After each fetch, token decimals are patched from Tycho's registry (which has exact
/// on-chain values for every indexed token) to fix any fallback-18 from the 1inch API.
async fn run_orderbook(
    client: Arc<OneinchClient>,
    orders_tx: watch::Sender<Arc<Vec<FusionOrder>>>,
    market_data: MarketData,
    interval: Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;
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

/// Iteration-level context shared across all per-order calls inside [`evaluate_backrun`].
struct BackrunContext<'a> {
    uuid: Uuid,
    block_ts: u64,
    base_fee: u64,
    block_number: u64,
    solve_time_ms: u64,
    orders_quoted: usize,
    backrunner: &'a Backrunner,
    /// The pending-state label registered in `try_evaluate`, used for the surplus swap quote.
    state_label: String,
}

/// Gates on profitability, then delegates settlement construction to [`assemble_backrun_tx`].
async fn build_backrun_tx(
    ctx: &BackrunContext<'_>,
    fusion_order: &FusionOrder,
    order_quote: &OrderQuote,
    fill_amount: U256,
) -> Option<BackrunTx> {
    let BackrunContext { uuid, block_ts, base_fee, backrunner, .. } = ctx;
    let fynd_tx = order_quote.transaction()?;

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
        "order passed profitability filter");

    // Optional pre-flight: static-call extension.getTakingAmount to verify the exact
    // on-chain price. Enabled only when `verify_onchain_taking` is set — adds one RPC
    // round-trip per order and is off by default for production throughput.
    //
    // We pass the pending block timestamp so the extension sees the same elapsed time as our
    // off-chain estimate — without it the eth_call runs at the confirmed block timestamp
    // (12 s earlier), which can land before the auction start and return the full start price.
    let onchain_taking = if backrunner.verify_onchain_taking {
        backrunner.oneinch.query_onchain_taking_amount(
            fusion_order,
            fill_amount,
            fill_amount,
            backrunner.resolver_address,
            *block_ts,
        ).await
    } else {
        None
    };

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
        None => {}
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
    let BackrunContext { uuid, base_fee, block_number, solve_time_ms, orders_quoted, backrunner, state_label, .. } =
        ctx;

    let surplus_amount = amount_out.saturating_sub(taking_estimate);
    let surplus_quote = if !surplus_amount.is_zero() {
        quote_surplus_swap(
            &backrunner.solver,
            &fusion_order.to_token,
            surplus_amount,
            backrunner.resolver_address,
            state_label.clone(),
        )
        .await
    } else {
        None
    };

    let surplus_calldata = surplus_quote
        .as_ref()
        .and_then(|q| q.transaction())
        .map_or_else(Vec::new, |tx| tx.data().to_vec());

    let raw_order = match build_order_fields(fusion_order) {
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

    let expected_profit = I256::try_from(surplus_amount).unwrap_or(I256::MAX);

    debug!(%uuid, block_number, solve_time_ms, orders_quoted,
        amount_out = %amount_out,
        taking_estimate = %taking_estimate,
        surplus = %surplus_amount,
        "backrun candidate built");

    Some(BackrunTx { tx: raw_tx, expected_profit_wei: expected_profit, expected_gas: 300_000 })
}

/// Quotes a surplus→WETH swap via Fynd using the same pending state label as the main quote.
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
    let order = Order::new(from_bytes, to_bytes, amount, OrderSide::Sell, sender);
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
