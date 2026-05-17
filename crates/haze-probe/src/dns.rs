//! DNS resolution latency probe via hickory-resolver.
//!
//! Resolvers are cached process-wide in [`DnsResolvers`], keyed by the
//! configured upstream (None for the system resolver). Each [`TokioAsyncResolver`]
//! owns a UDP socket plus an internal receive loop, so building one per host
//! would replicate the per-host surge-ping anti-pattern (hundreds of recv
//! tasks competing for the executor).

use std::{
    collections::HashMap,
    net::SocketAddr,
    str::FromStr,
    sync::{Arc, Mutex},
    time::Instant,
};

use async_trait::async_trait;
use hickory_resolver::{
    TokioAsyncResolver,
    config::{NameServerConfig, Protocol, ResolverConfig, ResolverOpts},
    proto::rr::RecordType,
};
use serde::{Deserialize, Serialize};

use crate::{Probe, ProbeContext, ProbeError, ProbeKind};

/// Process-wide cache of `TokioAsyncResolver`s keyed by upstream address.
///
/// `None` keys the system resolver. Building a resolver allocates a UDP
/// socket and spawns a recv loop, so we share aggressively across hosts.
pub struct DnsResolvers {
    inner: Mutex<HashMap<Option<SocketAddr>, Arc<TokioAsyncResolver>>>,
}

impl DnsResolvers {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, upstream: Option<SocketAddr>) -> Arc<TokioAsyncResolver> {
        let mut g = self.inner.lock().expect("dns resolvers mutex poisoned");
        if let Some(r) = g.get(&upstream) {
            return r.clone();
        }
        let resolver = match upstream {
            Some(sa) => {
                let mut cfg = ResolverConfig::new();
                cfg.add_name_server(NameServerConfig {
                    socket_addr: sa,
                    protocol: Protocol::Udp,
                    tls_dns_name: None,
                    trust_negative_responses: true,
                    bind_addr: None,
                });
                TokioAsyncResolver::tokio(cfg, ResolverOpts::default())
            }
            None => TokioAsyncResolver::tokio(ResolverConfig::default(), ResolverOpts::default()),
        };
        let arc = Arc::new(resolver);
        g.insert(upstream, arc.clone());
        arc
    }
}

impl Default for DnsResolvers {
    fn default() -> Self {
        Self::new()
    }
}

pub const CONFIG_SCHEMA: &str = r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "object",
  "required": ["query", "record_type"],
  "properties": {
    "query": { "type": "string", "description": "Name to resolve" },
    "record_type": {
      "type": "string",
      "enum": ["A","AAAA","MX","TXT","CNAME","NS","SOA"],
      "default": "A"
    },
    "resolver": {
      "type": "string",
      "description": "Upstream resolver socket addr, e.g. '8.8.8.8:53'. Omit to use the system resolver."
    }
  }
}"#;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct DnsConfig {
    pub query: String,
    pub record_type: String,
    pub resolver: Option<String>,
}

pub struct DnsProbe {
    cfg: DnsConfig,
    resolver: Arc<TokioAsyncResolver>,
    record_type: RecordType,
}

impl DnsProbe {
    pub fn new(
        cfg_value: &serde_json::Value,
        resolvers: &DnsResolvers,
    ) -> Result<Self, ProbeError> {
        let cfg: DnsConfig = serde_json::from_value(cfg_value.clone())
            .map_err(|e| ProbeError::Config(e.to_string()))?;
        let record_type = RecordType::from_str(&cfg.record_type)
            .map_err(|e| ProbeError::Config(format!("record_type: {e}")))?;
        let upstream = if let Some(addr) = &cfg.resolver {
            Some(
                addr.parse::<SocketAddr>()
                    .map_err(|e| ProbeError::Config(format!("resolver '{addr}': {e}")))?,
            )
        } else {
            None
        };
        let resolver = resolvers.get(upstream);
        Ok(Self {
            cfg,
            resolver,
            record_type,
        })
    }
}

#[async_trait]
impl Probe for DnsProbe {
    fn kind(&self) -> ProbeKind {
        ProbeKind::Dns
    }

    async fn measure_once(&self, ctx: &ProbeContext) -> Result<f32, ProbeError> {
        let start = Instant::now();
        let lookup = tokio::time::timeout(
            ctx.timeout,
            self.resolver
                .lookup(self.cfg.query.clone(), self.record_type),
        )
        .await
        .map_err(|_| ProbeError::Runtime(format!("dns {} timeout", self.cfg.query)))?;
        lookup.map_err(|e| ProbeError::Runtime(format!("dns {}: {e}", self.cfg.query)))?;
        Ok(start.elapsed().as_secs_f64() as f32 * 1000.0)
    }
}
