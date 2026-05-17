//! TCP CONNECT - measures SYN → ACK acknowledged time.

use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{Probe, ProbeContext, ProbeError, ProbeKind};

pub const CONFIG_SCHEMA: &str = r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "object",
  "required": ["host", "port"],
  "properties": {
    "host": { "type": "string" },
    "port": { "type": "integer", "minimum": 1, "maximum": 65535 }
  }
}"#;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct TcpConnectConfig {
    pub host: String,
    pub port: u16,
}

pub struct TcpConnectProbe {
    cfg: TcpConnectConfig,
}

impl TcpConnectProbe {
    pub fn new(cfg_value: &serde_json::Value) -> Result<Self, ProbeError> {
        let cfg: TcpConnectConfig = serde_json::from_value(cfg_value.clone())
            .map_err(|e| ProbeError::Config(e.to_string()))?;
        Ok(Self { cfg })
    }
}

#[async_trait]
impl Probe for TcpConnectProbe {
    fn kind(&self) -> ProbeKind {
        ProbeKind::TcpConnect
    }

    async fn measure_once(&self, ctx: &ProbeContext) -> Result<f32, ProbeError> {
        let addr = format!("{}:{}", self.cfg.host, self.cfg.port);
        let start = Instant::now();
        let connect = tokio::net::TcpStream::connect(&addr);
        match tokio::time::timeout(ctx.timeout, connect).await {
            Ok(Ok(stream)) => {
                let elapsed = start.elapsed();
                drop(stream);
                Ok(elapsed.as_secs_f64() as f32 * 1000.0)
            }
            Ok(Err(e)) => Err(ProbeError::Runtime(format!("tcp connect {addr}: {e}"))),
            Err(_) => Err(ProbeError::Runtime(format!("tcp connect {addr} timeout"))),
        }
    }
}
