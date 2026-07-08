use std::collections::HashMap;

use alloy::primitives::{Address, Bytes, Log, B256, I256, U256};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Events emitted by the block builder for each building iteration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BuildEvent {
    /// A new block-building iteration has started.
    IterationStart { uuid: Uuid, block: BlockEnv },
    /// A transaction was executed within this iteration.
    TxExecuted { uuid: Uuid, tx: ExecutedTx },
    /// The iteration completed successfully with final post-state.
    IterationComplete { uuid: Uuid, state: PostState },
    /// The iteration was aborted before completing.
    IterationAborted { uuid: Uuid },
}

/// Block-level environment for a building iteration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockEnv {
    pub block_number: u64,
    pub block_timestamp: u64,
    pub base_fee_per_gas: u64,
}

/// A transaction executed within a builder iteration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutedTx {
    pub tx_hash: B256,
    pub from: Address,
    pub to: Option<Address>,
    pub logs: Vec<Log>,
    pub gas_used: u64,
    pub status: bool,
}

/// Post-execution account state snapshot for a builder iteration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostState {
    pub accounts: HashMap<Address, AccountState>,
}

/// Snapshot of a single account after execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountState {
    pub balance: U256,
    pub nonce: u64,
    pub code_hash: B256,
    pub storage: HashMap<U256, U256>,
}

/// A backrun candidate to be submitted to the builder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackrunCandidate {
    pub uuid: Uuid,
    pub block_number: u64,
    pub txs: Vec<BackrunTx>,
}

/// A single transaction within a [`BackrunCandidate`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackrunTx {
    pub tx: RawTx,
    /// Expected surplus captured by the resolver, denominated in WETH (ETH wei).
    ///
    /// When the order's taker asset is WETH this is the raw surplus; otherwise it is
    /// the WETH output of the surplus→WETH swap, so the value is comparable across
    /// orders regardless of taker asset.
    pub expected_profit_wei: I256,
    pub expected_gas: u64,
}

/// An unsigned EIP-1559 transaction payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawTx {
    pub to: Option<Address>,
    pub value: U256,
    pub data: Bytes,
    pub gas_limit: u64,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
}
