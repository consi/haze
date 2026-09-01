//! Per-host probe scheduler.
//!
//! Owns one tokio task per enabled host. Each task ticks at the host's
//! `interval_secs` cadence, runs the configured probe `samples_per_period`
//! times, aggregates those observations into a single `Slot`, and writes it
//! to the host's `.hzc` chunk via `HzcStore`. Tasks are started with a random
//! initial jitter so a cluster of hosts don't all fire simultaneously.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU16, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use dashmap::DashMap;
use haze_store::{HzcStore, SampleEvent, SeriesStore, WorkerPools, aggregate};
use sqlx::SqlitePool;
use surge_ping::PingIdentifier;
use tokio::{
    sync::{Semaphore, broadcast, mpsc},
    task::JoinHandle,
    time::interval,
};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::{
    Probe, ProbeKind,
    dns::{DnsProbe, DnsResolvers},
    http::{HttpClients, HttpTotalProbe, HttpTtfbProbe},
    ping::{PingClients, PingProbe},
    run_period,
    tcp_connect::TcpConnectProbe,
    tls_connect::TlsConnectProbe,
};

/// Map a UUID to a deterministic phase inside `[0, window_ms)`. Even spread:
/// for N hosts the offsets approximate uniform distribution across the
/// window, so the scheduler doesn't get a thundering herd at second 0.
/// Stable across restarts because the input is just the UUID.
fn uuid_phase_offset(uuid: Uuid, window_ms: u64) -> u64 {
    let bytes = uuid.as_bytes();
    // Take the first 8 bytes as a u64; that's already a good pseudo-random
    // signal (Uuid v4 is mostly entropy) and avoids pulling in a hasher.
    let n = u64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]);
    n % window_ms
}

/// Pull the per-kind cap out of the settings-driven `WorkerPools`. The
/// scheduler builds one semaphore of this size per probe kind at boot;
/// changes to the settings take effect on the next restart.
fn cap_for(kind: ProbeKind, pools: &WorkerPools) -> usize {
    let v = match kind {
        ProbeKind::Ping => pools.probe_ping,
        ProbeKind::Dns => pools.probe_dns,
        ProbeKind::TcpConnect => pools.probe_tcp_connect,
        ProbeKind::TlsConnect => pools.probe_tls_connect,
        ProbeKind::HttpTtfb => pools.probe_http_ttfb,
        ProbeKind::HttpTotal => pools.probe_http_total,
    };
    v.max(1) as usize
}

#[derive(Debug, Clone)]
pub struct HostSpec {
    pub uuid: Uuid,
    pub probe_type: ProbeKind,
    pub probe_config: serde_json::Value,
    pub interval_secs: u32,
    pub samples_per_period: u32,
    /// HZC chunk window for this host. Per-host now (the global setting
    /// was removed because the existing-host case can't honour changes).
    pub chunk_window_secs: u32,
}

/// Commands the API sends to the scheduler when hosts are added/changed.
#[derive(Debug)]
pub enum SchedulerCmd {
    Add(HostSpec),
    Remove(Uuid),
    Restart(HostSpec),
    Shutdown,
}

pub struct Scheduler {
    store: Arc<HzcStore>,
    /// In-memory recent-slot ring buffer the alert evaluator reads.
    /// Probes append after each successful HZC write so the engine never
    /// has to touch disk on the hot path.
    series: Arc<SeriesStore>,
    pool: SqlitePool,
    /// Pool semaphore + the capacity it was created with. Tokio's
    /// `Semaphore::available_permits()` only tells us free slots, not the
    /// total, so we track the cap separately for utilisation reporting.
    semaphores: HashMap<ProbeKind, (Arc<Semaphore>, usize)>,
    handles: Arc<DashMap<Uuid, JoinHandle<()>>>,
    cmd_tx: mpsc::UnboundedSender<SchedulerCmd>,
    cmd_rx: Option<mpsc::UnboundedReceiver<SchedulerCmd>>,
    /// Optional fanout for the live replication SSE streams. When set,
    /// each successful sample write is broadcast on this channel so any
    /// active `/replication/slots/{id}/stream` consumer can forward it to
    /// the destination instance. `None` makes the broadcast a no-op (used
    /// in tests and for boots without the replication subsystem wired up).
    samples_tx: Option<broadcast::Sender<SampleEvent>>,
    /// One shared ICMP client per family. Avoids spawning a raw socket and
    /// reader task per host (which used to starve the executor with 1000+
    /// concurrent recv-tasks).
    ping_clients: Arc<PingClients>,
    /// Monotonic per-host ICMP identifier allocator. Wraps after 65 535
    /// hosts; collisions across long-lived hosts are harmless since
    /// surge-ping demuxes by `(addr, ident, seq)`.
    next_ping_id: Arc<AtomicU16>,
    /// Shared hickory resolvers, keyed by upstream. Each resolver owns a UDP
    /// socket + recv loop, so sharing avoids one-per-host fan-out.
    dns_resolvers: Arc<DnsResolvers>,
    /// Shared reqwest clients, keyed by (`verify_tls`, `follow_redirects`).
    /// At most four clients regardless of host count.
    http_clients: Arc<HttpClients>,
}

