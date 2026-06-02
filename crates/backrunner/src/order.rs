use serde::{Deserialize, Serialize};

/// A single breakpoint on the Fusion auction curve.
///
/// `delay_secs` is CUMULATIVE from `auction_start_time` (not relative to the previous point).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuctionPoint {
    /// Seconds elapsed from `auction_start_time` at which this breakpoint applies.
    pub delay_secs: u64,
    /// Required `to_token` output amount at this breakpoint.
    pub amount: u128,
}

/// A live 1inch Fusion limit order with its Dutch auction parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FusionOrder {
    pub order_id: String,
    /// Maker asset address (0x-prefixed hex, lowercase).
    pub from_token: String,
    /// Taker asset address (0x-prefixed hex, lowercase).
    pub to_token: String,
    /// Sell amount in the smallest token unit.
    pub making_amount: u128,
    /// Required output at the very start of the auction (most favourable for the user).
    pub auction_start_amount: u128,
    /// Minimum acceptable output (floor, reached at `auction_duration_secs`).
    pub auction_end_amount: u128,
    /// Total auction length in seconds.
    pub auction_duration_secs: u64,
    /// Unix timestamp at which the auction opened.
    pub auction_start_time: u64,
    /// Piecewise-linear decay curve breakpoints (may be empty).
    pub points: Vec<AuctionPoint>,
    pub from_token_symbol: Option<String>,
    pub to_token_symbol: Option<String>,
    pub from_token_decimals: u8,
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

/// Computes the gas-bump rate for an order given the pending block's base fee.
///
/// On-chain formula: `gasBump = gasBumpEstimate × baseFee_wei / (gasPriceEstimate × 10^6)`
/// This additional bump is added to the auction bump when computing `taking_amount`.
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
/// Returns `Some` when `unix_ts` is within the half-open window
/// `[auction_start_time, auction_start_time + auction_duration_secs)`.
/// Returns `None` when the order has expired or not yet started.
pub fn amount_at_timestamp(order: &FusionOrder, unix_ts: u64) -> Option<u128> {
    let elapsed = elapsed_secs(order, unix_ts)?;
    let (t0, a0, t1, a1) = find_segment(order, elapsed);
    Some(interpolate(t0, a0, t1, a1, elapsed))
}

fn elapsed_secs(order: &FusionOrder, unix_ts: u64) -> Option<u64> {
    if unix_ts < order.auction_start_time {
        return None;
    }
    let elapsed = unix_ts - order.auction_start_time;
    if elapsed >= order.auction_duration_secs {
        return None;
    }
    Some(elapsed)
}

fn find_segment(order: &FusionOrder, elapsed: u64) -> (u64, u128, u64, u128) {
    let mut seg_start_t: u64 = 0;
    let mut seg_start_a: u128 = order.auction_start_amount;

    for point in &order.points {
        if elapsed < point.delay_secs {
            return (seg_start_t, seg_start_a, point.delay_secs, point.amount);
        }
        seg_start_t = point.delay_secs;
        seg_start_a = point.amount;
    }

    (seg_start_t, seg_start_a, order.auction_duration_secs, order.auction_end_amount)
}

