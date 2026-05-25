//! Key/value system settings backed by a single `settings` table. Values are
//! stored as JSON literals so typed accessors can deserialise into whatever
//! shape they want.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::RetentionTier;

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub const KEY_RETENTION_TIERS: &str = "hzc.retention_tiers";
pub const KEY_COMPACTOR_INTERVAL_SECS: &str = "hzc.compactor_interval_secs";
pub const KEY_ROLLUP_INTERVAL_SECS: &str = "hzc.rollup_interval_secs";
pub const KEY_ROLLUP_SETTLED_AFTER_SECS: &str = "hzc.rollup_settled_after_secs";
pub const KEY_ROLLUP_INTER_HOST_PAUSE_MS: &str = "hzc.rollup_inter_host_pause_ms";
pub const KEY_ROLLUP_G2_SETTLED_AFTER_SECS: &str = "hzc.rollup_g2_settled_after_secs";
pub const KEY_ROLLUP_G3_SETTLED_AFTER_SECS: &str = "hzc.rollup_g3_settled_after_secs";
pub const KEY_WORKER_POOLS: &str = "runtime.worker_pools";
pub const KEY_ALERTING: &str = "alerting";
pub const KEY_HOST_DEFAULTS: &str = "hosts.defaults";
pub const KEY_PUBLIC_MODE: &str = "public_mode";
pub const KEY_INSTANCE_UUID: &str = "instance.uuid";

/// How often the compactor walks the host fleet. Read live by the
/// compactor task each cycle so changes apply without a restart.
pub const DEFAULT_COMPACTOR_INTERVAL_SECS: u32 = 3600;

/// How often the single-threaded daily-rollup task walks the host fleet.
/// 10 minutes balances responsiveness (a freshly-settled day gets bundled
/// quickly) against I/O pressure on the live writer.
pub const DEFAULT_ROLLUP_INTERVAL_SECS: u32 = 600;

/// Settle margin before a UTC day is eligible for bundling: 1 hour past
/// midnight UTC. Long enough to let any chunk that crossed midnight finish
/// sealing.
pub const DEFAULT_ROLLUP_SETTLED_AFTER_SECS: u32 = 3_600;

/// Settle margin before a UTC month is eligible for G2 bundling. One day
/// past the month boundary leaves room for the G1 daily rollup to finish
/// for every day in the month first.
pub const DEFAULT_ROLLUP_G2_SETTLED_AFTER_SECS: u32 = 86_400;

/// Settle margin before a UTC year is eligible for G3 bundling. Two days
/// past the year boundary leaves room for the G2 monthly rollup to settle
/// December first.
pub const DEFAULT_ROLLUP_G3_SETTLED_AFTER_SECS: u32 = 2 * 86_400;

/// Pause between hosts inside a single rollup pass. Gives the kernel
/// breathing room for the writer's WAL flushes and other I/O on the box.
pub const DEFAULT_ROLLUP_INTER_HOST_PAUSE_MS: u32 = 50;

/// Default chunk window for newly-created hosts.
///
/// Kept here so both the API (when serving the host create form's
/// defaults) and the scheduler agree. Chunk window is per-host (stored in
/// `hosts.chunk_window_secs` and in the host's `meta.json`), not a global
/// setting.
pub const DEFAULT_HOST_CHUNK_WINDOW_SECS: u32 = 3600;

/// Per-area concurrency caps.
///
/// Each field is the maximum number of in-flight async operations the
/// corresponding subsystem will allow at once - past that point new work
/// waits on a semaphore until a slot frees up.
///
/// Probe pools cap in-flight probe attempts of that kind. A permit is
/// held only for the duration of one `measure_once` call, not for the
/// whole sampling period, so the total number of hosts can exceed the
/// pool size (hosts block only while their attempt is actually executing).
/// PING is cheap (raw async ICMP), the rest open real sockets and so have
/// lower caps. `compactor` bounds how many host directories can be
/// compacted in parallel, and `alert_eval` bounds parallel alert-rule
/// evaluation.
///
/// Defaults sized for a typical 4-8 core box with ample headroom for
/// host counts well above the per-pool capacity. Adjustable from
/// `Settings -> Workers`; takes effect on the next server restart.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct WorkerPools {
    pub probe_ping: u32,
    pub probe_dns: u32,
    pub probe_tcp_connect: u32,
    pub probe_tls_connect: u32,
    pub probe_http_ttfb: u32,
    pub probe_http_total: u32,
    pub compactor: u32,
    pub alert_eval: u32,
    /// Max concurrent in-flight replication operations across every peer:
    /// catch-up range fetches, manifest reconciles, and per-rule SSE
    /// stream consumers. The default of 16 lets a few dozen rules share
    /// a small CPU + network budget without piling up on a slow peer.
    #[serde(default = "default_replication_workers")]
    pub replication: u32,
}

