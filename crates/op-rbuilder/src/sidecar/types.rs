//! Types for builder-to-sidecar callbacks.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors that can occur while calling back into the sidecar.
#[derive(Debug, Error)]
pub enum SidecarError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
}

/// Request sent to the sidecar after XT transactions are included.
#[derive(Debug, Clone, Serialize)]
pub struct ConfirmIncludedRequest {
    pub instance_ids: Vec<String>,
}

/// Request asking the sidecar to authorize a single transaction before it is
/// admitted to the pool (UC1 native send, UC2 contract deploy).
#[derive(Debug, Clone, Serialize)]
pub struct CheckTxRequest {
    pub from: String,
    pub is_create: bool,
    pub has_value: bool,
}

/// Sidecar verdict for a [`CheckTxRequest`].
#[derive(Debug, Clone, Deserialize)]
pub struct CheckTxResponse {
    pub allowed: bool,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub config_version: Option<u64>,
}