/// Snapshot of the scheduler's live worker utilisation. Used by the runtime
/// stats logger so operators can see how close we are to saturating each
/// per-probe pool.
#[derive(Debug)]
pub struct SchedulerStats {
    pub running_hosts: usize,
    pub pools: Vec<(ProbeKind, usize, usize)>,
}

impl Scheduler {
    pub fn new(
        store: Arc<HzcStore>,
        series: Arc<SeriesStore>,
        pool: SqlitePool,
        worker_pools: &WorkerPools,
    ) -> Self {
        let mut semaphores = HashMap::new();
        for kind in ProbeKind::ALL {
            let cap = cap_for(*kind, worker_pools);
            semaphores.insert(*kind, (Arc::new(Semaphore::new(cap)), cap));
        }
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        Self {
            store,
            series,
            pool,
            semaphores,
            handles: Arc::new(DashMap::new()),
            cmd_tx,
            cmd_rx: Some(cmd_rx),
            samples_tx: None,
            ping_clients: Arc::new(PingClients::new()),
            // Start at 1 - 0 is a valid identifier but conventionally avoided.
            next_ping_id: Arc::new(AtomicU16::new(1)),
            dns_resolvers: Arc::new(DnsResolvers::new()),
            http_clients: Arc::new(HttpClients::new()),
        }
    }

    /// Wire in the replication sample-fanout channel. Optional - if never
    /// called the broadcast is a no-op and the rest of the probe path runs
    /// unchanged. Call this once after `new` before `run`.
    #[must_use]
    pub fn with_samples_tx(mut self, tx: broadcast::Sender<SampleEvent>) -> Self {
        self.samples_tx = Some(tx);
        self
    }

    pub fn handle(&self) -> SchedulerHandle {
        SchedulerHandle {
            tx: self.cmd_tx.clone(),
            handles: self.handles.clone(),
            semaphores: self.semaphores.clone(),
        }
    }

    pub async fn run(mut self) -> Result<()> {
        let mut rx = self
            .cmd_rx
            .take()
            .context("scheduler already running (rx taken)")?;
        loop {
            let Some(cmd) = rx.recv().await else { break };
            match cmd {
                SchedulerCmd::Add(spec) => self.start_host(spec),
                SchedulerCmd::Remove(id) => self.stop_host(id),
                SchedulerCmd::Restart(spec) => {
                    self.stop_host(spec.uuid);
                    self.start_host(spec);
                }
                SchedulerCmd::Shutdown => {
                    info!("scheduler shutting down");
                    self.handles.iter().for_each(|h| h.abort());
                    self.handles.clear();
                    break;
                }
            }
        }
        Ok(())
    }

    fn start_host(&self, spec: HostSpec) {
        if self.handles.contains_key(&spec.uuid) {
            warn!(uuid = %spec.uuid, "scheduler asked to start a host that is already running");
            return;
        }
        let store = self.store.clone();
        let series = self.series.clone();
        let sem = self.semaphores[&spec.probe_type].0.clone();
        let handles = self.handles.clone();
        let ping_clients = self.ping_clients.clone();
        let ping_id = PingIdentifier(self.next_ping_id.fetch_add(1, Ordering::Relaxed));
        let dns_resolvers = self.dns_resolvers.clone();
        let http_clients = self.http_clients.clone();
        let samples_tx = self.samples_tx.clone();
        let id = spec.uuid;
        let handle = tokio::spawn(async move {
            if let Err(e) = host_loop(
                spec,
                store,
                series,
                sem,
                ping_clients,
                ping_id,
                dns_resolvers,
                http_clients,
                samples_tx,
            )
            .await
            {
                error!(uuid = %id, error = ?e, "host probe loop exited");
            }
            handles.remove(&id);
        });
        self.handles.insert(id, handle);
    }

