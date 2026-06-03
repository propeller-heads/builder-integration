use std::collections::{HashMap, HashSet};

use num_bigint::{BigInt, Sign};
use num_traits::ToPrimitive as _;
use tycho_common::{
    models::{
        blockchain::{Block, BlockAggregatedChanges, TxInput},
        protocol::{ComponentBalance, ProtocolComponentStateDelta},
        Chain,
    },
    traits::TxDeltaIndexer,
    Bytes,
};
use tycho_substreams::prelude::{
    Attribute, BalanceChange, ChangeType, EntityChanges, Transaction, TransactionChanges,
    TransactionChangesBuilder,
};

use crate::{
    balance::event_to_balance_deltas,
    events::{decode_log, event_to_current_tick, Pool, PoolEvent, TxRef},
    liquidity::{event_to_liquidity_delta, LiquidityChangeKind},
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
    /// Keyed by (component_hex_id, token_hex) -> running balance
    balances: HashMap<(String, String), BigInt>,
    tick_liquidity: HashMap<(String, i32), BigInt>,
    current_tick: HashMap<String, i64>,
    pool_liquidity: HashMap<String, BigInt>,
    baseline_tick_keys: HashSet<(String, i32)>,
}

impl TxDeltaIndexer for UniswapV3Processor {
    fn apply_block(&mut self, block: &BlockAggregatedChanges) -> anyhow::Result<()> {
        self.chain = block.chain;
        self.last_block = Some(block.block.clone());
        self.finalized_block_height = block.finalized_block_height;

        for (id, comp) in &block.new_protocol_components {
            if comp.tokens.len() >= 2 {
                self.pools.insert(
                    id.clone(),
                    Pool {
                        address: id
                            .trim_start_matches("0x")
                            .as_bytes()
                            .chunks(2)
                            .filter_map(|c| {
                                std::str::from_utf8(c)
                                    .ok()
                                    .and_then(|s| u8::from_str_radix(s, 16).ok())
                            })
                            .collect(),
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

        Ok(())
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
                        delta
                            .deleted_attributes
                            .insert(attr.name.clone());
                        delta
                            .updated_attributes
                            .remove(&attr.name);
                    } else {
                        delta
                            .updated_attributes
                            .insert(attr.name.clone(), Bytes::from(attr.value));
                        delta
                            .deleted_attributes
                            .remove(&attr.name);
                    }
                }
            }

            for bc in changes.balance_changes {
                let comp_id = String::from_utf8_lossy(&bc.component_id).into_owned();
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
                self.current_tick
                    .insert(component_id.to_string(), tick_i64);
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
        self.tick_liquidity
            .retain(|(pool_id, _), _| pool_id != id);
        self.baseline_tick_keys
            .retain(|(pool_id, _)| pool_id != id);
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

            tx_builders
                .entry(tx.hash().to_vec())
                .or_insert_with(|| {
                    let substreams_tx = Transaction {
                        hash: tx.hash().to_vec(),
                        from: tx.from().to_vec(),
                        to: tx.to().to_vec(),
                        index: tx.index(),
                    };
                    (tx.index(), TransactionChangesBuilder::new(&substreams_tx))
                });

            let hash_key = tx.hash().to_vec();
            let Some((_, builder)) = tx_builders.get_mut(&hash_key) else { continue };
            for event in events {
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
            self.current_tick
                .insert(pool_hex.clone(), new_tick);
        }

        for delta in event_to_balance_deltas(&event) {
            let token_hex = hex::encode(&delta.token);
            let running = self
                .balances
                .entry((pool_hex.clone(), token_hex))
                .or_default();
            *running += &delta.delta;
            let clamped = if *running < BigInt::default() {
                BigInt::default()
            } else {
                running.clone()
            };
            // component_id in BalanceChange must be UTF-8 encoded component id string
            let comp_id_bytes = pool_hex.as_bytes().to_vec();
            builder.add_balance_change(&BalanceChange {
                component_id: comp_id_bytes,
                token: delta.token.clone(),
                balance: clamped.to_bytes_be().1,
            });
        }

        for tick_delta in event_to_tick_deltas(&event) {
            let key = (pool_hex.clone(), tick_delta.tick_index);
            let existed_before = self.tick_liquidity.contains_key(&key)
                || self.baseline_tick_keys.contains(&key);
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
    log: &tycho_common::models::blockchain::LogInput,
    ordinal: u64,
) -> substreams_ethereum::pb::eth::v2::Log {
    substreams_ethereum::pb::eth::v2::Log {
        address: log.address().to_vec(),
        topics: log
            .topics()
            .iter()
            .map(|t| t.to_vec())
            .collect(),
        data: log.data().to_vec(),
        ordinal,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tycho_common::{
        models::{
            blockchain::{BlockAggregatedChanges, LogInput, TxInput},
            protocol::ProtocolComponent,
            Chain, ChangeType,
        },
        traits::TxDeltaIndexer,
        Bytes,
    };

    use super::UniswapV3Processor;

    fn make_processor() -> UniswapV3Processor {
        UniswapV3Processor::new(Chain::Ethereum, "uniswap_v3".to_string())
    }

    /// Build a minimal `BlockAggregatedChanges` that registers one pool.
    fn block_with_pool(pool_hex: &str) -> BlockAggregatedChanges {
        let token0 = Bytes::from(vec![0xaa; 20]);
        let token1 = Bytes::from(vec![0xbb; 20]);

        let mut comp = ProtocolComponent::default();
        comp.id = pool_hex.to_string();
        comp.chain = Chain::Ethereum;
        comp.change = ChangeType::Creation;
        comp.tokens = vec![token0, token1];

        let mut new_components = HashMap::new();
        new_components.insert(pool_hex.to_string(), comp);

        BlockAggregatedChanges {
            extractor: "uniswap_v3".to_string(),
            chain: Chain::Ethereum,
            new_protocol_components: new_components,
            ..Default::default()
        }
    }

    #[test]
    fn no_pools_returns_empty_deltas() {
        let mut proc = make_processor();
        let result = proc.generate_deltas(&[]);
        assert!(result.state_deltas.is_empty());
        assert!(result.component_balances.is_empty());
    }

    #[test]
    fn failed_tx_is_skipped() {
        let pool_hex = "aa".repeat(20);
        let pool_addr = Bytes::from(hex::decode(&pool_hex).unwrap());

        let mut proc = make_processor();
        proc.apply_block(&block_with_pool(&pool_hex)).unwrap();

        let log = LogInput::new(pool_addr, vec![], Bytes::default(), 0);
        let tx = TxInput::new(
            Bytes::from(vec![0x01; 32]),
            Bytes::from(vec![0x02; 20]),
            Bytes::from(vec![0x03; 20]),
            0,
            vec![log],
            false, // failed
        );

        let result = proc.generate_deltas(&[tx]);
        assert!(result.state_deltas.is_empty());
    }

    #[test]
    fn apply_block_does_not_mutate_on_generate_deltas() {
        let pool_hex = "cc".repeat(20);

        let mut proc = make_processor();
        proc.apply_block(&block_with_pool(&pool_hex)).unwrap();

        let first = proc.generate_deltas(&[]);
        let second = proc.generate_deltas(&[]);

        assert_eq!(first.state_deltas.len(), second.state_deltas.len());
    }
}
