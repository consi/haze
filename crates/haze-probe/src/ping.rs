//! ICMP echo (PING) probe. Uses surge-ping in MVP.
//!
//! All hosts share a single v4 and a single v6 [`Client`] (one ICMP socket
//! plus one socket-reader task per family), wired up at scheduler boot.
//! surge-ping demultiplexes incoming replies by `(addr, ident, seq)`, so
//! many concurrent hosts can ride the same socket. Each [`PingProbe`] gets
//! a unique [`PingIdentifier`] from the scheduler so reply routing stays
//! unambiguous.

use std::{
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicU16, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use surge_ping::{Client, Config, ICMP, PingIdentifier, PingSequence};

use crate::{Probe, ProbeContext, ProbeError, ProbeKind};

/// Process-wide ICMP clients, one per IP family.
///
/// Constructed once at scheduler boot. If a family's raw-socket open fails
/// (permissions, no IPv6 stack) the matching slot is `None` and any host
/// whose target resolves to that family will fail probe construction with a
/// clear error.
pub struct PingClients {
    v4: Option<Arc<Client>>,
    v6: Option<Arc<Client>>,
}

impl PingClients {
    pub fn new() -> Self {
        let v4 = match Client::new(&Config::builder().kind(ICMP::V4).build()) {
            Ok(c) => Some(Arc::new(c)),
            Err(e) => {
                tracing::warn!(error = %e, "ICMP v4 socket unavailable; v4 ping probes will fail (need CAP_NET_RAW capability)");
                None
            }
        };
        let v6 = match Client::new(&Config::builder().kind(ICMP::V6).build()) {
            Ok(c) => Some(Arc::new(c)),
            Err(e) => {
                tracing::warn!(error = %e, "ICMP v6 socket unavailable; v6 ping probes will fail");
                None
            }
        };
        Self { v4, v6 }
    }

    pub fn for_ip(&self, ip: IpAddr) -> Option<Arc<Client>> {
        match ip {
            IpAddr::V4(_) => self.v4.clone(),
            IpAddr::V6(_) => self.v6.clone(),
        }
    }
}

impl Default for PingClients {
    fn default() -> Self {
        Self::new()
    }
}

pub const CONFIG_SCHEMA: &str = r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "object",
  "required": ["target"],
  "properties": {
    "target": { "type": "string", "description": "Hostname or IP to ping" },
    "prefer_ipv6": { "type": "boolean", "default": false }
  }
}"#;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PingConfig {
    pub target: String,
    #[serde(default)]
    pub prefer_ipv6: bool,
}

pub struct PingProbe {
    cfg: PingConfig,
    target_ip: IpAddr,
    client: Arc<Client>,
    next_seq: Arc<AtomicU16>,
    identifier: PingIdentifier,
}

impl PingProbe {
    pub async fn new(
        cfg_value: &serde_json::Value,
        clients: &PingClients,
        identifier: PingIdentifier,
    ) -> Result<Self, ProbeError> {
        let cfg: PingConfig = serde_json::from_value(cfg_value.clone())
            .map_err(|e| ProbeError::Config(e.to_string()))?;
        let ip = resolve(&cfg.target, cfg.prefer_ipv6).await?;
        let client = clients.for_ip(ip).ok_or_else(|| {
            ProbeError::Runtime(format!(
                "no ICMP {} socket available for target {} (need CAP_NET_RAW capability)",
                if ip.is_ipv6() { "v6" } else { "v4" },
                cfg.target
            ))
        })?;
        Ok(Self {
            cfg,
            target_ip: ip,
            client,
            next_seq: Arc::new(AtomicU16::new(0)),
            identifier,
        })
    }
}

async fn resolve(target: &str, prefer_ipv6: bool) -> Result<IpAddr, ProbeError> {
    if let Ok(ip) = target.parse::<IpAddr>() {
        return Ok(ip);
    }
    // Append :0 so we can pass to lookup_host which expects host:port.
    let lookup = tokio::net::lookup_host(format!("{target}:0"))
        .await
        .map_err(|e| ProbeError::Resolve(format!("dns lookup '{target}': {e}")))?;
    let mut v4 = None;
    let mut v6 = None;
    for sa in lookup {
        match sa.ip() {
            IpAddr::V4(_) if v4.is_none() => v4 = Some(sa.ip()),
            IpAddr::V6(_) if v6.is_none() => v6 = Some(sa.ip()),
            _ => {}
        }
    }
    let chosen = if prefer_ipv6 { v6.or(v4) } else { v4.or(v6) };
    chosen.ok_or_else(|| ProbeError::Resolve(format!("no addresses for '{target}'")))
}

#[async_trait]
impl Probe for PingProbe {
    fn target_ip(&self) -> Option<IpAddr> {
        Some(self.target_ip)
    }
    fn kind(&self) -> ProbeKind {
        ProbeKind::Ping
    }

    async fn measure_once(&self, ctx: &ProbeContext) -> Result<f32, ProbeError> {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let mut pinger = self.client.pinger(self.target_ip, self.identifier).await;
        pinger.timeout(ctx.timeout);
        let payload = [0u8; 16];
        let result = pinger.ping(PingSequence(seq), &payload).await;
        match result {
            Ok((_packet, rtt)) => Ok(duration_ms(rtt)),
            Err(e) => Err(ProbeError::Runtime(format!(
                "ping {}: {e}",
                self.cfg.target
            ))),
        }
    }
}

fn duration_ms(d: Duration) -> f32 {
    (d.as_secs_f64() * 1000.0) as f32
}