    fn stop_host(&self, uuid: Uuid) {
        if let Some((_, h)) = self.handles.remove(&uuid) {
            h.abort();
        }
    }

    /// Load all enabled hosts from the DB and start them. Useful at boot.
    pub async fn bootstrap(&self) -> Result<()> {
        // Skip replication-owned hosts: they have no real probe config of
        // their own (we store a placeholder `{}`), their samples are
        // ingested by the replication worker via SSE/range pulls from
        // the source, and probing them locally would defeat the whole
        // point of cross-instance replication. They live in `hosts` only
        // so the tree + alerts + UI can reference them by UUID.
        let rows: Vec<BootstrapRow> = sqlx::query_as(
            "SELECT uuid, probe_type, probe_config, interval_secs, samples_per_period, \
                    chunk_window_secs \
             FROM hosts WHERE enabled = 1 AND replication_peer_id IS NULL",
        )
        .fetch_all(&self.pool)
        .await
        .context("loading hosts for scheduler bootstrap")?;

        for row in rows {
            let uuid = Uuid::from_slice(&row.uuid).context("invalid uuid in hosts row")?;
            let probe_type = parse_kind(&row.probe_type)?;
            let probe_config: serde_json::Value = serde_json::from_str(&row.probe_config)
                .with_context(|| format!("probe_config for {uuid}"))?;
            self.start_host(HostSpec {
                uuid,
                probe_type,
                probe_config,
                interval_secs: row.interval_secs as u32,
                samples_per_period: row.samples_per_period as u32,
                chunk_window_secs: row.chunk_window_secs as u32,
            });
        }
        info!(count = self.handles.len(), "scheduler bootstrapped");
        Ok(())
    }
}

#[derive(Clone)]
pub struct SchedulerHandle {
    tx: mpsc::UnboundedSender<SchedulerCmd>,
    /// Cloned from the scheduler so consumers (`stats()`, the periodic
    /// runtime logger) can introspect live state without going through
    /// the command channel.
    handles: Arc<DashMap<Uuid, JoinHandle<()>>>,
    semaphores: HashMap<ProbeKind, (Arc<Semaphore>, usize)>,
}

impl SchedulerHandle {
    pub fn add(&self, spec: HostSpec) {
        let _ = self.tx.send(SchedulerCmd::Add(spec));
    }
    pub fn remove(&self, uuid: Uuid) {
        let _ = self.tx.send(SchedulerCmd::Remove(uuid));
    }
    pub fn restart(&self, spec: HostSpec) {
        let _ = self.tx.send(SchedulerCmd::Restart(spec));
    }
    pub fn shutdown(&self) {
        let _ = self.tx.send(SchedulerCmd::Shutdown);
    }

    /// Live snapshot: number of running host loops + per-kind pool
    /// utilisation as `(kind, in_use, capacity)` tuples. `in_use` is the
    /// number of probe attempts currently holding a permit (i.e. in-flight
    /// I/O), not the number of active host loops - host count can exceed
    /// pool capacity since permits are scoped per attempt.
    pub fn stats(&self) -> SchedulerStats {
        let pools = self
            .semaphores
            .iter()
            .map(|(kind, (sem, cap))| {
                let used = cap.saturating_sub(sem.available_permits());
                (*kind, used, *cap)
            })
            .collect();
        SchedulerStats {
            running_hosts: self.handles.len(),
            pools,
        }
    }
}

#[derive(sqlx::FromRow)]
struct BootstrapRow {
    uuid: Vec<u8>,
    probe_type: String,
    probe_config: String,
    interval_secs: i64,
    samples_per_period: i64,
    chunk_window_secs: i64,
}

fn parse_kind(s: &str) -> Result<ProbeKind> {
    Ok(match s {
        "ping" => ProbeKind::Ping,
        "dns" => ProbeKind::Dns,
        "tcp_connect" => ProbeKind::TcpConnect,
        "tls_connect" => ProbeKind::TlsConnect,
        "http_ttfb" => ProbeKind::HttpTtfb,
        "http_total" => ProbeKind::HttpTotal,
        other => anyhow::bail!("unknown probe_type '{other}'"),
    })
}

