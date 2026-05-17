//! Alerting subsystem: rule storage, in-memory evaluation, webhook
//! notifications, and a warm-restart snapshot for the in-memory series.
//!
//! Layout:
//!   - `types`: public enums (Metric / Aggregation / Direction / Severity)
//!     and the `classify` helper.
//!   - `repo`:  sqlx queries shared with the API crate (rules + webhooks
//!     + state + snapshot).
//!   - `engine`: 60-second eval loop, reads only from the `SeriesStore`.
//!   - `snapshot`: periodic jittered flush + boot-time restore with
//!     stale-row drop.
//!   - `webhooks`: long-lived reqwest client + JSON POST helper.

pub mod engine;
pub mod repo;
pub mod snapshot;
pub mod types;
pub mod webhooks;

pub use engine::AlertEngine;
pub use snapshot::{RestoreReport, flush_once, restore, run_flush};
pub use types::{Aggregation, Direction, Metric, ResolveReason, Severity, TargetKind, classify};
pub use webhooks::WebhookClient;
