# tycho-builder-integration

A backrun engine for block builders. Your builder emits events as it constructs a block. The engine finds profitable arbitrage routes from those events and publishes transaction candidates back to your pipeline.

Two types define the integration boundary, in [`crates/builder-types/src/lib.rs`](crates/builder-types/src/lib.rs): `BuildEvent` goes in, `BackrunCandidate` comes out. Both are serde-enabled, so they cross process or network boundaries without extra work.

## Integration modes

**In-process** (lower latency). Add `backrunner` as a Rust dependency, construct a `Backrunner` with your configuration, and call `run()` with a tokio mpsc/watch channel pair. Your builder writes events to the sender; the backrunner publishes candidates to the watch channel. The full API is in [`crates/backrunner/src/lib.rs`](crates/backrunner/src/lib.rs), including an example at the top of the file.

**Out-of-process** (not yet implemented). The `backrunner` binary is the entry point for a message-queue transport. The binary compiles and runs, but queue wiring is not yet done. See the TODOs in [`crates/backrunner/src/main.rs`](crates/backrunner/src/main.rs).

## Read the code before you integrate

Read the source before you wire it into a live pipeline.

- [`crates/builder-types/src/lib.rs`](crates/builder-types/src/lib.rs): every type that crosses the boundary
- [`crates/backrunner/src/lib.rs`](crates/backrunner/src/lib.rs): the engine itself, event processing through to candidate construction

The engine uses [fynd-core](https://crates.io/crates/fynd-core) for route-finding and [tycho-simulation](https://crates.io/crates/tycho-simulation) for market data. Both are published crates with public source.
