//! Shared application state injected into every handler.

use std::{path::PathBuf, sync::Arc};

use haze_auth::PasskeyService;
use haze_probe::scheduler::SchedulerHandle;
use haze_store::{HzcStore, SeriesStore};
use sqlx::SqlitePool;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub hzc: Arc<HzcStore>,
    /// Root data dir (the parent of `hzc/`). Handlers use it for direct
    /// chunk reads without holding a `HostWriter` lock.
    pub data_dir: PathBuf,
    pub scheduler: SchedulerHandle,
    pub passkeys: Option<Arc<PasskeyService>>,
    /// In-memory ring buffer of recent slots per host. Written by the
    /// probe scheduler, read by the alert evaluator and the test-webhook
    /// route. Shared via Arc so handlers can hand a clone out cheaply.
    pub series: Arc<SeriesStore>,
}
