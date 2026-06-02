# UniswapV3 Pending Transaction Processor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `uniswap-v3-core` workspace crate that decodes UniswapV3 EVM logs into pending AMM state deltas, and register it with the backrunner so Fynd quotes use accurate pending pool state.

**Architecture:** Port `protocols/crates/uniswap-v3` from PR propeller-heads/tycho#1030 (commit `202d34f`) as a standalone workspace crate. Register `UniswapV3Processor` with `FyndBuilder::with_pending_indexer("uniswap_v3", ...)` in `Backrunner::build()`. No changes to the existing `generate_pending_update` call path.

**Tech Stack:** Rust stable, `tycho-common 0.302.x`, `tycho-substreams 0.8`, `substreams 0.5`, `substreams-ethereum 0.9`, `ethabi 18`, `num-bigint`, `cargo nextest`.

---

## Files

| Action | Path | Purpose |
|--------|------|---------|
| Modify | `Cargo.toml` | add workspace members + deps |
| Create | `crates/uniswap-v3-core/Cargo.toml` | crate manifest |
| Create | `crates/uniswap-v3-core/src/lib.rs` | module declarations |
| Create | `crates/uniswap-v3-core/src/abi/mod.rs` | allow generated-code lints |
| Create | `crates/uniswap-v3-core/src/abi/factory.rs` | PoolCreated ABI (copied) |
| Create | `crates/uniswap-v3-core/src/abi/pool.rs` | Swap/Mint/Burn/Collect ABI (copied) |
| Create | `crates/uniswap-v3-core/src/events.rs` | log decoder, PoolEvent types |
| Create | `crates/uniswap-v3-core/src/balance.rs` | token balance delta math |
| Create | `crates/uniswap-v3-core/src/liquidity.rs` | in-range liquidity tracking |
| Create | `crates/uniswap-v3-core/src/ticks.rs` | tick net-liquidity deltas |
| Create | `crates/uniswap-v3-core/src/output.rs` | attribute updates |
| Create | `crates/uniswap-v3-core/src/processor.rs` | UniswapV3Processor: TxDeltaIndexer |
| Modify | `crates/backrunner/Cargo.toml` | add uniswap-v3-core dep |
| Modify | `crates/backrunner/src/lib.rs` | register processor with builder |

---

## Task 1: Workspace scaffolding

**Files:** Modify `Cargo.toml` (workspace root at `/Users/kayibal/code/tycho-builder-integration/Cargo.toml`)

- [ ] **Step 1: Add uniswap-v3-core to workspace members**

  In `Cargo.toml`, find the `[workspace]` `members` array and add the new crate:

  ```toml
  members = ["crates/builder-types", "crates/backrunner", "crates/uniswap-v3-core"]
  ```

- [ ] **Step 2: Add new workspace dependencies**

  In `[workspace.dependencies]`, add after the existing `hex = "0.4"` line:

  ```toml
  # UniswapV3 native log processor
  tycho-substreams = "0.8"
  substreams = "0.5"
  substreams-ethereum = "0.9"
  ethabi = "18"
  num-traits = "0.2"
  ```

- [ ] **Step 3: Commit**

  ```bash
  cd /Users/kayibal/code/tycho-builder-integration
  git add Cargo.toml
  git commit -m "chore: add uniswap-v3-core to workspace, add substreams deps"
  ```

---

## Task 2: Crate skeleton

**Files:** Create `crates/uniswap-v3-core/Cargo.toml` and `crates/uniswap-v3-core/src/lib.rs`

- [ ] **Step 1: Create the crate manifest**

  Create `crates/uniswap-v3-core/Cargo.toml`:

  ```toml
  [package]
  name = "uniswap-v3-core"
  version.workspace = true
  edition.workspace = true
  license.workspace = true
  description = "Native UniswapV3 log processor implementing TxDeltaIndexer"

  [dependencies]
  tycho-substreams = { workspace = true }
  substreams = { workspace = true }
  substreams-ethereum = { workspace = true }
  ethabi = { workspace = true }
  tycho-common = { version = ">=0.302.0" }
  num-bigint = { workspace = true }
  num-traits = { workspace = true }
  hex = { workspace = true }

  [target.'cfg(target_arch = "wasm32")'.dependencies]
  getrandom = { version = "0.2", features = ["custom"] }

  [lints]
  workspace = true
  ```

- [ ] **Step 2: Create lib.rs with module declarations**

  Create `crates/uniswap-v3-core/src/lib.rs`:

  ```rust
  pub mod abi;
  pub mod balance;
  pub mod events;
  pub mod liquidity;
  pub mod output;
  pub mod processor;
  pub mod ticks;
  ```

- [ ] **Step 3: Verify the skeleton compiles**

  ```bash
  cd /Users/kayibal/code/tycho-builder-integration
  cargo check -p uniswap-v3-core 2>&1
  ```

  Expected: error about missing module files (that's fine — we haven't created them yet). The workspace membership and Cargo.toml should not produce errors. If you see "no such file or directory" for `lib.rs` that means the directory wasn't created — re-check the path.

---

## Task 3: ABI modules (generated code)

**Files:** Create `abi/mod.rs`, `abi/factory.rs`, `abi/pool.rs`

These are ethabi-generated files copied verbatim from the PR. The pool.rs is 4440 lines; use the `gh` CLI to fetch rather than typing.

- [ ] **Step 1: Create abi/mod.rs**

  Create `crates/uniswap-v3-core/src/abi/mod.rs`:

  ```rust
  #![allow(clippy::all, clippy::pedantic, clippy::nursery)]

  pub mod factory;
  pub mod pool;
  ```

- [ ] **Step 2: Fetch factory.rs from the PR**

  Run this from inside the repo root (`/Users/kayibal/code/tycho-builder-integration`):

  ```bash
  BLOB_SHA=$(gh api repos/propeller-heads/tycho/pulls/1030/files \
    --jq '.[] | select(.filename == "protocols/crates/uniswap-v3/src/abi/factory.rs") | .sha')
  gh api "repos/propeller-heads/tycho/git/blobs/$BLOB_SHA" \
    --jq '.content' | base64 -d \
    > crates/uniswap-v3-core/src/abi/factory.rs
  ```

- [ ] **Step 3: Fetch pool.rs from the PR**

  ```bash
  BLOB_SHA=$(gh api repos/propeller-heads/tycho/pulls/1030/files \
    --jq '.[] | select(.filename == "protocols/crates/uniswap-v3/src/abi/pool.rs") | .sha')
  gh api "repos/propeller-heads/tycho/git/blobs/$BLOB_SHA" \
    --jq '.content' | base64 -d \
    > crates/uniswap-v3-core/src/abi/pool.rs
  ```

