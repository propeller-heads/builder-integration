use std::str::FromStr;

use num_bigint::BigInt;

use crate::events::{PoolEvent, PoolEventKind};

pub struct AttributeUpdate {
    pub pool_address: Vec<u8>,
    pub name: String,
    pub value: Vec<u8>,
    pub is_creation: bool,
}

#[must_use]
pub fn event_to_attribute_updates(event: &PoolEvent) -> Vec<AttributeUpdate> {
    match &event.kind {
        PoolEventKind::Initialize { sqrt_price, tick }
        | PoolEventKind::Swap { sqrt_price, tick, .. } => {
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
                    value: BigInt::from_str(fee0_new)
                        .unwrap_or_default()
                        .to_signed_bytes_be(),
                    is_creation: false,
                },
                AttributeUpdate {
                    pool_address: event.pool_address.clone(),
                    name: "protocol_fees/token1".to_string(),
                    value: BigInt::from_str(fee1_new)
                        .unwrap_or_default()
                        .to_signed_bytes_be(),
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
        assert_eq!(updates[1].value, BigInt::from(42).to_signed_bytes_be());
    }

    #[test]
    fn mint_collect_produce_no_attribute_updates() {
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
