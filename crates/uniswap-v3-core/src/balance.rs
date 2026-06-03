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