- [ ] **Step 4: Verify compilation**

  ```bash
  cargo check -p uniswap-v3-core 2>&1
  ```

  Expected: errors about missing modules `events`, `balance`, etc. — NOT errors inside `abi/`. If you see errors inside the abi files themselves, the fetch likely truncated; re-run the fetch step.

- [ ] **Step 5: Commit**

  ```bash
  git add crates/uniswap-v3-core/
  git commit -m "feat(uniswap-v3-core): add crate skeleton and ABI modules"
  ```

---

## Task 4: Event types and log decoder

**Files:** Create `crates/uniswap-v3-core/src/events.rs`

- [ ] **Step 1: Create events.rs**

  Create `crates/uniswap-v3-core/src/events.rs`:

  ```rust
  use substreams_ethereum::Event;

  use crate::abi::pool::events::{
      Burn, Collect, CollectProtocol, Flash, Initialize, Mint, SetFeeProtocol, Swap,
  };

  #[derive(Clone)]
  pub struct Pool {
      pub address: Vec<u8>,
      pub token0: Vec<u8>,
      pub token1: Vec<u8>,
  }

  #[derive(Debug, Clone)]
  pub struct TxRef {
      pub hash: Vec<u8>,
      pub from: Vec<u8>,
      pub to: Vec<u8>,
      pub index: u64,
  }

  pub enum PoolEventKind {
      Initialize { sqrt_price: String, tick: i32 },
      Swap { amount0: String, amount1: String, sqrt_price: String, liquidity: String, tick: i32 },
      Mint { tick_lower: i32, tick_upper: i32, amount: String, amount0: String, amount1: String },
      Burn { tick_lower: i32, tick_upper: i32, amount: String, amount0: String, amount1: String },
      Collect { amount0: String, amount1: String },
      Flash { paid0: String, paid1: String },
      CollectProtocol { amount0: String, amount1: String },
      SetFeeProtocol { fee0_new: u64, fee1_new: u64 },
  }

  pub struct PoolEvent {
      pub log_ordinal: u64,
      pub pool_address: Vec<u8>,
      pub token0: Vec<u8>,
      pub token1: Vec<u8>,
      pub tx: TxRef,
      pub kind: PoolEventKind,
  }

  pub fn decode_log(
      log: &substreams_ethereum::pb::eth::v2::Log,
      pool: &Pool,
      tx: &TxRef,
  ) -> Option<PoolEvent> {
      let tx_ref = TxRef {
          hash: tx.hash.clone(),
          from: tx.from.clone(),
          to: tx.to.clone(),
          index: tx.index,
      };

      if let Some(init) = Initialize::match_and_decode(log) {
          return Some(PoolEvent {
              log_ordinal: log.ordinal,
              pool_address: pool.address.clone(),
              token0: pool.token0.clone(),
              token1: pool.token1.clone(),
              tx: tx_ref,
              kind: PoolEventKind::Initialize {
                  sqrt_price: init.sqrt_price_x96.to_string(),
                  tick: init.tick.into(),
              },
          });
      }

      if let Some(swap) = Swap::match_and_decode(log) {
          return Some(PoolEvent {
              log_ordinal: log.ordinal,
              pool_address: pool.address.clone(),
              token0: pool.token0.clone(),
              token1: pool.token1.clone(),
              tx: tx_ref,
              kind: PoolEventKind::Swap {
                  amount0: swap.amount0.to_string(),
                  amount1: swap.amount1.to_string(),
                  sqrt_price: swap.sqrt_price_x96.to_string(),
                  liquidity: swap.liquidity.to_string(),
                  tick: swap.tick.into(),
              },
          });
      }

      if let Some(flash) = Flash::match_and_decode(log) {
          return Some(PoolEvent {
              log_ordinal: log.ordinal,
              pool_address: pool.address.clone(),
              token0: pool.token0.clone(),
              token1: pool.token1.clone(),
              tx: tx_ref,
              kind: PoolEventKind::Flash {
                  paid0: flash.paid0.to_string(),
                  paid1: flash.paid1.to_string(),
              },
          });
      }

      if let Some(mint) = Mint::match_and_decode(log) {
          return Some(PoolEvent {
              log_ordinal: log.ordinal,
              pool_address: pool.address.clone(),
              token0: pool.token0.clone(),
              token1: pool.token1.clone(),
              tx: tx_ref,
              kind: PoolEventKind::Mint {
                  tick_lower: mint.tick_lower.into(),
                  tick_upper: mint.tick_upper.into(),
                  amount: mint.amount.to_string(),
                  amount0: mint.amount0.to_string(),
                  amount1: mint.amount1.to_string(),
              },
          });
      }

      if let Some(burn) = Burn::match_and_decode(log) {
          return Some(PoolEvent {
              log_ordinal: log.ordinal,
              pool_address: pool.address.clone(),
              token0: pool.token0.clone(),
              token1: pool.token1.clone(),
              tx: tx_ref,
              kind: PoolEventKind::Burn {
                  tick_lower: burn.tick_lower.into(),
                  tick_upper: burn.tick_upper.into(),
                  amount: burn.amount.to_string(),
                  amount0: burn.amount0.to_string(),
                  amount1: burn.amount1.to_string(),
              },
          });
      }

      if let Some(collect) = Collect::match_and_decode(log) {
          return Some(PoolEvent {
              log_ordinal: log.ordinal,
              pool_address: pool.address.clone(),
              token0: pool.token0.clone(),
              token1: pool.token1.clone(),
              tx: tx_ref,
              kind: PoolEventKind::Collect {
                  amount0: collect.amount0.to_string(),
                  amount1: collect.amount1.to_string(),
              },
          });
      }

      if let Some(set_fp) = SetFeeProtocol::match_and_decode(log) {
          return Some(PoolEvent {
              log_ordinal: log.ordinal,
              pool_address: pool.address.clone(),
              token0: pool.token0.clone(),
              token1: pool.token1.clone(),
              tx: tx_ref,
              kind: PoolEventKind::SetFeeProtocol {
                  fee0_new: set_fp.fee_protocol0_new.to_u64(),
                  fee1_new: set_fp.fee_protocol1_new.to_u64(),
              },
          });
      }

      if let Some(cp) = CollectProtocol::match_and_decode(log) {
          return Some(PoolEvent {
              log_ordinal: log.ordinal,
              pool_address: pool.address.clone(),
              token0: pool.token0.clone(),
              token1: pool.token1.clone(),
              tx: tx_ref,
              kind: PoolEventKind::CollectProtocol {
                  amount0: cp.amount0.to_string(),
                  amount1: cp.amount1.to_string(),
              },
          });
      }

      None
  }
  ```

- [ ] **Step 2: Check compilation**

  ```bash
  cargo check -p uniswap-v3-core 2>&1
  ```

  Expected: errors about the remaining missing modules (`balance`, `liquidity`, `ticks`, `output`, `processor`) — not errors inside `events.rs`.

