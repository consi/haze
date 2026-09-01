use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use axum::{Router, routing::get};
use dashmap::DashMap;
use haze_auth::PasskeyService;
use haze_probe::scheduler::Scheduler;
use haze_store::{HzcStore, SeriesStore, hzc::compactor, repo::settings};
use tower::Layer;
use tower_http::{
    compression::CompressionLayer, normalize_path::NormalizePathLayer, trace::TraceLayer,
};
use uuid::Uuid;

/// Per-host mutex map shared between the downsampling compactor scheduler
/// and the daily-rollup scheduler. Both acquire the per-host mutex before
/// mutating that host's `chunks/` directory, so they never race on the same
/// host. Across hosts they run independently.
type HostLocks = Arc<DashMap<Uuid, Arc<std::sync::Mutex<()>>>>;

fn host_lock(locks: &HostLocks, uuid: Uuid) -> Arc<std::sync::Mutex<()>> {
    locks
        .entry(uuid)
        .or_insert_with(|| Arc::new(std::sync::Mutex::new(())))
        .clone()
}

mod assets;
mod replication;

pub struct Config {
    pub bind: String,
    pub data_dir: PathBuf,
    /// Origin URL the browser sees (e.g. `https://haze.example.com`). Used for
    /// `WebAuthn` passkey ceremonies. If `None`, passkeys are disabled.
    pub origin: Option<String>,
    /// URL path prefix to deploy under, e.g. `/haze`. Empty string means
    /// the app is served at root `/`. Normalized inside `run()`.
    pub base_url: String,
}

/// Normalize `HAZE_BASE_URL`/`--base-url` into a canonical form:
/// - empty / `/` / whitespace → `""` (root mode, byte-identical to today)
/// - `foo` → `/foo`
/// - `/foo/` → `/foo`
/// - anything containing `://`, `?`, or `#` is rejected as it's not a
///   path prefix.
fn normalize_base(raw: &str) -> Result<String> {
    let s = raw.trim();
    if s.is_empty() || s == "/" {
        return Ok(String::new());
    }
    if s.contains("://") || s.contains('?') || s.contains('#') {
        anyhow::bail!(
            "HAZE_BASE_URL must be a URL path prefix (e.g. /haze), not a full URL: {raw:?}"
        );
    }
    let with_lead = if s.starts_with('/') {
        s.to_string()
    } else {
        format!("/{s}")
    };
    Ok(with_lead.trim_end_matches('/').to_string())
}