fn interpolate(t0: u64, a0: u128, t1: u64, a1: u128, t: u64) -> u128 {
    if t1 == t0 {
        return a0;
    }
    let span = u128::from(t1 - t0);
    let elapsed_in_segment = u128::from(t - t0);
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
            making_amount: 1_000,
            auction_start_amount: 1_000,
            auction_end_amount: 800,
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
            signature: "0x00".to_string(),
            extension: "0x".to_string(),
            salt: "0".to_string(),
            maker_address: "0x0000000000000000000000000000000000000000".to_string(),
            receiver_address: "0x0000000000000000000000000000000000000000".to_string(),
            maker_traits: "0".to_string(),
        }
    }

    #[test]
    fn before_start_returns_none() {
        assert_eq!(amount_at_timestamp(&simple_order(), 999), None);
    }

    #[test]
    fn after_end_returns_none() {
        assert_eq!(amount_at_timestamp(&simple_order(), 1_200), None);
    }

    #[test]
    fn at_start_returns_start_amount() {
        assert_eq!(amount_at_timestamp(&simple_order(), 1_000), Some(1_000));
    }

    #[test]
    fn linear_midpoint() {
        // elapsed = 100/200 → halfway between 1000 and 800 → 900
        assert_eq!(amount_at_timestamp(&simple_order(), 1_100), Some(900));
    }

    #[test]
    fn two_segment_breakpoint() {
        let order = FusionOrder {
            points: vec![AuctionPoint { delay_secs: 100, amount: 900 }],
            ..simple_order()
        };
        assert_eq!(amount_at_timestamp(&order, 1_050), Some(950));
        assert_eq!(amount_at_timestamp(&order, 1_100), Some(900));
        assert_eq!(amount_at_timestamp(&order, 1_150), Some(850));
    }

    #[test]
    fn is_gtc_detects_flat_long_order() {
        let gtc = FusionOrder {
            auction_start_amount: 1_000,
            auction_end_amount: 1_000,
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
            making_amount: 1_000_000_000_000_000_000,
            auction_start_amount: 1_001_000_000_000_000_000,
            auction_end_amount: 1_000_000_000_000_000_000,
            auction_duration_secs: 180,
            auction_start_time: 1_000,
            ..simple_order()
        };
        assert_eq!(
            amount_at_timestamp(&order, 1_090),
            Some(1_000_500_000_000_000_000u128)
        );
    }

    // ── Real-order regression tests ──────────────────────────────────────────
    //
    // Parameters decoded from live extension bytes observed in smoke run 3
    // (block 25230553, June 2026).  These anchor the formula to on-chain ground
    // truth and guard against silent arithmetic regressions.

    /// FABA→WETH order captured at block 25230553.
    ///
    /// Extension params decoded from TakingAmountData:
    ///   startTime=1780413608, duration=360s, initialRateBump=695823
    ///   floor=13_632_559_937_529_719 (order.takingAmount)
    ///   start_amount=apply_rate_bump(floor, 695823)=14_581_144_812_870_894
    ///
    /// Ground truth: smoke run 3 log with base_fee=0 → gas_bump=0.
    ///   taking_estimate=14573239938909718 at pending_ts=1780413611 (elapsed=3s)
    fn faba_order() -> FusionOrder {
        FusionOrder {
            order_id: "0x9d7a1175ffd8e3b62e32c68657a1fa7bc08a7d8f07161a31ce9ce14560448c54".into(),
            from_token: "0xfaba6f8e4a5e8ab82f62fe7c39859fa577269be3".into(),
            to_token: "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".into(),
            making_amount: 307_671_843_799_523_540,
            auction_start_amount: 14_581_144_812_870_894,
            auction_end_amount:   13_632_559_937_529_719,
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
            Some(14_573_239_938_909_718),
        );
    }

    #[test]
    fn faba_before_auction_start_returns_none() {
        // confirmed block 25230552 had timestamp=1780413599, which is 9s BEFORE startTime.
        // This replicates what the on-chain eth_call at "latest" would see — the auction
        // hasn't started, so the extension should be called with timestamp override instead.
        assert_eq!(amount_at_timestamp(&faba_order(), 1_780_413_599), None);
    }

    #[test]
    fn faba_gas_bump_at_base_fee() {
        // Base fee from confirmed block 25230552: 1_871_798_811 wei.
        let bump = compute_gas_bump(&faba_order(), 1_871_798_811);
        assert_eq!(bump, 309_453);
        let taking = faba_order().auction_end_amount
            .saturating_mul(bump)
            .saturating_add(9_999_999)
            / 10_000_000;
        assert_eq!(taking, 421_863_657_034_839);
    }

    #[test]
    fn faba_full_estimate_at_elapsed_3s() {
        // Combined auction price + gas bump — the value compared against amount_out.
        let order = faba_order();
        let block_ts = 1_780_413_611_u64;
        let base_fee = 1_871_798_811_u64;

        let auction_price = amount_at_timestamp(&order, block_ts).unwrap();
        let gas_bump      = compute_gas_bump(&order, base_fee);
        let gas_bump_tak  = order.auction_end_amount
            .saturating_mul(gas_bump)
            .saturating_add(9_999_999)
            / 10_000_000;
        assert_eq!(auction_price + gas_bump_tak, 14_995_103_595_944_557);
    }

    /// WETH→USDT order from encode_test.rs (block 25222660).
    ///
    /// Extension params decoded from TakingAmountData:
    ///   startTime=1780318371, duration=180s, initialRateBump=53347
    ///   floor=1_327_889_927, start_amount=1_334_973_822
    ///   gasBumpEstimate=3080, gasPriceEstimate=336 Mwei
    fn usdt_order() -> FusionOrder {
        FusionOrder {
            order_id: "0xddc5239bef2a6f7afc8967384e209ec5548215abda64e5a68e89e7e0741f2090".into(),
            from_token: "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2".into(),
            to_token:   "0xdac17f958d2ee523a2206206994597c13d831ec7".into(),
            making_amount: 671_300_000_000_000_000,
            auction_start_amount: 1_334_973_822,
            auction_end_amount:   1_327_889_927,
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
            Some(1_334_973_822),
        );
    }

    #[test]
    fn usdt_auction_at_elapsed_27s() {
        // elapsed=27: decay = (1334973822 - 1327889927) * 27 / 180 = 1062584
        // result = 1334973822 - 1062584 = 1333911238
        assert_eq!(
            amount_at_timestamp(&usdt_order(), 1_780_318_398),
            Some(1_333_911_238),
        );
    }

    #[test]
    fn usdt_auction_at_elapsed_90s() {
        assert_eq!(
            amount_at_timestamp(&usdt_order(), 1_780_318_461),
            Some(1_331_431_875),
        );
    }

    #[test]
    fn usdt_auction_expired_returns_none() {
        assert_eq!(amount_at_timestamp(&usdt_order(), 1_780_318_551), None);
    }
}