---

## Task 5: Math modules

**Files:** Create `balance.rs`, `liquidity.rs`, `ticks.rs`, `output.rs`

- [ ] **Step 1: Write tests for balance.rs** (create the test module before the implementation)

  Create `crates/uniswap-v3-core/src/balance.rs`:

  ```rust
  use std::str::FromStr;

  use num_bigint::BigInt;
  use tycho_substreams::pb::tycho::evm::v1::BalanceDelta as ProtoBalanceDelta;

  use crate::events::{PoolEvent, PoolEventKind};

  pub struct BalanceDelta {
      pub token: Vec<u8>,
      pub component_id: Vec<u8>,
      pub delta: BigInt,
  }

  impl From<BalanceDelta> for ProtoBalanceDelta {
      fn from(d: BalanceDelta) -> Self {
          Self {
              token: d.token,
              delta: d.delta.to_signed_bytes_be(),
              component_id: format!("0x{}", hex::encode(&d.component_id)).into_bytes(),
              ord: 0,
              tx: None,
          }
      }
  }

  pub fn event_to_balance_deltas(event: &PoolEvent) -> Vec<BalanceDelta> {
      let component_id = event.pool_address.clone();

      match &event.kind {
          PoolEventKind::Mint { amount0, amount1, .. } => vec![
              BalanceDelta {
                  token: event.token0.clone(),
                  component_id: component_id.clone(),
                  delta: BigInt::from_str(amount0).unwrap_or_default(),
              },
              BalanceDelta {
                  token: event.token1.clone(),
                  component_id,
                  delta: BigInt::from_str(amount1).unwrap_or_default(),
              },
          ],
          PoolEventKind::Collect { amount0, amount1 } => vec![
              BalanceDelta {
                  token: event.token0.clone(),
                  component_id: component_id.clone(),
                  delta: -BigInt::from_str(amount0).unwrap_or_default(),
              },
              BalanceDelta {
                  token: event.token1.clone(),
                  component_id,
                  delta: -BigInt::from_str(amount1).unwrap_or_default(),
              },
          ],
          // Burn balance changes are accounted for in the Collect event.
          PoolEventKind::Burn { .. } => vec![],
          PoolEventKind::Swap { amount0, amount1, .. } => vec![
              BalanceDelta {
                  token: event.token0.clone(),
                  component_id: component_id.clone(),
                  delta: BigInt::from_str(amount0).unwrap_or_default(),
              },
              BalanceDelta {
                  token: event.token1.clone(),
                  component_id,
                  delta: BigInt::from_str(amount1).unwrap_or_default(),
              },
          ],
          PoolEventKind::Flash { paid0, paid1 } => vec![
              BalanceDelta {
                  token: event.token0.clone(),
                  component_id: component_id.clone(),
                  delta: BigInt::from_str(paid0).unwrap_or_default(),
              },
              BalanceDelta {
                  token: event.token1.clone(),
                  component_id,
                  delta: BigInt::from_str(paid1).unwrap_or_default(),
              },
          ],
          PoolEventKind::CollectProtocol { amount0, amount1 } => vec![
              BalanceDelta {
                  token: event.token0.clone(),
                  component_id: component_id.clone(),
                  delta: -BigInt::from_str(amount0).unwrap_or_default(),
              },
              BalanceDelta {
                  token: event.token1.clone(),
                  component_id,
                  delta: -BigInt::from_str(amount1).unwrap_or_default(),
              },
          ],
          _ => vec![],
      }
  }

  #[cfg(test)]
  mod tests {
      use super::*;
      use crate::events::{PoolEvent, PoolEventKind, TxRef};

      fn make_event(kind: PoolEventKind) -> PoolEvent {
          PoolEvent {
              log_ordinal: 0,
              pool_address: vec![0xAA; 20],
              token0: vec![0x11; 20],
              token1: vec![0x22; 20],
              tx: TxRef { hash: vec![0; 32], from: vec![0; 20], to: vec![0; 20], index: 0 },
              kind,
          }
      }

      #[test]
      fn swap_positive_amount0_negative_amount1() {
          let event = make_event(PoolEventKind::Swap {
              amount0: "1000".to_string(),
              amount1: "-800".to_string(),
              sqrt_price: "1".to_string(),
              liquidity: "5000".to_string(),
              tick: 0,
          });
          let deltas = event_to_balance_deltas(&event);
          assert_eq!(deltas.len(), 2);
          assert_eq!(deltas[0].delta, BigInt::from(1000));
          assert_eq!(deltas[1].delta, BigInt::from(-800));
      }

      #[test]
      fn mint_adds_both_tokens() {
          let event = make_event(PoolEventKind::Mint {
              tick_lower: -100,
              tick_upper: 100,
              amount: "500".to_string(),
              amount0: "300".to_string(),
              amount1: "200".to_string(),
          });
          let deltas = event_to_balance_deltas(&event);
          assert_eq!(deltas.len(), 2);
          assert_eq!(deltas[0].delta, BigInt::from(300));
          assert_eq!(deltas[1].delta, BigInt::from(200));
      }

      #[test]
      fn collect_subtracts_both_tokens() {
          let event = make_event(PoolEventKind::Collect {
              amount0: "100".to_string(),
              amount1: "50".to_string(),
          });
          let deltas = event_to_balance_deltas(&event);
          assert_eq!(deltas[0].delta, BigInt::from(-100));
          assert_eq!(deltas[1].delta, BigInt::from(-50));
      }

      #[test]
      fn burn_produces_no_deltas() {
          let event = make_event(PoolEventKind::Burn {
              tick_lower: -100,
              tick_upper: 100,
              amount: "200".to_string(),
              amount0: "100".to_string(),
              amount1: "100".to_string(),
          });
          assert!(event_to_balance_deltas(&event).is_empty());
      }
  }
  ```