pub async fn run(cfg: Config) -> Result<()> {
    let base_url = normalize_base(&cfg.base_url)?;
    if base_url.is_empty() {
        tracing::info!("serving at root path /");
    } else {
        tracing::info!(base_url = %base_url, "serving under sub-path");
    }
    // Install rustls's `ring` crypto provider as the process-level default.
    // Required by rustls 0.23+: any consumer (tokio-rustls in the TLS-CONNECT
    // probe, reqwest in the HTTP probes, webauthn-rs's signature verification)
    // will otherwise panic with "Could not automatically determine the
    // process-level CryptoProvider".
    if rustls::crypto::ring::default_provider()
        .install_default()
        .is_err()
    {
        tracing::debug!("rustls crypto provider already installed");
    }

    tokio::fs::create_dir_all(&cfg.data_dir)
        .await
        .with_context(|| format!("creating data dir {}", cfg.data_dir.display()))?;

    haze_store::migrate(&cfg.data_dir).await?;
    let pool = haze_store::open_pool(&cfg.data_dir).await?;
    let hzc = Arc::new(HzcStore::new(&cfg.data_dir).context("opening hzc store")?);

    // Empty install: provision an admin account so the operator can sign
    // in. The plaintext is printed exactly once - they're expected to copy
    // it from the log and then either keep it or rotate it via /settings.
    ensure_bootstrap_admin(&pool).await?;

    let worker_pools = settings::worker_pools(&pool)
        .await
        .unwrap_or_else(|_| haze_store::default_worker_pools());
    tracing::info!(?worker_pools, "worker pool sizes loaded from settings");

    // In-memory series buffer the alert engine reads. Probes append into
    // it as they write to HZC; rehydrate from the periodic snapshot so a
    // warm restart doesn't have to wait out a full window of cold probes.
    let series = Arc::new(SeriesStore::new());
    let max_rule_window_secs = max_rule_window_secs(&pool).await;
    if let Err(e) = haze_alert::restore(
        &pool,
        &series,
        max_rule_window_secs,
        chrono::Utc::now().timestamp(),
    )
    .await
    {
        tracing::warn!(error = ?e, "alert series snapshot restore failed; starting cold");
    }

    // Built early so the probe scheduler can publish per-sample events to
    // it after each successful write. 4096 absorbs bursts; lagged
    // subscribers (replication SSE streams that fall behind) get `Lagged`
    // and reconnect through the catch-up path - no loss.
    let (samples_tx, _) = tokio::sync::broadcast::channel(4096);
    let scheduler = Scheduler::new(hzc.clone(), series.clone(), pool.clone(), &worker_pools)
        .with_samples_tx(samples_tx.clone());
    let scheduler_handle = scheduler.handle();
    scheduler.bootstrap().await.context("scheduler bootstrap")?;
    tokio::spawn(async move {
        if let Err(e) = scheduler.run().await {
            tracing::error!(error = ?e, "scheduler exited");
        }
    });

    // Periodic runtime-stats logger. Emits a single INFO line every 30 s
    // with the number of running host loops and per-pool utilisation so
    // operators can see at a glance when a pool is saturating. Cheap -
    // just inspects in-memory counters / semaphore permits.
    let replication_pool = haze_api::ReplicationPool::new(worker_pools.replication.max(1) as usize);
    {
        let handle = scheduler_handle.clone();
        let hzc_for_stats = hzc.clone();
        let replication_pool = replication_pool.clone();
        let tokio_workers =
            std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            tick.tick().await;
            loop {
                tick.tick().await;
                let stats = handle.stats();
                let hzc_hosts = hzc_for_stats.list_hosts().map_or(0, |h| h.len());
                let mut pools = stats
                    .pools
                    .iter()
                    .map(|(k, used, cap)| format!("{}={}/{}", k.as_str(), used, cap))
                    .collect::<Vec<_>>();
                pools.push(format!(
                    "replication={}/{}",
                    replication_pool.in_use(),
                    replication_pool.capacity,
                ));
                let pools = pools.join(" ");
                tracing::info!(
                    tokio_workers,
                    running_hosts = stats.running_hosts,
                    hzc_writers = hzc_hosts,
                    pools = %pools,
                    "runtime stats"
                );
            }
        });
    }
    tokio::spawn(haze_auth::sessions::run_cleanup_task(pool.clone()));

    // 64 slots is enough headroom for bursty mutations (multi-host imports,
    // alert state churn) without keeping much memory around - receivers
    // that lag past 64 events get a single "refetch all" notice and
    // continue. The actual values are tiny (`ChangeKind` is a copy enum).
    let (events_tx, _) = tokio::sync::broadcast::channel(64);
    // Stable per-process identity. Generated lazily by `instance_uuid` if
    // the row was missing (e.g. first boot post-migration).
    let instance_uuid = settings::instance_uuid(&pool)
        .await
        .context("loading instance uuid")?;
    tracing::info!(%instance_uuid, "instance identity loaded");

    // Replication supervisor: starts/stops a worker per enabled rule.
    // Workers acquire permits from `replication_pool`, so a misconfigured
    // peer can't starve probe scheduling. Logs every state transition at
    // INFO with `rule_uuid` so an admin can follow one rule end-to-end.
    replication::run_manager(
        pool.clone(),
        hzc.clone(),
        cfg.data_dir.clone(),
        instance_uuid,
        replication_pool.clone(),
        events_tx.clone(),
        samples_tx.clone(),
    );

    // Per-host mutex shared between the downsampling compactor and the
    // single-threaded daily-rollup task so they never race on the same host.
    let host_locks: HostLocks = Arc::new(DashMap::new());

    // Compactor: walk every host on a settings-driven cadence, aggregating
    // chunks per the current retention tiers. Both the cadence and the tiers
    // are re-read every few seconds so changes from the settings UI apply
    // without a restart - including shortening the interval while a previous
    // long sleep would otherwise still be in flight. Host compactions run
    // in parallel, bounded by `worker_pools.compactor`.
    {
        let pool = pool.clone();
        let data_dir = cfg.data_dir.clone();
        let hzc = hzc.clone();
        let host_locks = host_locks.clone();
        let compactor_parallel = worker_pools.compactor.max(1) as usize;
        tokio::spawn(async move {
            // How often we re-evaluate "has enough time passed since the
            // last compactor run?" Bounded so a settings change is noticed
            // within this many seconds rather than waiting out a prior
            // long sleep.
            let poll_tick = Duration::from_secs(5);
            let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(compactor_parallel));
            let mut last_run = tokio::time::Instant::now();
            loop {
                tokio::time::sleep(poll_tick).await;
                let interval = settings::compactor_interval_secs(&pool)
                    .await
                    .unwrap_or(3_600);
                if last_run.elapsed() < Duration::from_secs(u64::from(interval)) {
                    continue;
                }
                last_run = tokio::time::Instant::now();

                let tiers = match settings::retention_tiers(&pool).await {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(error = ?e, "compactor: failed to load retention tiers");
                        continue;
                    }
                };
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs() as i64);
                let hosts = hzc.list_hosts().unwrap_or_default();
                let host_count = hosts.len();
                let run_started = std::time::Instant::now();
                tracing::info!(
                    host_count,
                    parallel = compactor_parallel,
                    "compactor run started"
                );
                let mut handles = Vec::new();
                for uuid in hosts {
                    let permit = semaphore.clone().acquire_owned().await.unwrap();
                    let data_dir = data_dir.clone();
                    let tiers = tiers.clone();
                    let host_lock_ref = host_lock(&host_locks, uuid);
                    handles.push(tokio::task::spawn_blocking(move || {
                        // Serialize against the rollup task on the same host.
                        let _g = host_lock_ref
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let result = compactor::compact_host(&data_dir, uuid, &tiers, now);
                        drop(permit);
                        (uuid, result)
                    }));
                }
                let mut total_aggregated = 0usize;
                let mut total_deleted = 0usize;
                let mut total_consumed = 0usize;
                let mut failed_hosts = 0usize;
                for h in handles {
                    if let Ok((uuid, result)) = h.await {
                        match result {
                            Ok(r) => {
                                total_aggregated += r.aggregated_chunks;
                                total_deleted += r.deleted_chunks;
                                total_consumed += r.source_chunks_consumed;
                                if r.aggregated_chunks > 0 || r.deleted_chunks > 0 {
                                    tracing::debug!(
                                        %uuid,
                                        aggregated = r.aggregated_chunks,
                                        deleted = r.deleted_chunks,
                                        "compactor host done"
                                    );
                                }
                            }
                            Err(e) => {
                                failed_hosts += 1;
                                tracing::warn!(%uuid, error = ?e, "compactor host failed");
                            }
                        }
                    }
                }
                tracing::info!(
                    host_count,
                    aggregated_chunks = total_aggregated,
                    deleted_chunks = total_deleted,
                    source_chunks_consumed = total_consumed,
                    failed_hosts,
                    duration_ms = run_started.elapsed().as_millis() as u64,
                    "compactor run finished"
                );
            }
        });
    }

    // Daily-rollup task: single-threaded, walks every host sequentially on a
    // settings-driven cadence (default 10 min), bundles every per-window
    // chunk for a fully-settled UTC day into one zstd file, then deletes
    // the sources. Settle margin (default 1 h past UTC midnight) and the
    // inter-host pause are re-read live so changes from the settings UI
    // apply without a restart. Serializes per-host against the parallel
    // downsampling compactor via `host_locks`.
    {
        let pool = pool.clone();
        let data_dir = cfg.data_dir.clone();
        let hzc = hzc.clone();
        let host_locks = host_locks.clone();
        tokio::spawn(async move {
            let poll_tick = Duration::from_secs(5);
            let mut last_run = tokio::time::Instant::now();
            loop {
                tokio::time::sleep(poll_tick).await;
                let interval = settings::rollup_interval_secs(&pool)
                    .await
                    .unwrap_or(settings::DEFAULT_ROLLUP_INTERVAL_SECS);
                if last_run.elapsed() < Duration::from_secs(u64::from(interval)) {
                    continue;
                }
                last_run = tokio::time::Instant::now();

                let settled_after = settings::rollup_settled_after_secs(&pool)
                    .await
                    .unwrap_or(settings::DEFAULT_ROLLUP_SETTLED_AFTER_SECS);
                let settled_after_g2 = settings::rollup_g2_settled_after_secs(&pool)
                    .await
                    .unwrap_or(settings::DEFAULT_ROLLUP_G2_SETTLED_AFTER_SECS);
                let settled_after_g3 = settings::rollup_g3_settled_after_secs(&pool)
                    .await
                    .unwrap_or(settings::DEFAULT_ROLLUP_G3_SETTLED_AFTER_SECS);
                let pause_ms = settings::rollup_inter_host_pause_ms(&pool)
                    .await
                    .unwrap_or(settings::DEFAULT_ROLLUP_INTER_HOST_PAUSE_MS);
                // The G2/G3 tier-finality gate must see the same retention
                // tiers the compactor applies. If they can't be loaded,
                // skip the G2/G3 phases this pass (G1 needs no tiers).
                let tiers = match settings::retention_tiers(&pool).await {
                    Ok(t) => Some(t),
                    Err(e) => {
                        tracing::warn!(error = ?e, "rollup: failed to load retention tiers; skipping G2/G3 this pass");
                        None
                    }
                };
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs() as i64);
                let hosts = hzc.list_hosts().unwrap_or_default();
                let host_count = hosts.len();
                let run_started = std::time::Instant::now();
                tracing::info!(host_count, "rollup pass started");

                let data_dir_inner = data_dir.clone();
                let host_locks_inner = host_locks.clone();
                let join = tokio::task::spawn_blocking(move || {
                    let mut totals = (
                        0usize, 0usize, 0usize, 0usize, 0u64, 0u64, 0usize, 0usize, 0usize,
                    );
                    // (g1_bundled_days, g1_source_chunks, migrated, quarantined,
                    //  bytes_before, bytes_after, failed_hosts,
                    //  g2_bundled_months, g3_bundled_years)
                    let settled_after_i64 = i64::from(settled_after);
                    let settled_after_g2_i64 = i64::from(settled_after_g2);
                    let settled_after_g3_i64 = i64::from(settled_after_g3);
                    let pause = std::time::Duration::from_millis(u64::from(pause_ms));
                    for uuid in hosts {
                        let lock = host_lock(&host_locks_inner, uuid);
                        let guard = lock
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let host_started = std::time::Instant::now();
                        // G1 daily.
                        match haze_store::rollup_host(&data_dir_inner, uuid, now, settled_after_i64)
                        {
                            Ok((migration, rollup)) => {
                                totals.0 += rollup.bundled_days;
                                totals.1 += rollup.source_chunks_consumed;
                                totals.2 += migration.renamed;
                                totals.3 += migration.quarantined;
                                totals.4 += rollup.bytes_before;
                                totals.5 += rollup.bytes_after;
                                if rollup.did_work() || migration.touched_anything() {
                                    tracing::info!(
                                        %uuid,
                                        bundled_days = rollup.bundled_days,
                                        source_chunks = rollup.source_chunks_consumed,
                                        migrated = migration.renamed,
                                        quarantined = migration.quarantined,
                                        tmp_removed = migration.tmp_removed,
                                        elapsed_ms = host_started.elapsed().as_millis() as u64,
                                        "rollup host G1 done"
                                    );
                                }
                            }
                            Err(e) => {
                                totals.6 += 1;
                                tracing::warn!(%uuid, error = ?e, "rollup host G1 failed");
                                drop(guard);
                                if !pause.is_zero() {
                                    std::thread::sleep(pause);
                                }
                                continue;
                            }
                        }
                        // G2 monthly.
                        let Some(tiers) = &tiers else {
                            drop(guard);
                            if !pause.is_zero() {
                                std::thread::sleep(pause);
                            }
                            continue;
                        };
                        match haze_store::rollup_g2_host(
                            &data_dir_inner,
                            uuid,
                            now,
                            settled_after_g2_i64,
                            tiers,
                        ) {
                            Ok(rollup) => {
                                totals.7 += rollup.bundled_days;
                                totals.1 += rollup.source_chunks_consumed;
                                totals.4 += rollup.bytes_before;
                                totals.5 += rollup.bytes_after;
                                if rollup.did_work() {
                                    tracing::info!(
                                        %uuid,
                                        bundled_months = rollup.bundled_days,
                                        source_chunks = rollup.source_chunks_consumed,
                                        "rollup host G2 done"
                                    );
                                }
                            }
                            Err(e) => {
                                totals.6 += 1;
                                tracing::warn!(%uuid, error = ?e, "rollup host G2 failed");
                            }
                        }
                        // G3 yearly.
                        match haze_store::rollup_g3_host(
                            &data_dir_inner,
                            uuid,
                            now,
                            settled_after_g3_i64,
                            tiers,
                        ) {
                            Ok(rollup) => {
                                totals.8 += rollup.bundled_days;
                                totals.1 += rollup.source_chunks_consumed;
                                totals.4 += rollup.bytes_before;
                                totals.5 += rollup.bytes_after;
                                if rollup.did_work() {
                                    tracing::info!(
                                        %uuid,
                                        bundled_years = rollup.bundled_days,
                                        source_chunks = rollup.source_chunks_consumed,
                                        "rollup host G3 done"
                                    );
                                }
                            }
                            Err(e) => {
                                totals.6 += 1;
                                tracing::warn!(%uuid, error = ?e, "rollup host G3 failed");
                            }
                        }
                        drop(guard);
                        if !pause.is_zero() {
                            std::thread::sleep(pause);
                        }
                    }
                    totals
                });

                match join.await {
                    Ok((
                        bundled_days,
                        source_chunks,
                        migrated,
                        quarantined,
                        bytes_before,
                        bytes_after,
                        failed_hosts,
                        bundled_months,
                        bundled_years,
                    )) => {
                        let elapsed_ms = run_started.elapsed().as_millis() as u64;
                        if elapsed_ms > u64::from(interval) * 1_000 {
                            tracing::warn!(
                                elapsed_ms,
                                interval_secs = interval,
                                "rollup pass overran interval"
                            );
                        }
                        tracing::info!(
                            host_count,
                            bundled_days,
                            bundled_months,
                            bundled_years,
                            source_chunks,
                            migrated,
                            quarantined,
                            bytes_before,
                            bytes_after,
                            failed_hosts,
                            elapsed_ms,
                            "rollup pass complete"
                        );
                    }
                    Err(e) => {
                        tracing::error!(error = ?e, "rollup task join failed");
                    }
                }
            }
        });
    }

    // Alert engine - single tokio task polling every minute. Rule
    // evaluations fan out across `alert_eval` workers and read every
    // sample from the in-memory series store. Persisting the in-memory
    // buffer to SQLite happens in a separate jittered flush task so a
    // restart doesn't lose the warm window.
    let alerts = haze_alert::AlertEngine::new(
        pool.clone(),
        series.clone(),
        worker_pools.alert_eval.max(1) as usize,
    )
    .await
    .context("loading alert state cache")?;
    alerts.run();
    haze_alert::run_flush(pool.clone(), series.clone(), None);

    let passkeys = if let Some(origin) = &cfg.origin {
        match passkey_service_from_origin(origin) {
            Ok(svc) => Some(svc),
            Err(e) => {
                tracing::warn!(error = ?e, origin, "passkey service disabled");
                None
            }
        }
    } else {
        tracing::info!("HAZE_ORIGIN not set; passkeys disabled");
        None
    };

    // Shared shutdown notify: woken by `shutdown_signal()` before it
    // returns so the SSE handlers in haze-api can exit their `recv().await`
    // and let axum's graceful shutdown drain. Without this, an open
    // browser EventSource pins the server alive until the kill timeout.
    let shutdown = Arc::new(tokio::sync::Notify::new());
    // Anonymous-traffic rate limiter, sized from the stored public-mode
    // settings (defaults if missing). Hot-swapped when an admin saves
    // new limits via /api/v1/settings/public.
    let public_settings = settings::public_mode_settings(&pool)
        .await
        .unwrap_or_else(|_| haze_store::default_public_mode_settings());
    let limiters = haze_api::new_handle(&public_settings);
    let sse_per_ip = haze_api::new_sse_map();
    let app = build_app(
        haze_api::AppState {
            pool,
            hzc,
            data_dir: cfg.data_dir.clone(),
            scheduler: scheduler_handle,
            passkeys,
            series,
            events: events_tx,
            samples: samples_tx,
            instance_uuid,
            replication_pool,
            shutdown: shutdown.clone(),
            cookie_path: base_url.clone(),
            limiters,
            sse_per_ip,
        },
        &base_url,
    );

    let addr: SocketAddr = cfg.bind.parse().context("invalid --bind address")?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, "listening");

    // ConnectInfo<SocketAddr> is what the rate-limit middleware reads for
    // the per-IP key, so the listener must be served with connect-info
    // propagation (not the plain `into_make_service()` default).
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(shutdown))
    .await
    .context("axum serve failed")
}

