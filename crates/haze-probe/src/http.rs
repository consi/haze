//! HTTP TTFB / HTTP TOTAL probes via reqwest (rustls).
//!
//! Each probe opens a fresh TCP+TLS connection (idle-pool disabled) so
//! timings reflect a cold request. `reqwest::Client`s are cached
//! process-wide in [`HttpClients`], keyed by the only two settings that
//! must be baked into the client (`verify_tls`, `follow_redirects`) — so
//! we have at most four shared clients regardless of host count. Per-request
//! timeout/method live on the `RequestBuilder`, not the client.

use std::{collections::HashMap, sync::Mutex, time::Instant};

use async_trait::async_trait;
use reqwest::{Method, redirect::Policy};
use serde::{Deserialize, Serialize};

use crate::{Probe, ProbeContext, ProbeError, ProbeKind};

/// Shared `reqwest::Client` cache. Keyed by `(verify_tls, follow_redirects)`
/// so any number of hosts with the same security/redirect settings share one
/// hyper connector instead of constructing 586 of them.
pub struct HttpClients {
    inner: Mutex<HashMap<(bool, bool), reqwest::Client>>,
}

impl HttpClients {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(
        &self,
        verify_tls: bool,
        follow_redirects: bool,
    ) -> Result<reqwest::Client, ProbeError> {
        let key = (verify_tls, follow_redirects);
        {
            let g = self.inner.lock().expect("http clients mutex poisoned");
            if let Some(c) = g.get(&key) {
                return Ok(c.clone());
            }
        }
        let c = build_client_inner(verify_tls, follow_redirects)?;
        let mut g = self.inner.lock().expect("http clients mutex poisoned");
        Ok(g.entry(key).or_insert(c).clone())
    }
}

impl Default for HttpClients {
    fn default() -> Self {
        Self::new()
    }
}

fn build_client_inner(
    verify_tls: bool,
    follow_redirects: bool,
) -> Result<reqwest::Client, ProbeError> {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(!verify_tls)
        .pool_max_idle_per_host(0)
        .redirect(if follow_redirects {
            Policy::limited(5)
        } else {
            Policy::none()
        })
        .build()
        .map_err(|e| ProbeError::Runtime(format!("http client init: {e}")))
}

pub const TTFB_CONFIG_SCHEMA: &str = r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "object",
  "required": ["url"],
  "properties": {
    "url": { "type": "string", "format": "uri" },
    "method": { "type": "string", "default": "GET" },
    "expect_status": { "type": "string", "default": "2xx", "description": "'2xx', '3xx', or a specific code like '200'" },
    "verify_tls": { "type": "boolean", "default": true },
    "follow_redirects": { "type": "boolean", "default": false }
  }
}"#;

pub const TOTAL_CONFIG_SCHEMA: &str = r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "object",
  "required": ["url"],
  "properties": {
    "url": { "type": "string", "format": "uri" },
    "method": { "type": "string", "default": "GET" },
    "expect_status": { "type": "string", "default": "2xx" },
    "verify_tls": { "type": "boolean", "default": true },
    "follow_redirects": { "type": "boolean", "default": false },
    "max_bytes": { "type": "integer", "minimum": 0, "default": 65536 }
  }
}"#;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct HttpConfig {
    pub url: String,
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default = "default_expect_status")]
    pub expect_status: String,
    #[serde(default = "default_true")]
    pub verify_tls: bool,
    #[serde(default)]
    pub follow_redirects: bool,
    #[serde(default = "default_max_bytes")]
    pub max_bytes: usize,
}

fn default_method() -> String {
    "GET".into()
}
fn default_expect_status() -> String {
    "2xx".into()
}
fn default_true() -> bool {
    true
}
fn default_max_bytes() -> usize {
    65_536
}

pub struct HttpTtfbProbe {
    cfg: HttpConfig,
    method: Method,
    client: reqwest::Client,
}

pub struct HttpTotalProbe {
    cfg: HttpConfig,
    method: Method,
    client: reqwest::Client,
}

impl HttpTtfbProbe {
    pub fn new(cfg_value: &serde_json::Value, clients: &HttpClients) -> Result<Self, ProbeError> {
        let (cfg, method) = parse_cfg(cfg_value)?;
        let client = clients.get(cfg.verify_tls, cfg.follow_redirects)?;
        Ok(Self {
            cfg,
            method,
            client,
        })
    }
}

impl HttpTotalProbe {
    pub fn new(cfg_value: &serde_json::Value, clients: &HttpClients) -> Result<Self, ProbeError> {
        let (cfg, method) = parse_cfg(cfg_value)?;
        let client = clients.get(cfg.verify_tls, cfg.follow_redirects)?;
        Ok(Self {
            cfg,
            method,
            client,
        })
    }
}

