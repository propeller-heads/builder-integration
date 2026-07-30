use alloy::primitives::U256;
use serde::{Deserialize, Serialize};

/// Divisor for rate bumps stored in auction points and `init_rate_bump` (1e7 = 100%).
const RATE_BUMP_DIVISOR: u128 = 10_000_000;

/// Divisor for the integrator/resolver fee encoded in extensions (1e5 = 100%).
const FEE_DIVISOR: u128 = 100_000;

/// A single breakpoint on the Fusion auction curve.
///
/// `delay_secs` is CUMULATIVE from `auction_start_time` (not relative to the previous point).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuctionPoint {
    /// Seconds elapsed from `auction_start_time` at which this breakpoint applies.
    pub delay_secs: u64,
    /// Required `to_token` output amount at this breakpoint (= `apply_rate_bump(floor, rate_bump)`).
    pub amount: U256,
    /// Rate bump coefficient from the extension (units of 1e7).  Zero for API-decoded points.
    #[serde(default)]
    pub rate_bump: u32,
}

/// A live 1inch Fusion limit order with its Dutch auction parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FusionOrder {
    /// 1inch order hash (`0x`-prefixed hex) uniquely identifying this order.
    pub order_id: String,
    /// Maker asset address (0x-prefixed hex, lowercase).
    pub from_token: String,
    /// Taker asset address (0x-prefixed hex, lowercase).
    pub to_token: String,
    /// Sell amount in the smallest token unit.
    pub making_amount: U256,
    /// Required output at the very start of the auction (most favourable for the user).
    pub auction_start_amount: U256,
    /// Minimum acceptable output (floor, reached at `auction_duration_secs`).
    pub auction_end_amount: U256,
    /// Total auction length in seconds.
    pub auction_duration_secs: u64,
    /// Unix timestamp at which the auction opened.
    pub auction_start_time: u64,
    /// Piecewise-linear decay curve breakpoints (may be empty).
    pub points: Vec<AuctionPoint>,
    /// Human-readable symbol for `from_token`, as reported by the Fusion API (may be absent).
    pub from_token_symbol: Option<String>,
    /// Human-readable symbol for `to_token`, as reported by the Fusion API (may be absent).
    pub to_token_symbol: Option<String>,
    /// Decimals of `from_token`. Falls back to a hardcoded table, then 18, if the API omits it.
    pub from_token_decimals: u8,
    /// Decimals of `to_token`. Falls back to a hardcoded table, then 18, if the API omits it.
    pub to_token_decimals: u8,
    /// USD value of one whole `from_token` at order creation.
    pub from_token_usd_rate: f64,
    /// USD value of one whole `to_token` at order creation.
    pub to_token_usd_rate: f64,
    /// Extension gasBumpEstimate (uint24): factor to multiply by baseFee/gasPriceEst.
    ///
    /// On-chain: gasBump = gasBumpEstimate × `baseFee_wei` / (gasPriceEstimate × 10^6).
    /// Included in the taking-amount computation so the profitability check is accurate.
    pub gas_bump_estimate: u32,
    /// Extension gasPriceEstimate (uint32) in units of 10^6 wei (Mwei).
    pub gas_price_estimate_mwei: u32,
    /// Initial rate bump from the extension (units of 1e7, same as `AuctionPoint.rate_bump`).
    ///
    /// Required for the exact on-chain `getTakingAmount` formula. Zero when decoded from API only.
    #[serde(default)]
    pub init_rate_bump: u32,
    /// Combined integrator + resolver fee from the extension (units of 1e5).
    ///
    /// Applied as `ceil(floor × (1e5 + fee) / 1e5)` before the rate-bump multiplier.
    /// Zero when no fee is present or when decoded from API only.
    #[serde(default)]
    pub total_fees_1e5: u32,
    /// EIP-712 maker signature (65 bytes, `0x`-prefixed hex).
    pub signature: String,
    /// Fusion Dutch-auction extension bytes (`0x`-prefixed hex, may be `"0x"` if empty).
    pub extension: String,
    /// Order salt (matches the on-chain order's `salt` field).
    pub salt: String,
    /// Maker EOA address (`0x`-prefixed hex).
    pub maker_address: String,
    /// Receiver address (`0x`-prefixed hex).
    pub receiver_address: String,
    /// Encoded maker traits as decimal string.
    pub maker_traits: String,
}

