use alloy_primitives::Bytes;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct XtOrderKey {
    pub period_id: u64,
    pub sequence_number: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitXtRequest {
    pub instance_id: String,
    pub order: XtOrderKey,
    pub transactions: Vec<Bytes>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseXtRequest {
    pub instance_id: String,
    pub transactions: Vec<Bytes>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbortXtRequest {
    pub instance_id: String,
}

#[derive(Debug, Error)]
pub enum XtExecutionError {
    #[error("failed to decode XT transaction: {0}")]
    Decode(String),
    #[error("XT transaction signature recovery failed")]
    SignatureRecovery,
    #[error("XT transaction type is not supported")]
    InvalidTransactionType,
    #[error("XT transaction exceeds block limits: {0}")]
    LimitsExceeded(String),
    #[error("XT transaction execution failed: {0}")]
    ExecutionFailed(String),
}
