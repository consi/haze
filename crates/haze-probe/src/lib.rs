//! Probe engine: per-host scheduler + six probe types.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use haze_store::Observation;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

pub mod dns;
pub mod http;
pub mod ping;
pub mod scheduler;
pub mod tcp_connect;
pub mod tls_connect;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeKind {
    Ping,
    Dns,
    TcpConnect,
    TlsConnect,
    HttpTtfb,
    HttpTotal,
}

impl ProbeKind {
    pub const ALL: &'static [Self] = &[
        Self::Ping,
        Self::Dns,
        Self::TcpConnect,
        Self::TlsConnect,
        Self::HttpTtfb,
        Self::HttpTotal,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::Dns => "dns",
            Self::TcpConnect => "tcp_connect",
            Self::TlsConnect => "tls_connect",
            Self::HttpTtfb => "http_ttfb",
            Self::HttpTotal => "http_total",
        }
    }

    /// JSON-schema (draft 2020-12) describing the probe's `probe_config`.
    /// Returned as a string so callers can serve it verbatim.
    pub fn config_schema(self) -> &'static str {
        match self {
            Self::Ping => ping::CONFIG_SCHEMA,
            Self::Dns => dns::CONFIG_SCHEMA,
            Self::TcpConnect => tcp_connect::CONFIG_SCHEMA,
            Self::TlsConnect => tls_connect::CONFIG_SCHEMA,
            Self::HttpTtfb => http::TTFB_CONFIG_SCHEMA,
            Self::HttpTotal => http::TOTAL_CONFIG_SCHEMA,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("invalid probe config: {0}")]
    Config(String),
    #[error("probe runtime error: {0}")]
    Runtime(String),
}

/// One probe attempt's parameters. The scheduler clones this per attempt.
#[derive(Debug, Clone)]
pub struct ProbeContext {
    pub timeout: Duration,
}

#[async_trait]
pub trait Probe: Send + Sync {
    fn kind(&self) -> ProbeKind;

    /// One measurement attempt. `Ok(latency_ms)` on success; `Err` is a loss.
    async fn measure_once(&self, ctx: &ProbeContext) -> Result<f32, ProbeError>;
}

/// Drive a probe for `count` attempts evenly spaced over `period`.
///
/// Returns N observations (one per attempt). The probe decides internally
/// whether to fan out - by default we serialize with `period / count` delays
/// so we don't surprise targets with bursts.
///
/// The semaphore is acquired immediately before each `measure_once` and
/// released right after, so the inter-attempt sleep does not occupy pool
/// capacity. This lets the host count exceed the pool capacity: a host
/// blocks only while it actually has I/O in flight, not while sleeping.
pub async fn run_period(
    probe: &dyn Probe,
    count: u32,
    period: Duration,
    per_attempt_timeout: Duration,
    sem: &Arc<Semaphore>,
) -> Vec<Observation> {
    let mut out = Vec::with_capacity(count as usize);
    let ctx = ProbeContext {
        timeout: per_attempt_timeout,
    };
    let spacing = if count > 1 {
        period / count
    } else {
        Duration::from_millis(0)
    };
    for i in 0..count {
        if i > 0 {
            tokio::time::sleep(spacing).await;
        }
        let permit = sem.clone().acquire_owned().await.unwrap();
        let result = probe.measure_once(&ctx).await;
        drop(permit);
        out.push(match result {
            Ok(ms) => Observation::Latency(ms),
            Err(e) => {
                tracing::trace!(attempt = i, error = %e, "probe attempt failed");
                Observation::Loss
            }
        });
    }
    out
}

#[cfg(test)]
mod run_period_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    use super::*;

    /// Minimal `Probe` that sleeps `delay` per call and records call count.
    struct MockProbe {
        delay: Duration,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Probe for MockProbe {
        fn kind(&self) -> ProbeKind {
            ProbeKind::Ping
        }

        async fn measure_once(&self, _ctx: &ProbeContext) -> Result<f32, ProbeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            Ok(self.delay.as_secs_f64() as f32 * 1000.0)
        }
    }

    /// Pool=1 with two concurrent `run_period` calls. If a permit were held
    /// for the whole period (old behaviour) the second call would block until
    /// the first finished and wall time would roughly double. With per-attempt
    /// permits the spacing sleeps run in parallel, so total time is closer to
    /// one period plus a small serialization delay.
    #[tokio::test]
    async fn releases_permit_between_attempts() {
        let sem = Arc::new(Semaphore::new(1));
        let probe = MockProbe {
            delay: Duration::from_millis(10),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let count = 3;
        let period = Duration::from_millis(300); // spacing = 100ms
        let timeout = Duration::from_millis(50);

        let start = Instant::now();
        let (r1, r2) = tokio::join!(
            run_period(&probe, count, period, timeout, &sem),
            run_period(&probe, count, period, timeout, &sem),
        );
        let elapsed = start.elapsed();

        assert_eq!(r1.len(), 3);
        assert_eq!(r2.len(), 3);
        // Per-attempt permits: ~240ms expected. Old behaviour: ~460ms.
        // 400ms threshold leaves plenty of slack for CI jitter.
        assert!(
            elapsed < Duration::from_millis(400),
            "elapsed {elapsed:?} - permits seem to be held across the spacing sleep"
        );
    }

    /// `count == 1` skips the spacing sleep entirely and acquires exactly one
    /// permit.
    #[tokio::test]
    async fn single_attempt() {
        let sem = Arc::new(Semaphore::new(1));
        let calls = Arc::new(AtomicUsize::new(0));
        let probe = MockProbe {
            delay: Duration::from_millis(5),
            calls: calls.clone(),
        };

        let start = Instant::now();
        let obs = run_period(
            &probe,
            1,
            Duration::from_millis(300),
            Duration::from_millis(50),
            &sem,
        )
        .await;
        let elapsed = start.elapsed();

        assert_eq!(obs.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // No spacing sleep should have run.
        assert!(elapsed < Duration::from_millis(80), "elapsed {elapsed:?}");
    }

    /// With an empty pool, `acquire_owned` never resolves. We must NOT
    /// synthesise a `Loss` for permit contention - the call should pend.
    #[tokio::test]
    async fn blocks_when_pool_empty() {
        let sem = Arc::new(Semaphore::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let probe = MockProbe {
            delay: Duration::from_millis(10),
            calls: calls.clone(),
        };

        let res = tokio::time::timeout(
            Duration::from_millis(50),
            run_period(
                &probe,
                3,
                Duration::from_millis(300),
                Duration::from_millis(50),
                &sem,
            ),
        )
        .await;

        assert!(res.is_err(), "run_period must block; got {res:?}");
        // The probe was never reached because no permit was ever issued.
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
