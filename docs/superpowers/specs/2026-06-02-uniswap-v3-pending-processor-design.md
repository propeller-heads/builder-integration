# UniswapV3 Pending Transaction Processor

**Date:** 2026-06-02

## Goal

Add a native UniswapV3 log processor so the backrunner's `PendingBlockProcessor` can
simulate how pending transactions change UniswapV3 pool state before quoting. This fills
the missing step between "we have pending tx logs" and "Fynd sees an accurate pending AMM
state".

## Background

`fynd_core::PendingBlockProcessor` (used in `Backrunner`) already supports
`TxDeltaIndexer` plugins via `FyndBuilder::with_pending_indexer`. The trait lives in
`tycho_common::traits` and is already published in `tycho-common 0.302.5`.

PR propeller-heads/tycho#1030 adds `UniswapV3Processor: TxDeltaIndexer` to the tycho
repo. We copy that crate into our workspace and wire it in.

## New crate: `crates/uniswap-v3-core`

Source layout (copied verbatim from PR#1030):

```
crates/uniswap-v3-core/
  Cargo.toml
  src/
    lib.rs
    processor.rs      # UniswapV3Processor: TxDeltaIndexer
    events.rs         # log decoding, PoolEvent types
    balance.rs        # token balance delta math
    liquidity.rs      # in-range liquidity tracking
    ticks.rs          # tick net-liquidity deltas
    output.rs         # attribute updates (sqrt_price_x96, tick)
    abi/
      mod.rs
      factory.rs      # PoolCreated ABI (ethabi-generated)
      pool.rs         # Swap/Mint/Burn/Collect/Initialize ABI (ethabi-generated)
```

### `Cargo.toml` dependencies

Replace the PR's internal path deps with published crates:

| dep | version | note |
|-----|---------|------|
| `tycho-common` | `>=0.302.0` | already in workspace via `tycho-simulation` |
| `tycho-substreams` | `0.8` | new — `TransactionChangesBuilder`, `prelude::*` |
| `substreams` | `0.5` | new — protobuf types used by `tycho-substreams` |
| `substreams-ethereum` | `0.9` | new — `Event` trait, pb log types |
| `ethabi` | `18` | new — ABI decoding in generated `abi/` modules |
| `num-bigint`, `num-traits`, `hex` | workspace | already present |

`abi/mod.rs` carries `#![allow(clippy::all, clippy::pedantic, clippy::nursery)]` to
suppress warnings in generated code.

## Backrunner change

In `Backrunner::build()`, register one `UniswapV3Processor` with the builder before
`build_with_pending()`:

```rust
use uniswap_v3_core::processor::UniswapV3Processor;

let builder = FyndBuilder::new(...)
    .algorithm("bellman_ford")
    .with_pending_indexer(
        "uniswap_v3",
        Box::new(UniswapV3Processor::new(chain, "uniswap_v3".to_string())),
    );
```

- Extractor key `"uniswap_v3"` matches fynd-core's protocol registry and the Tycho feed's
  state message keys for UniswapV3 across all chains.
- `chain` is the already-parsed `Chain` value from `parse_chain(&config.chain)`.
- No other changes to `lib.rs` — `generate_pending_update` already passes `TxInput`s
  through the registered indexers.

`crates/backrunner/Cargo.toml` adds:
```toml
uniswap-v3-core = { path = "../uniswap-v3-core" }
```

## Workspace changes

`Cargo.toml`:
- Add `"crates/uniswap-v3-core"` to `[workspace] members`
- Add to `[workspace.dependencies]`:
  - `tycho-substreams = "0.8"`
  - `substreams = "0.5"`
  - `substreams-ethereum = "0.9"`
  - `ethabi = "18"`

## Data flow (after change)

```
BuildEvent::TxExecuted  →  TxInput list
                         ↓
PendingBlockProcessor::generate_pending_update
  ├── UniswapV3Processor::generate_deltas(txs)
  │     decodes Swap/Mint/Burn/Collect logs from known pools
  │     returns BlockAggregatedChanges with state_deltas + component_balances
  └── apply_deltas_ephemeral → Update (pending pool states)
                         ↓
Fynd solver::quote with pending state label
```

## Out of scope

- Other protocols (Uniswap V2, V4, etc.) — no `TxDeltaIndexer` exists for them yet.
- The PR's integration test (`tests/integration.rs`) — requires live RPC, not copied.
- The substreams WASM module changes in the PR — not relevant to our workspace.
