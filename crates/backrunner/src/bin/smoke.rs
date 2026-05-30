//! Long-running smoke test for the Backrunner pipeline.
//!
//! Subscribes to Fynd market events (one per Ethereum block) and issues one synthetic
//! builder iteration per block, exercising the complete pipeline on every block.
//!
//! Required env vars:
//!   `TYCHO_URL`      — Tycho WebSocket host
//!   `ETH_RPC_URL`    — Ethereum JSON-RPC endpoint (used by the backrunner internally)
//!   `TYCHO_API_KEY`  — (optional) Tycho API key
//!   `CHAIN_ID`       — (optional, default 1) 1inch Fusion chain ID

use std::{collections::HashMap, env, time::Duration};

use anyhow::{Context, Result};
use backrunner::{Backrunner, BackrunnerConfig};
use builder_types::{BackrunCandidate, BlockEnv, BuildEvent, PostState};
use fynd_core::{feed::market_data::MarketData, BlockInfo, MarketEvent};
use tokio::sync::{broadcast, mpsc, watch};
use uuid::Uuid;

const READY_TIMEOUT_MINS: u64 = 10;
const ORDERBOOK_WAIT_SECS: u64 = 15;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,backrunner=debug")),
        )
        .init();

    let tycho_url = env::var("TYCHO_URL").context("TYCHO_URL not set")?;
    let rpc_url = env::var("ETH_RPC_URL").context("ETH_RPC_URL not set")?;
    let tycho_api_key = env::var("TYCHO_API_KEY").ok();
    let chain_id: u64 = env::var("CHAIN_ID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    let config = BackrunnerConfig {
        chain: "ethereum".to_owned(),
        tycho_url,
        rpc_url,
        tycho_api_key,
        protocols: vec!["uniswap_v2".to_owned(), "uniswap_v3".to_owned()],
        min_tvl: 100.0,
        wallet_address: "0x0000000000000000000000000000000000000000".to_owned(),
        ready_timeout: Duration::from_mins(READY_TIMEOUT_MINS),
        chain_id,
    };

    tracing::info!("building backrunner (up to {READY_TIMEOUT_MINS} min)...");
    let backrunner = Backrunner::build(config).await.context("Backrunner::build failed")?;
    tracing::info!("market data ready");

    tracing::info!("waiting {ORDERBOOK_WAIT_SECS}s for first orderbook refresh...");
    tokio::time::sleep(Duration::from_secs(ORDERBOOK_WAIT_SECS)).await;
    let n = backrunner.active_order_count();
    if n == 0 {
        tracing::warn!(
            "orderbook still empty after {ORDERBOOK_WAIT_SECS}s — 1inch API may be down"
        );
    } else {
        tracing::info!("orderbook has {n} live Fusion orders");
    }

    // Extract handles before run() moves self.
    let mut market_rx = backrunner.subscribe_market_events();
    let market_data = backrunner.market_data();

    let (event_tx, event_rx) = mpsc::channel::<BuildEvent>(16);
    let (candidate_tx, mut candidate_rx) = watch::channel(None::<BackrunCandidate>);
    tokio::spawn(backrunner.run(event_rx, candidate_tx));

    tracing::info!("entering block loop...");
    run_block_loop(&mut market_rx, market_data, event_tx, &mut candidate_rx).await
}

async fn run_block_loop(
    market_rx: &mut broadcast::Receiver<MarketEvent>,
    market_data: MarketData,
    event_tx: mpsc::Sender<BuildEvent>,
    candidate_rx: &mut watch::Receiver<Option<BackrunCandidate>>,
) -> Result<()> {
    loop {
        let event = match market_rx.recv().await {
            Ok(e) => e,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(skipped = n, "market event receiver lagged; continuing");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => {
                tracing::warn!("market event channel closed; exiting");
                return Ok(());
            }
        };

        let MarketEvent::MarketUpdated { .. } = event;

        let Some(confirmed) = read_confirmed_block_info(&market_data) else {
            tracing::debug!("confirmed block not yet in market data; skipping");
            continue;
        };
        let block_number = confirmed.number() + 1;
        // Use confirmed block timestamp + 12s as the approximate next block timestamp.
        let block_timestamp = confirmed.timestamp() + 12;
        tracing::info!(block_number, "new block — issuing iteration");

        let uuid = Uuid::new_v4();
        let block_env = BlockEnv {
            block_number,
            block_timestamp,
            base_fee_per_gas: 0,
        };

        if event_tx
            .send(BuildEvent::IterationStart { uuid, block: block_env })
            .await
            .is_err()
        {
            tracing::error!("event channel closed — backrunner task exited");
            return Ok(());
        }

        if event_tx
            .send(BuildEvent::IterationComplete {
                uuid,
                state: PostState { accounts: HashMap::new() },
            })
            .await
            .is_err()
        {
            tracing::error!("event channel closed — backrunner task exited");
            return Ok(());
        }

        // Give evaluate_backrun time to complete before logging.
        tokio::time::sleep(Duration::from_secs(5)).await;
        let candidate = candidate_rx.borrow().clone();
        tracing::info!(block_number, ?candidate, "iteration result");
    }
}

/// Returns the block info for the last confirmed block, or `None` if not yet available.
fn read_confirmed_block_info(market_data: &MarketData) -> Option<BlockInfo> {
    let view = market_data.try_read_blocking()?;
    view.last_updated().cloned()
}