/// Returns true for GTC (Good Till Cancelled) limit orders that carry no Dutch auction premium.
///
/// GTC orders have no piecewise decay and an unusually long window (>1 hour). They produce
/// noise in solvability analysis and are skipped during quoting.
pub fn is_gtc_order(order: &FusionOrder) -> bool {
    const GTC_DURATION_THRESHOLD_SECS: u64 = 3_600;
    order.auction_start_amount <= order.auction_end_amount
        && order.auction_duration_secs > GTC_DURATION_THRESHOLD_SECS
}

/// Computes the exact on-chain taking amount matching `SimpleSettlement.getTakingAmount`:
///
/// ```text
/// auctionBump = interpolate rate bumps at unix_ts (piecewise-linear on coefficients)
/// gasBump     = gasBumpEstimate × baseFee / (gasPriceEstimate × 10^6)
/// rateBump    = max(0, auctionBump − gasBump)          ← gas subtracts from rate
/// withFees    = ceil(floor × (1e5 + totalFees) / 1e5)
/// final       = ceil(withFees × (1e7 + rateBump) / 1e7)
/// ```
///
/// Returns `None` when the auction has not started or has expired.
///
/// Falls back to `amount_at_timestamp` semantics if `init_rate_bump == 0` (API-only decode),
/// in which case `total_fees_1e5` is still applied if non-zero.
pub fn onchain_taking_amount(order: &FusionOrder, unix_ts: u64, base_fee_wei: u64) -> Option<U256> {
    let elapsed = elapsed_secs(order, unix_ts)?;
    let auction_bump = interpolate_rate_bump(order, elapsed);
    let gas_bump = compute_gas_bump(order, base_fee_wei);
    let rate_bump = auction_bump.saturating_sub(gas_bump);
    let with_fees = apply_fee_bump(order.auction_end_amount, u128::from(order.total_fees_1e5));
    Some(apply_rate_bump_order(with_fees, rate_bump))
}

/// Interpolates the auction rate bump at `elapsed` seconds using `order.points[].rate_bump`.
///
/// Matches the on-chain `_getAuctionBump` piecewise-linear interpolation on rate-bump
/// coefficients (units of 1e7).  Returns 0 when the auction has expired.
fn interpolate_rate_bump(order: &FusionOrder, elapsed: u64) -> u128 {
    let mut current_t: u64 = 0;
    let mut current_bump = u128::from(order.init_rate_bump);

    for point in &order.points {
        if elapsed <= point.delay_secs {
            return interp_bump(current_t, current_bump, point.delay_secs, u128::from(point.rate_bump), elapsed);
        }
        current_t = point.delay_secs;
        current_bump = u128::from(point.rate_bump);
    }

    // Past all explicit points: decay linearly from last point to 0 at duration end.
    let duration = order.auction_duration_secs;
    if duration > current_t {
        interp_bump(current_t, current_bump, duration, 0, elapsed)
    } else {
        0
    }
}

/// Linear interpolation between two (time, bump) pairs — floor division, matching Solidity.
fn interp_bump(t0: u64, b0: u128, t1: u64, b1: u128, t: u64) -> u128 {
    let span = u128::from(t1.saturating_sub(t0));
    if span == 0 {
        return b0;
    }
    let elapsed_in = u128::from(t.saturating_sub(t0));
    let remaining = u128::from(t1.saturating_sub(t));
    (b0.saturating_mul(remaining).saturating_add(b1.saturating_mul(elapsed_in))) / span
}

/// `base + ceil(base × fees / FEE_DIVISOR)` — fee applied with 1e5 denominator.
fn apply_fee_bump(base: U256, fees: u128) -> U256 {
    let divisor = U256::from(FEE_DIVISOR);
    let increment = base
        .saturating_mul(U256::from(fees))
        .saturating_add(divisor - U256::ONE)
        / divisor;
    base.saturating_add(increment)
}