- [ ] **Step 2: Create ticks.rs**

  Create `crates/uniswap-v3-core/src/ticks.rs`:

  ```rust
  use std::str::FromStr;

  use num_bigint::BigInt;

  use crate::events::{PoolEvent, PoolEventKind};

  pub struct TickDelta {
      pub pool_address: Vec<u8>,
      pub tick_index: i32,
      pub liquidity_net_delta: BigInt,
  }

  pub fn event_to_tick_deltas(event: &PoolEvent) -> Vec<TickDelta> {
      match &event.kind {
          PoolEventKind::Mint { tick_lower, tick_upper, amount, .. } => {
              let amount_val = BigInt::from_str(amount).unwrap_or_default();
              vec![
                  TickDelta {
                      pool_address: event.pool_address.clone(),
                      tick_index: *tick_lower,
                      liquidity_net_delta: amount_val.clone(),
                  },
                  TickDelta {
                      pool_address: event.pool_address.clone(),
                      tick_index: *tick_upper,
                      liquidity_net_delta: -amount_val,
                  },
              ]
          }
          PoolEventKind::Burn { tick_lower, tick_upper, amount, .. } => {
              let amount_val = BigInt::from_str(amount).unwrap_or_default();
              vec![
                  TickDelta {
                      pool_address: event.pool_address.clone(),
                      tick_index: *tick_lower,
                      liquidity_net_delta: -amount_val.clone(),
                  },
                  TickDelta {
                      pool_address: event.pool_address.clone(),
                      tick_index: *tick_upper,
                      liquidity_net_delta: amount_val,
                  },
              ]
          }
          _ => vec![],
      }
  }

  #[cfg(test)]
  mod tests {
      use super::*;
      use crate::events::{PoolEvent, PoolEventKind, TxRef};

      fn make_event(kind: PoolEventKind) -> PoolEvent {
          PoolEvent {
              log_ordinal: 0,
              pool_address: vec![0xAA; 20],
              token0: vec![0x11; 20],
              token1: vec![0x22; 20],
              tx: TxRef { hash: vec![0; 32], from: vec![0; 20], to: vec![0; 20], index: 0 },
              kind,
          }
      }

      #[test]
      fn mint_adds_to_lower_subtracts_from_upper() {
          let event = make_event(PoolEventKind::Mint {
              tick_lower: -100,
              tick_upper: 200,
              amount: "1000".to_string(),
              amount0: "0".to_string(),
              amount1: "0".to_string(),
          });
          let deltas = event_to_tick_deltas(&event);
          assert_eq!(deltas.len(), 2);
          assert_eq!(deltas[0].tick_index, -100);
          assert_eq!(deltas[0].liquidity_net_delta, BigInt::from(1000));
          assert_eq!(deltas[1].tick_index, 200);
          assert_eq!(deltas[1].liquidity_net_delta, BigInt::from(-1000));
      }

      #[test]
      fn burn_subtracts_from_lower_adds_to_upper() {
          let event = make_event(PoolEventKind::Burn {
              tick_lower: -100,
              tick_upper: 200,
              amount: "500".to_string(),
              amount0: "0".to_string(),
              amount1: "0".to_string(),
          });
          let deltas = event_to_tick_deltas(&event);
          assert_eq!(deltas[0].liquidity_net_delta, BigInt::from(-500));
          assert_eq!(deltas[1].liquidity_net_delta, BigInt::from(500));
      }

      #[test]
      fn swap_produces_no_tick_deltas() {
          let event = make_event(PoolEventKind::Swap {
              amount0: "100".to_string(),
              amount1: "-100".to_string(),
              sqrt_price: "1".to_string(),
              liquidity: "5000".to_string(),
              tick: 50,
          });
          assert!(event_to_tick_deltas(&event).is_empty());
      }
  }
  ```

- [ ] **Step 3: Create liquidity.rs**

  Create `crates/uniswap-v3-core/src/liquidity.rs`:

  ```rust
  use std::str::FromStr;

  use num_bigint::BigInt;

  use crate::events::{PoolEvent, PoolEventKind};

  pub enum LiquidityChangeKind {
      Delta,
      Absolute,
  }

  pub struct LiquidityDelta {
      pub pool_address: Vec<u8>,
      pub value: BigInt,
      pub kind: LiquidityChangeKind,
  }

  pub fn event_to_liquidity_delta(current_tick: i64, event: &PoolEvent) -> Option<LiquidityDelta> {
      match &event.kind {
          PoolEventKind::Mint { tick_lower, tick_upper, amount, .. } => {
              if current_tick >= i64::from(*tick_lower) && current_tick < i64::from(*tick_upper) {
                  Some(LiquidityDelta {
                      pool_address: event.pool_address.clone(),
                      value: BigInt::from_str(amount).unwrap_or_default(),
                      kind: LiquidityChangeKind::Delta,
                  })
              } else {
                  None
              }
          }
          PoolEventKind::Burn { tick_lower, tick_upper, amount, .. } => {
              if current_tick >= i64::from(*tick_lower) && current_tick < i64::from(*tick_upper) {
                  Some(LiquidityDelta {
                      pool_address: event.pool_address.clone(),
                      value: -BigInt::from_str(amount).unwrap_or_default(),
                      kind: LiquidityChangeKind::Delta,
                  })
              } else {
                  None
              }
          }
          PoolEventKind::Swap { liquidity, .. } => Some(LiquidityDelta {
              pool_address: event.pool_address.clone(),
              value: BigInt::from_str(liquidity).unwrap_or_default(),
              kind: LiquidityChangeKind::Absolute,
          }),
          _ => None,
      }
  }

  #[cfg(test)]
  mod tests {
      use super::*;
      use crate::events::{PoolEvent, PoolEventKind, TxRef};

      fn make_event(kind: PoolEventKind) -> PoolEvent {
          PoolEvent {
              log_ordinal: 0,
              pool_address: vec![0xAA; 20],
              token0: vec![0x11; 20],
              token1: vec![0x22; 20],
              tx: TxRef { hash: vec![0; 32], from: vec![0; 20], to: vec![0; 20], index: 0 },
              kind,
          }
      }

      #[test]
      fn mint_in_range_returns_delta() {
          let event = make_event(PoolEventKind::Mint {
              tick_lower: -100,
              tick_upper: 200,
              amount: "5000".to_string(),
              amount0: "0".to_string(),
              amount1: "0".to_string(),
          });
          // current_tick = 0 is between -100 and 200
          let result = event_to_liquidity_delta(0, &event);
          assert!(result.is_some());
          let d = result.unwrap();
          assert_eq!(d.value, BigInt::from(5000));
          assert!(matches!(d.kind, LiquidityChangeKind::Delta));
      }

      #[test]
      fn mint_out_of_range_returns_none() {
          let event = make_event(PoolEventKind::Mint {
              tick_lower: 100,
              tick_upper: 200,
              amount: "5000".to_string(),
              amount0: "0".to_string(),
              amount1: "0".to_string(),
          });
          // current_tick = 0 is below the range
          assert!(event_to_liquidity_delta(0, &event).is_none());
      }

      #[test]
      fn swap_returns_absolute_liquidity() {
          let event = make_event(PoolEventKind::Swap {
              amount0: "0".to_string(),
              amount1: "0".to_string(),
              sqrt_price: "1".to_string(),
              liquidity: "9999".to_string(),
              tick: 50,
          });
          let result = event_to_liquidity_delta(0, &event).unwrap();
          assert_eq!(result.value, BigInt::from(9999));
          assert!(matches!(result.kind, LiquidityChangeKind::Absolute));
      }
  }
  ```

