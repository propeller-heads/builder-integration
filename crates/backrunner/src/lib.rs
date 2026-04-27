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
//!
//! // Feed events from your builder loop:
//! event_tx.send(BuildEvent::IterationStart { uuid, block }).await?;
//! ```
//!
//! **External process** (standalone binary + message queue): not yet implemented.
//! Run `backrunner --help` for the binary's current CLI.

use std::collections::HashMap;
use std::time::Duration;

use builder_types::{BackrunCandidate, BlockEnv, BuildEvent, ExecutedTx, PostState};
use fynd_core::{FyndBuilder, Solver, SolverBuildError};
use tokio::sync::{mpsc, watch};
use tracing::{debug, error, warn};
use tycho_simulation::tycho_common::models::Chain;
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
}

struct PendingIteration {
    block: BlockEnv,
    txs: Vec<ExecutedTx>,
}

/// Consumes [`BuildEvent`]s from a block builder and publishes [`BackrunCandidate`]s.
///
/// See the [crate-level docs](crate) for integration examples.
pub struct Backrunner {
    solver: Solver,
}

impl Backrunner {
    /// Builds a [`Backrunner`] and waits for the market-data snapshot to arrive.
    pub async fn build(config: BackrunnerConfig) -> Result<Self, BuildError> {
        let chain = parse_chain(&config.chain)?;

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

        let solver = builder.build()?;

        solver
            .wait_until_ready(config.ready_timeout)
            .await
            .map_err(|_| BuildError::MarketDataTimeout)?;

        Ok(Self { solver })
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
            handle_event(&self.solver, &mut pending, &candidates, event).await;
        }

        tracing::info!("event channel closed, backrunner shutting down");
    }
}

fn parse_chain(s: &str) -> Result<Chain, BuildError> {
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
    solver: &Solver,
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
            let candidate = evaluate_backrun(solver, uuid, iter, state).await;
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
    solver: &Solver,
    uuid: Uuid,
    iter: PendingIteration,
    state: PostState,
) -> Option<BackrunCandidate> {
    debug!(
        %uuid,
        block_number = iter.block.block_number,
        tx_count = iter.txs.len(),
        touched_accounts = state.accounts.len(),
        "evaluating backrun opportunity",
    );

    // TODO: use state.accounts to identify which pool contracts changed storage
    // TODO: cross-reference touched addresses with solver.market_data() to find affected pools
    // TODO: for each affected pool, build a QuoteRequest and call solver.quote(request).await
    // TODO: if quote is profitable after gas cost, encode into BackrunTx via quote.transaction
    // TODO: return BackrunCandidate { uuid, block_number: iter.block.block_number, txs }

    let _ = (solver, iter, state);
    None
}
