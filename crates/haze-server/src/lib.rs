use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use axum::{Router, routing::get};
use haze_auth::PasskeyService;
use haze_probe::scheduler::Scheduler;
use haze_store::{HzcStore, SeriesStore, hzc::compactor, repo::settings};
use tower::Layer;
use tower_http::{
    compression::CompressionLayer, normalize_path::NormalizePathLayer, trace::TraceLayer,
};

mod assets;

pub struct Config {
    pub bind: String,
    pub data_dir: PathBuf,
    /// Origin URL the browser sees (e.g. `https://haze.example.com`). Used for
    /// `WebAuthn` passkey ceremonies. If `None`, passkeys are disabled.
    pub origin: Option<String>,
}

pub async fn run(cfg: Config) -> Result<()> {
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

    let scheduler = Scheduler::new(hzc.clone(), series.clone(), pool.clone(), &worker_pools);
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
    {
        let handle = scheduler_handle.clone();
        let hzc_for_stats = hzc.clone();
        let tokio_workers =
            std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            tick.tick().await;
            loop {
                tick.tick().await;
                let stats = handle.stats();
                let hzc_hosts = hzc_for_stats.list_hosts().map_or(0, |h| h.len());
                let pools = stats
                    .pools
                    .iter()
                    .map(|(k, used, cap)| format!("{}={}/{}", k.as_str(), used, cap))
                    .collect::<Vec<_>>()
                    .join(" ");
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
                    handles.push(tokio::task::spawn_blocking(move || {
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

    // 64 slots is enough headroom for bursty mutations (multi-host imports,
    // alert state churn) without keeping much memory around — receivers
    // that lag past 64 events get a single "refetch all" notice and
    // continue. The actual values are tiny (`ChangeKind` is a copy enum).
    let (events_tx, _) = tokio::sync::broadcast::channel(64);
    // Shared shutdown notify: woken by `shutdown_signal()` before it
    // returns so the SSE handlers in haze-api can exit their `recv().await`
    // and let axum's graceful shutdown drain. Without this, an open
    // browser EventSource pins the server alive until the kill timeout.
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let app = build_app(haze_api::AppState {
        pool,
        hzc,
        data_dir: cfg.data_dir.clone(),
        scheduler: scheduler_handle,
        passkeys,
        series,
        events: events_tx,
        shutdown: shutdown.clone(),
    });

    let addr: SocketAddr = cfg.bind.parse().context("invalid --bind address")?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, "listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown))
        .await
        .context("axum serve failed")
}

fn build_app(state: haze_api::AppState) -> axum::Router {
    // CompressionLayer covers only the /api nest — static assets are already
    // pre-compressed by the Vite build (assets.rs picks the right .gz/.br
    // variant by Accept-Encoding) and would otherwise be double-encoded.
    let api = haze_api::api_router(state).layer(CompressionLayer::new());
    let router = Router::new()
        .route("/healthz", get(healthz))
        .nest("/api", api)
        .fallback(assets::handler)
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
    // graceful shutdown — without this arm haze would ignore it and get
    // hard-killed at the grace timeout. If we ever spawn child processes,
    // PID 1 will also need a SIGCHLD reaper (or `tini` in the Dockerfile).
    tokio::select! {
        () = ctrl_c => tracing::info!("shutdown: SIGINT"),
        () = term => tracing::info!("shutdown: SIGTERM"),
    }
    // Wake every long-lived response handler (SSE) before axum starts its
    // drain. Without this, an idle browser tab's `/events` connection
    // holds the response stream open forever and axum::serve never
    // returns — Ctrl-C "stalls" for the user.
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

    let password = generate_password(16);
    let hash = haze_auth::password::hash(&password).context("hashing bootstrap password")?;
    haze_auth::user::create(pool, "admin", Some(&hash), haze_auth::user::Role::Admin)
        .await
        .context("creating bootstrap admin")?;

    tracing::info!("");
    tracing::info!("==============================================================");
    tracing::info!("  Bootstrap admin user created (empty database on first boot)");
    tracing::info!("    username: admin");
    tracing::info!("    password: {password}");
    tracing::info!("  Sign in, then change the password via Settings -> Users.");
    tracing::info!("  This message will not appear again.");
    tracing::info!("==============================================================");
    tracing::info!("");
    Ok(())
}

/// Crypto-safe password from a URL-safe alphabet. 16 chars over a 64-symbol
/// set is ~96 bits of entropy, plenty for a one-off bootstrap secret.
fn generate_password(len: usize) -> String {
    use rand::Rng;
    const CHARSET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789-_";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| char::from(CHARSET[rng.gen_range(0..CHARSET.len())]))
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
