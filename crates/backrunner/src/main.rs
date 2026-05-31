//! Standalone backrunner binary.
//!
//! Runs the backrunner as an external process. The in-process channel API lives in the
//! [`backrunner`] lib crate — this binary is the entry point for future message-queue
//! integration (not yet implemented).

use std::time::Duration;

use alloy::primitives::Address as AlloyAddress;
use anyhow::{Context, Result};
use backrunner::{Backrunner, BackrunnerConfig};
use builder_types::{BackrunCandidate, BuildEvent};
use clap::Parser;
use tokio::sync::{mpsc, watch};

#[derive(Debug, Parser)]
#[command(about = "Standalone backrunner process")]
struct Cli {
    #[arg(long, env = "TYCHO_URL", default_value = "app.propellerheads.xyz")]
    tycho_url: String,

    #[arg(long, env = "ETH_RPC_URL")]
    rpc_url: String,

    #[arg(long, env = "TYCHO_API_KEY")]
    tycho_api_key: Option<String>,

    #[arg(
        long,
        env = "PROTOCOLS",
        value_delimiter = ',',
        default_values = [
            "uniswap_v2", "uniswap_v3", "uniswap_v4",
            "sushiswap_v2", "pancakeswap_v2", "pancakeswap_v3",
            "vm:maverick_v2", "fluid_v1",
        ]
    )]
    protocols: Vec<String>,

    #[arg(long, env = "MIN_TVL", default_value_t = 1000.0)]
    min_tvl: f64,

    #[arg(long, env = "WALLET_ADDRESS")]
    wallet_address: String,

    #[arg(long, env = "CHAIN", default_value = "ethereum")]
    chain: String,

    #[arg(long, env = "CHAIN_ID", default_value_t = 1)]
    chain_id: u64,

    #[arg(
        long, env = "RESOLVER_ADDRESS",
        default_value = "0x0000000000000000000000000000000000000000"
    )]
    resolver_address: AlloyAddress,

    #[arg(
        long, env = "FYND_ROUTER",
        default_value = "0x1f8dB310f32D48B6180fF902EC60C586128cEf47"
    )]
    fynd_router: AlloyAddress,

    #[arg(long, env = "SLIPPAGE", default_value_t = 0.005)]
    slippage: f64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    let config = BackrunnerConfig {
        chain: cli.chain,
        tycho_url: cli.tycho_url,
        rpc_url: cli.rpc_url,
        tycho_api_key: cli.tycho_api_key,
        protocols: cli.protocols,
        min_tvl: cli.min_tvl,
        wallet_address: cli.wallet_address,
        ready_timeout: Duration::from_mins(3),
        chain_id: cli.chain_id,
        resolver_address: cli.resolver_address,
        fynd_router: cli.fynd_router,
        slippage: cli.slippage,
    };

    tracing::info!("building backrunner, waiting for market data...");
    let backrunner = Backrunner::build(config)
        .await
        .context("failed to build backrunner")?;
    tracing::info!("market data ready");

    let (event_tx, event_rx) = mpsc::channel::<BuildEvent>(1024);
    let (candidate_tx, _candidate_rx) = watch::channel(None::<BackrunCandidate>);

    // TODO: connect event_tx to an inbound message queue (e.g. NATS, Redis Streams)
    // TODO: forward _candidate_rx to an outbound queue for the builder to consume
    //
    // Drop event_tx so the backrunner loop exits cleanly until MQ is wired up.
    drop(event_tx);

    backrunner.run(event_rx, candidate_tx).await;

    Ok(())
}