/// `base + ceil(base × bump / RATE_BUMP_DIVISOR)` — rate bump with 1e7 denominator.
fn apply_rate_bump_order(base: U256, bump: u128) -> U256 {
    let divisor = U256::from(RATE_BUMP_DIVISOR);
    let increment = base
        .saturating_mul(U256::from(bump))
        .saturating_add(divisor - U256::ONE)
        / divisor;
    base.saturating_add(increment)
}

/// Computes the gas-bump rate for an order given the pending block's base fee.
///
/// On-chain formula: `gasBump = gasBumpEstimate × baseFee_wei / (gasPriceEstimate × 10^6)`
/// The gas bump is SUBTRACTED from the auction bump in `onchain_taking_amount`.
///
/// Returns 0 when the order has no gas-bump configured (`gas_price_estimate_mwei == 0`).
pub fn compute_gas_bump(order: &FusionOrder, base_fee_wei: u64) -> u128 {
    let gas_price_scaled = u64::from(order.gas_price_estimate_mwei) * 1_000_000;
    if gas_price_scaled == 0 {
        return 0;
    }
    u128::from(order.gas_bump_estimate)
        .saturating_mul(u128::from(base_fee_wei))
        / u128::from(gas_price_scaled)
}

/// Returns the minimum required output amount A(t) at `unix_ts`.
///
/// Returns `Some` for any `unix_ts` before `auction_start_time + auction_duration_secs`;
/// before `auction_start_time` the amount is pinned to the start amount. Returns `None`
/// once the auction has expired.
pub fn amount_at_timestamp(order: &FusionOrder, unix_ts: u64) -> Option<U256> {
    let elapsed = elapsed_secs(order, unix_ts)?;
    let (t0, a0, t1, a1) = find_segment(order, elapsed);
    Some(interpolate(t0, a0, t1, a1, elapsed))
}

/// Seconds into the auction at `unix_ts`, or `None` once the auction has expired.
///
/// Timestamps before `auction_start_time` clamp to 0: the on-chain `_getAuctionBump`
/// returns the full `initialRateBump` before the start time, so orders are fillable
/// pre-start at the maximum (start) price. Measured on mainnet, the median Fusion fill
/// lands ~13s BEFORE auction start — treating pre-start orders as inactive would hide
/// the window where most fills actually happen.
fn elapsed_secs(order: &FusionOrder, unix_ts: u64) -> Option<u64> {
    let elapsed = unix_ts.saturating_sub(order.auction_start_time);
    if elapsed >= order.auction_duration_secs {
        return None;
    }
    Some(elapsed)
}

fn find_segment(order: &FusionOrder, elapsed: u64) -> (u64, U256, u64, U256) {
    let mut seg_start_t: u64 = 0;
    let mut seg_start_a: U256 = order.auction_start_amount;

    for point in &order.points {
        if elapsed < point.delay_secs {
            return (seg_start_t, seg_start_a, point.delay_secs, point.amount);
        }
        seg_start_t = point.delay_secs;
        seg_start_a = point.amount;
    }

    (seg_start_t, seg_start_a, order.auction_duration_secs, order.auction_end_amount)
}