pub fn default_worker_pools() -> WorkerPools {
    WorkerPools {
        probe_ping: 4096,
        probe_dns: 1024,
        probe_tcp_connect: 1024,
        probe_tls_connect: 512,
        probe_http_ttfb: 512,
        probe_http_total: 512,
        compactor: 8,
        alert_eval: 32,
        replication: default_replication_workers(),
    }
}

fn default_replication_workers() -> u32 {
    16
}

/// Fetch the raw JSON value for `key`, or `None` if the row is missing.
pub async fn get_raw(pool: &SqlitePool, key: &str) -> Result<Option<String>, SettingsError> {
    let row: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE key = ?1")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(v,)| v))
}

/// Upsert a setting. `value_json` is the JSON literal to store.
pub async fn set_raw(
    pool: &SqlitePool,
    key: &str,
    value_json: &str,
    updated_by: Option<i64>,
) -> Result<(), SettingsError> {
    let now = Utc::now().timestamp();
    sqlx::query(
        "INSERT INTO settings (key, value, updated_at, updated_by) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, \
                                        updated_at = excluded.updated_at, \
                                        updated_by = excluded.updated_by",
    )
    .bind(key)
    .bind(value_json)
    .bind(now)
    .bind(updated_by)
    .execute(pool)
    .await?;
    Ok(())
}

/// Read `hzc.retention_tiers`, falling back to the built-in defaults.
pub async fn retention_tiers(pool: &SqlitePool) -> Result<Vec<RetentionTier>, SettingsError> {
    let raw = get_raw(pool, KEY_RETENTION_TIERS).await?;
    match raw {
        Some(v) => Ok(serde_json::from_str(&v)?),
        None => Ok(crate::default_retention_tiers()),
    }
}

/// Compactor schedule (seconds between passes). Falls back to the default
/// if missing or malformed so a bad value can't stop the compactor.
pub async fn compactor_interval_secs(pool: &SqlitePool) -> Result<u32, SettingsError> {
    let raw = get_raw(pool, KEY_COMPACTOR_INTERVAL_SECS).await?;
    Ok(raw
        .and_then(|v| serde_json::from_str::<u32>(v.trim()).ok())
        .unwrap_or(DEFAULT_COMPACTOR_INTERVAL_SECS))
}

pub async fn set_compactor_interval_secs(
    pool: &SqlitePool,
    value: u32,
    updated_by: Option<i64>,
) -> Result<(), SettingsError> {
    set_raw(
        pool,
        KEY_COMPACTOR_INTERVAL_SECS,
        &value.to_string(),
        updated_by,
    )
    .await
}

/// How often (in seconds) the single-threaded rollup task walks the host
/// fleet. Falls back to the default if missing or malformed.
pub async fn rollup_interval_secs(pool: &SqlitePool) -> Result<u32, SettingsError> {
    let raw = get_raw(pool, KEY_ROLLUP_INTERVAL_SECS).await?;
    Ok(raw
        .and_then(|v| serde_json::from_str::<u32>(v.trim()).ok())
        .unwrap_or(DEFAULT_ROLLUP_INTERVAL_SECS))
}

pub async fn set_rollup_interval_secs(
    pool: &SqlitePool,
    value: u32,
    updated_by: Option<i64>,
) -> Result<(), SettingsError> {
    set_raw(
        pool,
        KEY_ROLLUP_INTERVAL_SECS,
        &value.to_string(),
        updated_by,
    )
    .await
}

/// Settle margin (seconds past the UTC day boundary) before a day's chunks
/// are eligible for bundling.
pub async fn rollup_settled_after_secs(pool: &SqlitePool) -> Result<u32, SettingsError> {
    let raw = get_raw(pool, KEY_ROLLUP_SETTLED_AFTER_SECS).await?;
    Ok(raw
        .and_then(|v| serde_json::from_str::<u32>(v.trim()).ok())
        .unwrap_or(DEFAULT_ROLLUP_SETTLED_AFTER_SECS))
}

pub async fn set_rollup_settled_after_secs(
    pool: &SqlitePool,
    value: u32,
    updated_by: Option<i64>,
) -> Result<(), SettingsError> {
    set_raw(
        pool,
        KEY_ROLLUP_SETTLED_AFTER_SECS,
        &value.to_string(),
        updated_by,
    )
    .await
}

