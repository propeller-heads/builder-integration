# tycho-builder-integration

A backrun engine for block builders that acts as a [1inch Fusion](https://1inch.io/fusion/) resolver. At the end of each block-building iteration, it evaluates live Fusion orders against the final AMM state and finds profitable fill routes using Propellerheads' [Fynd](https://crates.io/crates/fynd-core) solver. When a route is profitable after gas, it produces a settlement transaction that your builder can append to the block.

The engine subscribes to DEX state via [Tycho](https://propellerheads.xyz) and continuously watches the 1inch Fusion orderbook. As your builder constructs a block it streams executed transactions to the engine, which tracks pending AMM state in real time (currently Uniswap V3 only; see [Pending state](#in-process-recommended) below). At `IterationComplete` it runs the solver against the updated state and publishes a candidate if one is found.

Two types define the integration boundary, in [`crates/builder-types/src/lib.rs`](crates/builder-types/src/lib.rs): `BuildEvent` goes in, `BackrunCandidate` comes out. Both are serde-enabled, so they cross process or network boundaries without extra work.

## Before you start

**EOA.** The backrunner submits transactions through a Propellerheads-operated resolver contract. Your integration needs a dedicated EOA whose address you send to us for whitelisting. Transactions in `BackrunCandidate` are pre-built but unsigned — your builder signs them with that EOA before inclusion.

**Tycho API key.** The engine subscribes to live DEX state via [Tycho](https://propellerheads.xyz). Provision an API key at [t.me/fynd_portal_bot](https://t.me/fynd_portal_bot). Pass it as `BackrunnerConfig::tycho_api_key`.

EOA whitelisting requires coordination with Propellerheads before your integration can go live.

## Integration modes

### In-process (recommended)

Add `backrunner` as a Rust dependency. Construct a `Backrunner`, create a tokio channel pair, and spawn `run()` alongside your builder loop. Your builder writes `BuildEvent`s to the sender; the backrunner publishes `BackrunCandidate`s to the watch channel.

```rust
use backrunner::{Backrunner, BackrunnerConfig};
use builder_types::{BackrunCandidate, BuildEvent};
use tokio::sync::{mpsc, watch};
use std::time::Duration;

let config = BackrunnerConfig {
    chain: "ethereum".to_string(),
    tycho_url: "app.propellerheads.xyz".to_string(),
    tycho_api_key: Some("your-api-key".to_string()),
    rpc_url: "https://your-rpc".to_string(),
    protocols: vec![
        "uniswap_v2", "uniswap_v3", "uniswap_v4",
        "sushiswap_v2", "pancakeswap_v2", "pancakeswap_v3",
        "vm:curve", "fluid_v1", "ekubo_v2", "ekubo_v3",
    ].into_iter().map(str::to_owned).collect(),
    resolver_address: "0x2B658151310A7793E88E9038b927d5B25EC6915e".parse()?,
    chain_id: 1,
    min_tvl: 10.0,
    slippage: 0.005,
    ready_timeout: Duration::from_secs(30),
    orderbook_interval: Duration::from_secs(12),
};

let backrunner = Backrunner::build(config).await?;
let (event_tx, event_rx) = mpsc::channel::<BuildEvent>(1024);
let (candidate_tx, candidate_rx) = watch::channel(None::<BackrunCandidate>);

tokio::spawn(backrunner.run(event_rx, candidate_tx));
```

**Pending state.** The engine uses pending AMM state (mid-block pool state derived from transactions seen so far) when evaluating routes. Currently only Uniswap V3 supports pending state; all other protocols fall back to confirmed state. Support for additional protocols is in progress.

For each block-building iteration your builder should emit:

```rust
// Start of iteration
event_tx.send(BuildEvent::IterationStart { uuid, block }).await?;

// After each transaction executes
event_tx.send(BuildEvent::TxExecuted { uuid, tx }).await?;

// When iteration finishes
event_tx.send(BuildEvent::IterationComplete { uuid, state }).await?;
// or, if aborted:
event_tx.send(BuildEvent::IterationAborted { uuid }).await?;
```

### Out-of-process

The `backrunner` binary exposes the same engine over a message queue transport. Compile it and deploy it as a sidecar; send `BuildEvent` JSON to its input queue and consume `BackrunCandidate` JSON from its output queue.

Queue wiring is not yet complete. See the TODOs in [`crates/backrunner/src/main.rs`](crates/backrunner/src/main.rs). The binary compiles and the engine runs; only the transport layer is missing.

## Using candidates

A `BackrunCandidate` contains one or more unsigned EIP-1559 transactions (`BackrunTx`). Each carries an `expected_profit_wei` and `expected_gas` estimate. Your builder decides whether to include them:

```rust
if let Some(candidate) = candidate_rx.borrow().as_ref() {
    for backrun_tx in &candidate.txs {
        if backrun_tx.expected_profit_wei > min_profit_threshold {
            let signed = your_eoa.sign_transaction(&backrun_tx.tx).await?;
            block.include(signed);
        }
    }
}
```

Candidates are keyed by `uuid` matching the originating `IterationStart` event. Stale candidates (from a previous iteration) should be discarded.

## Read the code before you integrate

- [`crates/builder-types/src/lib.rs`](crates/builder-types/src/lib.rs): every type that crosses the boundary
- [`crates/backrunner/src/lib.rs`](crates/backrunner/src/lib.rs): the engine itself, including a full config reference
- [`crates/backrunner/src/main.rs`](crates/backrunner/src/main.rs): the out-of-process entry point

The engine uses [fynd-core](https://crates.io/crates/fynd-core) for route-finding and [tycho-simulation](https://crates.io/crates/tycho-simulation) for market data. Both are published crates with public source.
