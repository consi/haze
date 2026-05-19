//! Shared application state injected into every handler.

use std::{path::PathBuf, sync::Arc};

use haze_auth::PasskeyService;
use haze_probe::scheduler::SchedulerHandle;
use haze_store::{HzcStore, SeriesStore};
use sqlx::SqlitePool;
use tokio::sync::{Notify, broadcast};

use crate::{
    events_routes::ChangeKind,
    rate_limit::{LimiterHandle, SsePerIpMap},
};

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
    /// Broadcast channel for domain-change notifications. Mutation routes
    /// `send` a `ChangeKind` after a successful write; the SSE endpoint at
    /// `/api/v1/events` subscribes a receiver per connection and forwards
    /// events to the browser so open tabs refresh without polling.
    pub events: broadcast::Sender<ChangeKind>,
    /// Wake-up for long-lived response handlers (currently just the SSE
    /// stream) to bail out at shutdown. Without this, axum's graceful
    /// shutdown blocks forever waiting for the held-open `/events`
    /// response to drain - `broadcast::Receiver::recv().await` never
    /// resolves on its own, so we'd sit until SIGKILL.
    pub shutdown: Arc<Notify>,
    /// `Path=` attribute for the session cookie. Equal to the normalized
    /// `HAZE_BASE_URL` (e.g. `/haze`) so the browser only sends the cookie
    /// back on URLs under the deployment sub-path. Empty string means root
    /// (handlers render `Path=/`).
    pub cookie_path: String,
    /// Per-IP token buckets the rate-limit middleware consults for
    /// anonymous requests. Built from `PublicModeSettings` at startup and
    /// hot-swapped when an admin saves new limits.
    pub limiters: LimiterHandle,
    /// Per-IP concurrent SSE connection counters. Each `/api/v1/events`
    /// handler holds a guard for the duration of the stream so a
    /// disconnect frees the slot automatically.
    pub sse_per_ip: SsePerIpMap,
}

impl AppState {
    /// Send a change notification on a best-effort basis. Ignores the case
    /// where there are no subscribers - that's normal when no browser tab
    /// happens to be open. Call this *after* the mutating SQL has committed.
    pub fn notify(&self, kind: ChangeKind) {
        let _ = self.events.send(kind);
    }
}
