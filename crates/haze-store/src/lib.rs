//! Persistent storage: `SQLite` for metadata, `.hzc` (Haze Chunks) for the
//! time-series. Chunks live in per-host directories and are self-contained;
//! `SQLite` never indexes them.

use std::path::Path;

use anyhow::{Context, Result};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};

pub mod aggregate;
pub mod clock;
pub mod hzc;
pub mod repo;
pub mod series_store;
pub mod slot;

pub use aggregate::{Observation, aggregate, consolidate};
pub use clock::{Clock, ManualClock, SystemClock, system_clock};
pub use hzc::{
    DEFAULT_ROLLUP_G2_SETTLED_AFTER_SECS, DEFAULT_ROLLUP_G3_SETTLED_AFTER_SECS,
    DEFAULT_ROLLUP_SETTLED_AFTER_SECS, HostWriter, HzcError, HzcStore, MigrationReport,
    RetentionTier, RollupReport, chunk_window_bounds, default_retention_tiers, host_directory,
    read_range, read_range_in_dir, rollup_g2_host, rollup_g3_host, rollup_host,
};
pub use repo::settings::{
    AlertingSettings, DEFAULT_HOST_CHUNK_WINDOW_SECS, HostDefaults, PublicModeSettings,
    WorkerPools, default_alerting_settings, default_host_defaults, default_public_mode_settings,
    default_worker_pools,
};
pub use series_store::SeriesStore;
pub use slot::{Sample, Slot};

const DB_FILENAME: &str = "haze.sqlite";

/// Apply embedded migrations against the `SQLite` DB in `data_dir`.
pub async fn migrate(data_dir: &Path) -> Result<()> {
    tokio::fs::create_dir_all(data_dir).await.ok();
    let pool = open_pool(data_dir).await?;
    sqlx::migrate!("../../migrations")
        .run(&pool)
        .await
        .context("running migrations")?;
    Ok(())
}

/// Open the metadata `SQLite` pool. Creates the DB file if missing.
pub async fn open_pool(data_dir: &Path) -> Result<SqlitePool> {
    let db_path = data_dir.join(DB_FILENAME);
    let opts = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(5));
    SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await
        .with_context(|| format!("opening sqlite at {}", db_path.display()))
}
