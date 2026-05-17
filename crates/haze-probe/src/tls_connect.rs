//! TLS CONNECT - measures TCP + TLS handshake time (start → TLS Finished).

use std::{sync::Arc, time::Instant};

use async_trait::async_trait;
use rustls::{RootCertStore, pki_types::ServerName};
use serde::{Deserialize, Serialize};
use tokio_rustls::TlsConnector;

use crate::{Probe, ProbeContext, ProbeError, ProbeKind};

pub const CONFIG_SCHEMA: &str = r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "object",
  "required": ["host", "port"],
  "properties": {
    "host": { "type": "string" },
    "port": { "type": "integer", "minimum": 1, "maximum": 65535, "default": 443 },
    "sni": { "type": "string", "description": "Override SNI hostname; defaults to host" },
    "verify": { "type": "boolean", "default": true }
  }
}"#;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct TlsConnectConfig {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub sni: Option<String>,
    #[serde(default = "default_true")]
    pub verify: bool,
}

fn default_true() -> bool {
    true
}

pub struct TlsConnectProbe {
    cfg: TlsConnectConfig,
    connector: TlsConnector,
}

impl TlsConnectProbe {
    pub fn new(cfg_value: &serde_json::Value) -> Result<Self, ProbeError> {
        let cfg: TlsConnectConfig = serde_json::from_value(cfg_value.clone())
            .map_err(|e| ProbeError::Config(e.to_string()))?;
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let mut client_cfg = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        if !cfg.verify {
            client_cfg
                .dangerous()
                .set_certificate_verifier(Arc::new(NoVerification));
        }
        Ok(Self {
            cfg,
            connector: TlsConnector::from(Arc::new(client_cfg)),
        })
    }
}

#[async_trait]
impl Probe for TlsConnectProbe {
    fn kind(&self) -> ProbeKind {
        ProbeKind::TlsConnect
    }

    async fn measure_once(&self, ctx: &ProbeContext) -> Result<f32, ProbeError> {
        let addr = format!("{}:{}", self.cfg.host, self.cfg.port);
        let sni = self.cfg.sni.as_deref().unwrap_or(&self.cfg.host);
        let server_name = ServerName::try_from(sni.to_string())
            .map_err(|e| ProbeError::Config(format!("invalid SNI '{sni}': {e}")))?;

        let start = Instant::now();
        let tcp = tokio::time::timeout(ctx.timeout, tokio::net::TcpStream::connect(&addr))
            .await
            .map_err(|_| ProbeError::Runtime(format!("tcp connect {addr} timeout")))?
            .map_err(|e| ProbeError::Runtime(format!("tcp connect {addr}: {e}")))?;
        let _stream = tokio::time::timeout(ctx.timeout, self.connector.connect(server_name, tcp))
            .await
            .map_err(|_| ProbeError::Runtime(format!("tls handshake {addr} timeout")))?
            .map_err(|e| ProbeError::Runtime(format!("tls handshake {addr}: {e}")))?;
        Ok(start.elapsed().as_secs_f64() as f32 * 1000.0)
    }
}

/// Dangerous: skips cert verification. Only used when `verify: false`.
#[derive(Debug)]
struct NoVerification;

impl rustls::client::danger::ServerCertVerifier for NoVerification {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
        ]
    }
}