/// Settle margin (seconds past the UTC month boundary) before a month's
/// G1 daily bundles are eligible for G2 monthly bundling.
pub async fn rollup_g2_settled_after_secs(pool: &SqlitePool) -> Result<u32, SettingsError> {
    let raw = get_raw(pool, KEY_ROLLUP_G2_SETTLED_AFTER_SECS).await?;
    Ok(raw
        .and_then(|v| serde_json::from_str::<u32>(v.trim()).ok())
        .unwrap_or(DEFAULT_ROLLUP_G2_SETTLED_AFTER_SECS))
}

pub async fn set_rollup_g2_settled_after_secs(
    pool: &SqlitePool,
    value: u32,
    updated_by: Option<i64>,
) -> Result<(), SettingsError> {
    set_raw(
        pool,
        KEY_ROLLUP_G2_SETTLED_AFTER_SECS,
        &value.to_string(),
        updated_by,
    )
    .await
}

/// Settle margin (seconds past the UTC year boundary) before a year's G2
/// monthly bundles are eligible for G3 yearly bundling.
pub async fn rollup_g3_settled_after_secs(pool: &SqlitePool) -> Result<u32, SettingsError> {
    let raw = get_raw(pool, KEY_ROLLUP_G3_SETTLED_AFTER_SECS).await?;
    Ok(raw
        .and_then(|v| serde_json::from_str::<u32>(v.trim()).ok())
        .unwrap_or(DEFAULT_ROLLUP_G3_SETTLED_AFTER_SECS))
}

pub async fn set_rollup_g3_settled_after_secs(
    pool: &SqlitePool,
    value: u32,
    updated_by: Option<i64>,
) -> Result<(), SettingsError> {
    set_raw(
        pool,
        KEY_ROLLUP_G3_SETTLED_AFTER_SECS,
        &value.to_string(),
        updated_by,
    )
    .await
}

/// Pause (milliseconds) the rollup loop sleeps between consecutive hosts.
pub async fn rollup_inter_host_pause_ms(pool: &SqlitePool) -> Result<u32, SettingsError> {
    let raw = get_raw(pool, KEY_ROLLUP_INTER_HOST_PAUSE_MS).await?;
    Ok(raw
        .and_then(|v| serde_json::from_str::<u32>(v.trim()).ok())
        .unwrap_or(DEFAULT_ROLLUP_INTER_HOST_PAUSE_MS))
}

pub async fn set_rollup_inter_host_pause_ms(
    pool: &SqlitePool,
    value: u32,
    updated_by: Option<i64>,
) -> Result<(), SettingsError> {
    set_raw(
        pool,
        KEY_ROLLUP_INTER_HOST_PAUSE_MS,
        &value.to_string(),
        updated_by,
    )
    .await
}

pub async fn set_retention_tiers(
    pool: &SqlitePool,
    tiers: &[RetentionTier],
    updated_by: Option<i64>,
) -> Result<(), SettingsError> {
    let json = serde_json::to_string(tiers)?;
    set_raw(pool, KEY_RETENTION_TIERS, &json, updated_by).await
}

/// Read the worker pool sizes from settings, falling back to defaults for
/// any field that's missing (e.g. when only a partial JSON was stored).
pub async fn worker_pools(pool: &SqlitePool) -> Result<WorkerPools, SettingsError> {
    let raw = get_raw(pool, KEY_WORKER_POOLS).await?;
    match raw {
        Some(v) => Ok(serde_json::from_str(&v).unwrap_or_else(|_| default_worker_pools())),
        None => Ok(default_worker_pools()),
    }
}

pub async fn set_worker_pools(
    pool: &SqlitePool,
    pools: &WorkerPools,
    updated_by: Option<i64>,
) -> Result<(), SettingsError> {
    let json = serde_json::to_string(pools)?;
    set_raw(pool, KEY_WORKER_POOLS, &json, updated_by).await
}

/// Alerting subsystem tunables.
///
/// All of these are read live by the engine on each cycle (the compactor
/// pattern): `eval_interval_secs` controls how often rules are checked,
/// `webhook_timeout_secs` is the per-POST reqwest timeout (a change
/// rebuilds the client on the next cycle), `snapshot_flush_interval_secs`
/// drives the periodic in-memory series checkpoint, and
/// `min_window_secs` / `max_window_secs` are the soft bounds the API
/// enforces when creating or updating an alert rule.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct AlertingSettings {
    pub eval_interval_secs: u32,
    pub webhook_timeout_secs: u32,
    pub snapshot_flush_interval_secs: u32,
    pub min_window_secs: u32,
    pub max_window_secs: u32,
}

pub fn default_alerting_settings() -> AlertingSettings {
    AlertingSettings {
        eval_interval_secs: 60,
        webhook_timeout_secs: 10,
        snapshot_flush_interval_secs: 300,
        min_window_secs: 30,
        max_window_secs: 7 * 86_400,
    }
}

