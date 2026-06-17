//! Sidecar callback client configuration.

use crate::args::SidecarArgs;
use std::time::Duration;

/// Configuration for the sidecar callback client.
#[derive(Debug, Clone)]
pub struct SidecarConfig {
    /// HTTP endpoint of the sidecar (e.g., "http://localhost:8082").
    /// If empty, sidecar callbacks are disabled.
    pub endpoint: String,

    /// Timeout for individual HTTP requests.
    pub request_timeout: Duration,

    /// Maximum number of retries when a callback request fails.
    pub max_retries: u32,

    /// Whether to gate transaction admission on the sidecar permission check.
    pub permissions_enabled: bool,
}

impl Default for SidecarConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            request_timeout: Duration::from_millis(200),
            max_retries: 5,
            permissions_enabled: false,
        }
    }
}

impl SidecarConfig {
    /// Returns true if sidecar callbacks are enabled.
    pub fn is_enabled(&self) -> bool {
        !self.endpoint.is_empty()
    }
}

impl From<&SidecarArgs> for SidecarConfig {
    fn from(args: &SidecarArgs) -> Self {
        Self {
            endpoint: args.endpoint.clone().unwrap_or_default(),
            request_timeout: Duration::from_millis(args.poll_timeout_ms),
            max_retries: args.max_retries,
            permissions_enabled: args.permissions_enabled,
        }
    }
}
