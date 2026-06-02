# backrunner crate

Block-builder backrun engine for 1inch Fusion Dutch-auction orders on Ethereum.

## What this crate does

Polls the 1inch Fusion active-orders API every 12 seconds, and on each confirmed
Ethereum block evaluates whether any open Fusion order can be profitably filled using
a Fynd (Tycho) DEX route. If profitable, it encodes the settlement calldata and
publishes a `BackrunCandidate`.

Integration: the builder drives the backrunner by sending `BuildEvent`s over an mpsc
channel; the backrunner returns candidates over a watch channel. See `lib.rs` top-level
doc for the minimal wiring snippet.

## File map

| File | Responsibility |
|------|---------------|
| `src/lib.rs` | Public API: `Backrunner`, `BackrunnerConfig`, event loop, profitability logic |
| `src/order.rs` | `FusionOrder`, on-chain taking-amount formula, Dutch auction interpolation |
| `src/client.rs` | 1inch Fusion orders API client + extension decoding |
| `src/abi.rs` | `build_settle_calldata` — encodes `settleOrders(fillOrderArgs(...))` |
| `src/encode_test.rs` | Integration tests for `build_settle_calldata` using real order data |
| `src/bin/smoke.rs` | Long-running smoke test: one synthetic builder iteration per block |
| `bytecode/BackrunResolver.runtime.hex` | Compiled `BackrunResolver.sol` runtime bytecode |

## Key invariants

### Dutch-auction formula (`order.rs:onchain_taking_amount`)

Matches `SimpleSettlement._getTakingAmount` exactly. Order matters:

```
auctionBump = piecewise-linear interpolation of rate-bump coefficients (units 1e7)
gasBump     = gasBumpEstimate × baseFee_wei / (gasPriceEstimate × 1_000_000)
rateBump    = max(0, auctionBump − gasBump)
withFees    = ceil(floor × (1e5 + totalFees) / 1e5)   ← fee FIRST
final       = ceil(withFees × (1e7 + rateBump) / 1e7) ← rate bump SECOND
```

**Fee before rate-bump** — this is the opposite of the 1inch TypeScript SDK static method
`AmountCalculator.calcAuctionTakingAmount`, which does rate-first/fee-second/floor.
The instance method `getRequiredTakingAmount` and the on-chain contract match our order.

**`interp_bump` uses floor division** matching Solidity's integer semantics.
`AuctionPoint.delay_secs` is CUMULATIVE from `auction_start_time` (not relative).

### Extension decoding (`client.rs:decode_extension_params`)

The 1inch API's `auctionStartDate`/`auctionEndDate`/`initialRateBump`/`points` fields
are often wrong or empty. Always decode canonical values from extension bytes:

```
hex offsets (after stripping 0x):
  [0:64]    LOP section-length header (32 bytes)
  [64:104]  Dutch-auction extension address (20 bytes)
  [104:110] gasBumpEstimate  uint24
  [110:118] gasPriceEstimate uint32 (Mwei = 10^6 wei)
  [118:126] startTime        uint32
  [126:132] duration         uint24
  [132:138] initialRateBump  uint24
  [138:140] point count      uint8
  [140+]    N × (coeff uint24 [6 chars] + timeDelta_relative uint16 [4 chars])
  [after points] integratorFee uint16 | integratorShare uint8 | resolverFee uint16 | ...
```

Extension `timeDelta` values are **relative** (each added to the previous cumulative time).
`AuctionPoint.delay_secs` stored in `FusionOrder` is cumulative.

### On-chain pre-flight check (`lib.rs:query_onchain_taking_amount`)

Before encoding a fill, we static-call `extension.getTakingAmount(...)` via
`LOP.simulate()` with a block timestamp override set to the **pending** block timestamp.
Without the timestamp override, `eth_call` runs at the confirmed block time (~12s earlier),
which can land before auction start and return the full start price.

`LOP.simulate()` always reverts with `SimulationResults(bool success, bytes result)`.
We decode the revert data — if `success == false` the order is expired or invalid.

### Settlement calldata (`abi.rs`)

Call chain: `executor → resolver.settleOrders(fill_calldata)`
  `→ lop.fillOrderArgs(order, r, vs, amount, takerTraits, args)`
  `→ resolver.takerInteraction(..., extraData)`
  `→ fynd_router.call(primary_swap_calldata)`

`args` layout (LOP `_parseArgs`):
```
[resolver_addr 20B] [extension bytes] [interaction bytes]
```

`interaction` layout:
```
[resolver_addr 20B raw] [abi_encode_sequence(router, primaryCalldata, surplusCalldata)]
```

`takerTraits` flags:
- Bit 255 (`MAKER_AMOUNT_FLAG`): `amount` param is making-amount units; threshold = max taking
- Bit 251 (`ARGS_HAS_TARGET`): first 20 bytes of args are the `makerAsset` target
- Bits 224-247: extension byte length
- Bits 200-223: interaction byte length
- Bits 0-184: `taking_amount` threshold (LOP reverts `TakingAmountTooHigh` if exceeded)

EOA-signed orders: `fillOrderArgs` (ecrecover, EIP-2098 compact `vs`).
ERC-1271 makers: `fillContractOrderArgs`.

## Running the smoke test

```bash
RUST_LOG=warn,backrunner=debug \
TYCHO_URL=tycho-beta.propellerheads.xyz \
TYCHO_API_KEY=... \
ETH_RPC_URL=... \
  cargo run --bin smoke
```

Key log lines to watch:
- `order passed profitability filter — querying on-chain taking amount` → eth_call about to fire
- `eth_call SUCCESS ✓` → fill would succeed at this block
- `eth_call REVERTED` → fill reverted; trace logged on first tx

After contract changes, regenerate the embedded bytecode:
```bash
forge script contracts/script/PrintBytecode.s.sol --silent
cp contracts/out/BackrunResolver.runtime.hex crates/backrunner/bytecode/BackrunResolver.runtime.hex
```

## Running tests

```bash
cargo test -p backrunner
```

Tests are colocated: `order.rs` has unit tests for the taking-amount formula,
`client.rs` has extension-decoding tests, `abi.rs` has calldata-encoding tests,
`encode_test.rs` has integration tests from real smoke-run orders.

All regression tests in `order.rs` are anchored to real orders observed in live smoke runs
and verified against `query_onchain_taking_amount` ground truth.

## Known gaps

- **Resolver fee in extension section 3**: the extension's third data section can encode an
  additional resolver-tier fee. We don't decode it off-chain — our `taking_estimate` can be
  ~0.5% lower than the actual on-chain value. The `query_onchain_taking_amount` pre-flight
  handles this correctly at runtime; these tests verify the auction-curve arithmetic only.

- **GTC orders filtered**: `is_gtc_order` drops orders with `start_amount <= end_amount` and
  `duration > 1h`. These are limit orders with no auction premium and no backrun opportunity.

- **Partial fills**: `query_remaining_making_amount` reads `LOP.remainingInvalidatorForOrder`
  to detect partial fills before quoting Fynd. Fresh orders return `U256::MAX` (never touched).
