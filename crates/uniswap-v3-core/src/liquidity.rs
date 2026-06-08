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

#[must_use]
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
    use crate::events::TxRef;

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
        let result = event_to_liquidity_delta(0, &event);
        assert!(result.is_some());
        let d = result.expect("mint in range should return Some");
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
        let result = event_to_liquidity_delta(0, &event).expect("swap should return Some");
        assert_eq!(result.value, BigInt::from(9999));
        assert!(matches!(result.kind, LiquidityChangeKind::Absolute));
    }
}