fn parse_cfg(cfg_value: &serde_json::Value) -> Result<(HttpConfig, Method), ProbeError> {
    let cfg: HttpConfig =
        serde_json::from_value(cfg_value.clone()).map_err(|e| ProbeError::Config(e.to_string()))?;
    let method = Method::from_bytes(cfg.method.as_bytes())
        .map_err(|e| ProbeError::Config(format!("method: {e}")))?;
    // Front-load validation so a malformed status spec fails at probe
    // construction instead of at every measurement.
    for tok in cfg.expect_status.split(',') {
        let t = tok.trim();
        if t.is_empty() {
            return Err(ProbeError::Config(
                "expect_status contains an empty entry".into(),
            ));
        }
        if !is_valid_status_token(t) {
            return Err(ProbeError::Config(format!(
                "expect_status entry '{t}' is invalid (expected 100-599 or 2xx/3xx/4xx/5xx)"
            )));
        }
    }
    Ok((cfg, method))
}

/// A status token is either an exact 3-digit code in `100..=599`, or one of
/// `1xx 2xx 3xx 4xx 5xx`. Comma-separated lists of these tokens are OR'd.
fn is_valid_status_token(t: &str) -> bool {
    if let Ok(code) = t.parse::<u16>() {
        return (100..=599).contains(&code);
    }
    matches!(t, "1xx" | "2xx" | "3xx" | "4xx" | "5xx")
}

/// Match `actual` against a comma-separated list of expected status tokens.
/// Each token is either an exact 3-digit code or a class like `2xx`. Any
/// matching token causes the overall check to pass.
fn status_ok(expected: &str, actual: u16) -> bool {
    expected
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .any(|tok| token_matches(tok, actual))
}

fn token_matches(tok: &str, actual: u16) -> bool {
    if let Ok(code) = tok.parse::<u16>() {
        return code == actual;
    }
    match tok {
        "1xx" => (100..200).contains(&actual),
        "2xx" => (200..300).contains(&actual),
        "3xx" => (300..400).contains(&actual),
        "4xx" => (400..500).contains(&actual),
        "5xx" => (500..600).contains(&actual),
        _ => false,
    }
}

#[async_trait]
impl Probe for HttpTtfbProbe {
    fn kind(&self) -> ProbeKind {
        ProbeKind::HttpTtfb
    }

    async fn measure_once(&self, ctx: &ProbeContext) -> Result<f32, ProbeError> {
        let start = Instant::now();
        let resp = self
            .client
            .request(self.method.clone(), &self.cfg.url)
            .timeout(ctx.timeout)
            .send()
            .await
            .map_err(|e| ProbeError::Runtime(format!("http {}: {e}", self.cfg.url)))?;
        let ttfb = start.elapsed();
        if !status_ok(&self.cfg.expect_status, resp.status().as_u16()) {
            return Err(ProbeError::Runtime(format!(
                "http {} status {} (expected {})",
                self.cfg.url,
                resp.status(),
                self.cfg.expect_status
            )));
        }
        Ok(ttfb.as_secs_f64() as f32 * 1000.0)
    }
}

#[async_trait]
impl Probe for HttpTotalProbe {
    fn kind(&self) -> ProbeKind {
        ProbeKind::HttpTotal
    }

    async fn measure_once(&self, ctx: &ProbeContext) -> Result<f32, ProbeError> {
        use futures::StreamExt;
        let start = Instant::now();
        let resp = self
            .client
            .request(self.method.clone(), &self.cfg.url)
            .timeout(ctx.timeout)
            .send()
            .await
            .map_err(|e| ProbeError::Runtime(format!("http {}: {e}", self.cfg.url)))?;
        if !status_ok(&self.cfg.expect_status, resp.status().as_u16()) {
            return Err(ProbeError::Runtime(format!(
                "http {} status {} (expected {})",
                self.cfg.url,
                resp.status(),
                self.cfg.expect_status
            )));
        }
        // Read body up to max_bytes; stop early if cap is hit.
        let mut stream = resp.bytes_stream();
        let mut read: usize = 0;
        while let Some(chunk) = stream.next().await {
            let bytes = chunk
                .map_err(|e| ProbeError::Runtime(format!("http body {}: {e}", self.cfg.url)))?;
            read = read.saturating_add(bytes.len());
            if read >= self.cfg.max_bytes {
                break;
            }
        }
        Ok(start.elapsed().as_secs_f64() as f32 * 1000.0)
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;

    #[test]
    fn single_exact_code() {
        assert!(status_ok("200", 200));
        assert!(!status_ok("200", 201));
    }

    #[test]
    fn class_match() {
        assert!(status_ok("2xx", 204));
        assert!(!status_ok("2xx", 301));
    }

    #[test]
    fn comma_separated_list() {
        // Any token matching is enough.
        assert!(status_ok("200, 301, 4xx", 200));
        assert!(status_ok("200, 301, 4xx", 301));
        assert!(status_ok("200, 301, 4xx", 404));
        assert!(!status_ok("200, 301, 4xx", 500));
    }

    #[test]
    fn validates_tokens() {
        assert!(is_valid_status_token("200"));
        assert!(is_valid_status_token("2xx"));
        assert!(!is_valid_status_token("99"));
        assert!(!is_valid_status_token("600"));
        assert!(!is_valid_status_token("ok"));
        assert!(!is_valid_status_token("2XX"));
    }
}