- [ ] **Step 4: Create output.rs**

  Create `crates/uniswap-v3-core/src/output.rs`:

  ```rust
  use std::str::FromStr;

  use num_bigint::BigInt;

  use crate::events::{PoolEvent, PoolEventKind};

  pub struct AttributeUpdate {
      pub pool_address: Vec<u8>,
      pub name: String,
      pub value: Vec<u8>,
      pub is_creation: bool,
  }

  pub fn event_to_attribute_updates(event: &PoolEvent) -> Vec<AttributeUpdate> {
      match &event.kind {
          PoolEventKind::Initialize { sqrt_price, tick } => {
              vec![
                  AttributeUpdate {
                      pool_address: event.pool_address.clone(),
                      name: "sqrt_price_x96".to_string(),
                      value: BigInt::from_str(sqrt_price)
                          .unwrap_or_default()
                          .to_signed_bytes_be(),
                      is_creation: false,
                  },
                  AttributeUpdate {
                      pool_address: event.pool_address.clone(),
                      name: "tick".to_string(),
                      value: BigInt::from(*tick).to_signed_bytes_be(),
                      is_creation: false,
                  },
              ]
          }
          PoolEventKind::Swap { sqrt_price, tick, .. } => {
              vec![
                  AttributeUpdate {
                      pool_address: event.pool_address.clone(),
                      name: "sqrt_price_x96".to_string(),
                      value: BigInt::from_str(sqrt_price)
                          .unwrap_or_default()
                          .to_signed_bytes_be(),
                      is_creation: false,
                  },
                  AttributeUpdate {
                      pool_address: event.pool_address.clone(),
                      name: "tick".to_string(),
                      value: BigInt::from(*tick).to_signed_bytes_be(),
                      is_creation: false,
                  },
              ]
          }
          PoolEventKind::SetFeeProtocol { fee0_new, fee1_new } => {
              vec![
                  AttributeUpdate {
                      pool_address: event.pool_address.clone(),
                      name: "protocol_fees/token0".to_string(),
                      value: BigInt::from(*fee0_new).to_signed_bytes_be(),
                      is_creation: false,
                  },
                  AttributeUpdate {
                      pool_address: event.pool_address.clone(),
                      name: "protocol_fees/token1".to_string(),
                      value: BigInt::from(*fee1_new).to_signed_bytes_be(),
                      is_creation: false,
                  },
              ]
          }
          _ => vec![],
      }
  }

  #[cfg(test)]
  mod tests {
      use super::*;
      use crate::events::{PoolEvent, PoolEventKind, TxRef};

      fn make_event(kind: PoolEventKind) -> PoolEvent {
          PoolEvent {
              log_ordinal: 0,
              pool_address: vec![0xAA; 20],
              token0: vec![0x11; 20],
              token1: vec![0x22; 20],
              tx: TxRef { hash: vec![0; 32], from: vec![0; 20], to: vec![0; 20], index: 0 },
              kind,
          }
      }

      #[test]
      fn swap_emits_sqrt_price_and_tick() {
          let event = make_event(PoolEventKind::Swap {
              amount0: "0".to_string(),
              amount1: "0".to_string(),
              sqrt_price: "79228162514264337593543950336".to_string(),
              liquidity: "1000".to_string(),
              tick: 42,
          });
          let updates = event_to_attribute_updates(&event);
          assert_eq!(updates.len(), 2);
          assert_eq!(updates[0].name, "sqrt_price_x96");
          assert_eq!(updates[1].name, "tick");
          // tick=42 encoded as signed big-endian
          assert_eq!(updates[1].value, BigInt::from(42).to_signed_bytes_be());
      }

      #[test]
      fn mint_burn_collect_produce_no_attribute_updates() {
          for kind in [
              PoolEventKind::Mint {
                  tick_lower: 0,
                  tick_upper: 100,
                  amount: "1".to_string(),
                  amount0: "1".to_string(),
                  amount1: "1".to_string(),
              },
              PoolEventKind::Collect { amount0: "1".to_string(), amount1: "1".to_string() },
          ] {
              assert!(event_to_attribute_updates(&make_event(kind)).is_empty());
          }
      }
  }
  ```

- [ ] **Step 5: Run the tests**

  ```bash
  cd /Users/kayibal/code/tycho-builder-integration
  cargo nextest run -p uniswap-v3-core 2>&1
  ```

  Expected: all tests in `balance`, `ticks`, `liquidity`, `output` pass. The crate will still error because `processor.rs` is missing — that's fine, tests won't run for missing modules.

  If you see compile errors inside the test modules rather than the processor, fix them before proceeding.

- [ ] **Step 6: Commit**

  ```bash
  git add crates/uniswap-v3-core/
  git commit -m "feat(uniswap-v3-core): add events, math modules with unit tests"
  ```

---

## Task 6: Processor

**Files:** Create `crates/uniswap-v3-core/src/processor.rs`

