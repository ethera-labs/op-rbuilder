//! Types for sidecar communication.

use alloy_primitives::{Bytes, B256};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors that can occur during sidecar communication and transaction execution.
#[derive(Debug, Error)]
pub enum SidecarError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Sidecar returned hold but max retries ({max_retries}) exceeded")]
    HoldTimeout { max_retries: u32 },

    #[error("Sidecar endpoint not configured")]
    NotConfigured,

    #[error("Failed to decode required sidecar transaction: {0}")]
    DecodeError(String),

    #[error("Required sidecar transaction signature recovery failed")]
    SignatureRecoveryFailed,

    #[error("Required sidecar transaction cannot be deposit or blob")]
    InvalidTransactionType,

    #[error("Required sidecar transaction exceeds block limits: {0}")]
    LimitsExceeded(String),

    #[error("Required sidecar transaction execution failed: {0}")]
    ExecutionFailed(String),

    #[error("Required sidecar transaction reverted: {0}")]
    TransactionReverted(String),
}

/// Request sent to the sidecar when polling for transactions.
#[derive(Debug, Clone, Serialize)]
pub struct PollRequest {
    /// Chain ID of this rollup.
    pub chain_id: u64,

    /// Current block number being built.
    pub block_number: u64,

    /// Index of the flashblock within the current block (0-indexed).
    pub flashblock_index: u64,

    /// State root of the parent block.
    pub state_root: B256,

    /// Timestamp of the block being built.
    pub timestamp: u64,

    /// Gas limit available for this flashblock.
    pub gas_limit: u64,

    /// Optional state overrides representing the in-progress builder state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_overrides: Option<serde_json::Value>,

    /// Instance IDs of cross-chain transactions successfully executed in the
    /// previous flashblock. The sidecar removes these from its pending set,
    /// implementing the "confirm back" step of the pull protocol.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub confirmed_instance_ids: Vec<String>,
}

/// Response from the sidecar when polling for transactions.
#[derive(Debug, Clone, Deserialize)]
pub struct PollResponse {
    /// If true, the builder should wait and retry.
    pub hold: bool,

    /// Milliseconds to wait before retrying (when hold is true).
    #[serde(default)]
    pub poll_after_ms: Option<u64>,

    /// Transactions to include in the block.
    #[serde(default)]
    pub transactions: Vec<ExternalTransaction>,
}

/// A transaction received from the sidecar.
#[derive(Debug, Clone, Deserialize)]
pub struct ExternalTransaction {
    /// RLP-encoded transaction bytes.
    pub raw: Bytes,

    /// If true, this transaction MUST be included or the block build fails.
    /// If false, the transaction is optional and can be skipped on failure.
    #[serde(default)]
    pub required: bool,

    /// Optional instance ID for tracking cross-chain transaction coordination.
    #[serde(default)]
    pub instance_id: Option<String>,
}