pub async fn alerting_settings(pool: &SqlitePool) -> Result<AlertingSettings, SettingsError> {
    let raw = get_raw(pool, KEY_ALERTING).await?;
    match raw {
        Some(v) => Ok(serde_json::from_str(&v).unwrap_or_else(|_| default_alerting_settings())),
        None => Ok(default_alerting_settings()),
    }
}

pub async fn set_alerting_settings(
    pool: &SqlitePool,
    settings: &AlertingSettings,
    updated_by: Option<i64>,
) -> Result<(), SettingsError> {
    let json = serde_json::to_string(settings)?;
    set_raw(pool, KEY_ALERTING, &json, updated_by).await
}

/// Defaults pre-filled into the host creation form.
///
/// Operators set these once so they don't have to re-enter the same
/// `interval_secs` / `samples_per_period` for every new host. The
/// values are advisory; the modal lets the user override them per-host.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct HostDefaults {
    pub interval_secs: u32,
    pub samples_per_period: u32,
}

pub fn default_host_defaults() -> HostDefaults {
    HostDefaults {
        interval_secs: 60,
        samples_per_period: 20,
    }
}

pub async fn host_defaults(pool: &SqlitePool) -> Result<HostDefaults, SettingsError> {
    let raw = get_raw(pool, KEY_HOST_DEFAULTS).await?;
    match raw {
        Some(v) => Ok(serde_json::from_str(&v).unwrap_or_else(|_| default_host_defaults())),
        None => Ok(default_host_defaults()),
    }
}

pub async fn set_host_defaults(
    pool: &SqlitePool,
    defaults: &HostDefaults,
    updated_by: Option<i64>,
) -> Result<(), SettingsError> {
    let json = serde_json::to_string(defaults)?;
    set_raw(pool, KEY_HOST_DEFAULTS, &json, updated_by).await
}

/// Public-mode + anonymous rate-limit configuration.
///
/// When `enabled`, anonymous browsers can view the tree, group/host
/// details, and host series via `/api/v1/{tree,groups,hosts,events}`.
/// Write endpoints are unaffected - they still demand an authenticated
/// `CurrentUser`. The per-IP token-bucket sizes throttle anonymous
/// traffic only; authenticated requests bypass the limiter entirely.
///
/// Two limiter classes:
/// - `light` covers cheap calls (server-info, tree, groups, hosts list/detail).
/// - `series` covers `/hosts/{uuid}/series`, which a viewer can hit many
///   times in succession while paging through hosts.
///
/// `sse_max_per_ip` caps simultaneous SSE connections from one IP so an
/// attacker can't pin thousands of broadcast subscribers open.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct PublicModeSettings {
    pub enabled: bool,
    pub light_per_minute: u32,
    pub light_burst: u32,
    pub series_per_minute: u32,
    pub series_burst: u32,
    pub sse_max_per_ip: u32,
}

pub fn default_public_mode_settings() -> PublicModeSettings {
    PublicModeSettings {
        enabled: false,
        light_per_minute: 300,
        light_burst: 60,
        series_per_minute: 1200,
        series_burst: 120,
        sse_max_per_ip: 4,
    }
}

pub async fn public_mode_settings(pool: &SqlitePool) -> Result<PublicModeSettings, SettingsError> {
    let raw = get_raw(pool, KEY_PUBLIC_MODE).await?;
    match raw {
        Some(v) => Ok(serde_json::from_str(&v).unwrap_or_else(|_| default_public_mode_settings())),
        None => Ok(default_public_mode_settings()),
    }
}

pub async fn set_public_mode_settings(
    pool: &SqlitePool,
    settings: &PublicModeSettings,
    updated_by: Option<i64>,
) -> Result<(), SettingsError> {
    let json = serde_json::to_string(settings)?;
    set_raw(pool, KEY_PUBLIC_MODE, &json, updated_by).await
}

/// Stable per-instance UUID.
///
/// Generated on first read if missing so every running Haze process has
/// one regardless of upgrade order. Used by replication to detect loops
/// in multi-hop topologies and to identify the destination on
/// `POST /replication/slots`.
pub async fn instance_uuid(pool: &SqlitePool) -> Result<Uuid, SettingsError> {
    if let Some(raw) = get_raw(pool, KEY_INSTANCE_UUID).await? {
        if let Ok(parsed) = serde_json::from_str::<String>(&raw) {
            if let Ok(u) = Uuid::parse_str(&parsed) {
                return Ok(u);
            }
        }
    }
    let fresh = Uuid::new_v4();
    let json = serde_json::to_string(&fresh.to_string())?;
    set_raw(pool, KEY_INSTANCE_UUID, &json, None).await?;
    Ok(fresh)
}