fn build_app(state: haze_api::AppState, base: &str) -> axum::Router {
    let api = haze_api::api_router(state);
    let base_for_handler = base.to_owned();
    let assets_handler = move |req| assets::handler(req, base_for_handler.clone());

    // Inner router: API + asset fallback. When a base is configured this
    // whole router is nested under it (`${base}/api/...`, `${base}/...`),
    // and `/healthz` is also mounted inside so a reverse-proxied probe
    // works. In root mode the inner router is merged directly and
    // `/healthz` lives only at the top level (avoiding an overlap).
    //
    // CompressionLayer is applied here (covers /api and the asset
    // fallback). The asset handler always rewrites the `__HAZE_BASE__`
    // placeholder baked in by SvelteKit's `kit.paths.base`, so the build
    // step's pre-compressed `.br`/`.gz` siblings can't be served as-is
    // and we rely on on-the-fly compression instead.
    let mut inner = Router::new().nest("/api", api).fallback(assets_handler);
    if !base.is_empty() {
        // Mirror /healthz under the sub-path so the frontend's
        // restart-recovery poll (which uses `${base}/healthz`) and any
        // reverse-proxied health checks reach the same handler.
        inner = inner.route("/healthz", get(healthz));
    }
    let inner = inner.layer(CompressionLayer::new());

    let mounted = if base.is_empty() {
        inner
    } else {
        Router::new().nest(base, inner)
    };

    let router = Router::new()
        // `/healthz` is always reachable at the root path regardless of
        // `HAZE_BASE_URL` so container/k8s liveness probes don't need to
        // know about the deployment sub-path. When a base is set, the
        // same probe is also mounted under it (see `inner` above) so the
        // reverse-proxied frontend's restart poll can reach it.
        .route("/healthz", get(healthz))
        .merge(mounted)
        .layer(TraceLayer::new_for_http());
    // NormalizePathLayer trims trailing slashes before routing so `/api/v1/groups/`
    // matches the same route as `/api/v1/groups`. Applied outside the router so
    // the rewrite happens before route matching.
    Router::new().fallback_service(NormalizePathLayer::trim_trailing_slash().layer(router))
}