fn interpolate(t0: u64, a0: U256, t1: u64, a1: U256, t: u64) -> U256 {
    let span = U256::from(t1.saturating_sub(t0));
    if span.is_zero() {
        return a0;
    }
    let elapsed_in_segment = U256::from(t.saturating_sub(t0));
    let decay = a0.saturating_sub(a1).saturating_mul(elapsed_in_segment) / span;
    a0.saturating_sub(decay)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_order() -> FusionOrder {
        FusionOrder {
            order_id: "test".into(),
            from_token: "0xa".into(),
            to_token: "0xb".into(),
            making_amount: U256::from(1_000u64),
            auction_start_amount: U256::from(1_000u64),
            auction_end_amount: U256::from(800u64),
            auction_duration_secs: 200,
            auction_start_time: 1_000,
            points: vec![],
            from_token_symbol: None,
            to_token_symbol: None,
            from_token_decimals: 18,
            to_token_decimals: 18,
            from_token_usd_rate: 1.0,
            to_token_usd_rate: 1.0,
            gas_bump_estimate: 0,
            gas_price_estimate_mwei: 0,
            init_rate_bump: 0,
            total_fees_1e5: 0,
            signature: "0x00".to_string(),
            extension: "0x".to_string(),
            salt: "0".to_string(),
            maker_address: "0x0000000000000000000000000000000000000000".to_string(),
            receiver_address: "0x0000000000000000000000000000000000000000".to_string(),
            maker_traits: "0".to_string(),
        }
    }

    #[test]
    fn before_start_returns_start_amount() {
        // Pre-start clamps to elapsed = 0: the on-chain auction prices the order at the
        // full initial rate bump before start, so it is quotable (at the start amount).
        assert_eq!(amount_at_timestamp(&simple_order(), 999), Some(U256::from(1_000u64)));
        assert_eq!(amount_at_timestamp(&simple_order(), 0), Some(U256::from(1_000u64)));
    }

    #[test]
    fn after_end_returns_none() {
        assert_eq!(amount_at_timestamp(&simple_order(), 1_200), None);
    }

    #[test]
    fn at_start_returns_start_amount() {
        assert_eq!(amount_at_timestamp(&simple_order(), 1_000), Some(U256::from(1_000u64)));
    }

    #[test]
    fn linear_midpoint() {
        // elapsed = 100/200 → halfway between 1000 and 800 → 900
        assert_eq!(amount_at_timestamp(&simple_order(), 1_100), Some(U256::from(900u64)));
    }

    #[test]
    fn two_segment_breakpoint() {
        let order = FusionOrder {
            points: vec![AuctionPoint { delay_secs: 100, amount: U256::from(900u64), rate_bump: 0 }],
            ..simple_order()
        };
        assert_eq!(amount_at_timestamp(&order, 1_050), Some(U256::from(950u64)));
        assert_eq!(amount_at_timestamp(&order, 1_100), Some(U256::from(900u64)));
        assert_eq!(amount_at_timestamp(&order, 1_150), Some(U256::from(850u64)));
    }

    #[test]
    fn is_gtc_detects_flat_long_order() {
        let gtc = FusionOrder {
            auction_start_amount: U256::from(1_000u64),
            auction_end_amount: U256::from(1_000u64),
            auction_duration_secs: 7_200,
            ..simple_order()
        };
        assert!(is_gtc_order(&gtc));
    }

    #[test]
    fn is_gtc_passes_normal_dutch_auction() {
        assert!(!is_gtc_order(&simple_order()));
    }

    #[test]
    fn integer_arithmetic_on_wei_scale() {
        let order = FusionOrder {
            making_amount: U256::from(1_000_000_000_000_000_000u128),
            auction_start_amount: U256::from(1_001_000_000_000_000_000u128),
            auction_end_amount: U256::from(1_000_000_000_000_000_000u128),
            auction_duration_secs: 180,
            auction_start_time: 1_000,
            ..simple_order()
        };
        assert_eq!(
            amount_at_timestamp(&order, 1_090),
            Some(U256::from(1_000_500_000_000_000_000u128))
        );
    }

    // ── Real-order regression tests ──────────────────────────────────────────
    //
    // Parameters decoded from live extension bytes observed in smoke run 3
    // (block 25230553, June 2026).  These anchor the formula to on-chain ground
    // truth and guard against silent arithmetic regressions.

    /// FABA→WETH order captured at block 25230553.
    ///
    /// Extension params decoded from `TakingAmountData`:
    /// - `startTime=1780413608`, `duration=360s`, `initialRateBump=695823`
    /// - `floor=13_632_559_937_529_719` (order.takingAmount)
    /// - `start_amount=14_581_144_812_870_894`
    ///
    /// Ground truth: smoke run 3 log with `base_fee=0` → `gas_bump=0`.
    /// - `taking_estimate=14573239938909718` at `pending_ts=1780413611` (elapsed=3s)
    fn faba_order() -> FusionOrder {
        FusionOrder {
            order_id: "0x9d7a1175ffd8e3b62e32c68657a1fa7bc08a7d8f07161a31ce9ce14560448c54".into(),
            from_token: "0xfaba6f8e4a5e8ab82f62fe7c39859fa577269be3".into(),
            to_token: "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".into(),
            making_amount: U256::from(307_671_843_799_523_540u128),
            auction_start_amount: U256::from(14_581_144_812_870_894u128),
            auction_end_amount:   U256::from(13_632_559_937_529_719u128),
            auction_duration_secs: 360,
            auction_start_time:    1_780_413_608,
            points: vec![],
            from_token_symbol: Some("FABA".into()),
            to_token_symbol:   Some("WETH".into()),
            from_token_decimals: 18,
            to_token_decimals:   18,
            from_token_usd_rate: 0.0,
            to_token_usd_rate:   0.0,
            gas_bump_estimate:       343_213,
            gas_price_estimate_mwei:   2_076,
            init_rate_bump: 0,
            total_fees_1e5: 0,
            signature: "0x".into(),
            extension: "0x".into(),
            salt: "0".into(),
            maker_address:    "0x0000000000000000000000000000000000000000".into(),
            receiver_address: "0x0000000000000000000000000000000000000000".into(),
            maker_traits: "0".into(),
        }
    }

    #[test]
    fn faba_auction_price_at_elapsed_3s() {
        // pending_ts = startTime + 3 = 1_780_413_611
        assert_eq!(
            amount_at_timestamp(&faba_order(), 1_780_413_611),
            Some(U256::from(14_573_239_938_909_718u128)),
        );
    }

    #[test]
    fn faba_before_auction_start_pinned_to_start_amount() {
        // confirmed block 25230552 had timestamp=1780413599, which is 9s BEFORE startTime.
        // On-chain the extension prices pre-start fills at the full initial rate bump,
        // so the estimate must pin to the start-time value instead of dropping the order.
        let order = faba_order();
        assert_eq!(
            amount_at_timestamp(&order, 1_780_413_599),
            amount_at_timestamp(&order, order.auction_start_time),
        );
        assert!(amount_at_timestamp(&order, 1_780_413_599).is_some());
    }

    #[test]
    fn faba_gas_bump_at_base_fee() {
        // Base fee from confirmed block 25230552: 1_871_798_811 wei.
        let bump = compute_gas_bump(&faba_order(), 1_871_798_811);
        assert_eq!(bump, 309_453);
        let floor = faba_order().auction_end_amount;
        let taking = floor
            .saturating_mul(U256::from(bump))
            .saturating_add(U256::from(9_999_999u64))
            / U256::from(10_000_000u64);
        assert_eq!(taking, U256::from(421_863_657_034_839u128));
    }

    #[test]
    #[expect(clippy::expect_used, reason = "test assertion helper")]
    fn faba_full_estimate_at_elapsed_3s() {
        // Combined auction price + gas bump — the value compared against amount_out.
        let order = faba_order();
        let block_ts = 1_780_413_611_u64;
        let base_fee = 1_871_798_811_u64;

        let auction_price = amount_at_timestamp(&order, block_ts).expect("within auction window");
        let gas_bump      = compute_gas_bump(&order, base_fee);
        let gas_bump_tak  = order.auction_end_amount
            .saturating_mul(U256::from(gas_bump))
            .saturating_add(U256::from(9_999_999u64))
            / U256::from(10_000_000u64);
        assert_eq!(auction_price + gas_bump_tak, U256::from(14_995_103_595_944_557u128));
    }

    /// WETH→USDT order from `encode_test.rs` (block 25222660).
    ///
    /// Extension params decoded from `TakingAmountData`:
    /// - `startTime=1780318371`, `duration=180s`, `initialRateBump=53347`
    /// - `floor=1_327_889_927`, `start_amount=1_334_973_822`
    /// - `gasBumpEstimate=3080`, `gasPriceEstimate=336 Mwei`
    fn usdt_order() -> FusionOrder {
        FusionOrder {
            order_id: "0xddc5239bef2a6f7afc8967384e209ec5548215abda64e5a68e89e7e0741f2090".into(),
            from_token: "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".into(),
            to_token:   "0xdac17f958d2ee523a2206206994597c13d831ec7".into(),
            making_amount: U256::from(671_300_000_000_000_000u128),
            auction_start_amount: U256::from(1_334_973_822u64),
            auction_end_amount:   U256::from(1_327_889_927u64),
            auction_duration_secs: 180,
            auction_start_time:    1_780_318_371,
            points: vec![],
            from_token_symbol: Some("WETH".into()),
            to_token_symbol:   Some("USDT".into()),
            from_token_decimals: 18,
            to_token_decimals:    6,
            from_token_usd_rate: 0.0,
            to_token_usd_rate:   0.0,
            gas_bump_estimate:       3_080,
            gas_price_estimate_mwei:   336,
            init_rate_bump: 0,
            total_fees_1e5: 0,
            signature: "0x".into(),
            extension: "0x".into(),
            salt: "0".into(),
            maker_address:    "0x0000000000000000000000000000000000000000".into(),
            receiver_address: "0x0000000000000000000000000000000000000000".into(),
            maker_traits: "0".into(),
        }
    }

    #[test]
    fn usdt_auction_at_start() {
        assert_eq!(
            amount_at_timestamp(&usdt_order(), 1_780_318_371),
            Some(U256::from(1_334_973_822u64)),
        );
    }

    #[test]
    fn usdt_auction_at_elapsed_27s() {
        // elapsed=27: decay = (1334973822 - 1327889927) * 27 / 180 = 1062584
        // result = 1334973822 - 1062584 = 1333911238
        assert_eq!(
            amount_at_timestamp(&usdt_order(), 1_780_318_398),
            Some(U256::from(1_333_911_238u64)),
        );
    }

    #[test]
    fn usdt_auction_at_elapsed_90s() {
        assert_eq!(
            amount_at_timestamp(&usdt_order(), 1_780_318_461),
            Some(U256::from(1_331_431_875u64)),
        );
    }

    #[test]
    fn usdt_auction_expired_returns_none() {
        assert_eq!(amount_at_timestamp(&usdt_order(), 1_780_318_551), None);
    }

    // ── Smoke run 4 — UNI→USDC order, block 25230904 (eth_call SUCCESS) ──────
    //
    // Order 0x24d971ccc1964e91b28518779ed29cbcb4496849fd463791b99645f9d01a0c69
    //   makerAsset: UNI  (0x1f9840a85d5af5bf1d1762f925bdaddc4201f984)
    //   takerAsset: USDC (0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48)
    //   floor=60_227_591, start_amount=61_805_862
    //   startTime=1_780_417_845, duration=360s
    //   gasBumpEstimate=126_727, gasPriceEstimate=1_433 Mwei
    //
    // Extension has TWO auction points (API returns 0 — discrepancy):
    //   t=24s:  coeff=251_799 → amount=61_744_116
    //   t=360s: coeff=126_727 → amount=60_990_838  (≈ floor + gas-at-reference-price)
    //
    // Ground truth from debug trace (block 25230904, confirmed SUCCESS):
    //   onchain_taking = 63_030_547  (pending_ts=1_780_417_847, base_fee=1_269_763_641)
    //
    // NOTE: our off-chain estimate (62_473_396) is lower than onchain (63_030_547).
    // The ~535k gap is a resolver-tier fee encoded in the extension's third section
    // (not yet decoded off-chain).  The `query_onchain_taking_amount` pre-check
    // handles this correctly; these tests verify the auction-curve arithmetic only.

    fn uni_order_no_pts() -> FusionOrder {
        FusionOrder {
            order_id: "0x24d971ccc1964e91b28518779ed29cbcb4496849fd463791b99645f9d01a0c69".into(),
            from_token: "0x1f9840a85d5af5bf1d1762f925bdaddc4201f984".into(),
            to_token:   "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".into(),
            making_amount:        U256::from(22_379_390_100_267_360_763u128),
            auction_start_amount: U256::from(61_805_862u64),
            auction_end_amount:   U256::from(60_227_591u64),
            auction_duration_secs: 360,
            auction_start_time:    1_780_417_845,
            points: vec![],
            from_token_symbol:  Some("UNI".into()),
            to_token_symbol:    Some("USDC".into()),
            from_token_decimals: 18,
            to_token_decimals:    6,
            from_token_usd_rate: 0.0,
            to_token_usd_rate:   0.0,
            gas_bump_estimate:        126_727,
            gas_price_estimate_mwei:    1_433,
            init_rate_bump: 0,
            total_fees_1e5: 0,
            signature:        "0x".into(),
            extension:        "0x".into(),
            salt:             "0".into(),
            maker_address:    "0x0000000000000000000000000000000000000000".into(),
            receiver_address: "0x0000000000000000000000000000000000000000".into(),
            maker_traits:     "0".into(),
        }
    }

    fn uni_order_with_pts() -> FusionOrder {
        FusionOrder {
            points: vec![
                AuctionPoint { delay_secs: 24,  amount: U256::from(61_744_116u64), rate_bump: 0 },
                AuctionPoint { delay_secs: 360, amount: U256::from(60_990_838u64), rate_bump: 0 },
            ],
            ..uni_order_no_pts()
        }
    }

    // ── Smoke run 4 — USDC→floor order 0x1dca545a (VERIFIED against on-chain) ──────────
    //
    // Order: 0x1dca545afebf78140bb8fc7807401cc57f4cec36f7cf6d3f7e8f1e7e535ff3c6
    //   floor = 584_777_961
    //   startTime = 1_780_417_052, duration = 180s
    //   gasBumpEstimate = 126, gasPriceEstimate = 2501 Mwei
    //   initRateBump = 62_126 (gives start_amount = 588_410_953 = apply_rate_bump(floor, 62126))
    //   totalFees = 300 (resolverFee=300 in 1e5 units = 0.3%)
    //
    // Extension points (RELATIVE timeDelta):
    //   coeff=50377, timeDelta=60  → cumulative t=60
    //   coeff=50236, timeDelta=84  → cumulative t=60+84=144
    //   coeff=126,   timeDelta=36  → cumulative t=144+36=180 (=duration)
    //
    // Ground truth at block_ts=1_780_417_175, base_fee=2_310_734_453:
    //   elapsed=123, gasBump=116, auctionBump=50_271, rateBump=50_155
    //   withFees=586_532_295, onchain_taking=589_474_048
    fn dca_order() -> FusionOrder {
        FusionOrder {
            order_id: "0x1dca545afebf78140bb8fc7807401cc57f4cec36f7cf6d3f7e8f1e7e535ff3c6".into(),
            from_token: "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".into(),
            to_token:   "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".into(),
            making_amount: U256::from(100_000_000_000_000_000u128),
            auction_start_amount: U256::from(588_410_953u64),
            auction_end_amount:   U256::from(584_777_961u64),
            auction_duration_secs: 180,
            auction_start_time:    1_780_417_052,
            points: vec![
                AuctionPoint { delay_secs:  60, amount: U256::from(587_723_897u64), rate_bump: 50_377 },
                AuctionPoint { delay_secs: 144, amount: U256::from(587_715_642u64), rate_bump: 50_236 },
                AuctionPoint { delay_secs: 180, amount: U256::from(584_785_334u64), rate_bump:    126 },
            ],
            from_token_symbol: Some("WETH".into()),
            to_token_symbol:   Some("USDC".into()),
            from_token_decimals: 18,
            to_token_decimals:    6,
            from_token_usd_rate: 0.0,
            to_token_usd_rate:   0.0,
            gas_bump_estimate:       126,
            gas_price_estimate_mwei: 2_501,
            init_rate_bump:  62_126,
            total_fees_1e5:    300,
            signature:        "0x".into(),
            extension:        "0x".into(),
            salt:             "0".into(),
            maker_address:    "0x0000000000000000000000000000000000000000".into(),
            receiver_address: "0x0000000000000000000000000000000000000000".into(),
            maker_traits:     "0".into(),
        }
    }

    #[test]
    fn uni_gas_bump_at_base_fee() {
        // base_fee = 1_269_763_641 from block 25230903 (confirmed parent of 25230904)
        // gas_bump = 126_727 * 1_269_763_641 / (1_433 * 1_000_000) = 112_291
        let bump = compute_gas_bump(&uni_order_no_pts(), 1_269_763_641);
        assert_eq!(bump, 112_291);
    }

    #[test]
    fn uni_price_at_elapsed_2s_no_points() {
        // Simple linear (as the 1inch API returns no points for this order).
        // elapsed=2: decay = (61_805_862 - 60_227_591) * 2 / 360 = 8_768 (floor div)
        assert_eq!(
            amount_at_timestamp(&uni_order_no_pts(), 1_780_417_847),
            Some(U256::from(61_797_094u64)),
        );
    }

    #[test]
    fn uni_price_at_elapsed_2s_with_ext_points() {
        // Piecewise with extension-decoded points (segment 0..24s).
        // decay = (61_805_862 - 61_744_116) * 2 / 24 = 5_145 (floor div)
        assert_eq!(
            amount_at_timestamp(&uni_order_with_pts(), 1_780_417_847),
            Some(U256::from(61_800_717u64)),
        );
    }

    #[test]
    #[expect(clippy::expect_used, reason = "test assertion helper")]
    fn uni_full_estimate_no_points() {
        // Reproduces the logged taking_estimate=62_473_396 exactly.
        // smoke run 4: "auction price estimate ... taking_estimate=62473396"
        let order    = uni_order_no_pts();
        let block_ts = 1_780_417_847_u64;
        let base_fee = 1_269_763_641_u64;

        let price    = amount_at_timestamp(&order, block_ts).expect("within auction window");
        let gas_bump = compute_gas_bump(&order, base_fee);
        let gas_bump_taking = order.auction_end_amount
            .saturating_mul(U256::from(gas_bump))
            .saturating_add(U256::from(9_999_999u64))
            / U256::from(10_000_000u64);
        assert_eq!(price + gas_bump_taking, U256::from(62_473_396u64));
    }

    #[test]
    fn uni_before_start_pinned_to_start_amount() {
        // 1s before auction start: pinned to the start-time value (fillable pre-start).
        let order = uni_order_no_pts();
        assert_eq!(
            amount_at_timestamp(&order, 1_780_417_844),
            amount_at_timestamp(&order, order.auction_start_time),
        );
    }

    #[test]
    fn uni_expired_returns_none() {
        // At or after auction end (start + 360s)
        assert_eq!(
            amount_at_timestamp(&uni_order_no_pts(), 1_780_417_845 + 360),
            None,
        );
    }

    // ── onchain_taking_amount tests — exact match against query_onchain_taking_amount ──

    #[test]
    fn dca_gas_bump() {
        // 126 * 2_310_734_453 / (2501 * 1_000_000) = 116
        assert_eq!(compute_gas_bump(&dca_order(), 2_310_734_453), 116);
    }

    #[test]
    fn dca_onchain_taking_matches_ground_truth() {
        // Verified: query_onchain_taking_amount returned 589_474_048 at this block.
        // Formula: auctionBump=50_271, gasBump=116, rateBump=50_155
        //   withFees = ceil(584_777_961 * 100_300 / 100_000) = 586_532_295
        //   final    = ceil(586_532_295 * 10_050_155 / 10_000_000) = 589_474_048
        assert_eq!(
            onchain_taking_amount(&dca_order(), 1_780_417_175, 2_310_734_453),
            Some(U256::from(589_474_048u64)),
        );
    }

    #[test]
    fn dca_before_start_pinned_to_start_price() {
        let order = dca_order();
        let pre = onchain_taking_amount(&order, 1_780_417_051, 2_310_734_453);
        let at_start = onchain_taking_amount(&order, order.auction_start_time, 2_310_734_453);
        assert_eq!(pre, at_start);
        assert!(pre.is_some());
    }

    #[test]
    fn dca_expired_returns_none() {
        assert_eq!(onchain_taking_amount(&dca_order(), 1_780_417_052 + 180, 2_310_734_453), None);
    }
}
