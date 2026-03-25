//! HTTP client for builder-to-sidecar XT inclusion callbacks.

use super::{
    config::SidecarConfig,
    types::{ConfirmIncludedRequest, SidecarError},
};
use reqwest::Client;
use std::{
    collections::BTreeSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tracing::{debug, warn};

/// Client for confirming included XT instance IDs back to the sidecar.
#[derive(Debug, Clone)]
pub struct SidecarClient {
    client: Client,
    config: SidecarConfig,
    pending_confirmations: Arc<Mutex<BTreeSet<String>>>,
    flush_in_progress: Arc<AtomicBool>,
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
            flush_in_progress: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Returns true if the sidecar callback integration is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.is_enabled()
    }

    /// Enqueue confirmed XT instance IDs and schedule a background flush.
    pub fn confirm_included(&self, instance_ids: Vec<String>) {
        if instance_ids.is_empty() || !self.is_enabled() {
            return;
        }

        if let Ok(mut pending) = self.pending_confirmations.lock() {
            pending.extend(instance_ids);
        }

        self.spawn_flush_task();
    }

    fn spawn_flush_task(&self) {
        if self
            .flush_in_progress
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.flush_in_progress.store(false, Ordering::Release);
            warn!(
                target: "sidecar",
                "No Tokio runtime available for XT confirmation flush"
            );
            return;
        };

        let client = self.clone();
        handle.spawn(async move {
            client.flush_pending_confirmations().await;
        });
    }

    async fn flush_pending_confirmations(self) {
        loop {
            let queued = self.take_pending_confirmations();
            if queued.is_empty() {
                self.flush_in_progress.store(false, Ordering::Release);

                if self.has_pending_confirmations()
                    && self
                        .flush_in_progress
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                {
                    continue;
                }

                break;
            }

            let count = queued.len();
            if let Err(err) = self.confirm_instances(queued).await {
                warn!(
                    target: "sidecar",
                    %err,
                    "Failed to confirm included XT instances to sidecar"
                );
                self.flush_in_progress.store(false, Ordering::Release);
                break;
            }

            debug!(
                target: "sidecar",
                count,
                "Confirmed included XT instances to sidecar"
            );
        }
    }

    fn take_pending_confirmations(&self) -> Vec<String> {
        let queued = self
            .pending_confirmations
            .lock()
            .map(|mut pending| std::mem::take(&mut *pending))
            .unwrap_or_default();

        queued.into_iter().collect()
    }

    fn has_pending_confirmations(&self) -> bool {
        self.pending_confirmations
            .lock()
            .map(|pending| !pending.is_empty())
            .unwrap_or(false)
    }

    async fn confirm_instances(&self, instance_ids: Vec<String>) -> Result<(), SidecarError> {
        let request = ConfirmIncludedRequest {
            instance_ids: instance_ids.clone(),
        };
        let url = format!(
            "{}/ethera/confirm",
            self.config.endpoint.trim_end_matches('/')
        );

        let mut last_error = None;
        for attempt in 0..=self.config.max_retries {
            match self.client.post(&url).json(&request).send().await {
                Ok(response) => match response.error_for_status() {
                    Ok(response) => {
                        drop(response);
                        return Ok(());
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

        if let Ok(mut pending) = self.pending_confirmations.lock() {
            pending.extend(request.instance_ids);
        }

        Err(last_error
            .expect("confirmation retry loop must store the last error")
            .into())
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
