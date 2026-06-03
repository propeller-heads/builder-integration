use std::str::FromStr;

use num_bigint::BigInt;

use crate::events::{PoolEvent, PoolEventKind};

pub struct TickDelta {
    pub pool_address: Vec<u8>,
    pub tick_index: i32,
    pub liquidity_net_delta: BigInt,
}

#[must_use]
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
