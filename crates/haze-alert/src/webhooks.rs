//! Webhook delivery: long-lived reqwest client + JSON POST helper.
//!
//! Failures are logged and swallowed - a 5xx from the receiver should not
//! crash the eval loop. The receiver is expected to be idempotent
//! (transitions are persisted before the POST goes out, so a duplicate
//! delivery is a re-notification of the same state, not a logical glitch).

use std::time::Duration;

use serde_json::Value;
use tracing::{debug, warn};

#[derive(Debug, Clone)]
pub struct WebhookClient {
    inner: reqwest::Client,
    timeout_secs: u32,
}

impl WebhookClient {
    pub fn new() -> Self {
        Self::with_timeout(10)
    }

    pub fn with_timeout(timeout_secs: u32) -> Self {
        let inner = reqwest::Client::builder()
            .timeout(Duration::from_secs(u64::from(timeout_secs.max(1))))
            .build()
            .expect("reqwest client");
        Self {
            inner,
            timeout_secs,
        }
    }

    pub fn timeout_secs(&self) -> u32 {
        self.timeout_secs
    }

    pub async fn post(&self, url: &str, headers: &[(String, String)], payload: &Value) {
        let mut req = self.inner.post(url).json(payload);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        match req.send().await {
            Ok(r) if !r.status().is_success() => {
                warn!(url = %url, status = %r.status(), "webhook returned non-2xx");
            }
            Ok(_) => debug!(url = %url, "webhook delivered"),
            Err(e) => warn!(url = %url, error = ?e, "webhook delivery failed"),
        }
    }

    /// Synthetic payload helper for the "test webhook" UI button.
    /// Returns the HTTP status code (or `None` if the request failed
    /// before getting a response) plus a short detail string the UI can
    /// show inline.
    pub async fn test(&self, url: &str, headers: &[(String, String)]) -> (Option<u16>, String) {
        let payload = serde_json::json!({
            "rule_uuid": "00000000-0000-0000-0000-000000000000",
            "rule_name": "test-webhook",
            "host_uuid": "00000000-0000-0000-0000-000000000000",
            "from": "ok",
            "to": "critical",
            "metric": "median",
            "aggregation": "max",
            "direction": "above",
            "value": 999.0,
            "threshold": 100.0,
            "window_secs": 300,
            "ts": chrono::Utc::now().timestamp(),
            "test": true,
        });
        let mut req = self.inner.post(url).json(&payload);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        match req.send().await {
            Ok(r) => {
                let status = r.status();
                (
                    Some(status.as_u16()),
                    status.canonical_reason().unwrap_or("").into(),
                )
            }
            Err(e) => (None, e.to_string()),
        }
    }
}

impl Default for WebhookClient {
    fn default() -> Self {
        Self::new()
    }
}
