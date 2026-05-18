//! /api/v1/events — Server-Sent Events stream that pushes domain-change
//! notifications to authenticated browser sessions.
//!
//! The wire format is one `event: change` per push, with the kind name as
//! the data payload (e.g. `tree`, `alerts`, `webhooks`, `users`,
//! `settings`). The frontend bumps the matching reload counter when a kind
//! arrives so any page currently rendering that domain refetches.
//!
//! Authentication is the same as the rest of `/api/v1/*`: the session
//! middleware in `middleware.rs` attaches the user, and the `CurrentUser`
//! extractor rejects unauthenticated requests with 401 before any stream
//! headers go out. The browser's `EventSource` auto-reconnects on
//! disconnect; if the session has been revoked the reconnect attempt also
//! returns 401, which the frontend interprets as "session gone → redirect
//! to /login".
//!
//! `tower-http`'s `DefaultPredicate` already excludes `text/event-stream`
//! from gzip/br compression, so no special wiring is required to keep the
//! stream from being buffered by the wrapping `CompressionLayer`.

use std::{convert::Infallible, time::Duration};

use axum::{
    Router,
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
    routing::get,
};
use futures::Stream;
use haze_auth::CurrentUser;
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;

use crate::state::AppState;

/// Domain categories the frontend cares about.
///
/// Granular enough that the alerting page doesn't refetch when a host
/// changes, but coarse enough that we don't have to ship per-entity diffs
/// over the wire — the client just re-issues whatever list query it
/// already uses.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// Group or host CRUD — anything that affects the sidebar tree.
    Tree,
    /// Alert rule CRUD or state transitions.
    Alerts,
    /// Webhook library CRUD.
    Webhooks,
    /// User CRUD, password resets, passkey/API-token changes.
    Users,
    /// System settings (storage, workers, alerting tunables, host defaults).
    Settings,
}

impl ChangeKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Tree => "tree",
            Self::Alerts => "alerts",
            Self::Webhooks => "webhooks",
            Self::Users => "users",
            Self::Settings => "settings",
        }
    }
}

pub fn router() -> Router<AppState> {
    Router::new().route("/", get(stream))
}

/// `GET /api/v1/events` — subscribe to domain-change pushes.
async fn stream(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // One receiver per connection. `broadcast` drops old events for slow
    // receivers (the `Lagged` variant); we surface that to the client as a
    // generic "refetch everything" signal rather than tearing down the
    // stream — refetch is idempotent.
    let rx = state.events.subscribe();
    let shutdown = state.shutdown;
    // The `shutdown` arm is critical: without it `recv().await` parks
    // forever, axum's graceful shutdown waits for this response stream to
    // end, and `docker stop` / Ctrl-C stalls until the kill timeout fires.
    let s = futures::stream::unfold((rx, shutdown), |(mut rx, shutdown)| async move {
        tokio::select! {
            r = rx.recv() => match r {
                Ok(kind) => {
                    let event = Event::default().event("change").data(kind.as_str());
                    Some((Ok(event), (rx, shutdown)))
                }
                Err(RecvError::Lagged(skipped)) => {
                    tracing::debug!(skipped, "SSE subscriber lagged; sending refetch-all");
                    let event = Event::default().event("change").data("all");
                    Some((Ok(event), (rx, shutdown)))
                }
                Err(RecvError::Closed) => None,
            },
            () = shutdown.notified() => None,
        }
    });
    Sse::new(s).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(20))
            .text("ping"),
    )
}
