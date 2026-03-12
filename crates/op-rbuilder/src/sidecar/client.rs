//! HTTP client for communicating with the compose sidecar.

use super::{
    config::SidecarConfig,
    types::{ExternalTransaction, PollRequest, PollResponse, SidecarError},
};
use reqwest::Client;
use std::sync::Mutex;
use std::time::Duration;
use tracing::{debug, trace, warn};

/// Client for polling the compose sidecar for cross-chain transactions.
#[derive(Debug, Clone)]
pub struct SidecarClient {
    client: Client,
    config: SidecarConfig,
    /// Instance IDs queued to be confirmed on the next poll. Populated after
    /// a successful sidecar transaction execution, drained into each request.
    pending_confirmations: std::sync::Arc<Mutex<Vec<String>>>,
}

impl SidecarClient {
    /// Creates a new sidecar client with the given configuration.
    pub fn new(config: SidecarConfig) -> Self {
        let client = Client::builder()
            .timeout(config.poll_timeout)
            .pool_max_idle_per_host(2)
            .build()
            .expect("failed to build HTTP client");

        Self { client, config, pending_confirmations: Default::default() }
    }

    /// Record instance IDs of XTs successfully executed by the builder.
    /// These will be sent to the sidecar on the next poll so it can remove
    /// them from its pending set.
    pub fn confirm_executed(&self, instance_ids: Vec<String>) {
        if instance_ids.is_empty() {
            return;
        }
        if let Ok(mut pending) = self.pending_confirmations.lock() {
            pending.extend(instance_ids);
        }
    }

    /// Returns true if the sidecar integration is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.is_enabled()
    }

    /// Polls the sidecar for external transactions.
    ///
    /// This method handles the hold/retry loop internally, waiting up to
    /// `max_retries` times if the sidecar indicates the builder should hold.
    ///
    /// Returns `Ok(None)` if sidecar is disabled.
    /// Returns `Ok(Some(vec![]))` if sidecar has no transactions.
    /// Returns `Ok(Some(txs))` if sidecar returns transactions.
    /// Returns `Err` on HTTP failure or hold timeout.
    pub async fn poll_transactions(
        &self,
        request: &PollRequest,
    ) -> Result<Option<Vec<ExternalTransaction>>, SidecarError> {
        if !self.config.is_enabled() {
            return Ok(None);
        }

        let url = format!("{}/transactions", self.config.endpoint);
        let mut retries = 0u32;

        // Drain confirmations once before the first attempt. Retries (hold
        // responses) reuse the same request object and don't re-drain.
        let confirmed = self
            .pending_confirmations
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default();
        let request = if confirmed.is_empty() {
            std::borrow::Cow::Borrowed(request)
        } else {
            let mut req = request.clone();
            req.confirmed_instance_ids = confirmed;
            std::borrow::Cow::Owned(req)
        };
        let request = &*request;

        loop {
            trace!(
                target: "sidecar",
                chain_id = request.chain_id,
                block_number = request.block_number,
                flashblock_index = request.flashblock_index,
                retry = retries,
                confirmed = request.confirmed_instance_ids.len(),
                "Polling sidecar"
            );

            // If the HTTP call fails, re-queue the confirmations so they are
            // retried on the next poll. Sending duplicate confirmations is
            // idempotent: the sidecar ignores IDs it no longer tracks.
            let poll_result: Result<PollResponse, reqwest::Error> = async {
                self.client
                    .post(&url)
                    .json(request)
                    .send()
                    .await?
                    .error_for_status()?
                    .json::<PollResponse>()
                    .await
            }
            .await;
            let response = match poll_result {
                Ok(r) => r,
                Err(e) => {
                    if !request.confirmed_instance_ids.is_empty() {
                        self.confirm_executed(request.confirmed_instance_ids.to_vec());
                    }
                    return Err(e.into());
                }
            };

            if !response.hold {
                if response.transactions.is_empty() {
                    debug!(
                        target: "sidecar",
                        block_number = request.block_number,
                        flashblock_index = request.flashblock_index,
                        "No external transactions from sidecar"
                    );
                } else {
                    debug!(
                        target: "sidecar",
                        block_number = request.block_number,
                        flashblock_index = request.flashblock_index,
                        count = response.transactions.len(),
                        required_count = response.transactions.iter().filter(|t| t.required).count(),
                        "Received external transactions from sidecar"
                    );
                }
                return Ok(Some(response.transactions));
            }

            retries += 1;
            if retries > self.config.max_retries {
                warn!(
                    target: "sidecar",
                    block_number = request.block_number,
                    flashblock_index = request.flashblock_index,
                    max_retries = self.config.max_retries,
                    "Sidecar hold timeout exceeded"
                );
                return Err(SidecarError::HoldTimeout {
                    max_retries: self.config.max_retries,
                });
            }

            let delay_ms = response.poll_after_ms.unwrap_or(50);
            trace!(
                target: "sidecar",
                delay_ms,
                retry = retries,
                max_retries = self.config.max_retries,
                "Sidecar holding, will retry"
            );
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disabled_client() {
        let config = SidecarConfig::default();
        let client = SidecarClient::new(config);
        assert!(!client.is_enabled());
    }

    #[test]
    fn test_enabled_client() {
        let config = SidecarConfig {
            endpoint: "http://localhost:8082".to_string(),
            ..Default::default()
        };
        let client = SidecarClient::new(config);
        assert!(client.is_enabled());
    }
}