async fn build_probe(
    kind: ProbeKind,
    cfg: &serde_json::Value,
    ping_clients: &PingClients,
    ping_id: PingIdentifier,
    dns_resolvers: &DnsResolvers,
    http_clients: &HttpClients,
) -> Result<Box<dyn Probe>, crate::ProbeError> {
    Ok(match kind {
        ProbeKind::Ping => Box::new(PingProbe::new(cfg, ping_clients, ping_id).await?),
        ProbeKind::Dns => Box::new(DnsProbe::new(cfg, dns_resolvers)?),
        ProbeKind::TcpConnect => Box::new(TcpConnectProbe::new(cfg)?),
        ProbeKind::TlsConnect => Box::new(TlsConnectProbe::new(cfg)?),
        ProbeKind::HttpTtfb => Box::new(HttpTtfbProbe::new(cfg, http_clients)?),
        ProbeKind::HttpTotal => Box::new(HttpTotalProbe::new(cfg, http_clients)?),
    })
}

#[allow(clippy::too_many_arguments)]
async fn host_loop(
    spec: HostSpec,
    store: Arc<HzcStore>,
    series: Arc<SeriesStore>,
    sem: Arc<Semaphore>,
    ping_clients: Arc<PingClients>,
    ping_id: PingIdentifier,
    dns_resolvers: Arc<DnsResolvers>,
    http_clients: Arc<HttpClients>,
    samples_tx: Option<broadcast::Sender<SampleEvent>>,
) -> Result<()> {
    let HostSpec {
        uuid,
        probe_type,
        probe_config,
        interval_secs,
        samples_per_period,
        chunk_window_secs,
    } = spec;

    let probe = build_probe(
        probe_type,
        &probe_config,
        &ping_clients,
        ping_id,
        &dns_resolvers,
        &http_clients,
    )
    .await
    .with_context(|| {
        format!(
            "building probe for host {uuid} (kind={})",
            probe_type.as_str()
        )
    })?;
    info!(uuid = %uuid, kind = probe_type.as_str(), interval_secs, samples_per_period, "host loop starting");

    // Spread the first probe across the period so 10 k hosts on the same
    // cadence don't all fire at the same instant. The host's UUID provides
    // a deterministic phase (perfectly even spread, stable across restarts);
    // a small random jitter on top breaks clock-second alignment so workers
    // don't burst at integer boundaries.
    let window_ms = u64::from(interval_secs) * 1000;
    let phase_ms = if window_ms > 0 {
        uuid_phase_offset(uuid, window_ms)
    } else {
        0
    };
    let jitter_ms = rand::random_range(0..200u64);
    let start_delay = phase_ms
        .saturating_add(jitter_ms)
        .min(window_ms.saturating_sub(1));
    tokio::time::sleep(Duration::from_millis(start_delay)).await;

    let period = Duration::from_secs(u64::from(interval_secs));
    let attempt_timeout = period.mul_f32(0.75) / samples_per_period.max(1);

    let mut tick = interval(period);
    tick.tick().await;

    loop {
        let ts = chrono::Utc::now().timestamp();
        // The semaphore is acquired per attempt inside `run_period`, not
        // for the whole period - so host count can exceed pool capacity.
        let observations = run_period(
            probe.as_ref(),
            samples_per_period,
            period,
            attempt_timeout,
            &sem,
        )
        .await;
        let slot = aggregate(&observations);

        // Storage errors must never kill the host loop. Losing one period's
        // sample is recoverable; losing the loop until daemon restart is not.
        match store.writer(uuid, interval_secs, chunk_window_secs) {
            Ok(writer) => match writer.write_sample(ts, slot) {
                Ok(()) => {
                    debug!(uuid = %uuid, ts, median = ?slot.median, loss = ?slot.loss_pct, "wrote probe slot");
                    // Mirror into the in-memory series so the alert evaluator
                    // reads from RAM instead of replaying chunks on every tick.
                    series.append(uuid, ts, slot);
                    // Fan out to any live replication SSE streams. `send`
                    // is fire-and-forget; an `Err` only means there are no
                    // subscribers right now (the common case) or the
                    // channel buffer is full (lagged receivers reconnect
                    // via catch-up). Ignored on purpose.
                    if let Some(tx) = samples_tx.as_ref() {
                        let _ = tx.send(SampleEvent {
                            host_uuid: uuid,
                            timestamp_secs: ts,
                            slot,
                        });
                    }
                }
                Err(e) => warn!(uuid = %uuid, error = %e, "write_sample failed"),
            },
            Err(e) => warn!(uuid = %uuid, error = %e, "store.writer() failed; skipping period"),
        }

        tick.tick().await;
    }
}
