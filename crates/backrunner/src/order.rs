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
}
