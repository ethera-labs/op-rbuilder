//! Types for builder-to-sidecar callbacks.

use serde::Serialize;
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
