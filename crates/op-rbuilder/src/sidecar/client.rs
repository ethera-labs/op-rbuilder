//! HTTP client for builder-to-sidecar XT inclusion callbacks.

use super::{
    config::SidecarConfig,
    types::{ConfirmIncludedRequest, SidecarError},
};
use reqwest::Client;
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::Duration,
};
use tracing::{debug, warn};

/// Client for confirming included XT instance IDs back to the sidecar.
#[derive(Debug, Clone)]
pub struct SidecarClient {
    client: Client,
    config: SidecarConfig,
    pending_confirmations: Arc<Mutex<BTreeSet<String>>>,
}

impl SidecarClient {
    /// Creates a new sidecar client with the given configuration.
    pub fn new(config: SidecarConfig) -> Self {
        let client = Client::builder()
            .timeout(config.request_timeout)
            .pool_max_idle_per_host(2)
            .build()
            .expect("failed to build HTTP client");

        Self {
            client,
            config,
            pending_confirmations: Default::default(),
        }
    }

    /// Returns true if the sidecar callback integration is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.is_enabled()
    }

    /// Enqueue confirmed XT instance IDs and flush the callback queue.
    pub fn confirm_included(&self, instance_ids: Vec<String>) {
        if instance_ids.is_empty() || !self.is_enabled() {
            return;
        }

        if let Ok(mut pending) = self.pending_confirmations.lock() {
            pending.extend(instance_ids);
        }

        if let Err(err) = self.flush_pending_confirmations() {
            warn!(
                target: "sidecar",
                %err,
                "Failed to confirm included XT instances to sidecar"
            );
        }
    }

    fn flush_pending_confirmations(&self) -> Result<(), SidecarError> {
        let queued = self
            .pending_confirmations
            .lock()
            .map(|mut pending| std::mem::take(&mut *pending))
            .unwrap_or_default();
        if queued.is_empty() {
            return Ok(());
        }

        let instance_ids: Vec<String> = queued.into_iter().collect();
        let request = ConfirmIncludedRequest {
            instance_ids: instance_ids.clone(),
        };
        let url = format!(
            "{}/ethera/confirm",
            self.config.endpoint.trim_end_matches('/')
        );

        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut last_error = None;
                for attempt in 0..=self.config.max_retries {
                    match self.client.post(&url).json(&request).send().await {
                        Ok(response) => match response.error_for_status() {
                            Ok(response) => {
                                drop(response);
                                return Ok::<(), reqwest::Error>(());
                            }
                            Err(err) => {
                                last_error = Some(err);
                            }
                        },
                        Err(err) => {
                            last_error = Some(err);
                        }
                    }
                    if attempt == self.config.max_retries {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(last_error.expect("confirmation retry loop must store the last error"))
            })
        });

        match result {
            Ok(()) => {
                debug!(
                    target: "sidecar",
                    count = request.instance_ids.len(),
                    "Confirmed included XT instances to sidecar"
                );
                Ok(())
            }
            Err(err) => {
                if let Ok(mut pending) = self.pending_confirmations.lock() {
                    pending.extend(request.instance_ids);
                }
                Err(err.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::sidecar::SidecarConfig;

    #[test]
    fn disabled_config_is_reported_as_disabled() {
        let config = SidecarConfig::default();
        assert!(!config.is_enabled());
    }

    #[test]
    fn configured_endpoint_is_reported_as_enabled() {
        let config = SidecarConfig {
            endpoint: "http://localhost:8082".to_string(),
            ..Default::default()
        };
        assert!(config.is_enabled());
    }
}