- [ ] **Step 1: Create processor.rs**

  Create `crates/uniswap-v3-core/src/processor.rs`:

  ```rust
  use std::collections::{HashMap, HashSet};

  use num_bigint::{BigInt, Sign};
  use num_traits::ToPrimitive as _;
  use tycho_common::{
      models::{
          blockchain::{Block, BlockAggregatedChanges, LogInput, TxInput},
          protocol::{ComponentBalance, ProtocolComponentStateDelta},
          Chain,
      },
      traits::TxDeltaIndexer,
      Bytes,
  };
  use tycho_substreams::prelude::{
      Attribute, BalanceChange, ChangeType, EntityChanges, Transaction as SubstreamsTx,
      TransactionChanges, TransactionChangesBuilder,
  };

  use crate::{
      balance::event_to_balance_deltas,
      events::{decode_log, Pool, PoolEvent, TxRef},
      liquidity::{event_to_current_tick, event_to_liquidity_delta, LiquidityChangeKind},
      output::event_to_attribute_updates,
      ticks::event_to_tick_deltas,
  };

  #[derive(Clone)]
  pub struct UniswapV3Processor {
      chain: Chain,
      extractor: String,
      last_block: Option<Block>,
      finalized_block_height: u64,
      pools: HashMap<String, Pool>,
      balances: HashMap<(String, String), BigInt>,
      tick_liquidity: HashMap<(String, i32), BigInt>,
      current_tick: HashMap<String, i64>,
      pool_liquidity: HashMap<String, BigInt>,
      baseline_tick_keys: HashSet<(String, i32)>,
  }

  impl TxDeltaIndexer for UniswapV3Processor {
      fn apply_block(&mut self, block: &BlockAggregatedChanges) {
          self.chain = block.chain;
          self.last_block = Some(block.block.clone());
          self.finalized_block_height = block.finalized_block_height;

          for (id, comp) in &block.new_protocol_components {
              if comp.tokens.len() >= 2 {
                  self.pools.insert(
                      id.clone(),
                      Pool {
                          address: hex::decode(id).unwrap_or_default(),
                          token0: comp.tokens[0].to_vec(),
                          token1: comp.tokens[1].to_vec(),
                      },
                  );
              }
          }

          for (component_id, delta) in &block.state_deltas {
              self.apply_state_delta(component_id, delta);
          }

          for (component_id, token_balances) in &block.component_balances {
              for (token_bytes, balance) in token_balances {
                  let token_hex = hex::encode(token_bytes.as_ref());
                  let balance_val =
                      BigInt::from_bytes_be(Sign::Plus, balance.balance.as_ref());
                  self.balances
                      .insert((component_id.clone(), token_hex), balance_val);
              }
          }

          for id in block.deleted_protocol_components.keys() {
              self.remove_pool(id);
          }
      }

      fn generate_deltas(&mut self, txs: &[TxInput]) -> BlockAggregatedChanges {
          let mut scratch = self.clone();
          let tx_changes = scratch.build_tx_changes(txs);

          let mut state_deltas: HashMap<String, ProtocolComponentStateDelta> = HashMap::new();
          let mut component_balances: HashMap<String, HashMap<Bytes, ComponentBalance>> =
              HashMap::new();

          for changes in tx_changes {
              let tx_hash = changes
                  .tx
                  .as_ref()
                  .map(|t| Bytes::from(t.hash.clone()))
                  .unwrap_or_default();

              for ec in changes.entity_changes {
                  let delta = state_deltas
                      .entry(ec.component_id.clone())
                      .or_insert_with(|| ProtocolComponentStateDelta {
                          component_id: ec.component_id.clone(),
                          updated_attributes: HashMap::new(),
                          deleted_attributes: HashSet::new(),
                      });
                  for attr in ec.attributes {
                      if attr.change == i32::from(ChangeType::Deletion) {
                          delta.deleted_attributes.insert(attr.name.clone());
                          delta.updated_attributes.remove(&attr.name);
                      } else {
                          delta
                              .updated_attributes
                              .insert(attr.name.clone(), Bytes::from(attr.value));
                          delta.deleted_attributes.remove(&attr.name);
                      }
                  }
              }

              for bc in changes.balance_changes {
                  let comp_id = hex::encode(&bc.component_id);
                  let token = Bytes::from(bc.token);
                  let balance = Bytes::from(bc.balance);
                  let balance_float = BigInt::from_bytes_be(Sign::Plus, balance.as_ref())
                      .to_f64()
                      .unwrap_or(f64::MAX);
                  component_balances
                      .entry(comp_id.clone())
                      .or_default()
                      .insert(
                          token.clone(),
                          ComponentBalance {
                              token,
                              balance,
                              balance_float,
                              modify_tx: tx_hash.clone(),
                              component_id: comp_id,
                          },
                      );
              }
          }

          BlockAggregatedChanges {
              extractor: self.extractor.clone(),
              chain: self.chain,
              block: self.pending_block(),
              finalized_block_height: self.finalized_block_height,
              state_deltas,
              component_balances,
              ..Default::default()
          }
      }
  }

  impl UniswapV3Processor {
      pub fn new(chain: Chain, extractor: String) -> Self {
          Self {
              chain,
              extractor,
              last_block: None,
              finalized_block_height: 0,
              pools: HashMap::new(),
              balances: HashMap::new(),
              tick_liquidity: HashMap::new(),
              current_tick: HashMap::new(),
              pool_liquidity: HashMap::new(),
              baseline_tick_keys: HashSet::new(),
          }
      }

      fn pending_block(&self) -> Block {
          match &self.last_block {
              Some(b) => Block {
                  number: b.number + 1,
                  hash: Bytes::default(),
                  parent_hash: b.hash.clone(),
                  chain: b.chain,
                  ts: b.ts,
              },
              None => Block::default(),
          }
      }

      fn apply_state_delta(
          &mut self,
          component_id: &str,
          delta: &ProtocolComponentStateDelta,
      ) {
          for attr_name in &delta.deleted_attributes {
              if attr_name == "tick" {
                  self.current_tick.remove(component_id);
              } else if attr_name == "liquidity" {
                  self.pool_liquidity.remove(component_id);
              } else if let Some(rest) = attr_name.strip_prefix("ticks/") {
                  if let Some(idx_str) = rest.strip_suffix("/net-liquidity") {
                      if let Ok(idx) = idx_str.parse::<i32>() {
                          let key = (component_id.to_string(), idx);
                          self.tick_liquidity.remove(&key);
                          self.baseline_tick_keys.remove(&key);
                      }
                  }
              }
          }

          for (attr_name, attr_val) in &delta.updated_attributes {
              if attr_name == "tick" {
                  let tick_val = BigInt::from_signed_bytes_be(attr_val.as_ref());
                  let (sign, digits) = tick_val.to_u64_digits();
                  let magnitude = digits.first().copied().unwrap_or(0) as i64;
                  let tick_i64 = if sign == Sign::Minus { -magnitude } else { magnitude };
                  self.current_tick.insert(component_id.to_string(), tick_i64);
              } else if attr_name == "liquidity" {
                  self.pool_liquidity.insert(
                      component_id.to_string(),
                      BigInt::from_signed_bytes_be(attr_val.as_ref()),
                  );
              } else if let Some(rest) = attr_name.strip_prefix("ticks/") {
                  if let Some(idx_str) = rest.strip_suffix("/net-liquidity") {
                      if let Ok(idx) = idx_str.parse::<i32>() {
                          let key = (component_id.to_string(), idx);
                          self.tick_liquidity.insert(
                              key.clone(),
                              BigInt::from_signed_bytes_be(attr_val.as_ref()),
                          );
                          self.baseline_tick_keys.insert(key);
                      }
                  }
              }
          }
      }

      fn remove_pool(&mut self, id: &str) {
          self.pools.remove(id);
          self.current_tick.remove(id);
          self.pool_liquidity.remove(id);
          self.balances.retain(|(pool_id, _), _| pool_id != id);
          self.tick_liquidity.retain(|(pool_id, _), _| pool_id != id);
          self.baseline_tick_keys.retain(|(pool_id, _)| pool_id != id);
      }

      fn build_tx_changes(&mut self, txs: &[TxInput]) -> Vec<TransactionChanges> {
          let mut tx_builders: HashMap<Vec<u8>, (u64, TransactionChangesBuilder)> =
              HashMap::new();

          for tx in txs {
              if !tx.succeeded() {
                  continue;
              }

              let tx_ref = TxRef {
                  hash: tx.hash().to_vec(),
                  from: tx.from().to_vec(),
                  to: tx.to().to_vec(),
                  index: tx.index(),
              };

              let mut events: Vec<PoolEvent> = Vec::new();
              for log in tx.logs() {
                  let pool_hex = hex::encode(log.address().as_ref());
                  let Some(pool) = self.pools.get(&pool_hex) else { continue };
                  let ordinal = tx.index() * 100_000 + u64::from(log.log_index());
                  let pb_log = log_input_to_pb(log, ordinal);
                  if let Some(event) = decode_log(&pb_log, pool, &tx_ref) {
                      events.push(event);
                  }
              }

              if events.is_empty() {
                  continue;
              }

              tx_builders.entry(tx.hash().to_vec()).or_insert_with(|| {
                  let substreams_tx = SubstreamsTx {
                      hash: tx.hash().to_vec(),
                      from: tx.from().to_vec(),
                      to: tx.to().to_vec(),
                      index: tx.index(),
                  };
                  (tx.index(), TransactionChangesBuilder::new(&substreams_tx))
              });

              for event in events {
                  let (_, builder) = tx_builders
                      .get_mut(tx.hash().as_ref())
                      .expect("builder inserted above");
                  self.apply_event(event, builder);
              }
          }

          let mut ordered: Vec<(u64, TransactionChangesBuilder)> =
              tx_builders.into_values().collect();
          ordered.sort_unstable_by_key(|(idx, _)| *idx);
          ordered
              .into_iter()
              .filter_map(|(_, b)| b.build())
              .collect()
      }

      fn apply_event(&mut self, event: PoolEvent, builder: &mut TransactionChangesBuilder) {
          let pool_hex = hex::encode(&event.pool_address);

          if let Some(new_tick) = event_to_current_tick(&event) {
              self.current_tick.insert(pool_hex.clone(), new_tick);
          }

          for delta in event_to_balance_deltas(&event) {
              let token_hex = hex::encode(&delta.token);
              let running = self.balances.entry((pool_hex.clone(), token_hex)).or_default();
              *running += &delta.delta;
              let clamped =
                  if *running < BigInt::default() { BigInt::default() } else { running.clone() };
              builder.add_balance_change(&BalanceChange {
                  component_id: event.pool_address.clone(),
                  token: delta.token.clone(),
                  balance: clamped.to_bytes_be().1,
              });
          }

          for tick_delta in event_to_tick_deltas(&event) {
              let key = (pool_hex.clone(), tick_delta.tick_index);
              let existed_before =
                  self.tick_liquidity.contains_key(&key) || self.baseline_tick_keys.contains(&key);
              let running = self.tick_liquidity.entry(key).or_default();
              *running += &tick_delta.liquidity_net_delta;
              let new_val = running.clone();

              let change_type = if !existed_before {
                  ChangeType::Creation
              } else if new_val == BigInt::default() {
                  ChangeType::Deletion
              } else {
                  ChangeType::Update
              };

              builder.add_entity_change(&EntityChanges {
                  component_id: pool_hex.clone(),
                  attributes: vec![Attribute {
                      name: format!("ticks/{}/net-liquidity", tick_delta.tick_index),
                      value: new_val.to_signed_bytes_be(),
                      change: change_type.into(),
                  }],
              });
          }

          let cur_tick = *self.current_tick.get(&pool_hex).unwrap_or(&0);
          if let Some(liq_delta) = event_to_liquidity_delta(cur_tick, &event) {
              let running = self.pool_liquidity.entry(pool_hex.clone()).or_default();
              match liq_delta.kind {
                  LiquidityChangeKind::Delta => *running += &liq_delta.value,
                  LiquidityChangeKind::Absolute => *running = liq_delta.value.clone(),
              }
              builder.add_entity_change(&EntityChanges {
                  component_id: pool_hex.clone(),
                  attributes: vec![Attribute {
                      name: "liquidity".to_string(),
                      value: running.to_signed_bytes_be(),
                      change: ChangeType::Update.into(),
                  }],
              });
          }

          for attr_update in event_to_attribute_updates(&event) {
              let comp_id = hex::encode(&attr_update.pool_address);
              let change_type = if attr_update.is_creation {
                  ChangeType::Creation
              } else {
                  ChangeType::Update
              };
              builder.add_entity_change(&EntityChanges {
                  component_id: comp_id,
                  attributes: vec![Attribute {
                      name: attr_update.name,
                      value: attr_update.value,
                      change: change_type.into(),
                  }],
              });
          }
      }
  }

  fn log_input_to_pb(
      log: &LogInput,
      ordinal: u64,
  ) -> substreams_ethereum::pb::eth::v2::Log {
      substreams_ethereum::pb::eth::v2::Log {
          address: log.address().to_vec(),
          topics: log.topics().iter().map(|t| t.to_vec()).collect(),
          data: log.data().to_vec(),
          ordinal,
          ..Default::default()
      }
  }
  ```

  **Note on `event_to_current_tick`:** The `events.rs` module does not export this function yet — it is called in `processor.rs`. Add it to `events.rs` now:

  At the bottom of `crates/uniswap-v3-core/src/events.rs`, append:

  ```rust
  /// Returns the new current tick if this event changes it (Swap or Initialize).
  pub fn event_to_current_tick(event: &PoolEvent) -> Option<i64> {
      match &event.kind {
          PoolEventKind::Swap { tick, .. } | PoolEventKind::Initialize { tick, .. } => {
              Some(i64::from(*tick))
          }
          _ => None,
      }
  }
  ```

