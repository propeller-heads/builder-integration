//! Long-running smoke test for the Backrunner pipeline.
//!
//! Subscribes to Fynd market events (one per Ethereum block) and issues one synthetic
//! builder iteration per block, exercising the complete pipeline on every block.
//!
//! For each fill candidate, `eth_call` is issued with a state override that injects
//! the `BackrunResolver` bytecode at the resolver address.  This means a live
//! deployment is **not required** — set `RESOLVER_ADDRESS` to any address you want
//! to call from (default: a fixed virtual address).  The bytecode used is the one
//! compiled into this binary; regenerate it after contract changes with:
//!
//!   forge script contracts/script/PrintBytecode.s.sol --silent
//!   cp contracts/out/BackrunResolver.runtime.hex \
//!      crates/backrunner/bytecode/BackrunResolver.runtime.hex
//!
//! Required env vars:
//!   `TYCHO_URL`         — Tycho WebSocket host
//!   `ETH_RPC_URL`       — Ethereum JSON-RPC endpoint
//!   `TYCHO_API_KEY`     — (optional) Tycho API key
//!   `CHAIN_ID`          — (optional, default 1) 1inch Fusion chain ID
//!   `RESOLVER_ADDRESS`  — (optional) override the virtual resolver address

use std::{collections::HashMap, env, time::Duration};

use alloy::primitives::{address, Address as AlloyAddress, Bytes as AlloyBytes};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::state::AccountOverride;
use anyhow::{Context, Result};
use backrunner::{Backrunner, BackrunnerConfig};
use builder_types::{BackrunCandidate, BlockEnv, BuildEvent, PostState};
use fynd_core::{feed::market_data::MarketData, BlockInfo, MarketEvent};
use tokio::sync::{broadcast, mpsc, watch};
use uuid::Uuid;

const READY_TIMEOUT_MINS: u64 = 10;
const ORDERBOOK_WAIT_SECS: u64 = 15;

/// Fynd/Tycho router on Ethereum mainnet.
const FYND_ROUTER: AlloyAddress = address!("1f8dB310f32D48B6180fF902EC60C586128cEf47");

/// Synthetic address used when no `RESOLVER_ADDRESS` is set.
/// The bytecode override makes deployment unnecessary.
const VIRTUAL_RESOLVER: AlloyAddress = address!("0000000000000000000000000000000000001234");

/// `BackrunResolver` runtime bytecode compiled from `contracts/src/BackrunResolver.sol`.
/// Injected via state override on every `eth_call`, so no on-chain deployment is needed.
///
/// Regenerate after contract changes:
///   `forge script contracts/script/PrintBytecode.s.sol --silent`
///   `cp contracts/out/BackrunResolver.runtime.hex crates/backrunner/bytecode/BackrunResolver.runtime.hex`
const RESOLVER_BYTECODE_HEX: &str =
    include_str!("../../bytecode/BackrunResolver.runtime.hex");

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

    let provider = ProviderBuilder::new()
        .connect_http(rpc_url.parse().context("invalid ETH_RPC_URL")?);

    // If RESOLVER_ADDRESS is set to a real address, trust the deployed contract's code.
    // Otherwise use the virtual address with a bytecode state override so no deployment
    // is needed.
    let deployed_addr: Option<AlloyAddress> = env::var("RESOLVER_ADDRESS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|a: &AlloyAddress| !a.is_zero());

    let (resolver_addr, resolver_bytecode) = match deployed_addr {
        Some(addr) => {
            tracing::info!(%addr, "using deployed resolver (no bytecode override)");
            (addr, None)
        }
        None => {
            let bytecode: AlloyBytes = RESOLVER_BYTECODE_HEX
                .trim()
                .parse()
                .context("embedded resolver bytecode is not valid hex")?;
            tracing::info!(
                addr = %VIRTUAL_RESOLVER,
                bytecode_bytes = bytecode.len(),
                "no RESOLVER_ADDRESS set — using virtual address with bytecode override"
            );
            (VIRTUAL_RESOLVER, Some(bytecode))
        }
    };

    let chain_id: u64 = env::var("CHAIN_ID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    let config = BackrunnerConfig {
        chain: "ethereum".to_owned(),
        tycho_url,
        rpc_url,
        tycho_api_key,
        protocols: vec![
            "uniswap_v2".to_owned(),
            "uniswap_v3".to_owned(),
            "uniswap_v4".to_owned(),
            "uniswap_v4_hooks".to_owned(),
            "sushiswap_v2".to_owned(),
            "pancakeswap_v2".to_owned(),
            "pancakeswap_v3".to_owned(),
            "vm:curve".to_owned(),
            "vm:balancer_v2".to_owned(),
            "vm:maverick_v2".to_owned(),
            "fluid_v1".to_owned(),
            "cowamm".to_owned(),
            "rocketpool".to_owned(),
            "erc4626".to_owned(),
        ],
        min_tvl: 100.0,
        wallet_address: "0x0000000000000000000000000000000000000000".to_owned(),
        ready_timeout: Duration::from_mins(READY_TIMEOUT_MINS),
        chain_id,
        resolver_address: resolver_addr,
        fynd_router: FYND_ROUTER,
        slippage: 0.005,
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
    run_block_loop(&mut market_rx, market_data, event_tx, &mut candidate_rx, provider, resolver_addr, resolver_bytecode).await
}

async fn run_block_loop(
    market_rx: &mut broadcast::Receiver<MarketEvent>,
    market_data: MarketData,
    event_tx: mpsc::Sender<BuildEvent>,
    candidate_rx: &mut watch::Receiver<Option<BackrunCandidate>>,
    provider: impl Provider + Clone + 'static,
    resolver_addr: AlloyAddress,
    resolver_bytecode: Option<AlloyBytes>,
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

        // Give evaluate_backrun time to complete before reading the candidate.
        tokio::time::sleep(Duration::from_secs(5)).await;
        let candidate = candidate_rx.borrow().clone();
        match &candidate {
            None => {
                tracing::info!(block_number, "iteration result: no candidate");
            }
            Some(c) if c.txs.is_empty() => {
                tracing::info!(block_number, "iteration result: empty candidate");
            }
            Some(c) => {
                tracing::info!(
                    block_number,
                    txs = c.txs.len(),
                    "candidate found — validating via eth_call with bytecode override"
                );
                for (i, backrun_tx) in c.txs.iter().enumerate() {
                    let tx_req = alloy::rpc::types::TransactionRequest::default()
                        .from(resolver_addr)
                        .to(backrun_tx.tx.to.unwrap_or_default())
                        .value(backrun_tx.tx.value)
                        .input(backrun_tx.tx.data.clone().into());

                    let state_override = resolver_bytecode.as_ref().map(|bytecode| {
                        let mut m = alloy::rpc::types::state::StateOverride::default();
                        m.insert(resolver_addr, AccountOverride {
                            code: Some(bytecode.clone()),
                            ..Default::default()
                        });
                        m
                    });
                    let result = provider.call(tx_req).overrides_opt(state_override).await;

                    match result {
                        Ok(output) => {
                            tracing::info!(
                                block_number,
                                tx_index = i,
                                output_bytes = output.len(),
                                "eth_call SUCCESS ✓"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                block_number,
                                tx_index = i,
                                error = %e,
                                "eth_call REVERTED"
                            );
                        }
                    }
                }
            }
        }
    }
}

/// Returns the block info for the last confirmed block, or `None` if not yet available.
fn read_confirmed_block_info(market_data: &MarketData) -> Option<BlockInfo> {
    let view = market_data.try_read_blocking()?;
    view.last_updated().cloned()
}