async fn healthz() -> &'static str {
    "ok"
}

/// Longest window across enabled alert rules, in seconds.
///
/// Used by the boot-time series restore to decide which snapshots are
/// fresh enough to load: anything whose newest sample predates
/// `now - max_rule_window_secs` would be acting on stale data. Falls back
/// to one hour if no rules exist or the query fails.
async fn max_rule_window_secs(pool: &sqlx::SqlitePool) -> i64 {
    const DEFAULT_SECS: i64 = 3600;
    let row: Result<Option<(Option<i64>,)>, _> =
        sqlx::query_as("SELECT MAX(window_secs) FROM alert_rules WHERE enabled = 1")
            .fetch_optional(pool)
            .await;
    match row {
        Ok(Some((Some(secs),))) if secs > 0 => secs,
        Ok(_) => DEFAULT_SECS,
        Err(e) => {
            tracing::warn!(error = ?e, "max_rule_window_secs query failed; using default");
            DEFAULT_SECS
        }
    }
}

async fn shutdown_signal(shutdown: Arc<tokio::sync::Notify>) {
    use tokio::signal::unix::{SignalKind, signal};

    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let term = async {
        match signal(SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                // If the SIGTERM handler can't be installed (e.g. the process
                // somehow lacks the capability), fall back to ctrl_c-only by
                // pending forever on this arm of the select.
                tracing::warn!(error = ?e, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };
    // PID 1 in distroless: Docker/Kubernetes deliver SIGTERM on `stop` and
    // graceful shutdown - without this arm haze would ignore it and get
    // hard-killed at the grace timeout. If we ever spawn child processes,
    // PID 1 will also need a SIGCHLD reaper (or `tini` in the Dockerfile).
    tokio::select! {
        () = ctrl_c => tracing::info!("shutdown: SIGINT"),
        () = term => tracing::info!("shutdown: SIGTERM"),
    }
    // Wake every long-lived response handler (SSE) before axum starts its
    // drain. Without this, an idle browser tab's `/events` connection
    // holds the response stream open forever and axum::serve never
    // returns - Ctrl-C "stalls" for the user.
    shutdown.notify_waiters();
}

/// First-boot admin provisioning. If the users table is empty, create an
/// `admin` user with a random 16-character password (URL-safe alphabet) and
/// log the plaintext exactly once at INFO so the operator can sign in.
async fn ensure_bootstrap_admin(pool: &sqlx::SqlitePool) -> Result<()> {
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await
        .context("counting users")?;
    if count > 0 {
        return Ok(());
    }

    let env_pw = std::env::var("HAZE_BOOTSTRAP_PASSWORD")
        .ok()
        .filter(|s| !s.is_empty());
    let from_env = env_pw.is_some();
    let password = env_pw.unwrap_or_else(|| generate_password(16));
    let hash = haze_auth::password::hash(&password).context("hashing bootstrap password")?;
    haze_auth::user::create(pool, "admin", Some(&hash), haze_auth::user::Role::Admin)
        .await
        .context("creating bootstrap admin")?;

    tracing::info!("");
    tracing::info!("==============================================================");
    tracing::info!("  Bootstrap admin user created (empty database on first boot)");
    tracing::info!("    username: admin");
    if from_env {
        tracing::info!("    password: (taken from HAZE_BOOTSTRAP_PASSWORD env var; not logged)");
    } else {
        tracing::info!("    password: {password}");
    }
    tracing::info!("  Sign in, then change the password via Settings -> Users.");
    tracing::info!("  This message will not appear again.");
    tracing::info!("==============================================================");
    tracing::info!("");
    Ok(())
}

/// Crypto-safe password from a URL-safe alphabet. 16 chars over a 64-symbol
/// set is ~96 bits of entropy, plenty for a one-off bootstrap secret.
fn generate_password(len: usize) -> String {
    const CHARSET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789-_";
    (0..len)
        .map(|_| char::from(CHARSET[rand::random_range(0..CHARSET.len())]))
        .collect()
}

fn passkey_service_from_origin(origin: &str) -> Result<std::sync::Arc<PasskeyService>> {
    let parsed = url::Url::parse(origin).context("parsing HAZE_ORIGIN")?;
    let rp_id = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("HAZE_ORIGIN has no host"))?
        .to_owned();
    PasskeyService::new(&rp_id, "Haze", origin).map_err(|e| anyhow::anyhow!("passkey service: {e}"))
}