- [ ] **Step 2: Verify compilation**

  ```bash
  cargo check -p uniswap-v3-core 2>&1
  ```

  Expected: `Finished` with no errors. If there are type mismatches from `tycho_common`, check that `tycho-common` resolved to `>=0.302.0` in `Cargo.lock` — run `cargo check --workspace` to update the lockfile if needed.

- [ ] **Step 3: Run all tests**

  ```bash
  cargo nextest run -p uniswap-v3-core 2>&1
  ```

  Expected: all unit tests pass.

- [ ] **Step 4: Commit**

  ```bash
  git add crates/uniswap-v3-core/
  git commit -m "feat(uniswap-v3-core): add UniswapV3Processor implementing TxDeltaIndexer"
  ```

---

## Task 7: Processor unit test

**Files:** Add `#[cfg(test)]` block at the bottom of `crates/uniswap-v3-core/src/processor.rs`

This test verifies the full pipeline: apply a snapshot block → call `generate_deltas` with a succeeded tx containing no UniswapV3 logs → assert no state deltas are returned.

- [ ] **Step 1: Write the test**

  Append to the bottom of `crates/uniswap-v3-core/src/processor.rs`:

  ```rust
  #[cfg(test)]
  mod tests {
      use std::collections::HashMap;

      use tycho_common::{
          models::{
              blockchain::{Block, BlockAggregatedChanges, LogInput, TxInput},
              protocol::ProtocolComponent,
              Chain,
          },
          Bytes,
      };

      use super::UniswapV3Processor;

      fn make_block(number: u64, pool_addr_hex: &str, token0: &[u8], token1: &[u8]) -> BlockAggregatedChanges {
          let mut new_protocol_components = HashMap::new();
          new_protocol_components.insert(
              pool_addr_hex.to_string(),
              ProtocolComponent {
                  id: pool_addr_hex.to_string(),
                  tokens: vec![Bytes::from(token0.to_vec()), Bytes::from(token1.to_vec())],
                  ..Default::default()
              },
          );
          BlockAggregatedChanges {
              extractor: "uniswap_v3".to_string(),
              chain: Chain::Ethereum,
              block: Block { number, ..Default::default() },
              finalized_block_height: number,
              new_protocol_components,
              state_deltas: HashMap::new(),
              component_balances: HashMap::new(),
              ..Default::default()
          }
      }

      fn make_tx(hash: &[u8], logs: Vec<LogInput>, succeeded: bool) -> TxInput {
          TxInput::new(
              Bytes::from(hash.to_vec()),
              Bytes::from(vec![0u8; 20]),
              Bytes::from(vec![0u8; 20]),
              0,
              logs,
              succeeded,
          )
      }

      #[test]
      fn no_pools_returns_empty_deltas() {
          let mut proc = UniswapV3Processor::new(Chain::Ethereum, "uniswap_v3".to_string());
          let tx = make_tx(&[1u8; 32], vec![], true);
          let result = proc.generate_deltas(&[tx]);
          assert!(result.state_deltas.is_empty());
          assert!(result.component_balances.is_empty());
      }

      #[test]
      fn failed_tx_is_skipped() {
          let token0 = vec![0x11u8; 20];
          let token1 = vec![0x22u8; 20];
          let pool_addr = vec![0xAAu8; 20];
          let pool_hex = hex::encode(&pool_addr);

          let mut proc = UniswapV3Processor::new(Chain::Ethereum, "uniswap_v3".to_string());
          proc.apply_block(&make_block(100, &pool_hex, &token0, &token1));

          // A failed tx with logs at the pool address — should produce no deltas.
          let log = LogInput::new(
              Bytes::from(pool_addr.clone()),
              vec![],
              Bytes::default(),
              0,
          );
          let tx = make_tx(&[2u8; 32], vec![log], false);
          let result = proc.generate_deltas(&[tx]);
          assert!(result.state_deltas.is_empty());
      }

      #[test]
      fn apply_block_does_not_mutate_on_generate_deltas() {
          let token0 = vec![0x11u8; 20];
          let token1 = vec![0x22u8; 20];
          let pool_addr = vec![0xAAu8; 20];
          let pool_hex = hex::encode(&pool_addr);

          let mut proc = UniswapV3Processor::new(Chain::Ethereum, "uniswap_v3".to_string());
          proc.apply_block(&make_block(100, &pool_hex, &token0, &token1));

          // generate_deltas twice with the same (empty) txs → same result both times.
          let txs: &[TxInput] = &[];
          let r1 = proc.generate_deltas(txs);
          let r2 = proc.generate_deltas(txs);
          assert_eq!(r1.state_deltas.len(), r2.state_deltas.len());
      }
  }
  ```

