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

mod client;
mod order;

use std::{collections::HashMap, sync::Arc, time::Duration};

use builder_types::{BackrunCandidate, BlockEnv, BuildEvent, ExecutedTx, PostState};
use client::OneinchClient;
use fynd_core::{
    feed::market_data::MarketData, FyndBuilder, MarketEvent, Order, OrderSide,
    PendingBlockProcessor, PendingError, QuoteOptions, QuoteRequest, Solver, SolveError,
    SolverBuildError,
};
use num_bigint::BigUint;
use order::{amount_at_timestamp, is_gtc_order, FusionOrder};
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, error, warn};
use tycho_simulation::tycho_client::feed::BlockHeader;
use tycho_simulation::tycho_common::{
    models::blockchain::{LogInput, TxInput},
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
        .algorithm("most_liquid");

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

        tokio::spawn(run_orderbook(Arc::new(oneinch), orders_tx));

        Ok(Self { solver, pending: tokio::sync::Mutex::new(pending), orders_rx })
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
        view.state_label()?.parse().ok()
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
async fn run_orderbook(
    client: Arc<OneinchClient>,
    orders_tx: watch::Sender<Arc<Vec<FusionOrder>>>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(12));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;
        match client.fetch_active_orders().await {
            Ok(orders) => {
                let filtered: Vec<FusionOrder> =
                    orders.into_iter().filter(|o| !is_gtc_order(o)).collect();
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

    match try_evaluate(backrunner, uuid, &iter, &active).await {
        Ok(Some(quote)) => {
            debug!(
                %uuid,
                block_number = iter.block.block_number,
                solve_time_ms = quote.solve_time_ms(),
                orders_quoted = active.len(),
                "backrun quote received",
            );
        }
        Ok(None) => {
            debug!(%uuid, "parent block not yet confirmed, skipping");
        }
        Err(e) => {
            warn!(%uuid, error = %e, "evaluate_backrun failed");
        }
    }

    None
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

    let fynd_orders: Vec<Order> =
        active_orders.iter().filter_map(|o| fusion_order_to_fynd(o)).collect();

    let options = QuoteOptions::default().with_state_label(label.clone());
    let request = QuoteRequest::new(fynd_orders, options);
    let quote_result = backrunner.solver.quote(request).await;

    backrunner.solver.market_data().remove_labeled_state(&label).await;

    Ok(Some(quote_result.map_err(EvaluateError::Solve)?))
}

/// Converts a [`FusionOrder`] to a fynd [`Order`].
///
/// Returns `None` when the token address is malformed — these are silently skipped.
fn fusion_order_to_fynd(fusion: &FusionOrder) -> Option<Order> {
    let from_bytes = parse_address(&fusion.from_token)
        .map_err(|e| tracing::warn!(order_id = %fusion.order_id, "bad from_token: {e}"))
        .ok()?;
    let to_bytes = parse_address(&fusion.to_token)
        .map_err(|e| tracing::warn!(order_id = %fusion.order_id, "bad to_token: {e}"))
        .ok()?;
    let amount = BigUint::from(fusion.making_amount);
    Some(
        Order::new(from_bytes, to_bytes, amount, OrderSide::Sell, TychoBytes::zero(20))
            .with_id(fusion.order_id.clone()),
    )
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
