//! HTTP client for builder-to-sidecar XT inclusion callbacks.

use super::{
    config::SidecarConfig,
    types::{ConfirmIncludedRequest, SidecarError},
};
use reqwest::Client;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Confirms included XT instance IDs back to the sidecar via batched HTTP POSTs.
#[derive(Debug, Clone)]
pub struct SidecarClient {
    tx: Option<mpsc::UnboundedSender<Vec<String>>>,
}

impl SidecarClient {
    pub fn new(config: SidecarConfig) -> Self {
        if !config.is_enabled() {
            return Self { tx: None };
        }

        let client = Client::builder()
            .timeout(config.request_timeout)
            .pool_max_idle_per_host(2)
            .build()
            .expect("failed to build HTTP client");

        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(Self::flush_worker(rx, client, config));

        Self { tx: Some(tx) }
    }

    pub fn is_enabled(&self) -> bool {
        self.tx.is_some()
    }

    pub fn confirm_included(&self, instance_ids: Vec<String>) {
        if instance_ids.is_empty() {
            return;
        }
        if let Some(tx) = &self.tx {
            let _ = tx.send(instance_ids);
        }
    }

    async fn flush_worker(
        mut rx: mpsc::UnboundedReceiver<Vec<String>>,
        client: Client,
        config: SidecarConfig,
    ) {
        let url = format!("{}/ethera/confirm", config.endpoint.trim_end_matches('/'));

        loop {
            let Some(ids) = rx.recv().await else {
                break;
            };
            let mut batch = ids;

            while let Ok(ids) = rx.try_recv() {
                batch.extend(ids);
            }

            let count = batch.len();
            if let Err(err) = confirm_instances(&client, &url, &config, batch).await {
                warn!(target: "sidecar", %err, count, "Failed to confirm XT instances to sidecar");
            } else {
                debug!(target: "sidecar", count, "Confirmed XT instances to sidecar");
            }
        }
    }
}

async fn confirm_instances(
    client: &Client,
    url: &str,
    config: &SidecarConfig,
    instance_ids: Vec<String>,
) -> Result<(), SidecarError> {
    let request = ConfirmIncludedRequest { instance_ids };
    let mut last_error = None;

    for attempt in 0..=config.max_retries {
        match client.post(url).json(&request).send().await {
            Ok(response) => match response.error_for_status() {
                Ok(response) => {
                    drop(response);
                    return Ok(());
                }
                Err(err) => last_error = Some(err),
            },
            Err(err) => last_error = Some(err),
        }
        if attempt < config.max_retries {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    Err(last_error
        .expect("retry loop always stores the last error before returning")
        .into())
}