- [ ] **Step 2: Run the new tests**

  ```bash
  cargo nextest run -p uniswap-v3-core 2>&1
  ```

  Expected: all tests pass including the three new processor tests.

- [ ] **Step 3: Commit**

  ```bash
  git add crates/uniswap-v3-core/src/processor.rs
  git commit -m "test(uniswap-v3-core): add processor unit tests"
  ```

---

## Task 8: Wire processor into backrunner

**Files:** Modify `crates/backrunner/Cargo.toml` and `crates/backrunner/src/lib.rs`

- [ ] **Step 1: Add dependency**

  In `crates/backrunner/Cargo.toml`, add to `[dependencies]`:

  ```toml
  uniswap-v3-core = { path = "../uniswap-v3-core" }
  ```

- [ ] **Step 2: Register the processor in Backrunner::build()**

  In `crates/backrunner/src/lib.rs`, add the import at the top (after existing `use` statements):

  ```rust
  use uniswap_v3_core::processor::UniswapV3Processor;
  ```

  Then find the `FyndBuilder::new(...)` call in `Backrunner::build()` (around line 188) and add `.with_pending_indexer(...)` before `.algorithm(...)`:

  **Before:**
  ```rust
  let builder = FyndBuilder::new(
      chain,
      config.tycho_url,
      config.rpc_url.clone(),
      config.protocols,
      config.min_tvl,
  )
  .algorithm("bellman_ford");
  ```

  **After:**
  ```rust
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
  ```

  Note: `chain` is the result of `parse_chain(&config.chain)?` earlier in the function. It is `Copy` so it can be used twice.

- [ ] **Step 3: Verify the workspace compiles**

  ```bash
  cd /Users/kayibal/code/tycho-builder-integration
  cargo check --workspace 2>&1
  ```

  Expected: `Finished` with no errors. If you see a "use of moved value: chain" error, verify `Chain` derives `Copy` in tycho-common — it does in 0.302.x.

- [ ] **Step 4: Commit**

  ```bash
  git add crates/backrunner/Cargo.toml crates/backrunner/src/lib.rs
  git commit -m "feat(backrunner): register UniswapV3Processor for pending AMM state"
  ```

---

## Task 9: Final verification

- [ ] **Step 1: Run clippy across the workspace**

  ```bash
  cd /Users/kayibal/code/tycho-builder-integration
  cargo clippy --all-targets --all-features -- -D warnings 2>&1
  ```

  The `abi/` modules already have `#![allow(clippy::all)]` so generated-code lints won't surface. Fix any warnings in `processor.rs`, `events.rs`, or the math modules before proceeding. Common ones:
  - `clippy::expect_used` — replace `.expect("builder inserted above")` with a proper `if let` or return-early pattern if clippy denies it at workspace level
  - `clippy::unwrap_used` — same treatment

  If `clippy::expect_used` fires in `build_tx_changes`, the fix is:

  ```rust
  // Replace:
  let (_, builder) = tx_builders
      .get_mut(tx.hash().as_ref())
      .expect("builder inserted above");

  // With:
  let Some((_, builder)) = tx_builders.get_mut(tx.hash().as_ref()) else {
      continue;
  };
  ```

- [ ] **Step 2: Run all tests**

  ```bash
  cargo nextest run --workspace 2>&1
  ```

  Expected: all tests pass. The backrunner crate has no tests that exercise the live Tycho connection, so the new integration doesn't break anything.

- [ ] **Step 3: Update Cargo.lock**

  After adding new crates, update the lockfile minimally:

  ```bash
  cargo check --workspace
  ```

- [ ] **Step 4: Final commit**

  ```bash
  git add Cargo.lock
  git commit -m "chore: update Cargo.lock after uniswap-v3-core integration"
  ```
