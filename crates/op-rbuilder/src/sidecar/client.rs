//! HTTP client for builder-to-sidecar XT inclusion callbacks.

use super::{
    config::SidecarConfig,
    types::{CheckTxRequest, CheckTxResponse, ConfirmIncludedRequest, SidecarError},
};
use alloy_primitives::Address;
use reqwest::Client;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Sidecar route that confirms included XT instance IDs.
const CONFIRM_PATH: &str = "/ethera/confirm";
/// Sidecar route that authorizes a transaction before pool admission.
const CHECK_TX_PATH: &str = "/permissions/check-tx";
/// Backoff between confirm retries.
const CONFIRM_RETRY_BACKOFF: Duration = Duration::from_millis(25);

/// Synchronous permission-check endpoint backed by the sidecar.
#[derive(Debug, Clone)]
struct CheckCaller {
    client: Client,
    url: String,
}

/// Confirms included XT instance IDs back to the sidecar via batched HTTP POSTs.
#[derive(Debug, Clone)]
pub struct SidecarClient {
    tx: Option<mpsc::UnboundedSender<Vec<String>>>,
    check: Option<CheckCaller>,
}

fn http_client(config: &SidecarConfig) -> Client {
    Client::builder()
        .timeout(config.request_timeout)
        .pool_max_idle_per_host(2)
        .build()
        .expect("failed to build HTTP client")
}

fn endpoint_url(endpoint: &str, path: &str) -> String {
    format!("{}{path}", endpoint.trim_end_matches('/'))
}

impl SidecarClient {
    pub fn new(config: SidecarConfig) -> Self {
        if !config.is_enabled() {
            return Self {
                tx: None,
                check: None,
            };
        }

        let client = http_client(&config);
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(Self::flush_worker(rx, client, config));

        Self {
            tx: Some(tx),
            check: None,
        }
    }

    /// Build a check-only client for gating transaction admission. Returns
    /// `None` when permission enforcement is disabled or no endpoint is set, so
    /// callers skip the gate entirely rather than spawning the confirm worker.
    pub fn for_permissions(config: &SidecarConfig) -> Option<Self> {
        if !config.is_enabled() || !config.permissions_enabled {
            return None;
        }
        Some(Self {
            tx: None,
            check: Some(CheckCaller {
                client: http_client(config),
                url: endpoint_url(&config.endpoint, CHECK_TX_PATH),
            }),
        })
    }

    /// Ask the sidecar whether `from` may submit a transaction with the given
    /// classification. A single attempt: the caller fails closed on any error.
    pub async fn check_tx(
        &self,
        from: Address,
        is_create: bool,
        has_value: bool,
    ) -> Result<CheckTxResponse, SidecarError> {
        let Some(check) = &self.check else {
            return Ok(CheckTxResponse {
                allowed: true,
                reason: None,
                config_version: None,
            });
        };

        let request = CheckTxRequest {
            from: format!("{from:#x}"),
            is_create,
            has_value,
        };
        let response = check
            .client
            .post(&check.url)
            .json(&request)
            .send()
            .await?
            .error_for_status()?;
        response.json::<CheckTxResponse>().await.map_err(Into::into)
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
        let url = endpoint_url(&config.endpoint, CONFIRM_PATH);

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
            tokio::time::sleep(CONFIRM_RETRY_BACKOFF).await;
        }
    }

    Err(last_error
        .expect("retry loop always stores the last error before returning")
        .into())
}
