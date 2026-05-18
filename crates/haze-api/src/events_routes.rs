//! /api/v1/events — Server-Sent Events stream that pushes domain-change
//! notifications to authenticated browser sessions.
//!
//! The wire format is one `event: change` per push, with the kind name as
//! the data payload (e.g. `tree`, `alerts`, `webhooks`, `users`,
//! `settings`). The frontend bumps the matching reload counter when a kind
//! arrives so any page currently rendering that domain refetches.
//!
//! Authentication is the same as the rest of `/api/v1/*`: the session
//! middleware in `middleware.rs` attaches the user, and the `ViewerAccess`
//! extractor either accepts the authenticated user, lets the request
//! through anonymously when public mode is enabled, or rejects with 401.
//! The browser's `EventSource` auto-reconnects on disconnect; if the
//! session has been revoked (or public mode flipped off) the reconnect
//! attempt also returns 401, which the frontend interprets as "session
//! gone → redirect to /login".
//!
//! Anonymous connections additionally compete for a per-IP slot capped
//! by `PublicModeSettings.sse_max_per_ip`, so a single attacker IP can't
//! pin thousands of broadcast subscribers open.
//!
//! `tower-http`'s `DefaultPredicate` already excludes `text/event-stream`
//! from gzip/br compression, so no special wiring is required to keep the
//! stream from being buffered by the wrapping `CompressionLayer`.

use std::{convert::Infallible, net::SocketAddr, time::Duration};

use axum::{
    Router,
    extract::{ConnectInfo, State},
    http::StatusCode,
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::get,
};
use haze_store::repo::settings;
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;

use crate::{error::ApiError, middleware::ViewerAccess, rate_limit::SseGuard, state::AppState};

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
///
/// Returns a typed `Response` rather than `Sse<…>` directly so we can
/// reject anonymous clients that exceed the per-IP SSE concurrency cap
/// with a 429 before the stream headers go out.
async fn stream(
    viewer: ViewerAccess,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<AppState>,
) -> Result<Response, ApiError> {
    // Only enforce the per-IP SSE cap on anonymous connections —
    // authenticated users have explicit accounts, so the limit would
    // mostly punish multi-tab operators. The cap exists to stop a public
    // attacker pinning thousands of broadcast subscribers open.
    let sse_guard = if viewer.user.is_none() {
        let public = settings::public_mode_settings(&state.pool).await?;
        match SseGuard::try_acquire(&state.sse_per_ip, addr.ip(), public.sse_max_per_ip) {
            Some(g) => Some(g),
            None => {
                return Ok((
                    StatusCode::TOO_MANY_REQUESTS,
                    "sse connections per ip exceeded\n",
                )
                    .into_response());
            }
        }
    } else {
        None
    };

    // One receiver per connection. `broadcast` drops old events for slow
    // receivers (the `Lagged` variant); we surface that to the client as a
    // generic "refetch everything" signal rather than tearing down the
    // stream — refetch is idempotent.
    let rx = state.events.subscribe();
    let shutdown = state.shutdown;
    // The `shutdown` arm is critical: without it `recv().await` parks
    // forever, axum's graceful shutdown waits for this response stream to
    // end, and `docker stop` / Ctrl-C stalls until the kill timeout fires.
    //
    // `sse_guard` is moved into the unfold state so it survives for the
    // life of the stream and only drops (releasing the per-IP slot) when
    // the client disconnects or the server shuts down.
    let s = futures::stream::unfold(
        (rx, shutdown, sse_guard),
        |(mut rx, shutdown, guard)| async move {
            tokio::select! {
                r = rx.recv() => match r {
                    Ok(kind) => {
                        let event = Event::default().event("change").data(kind.as_str());
                        Some((Ok::<_, Infallible>(event), (rx, shutdown, guard)))
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        tracing::debug!(skipped, "SSE subscriber lagged; sending refetch-all");
                        let event = Event::default().event("change").data("all");
                        Some((Ok::<_, Infallible>(event), (rx, shutdown, guard)))
                    }
                    Err(RecvError::Closed) => None,
                },
                () = shutdown.notified() => None,
            }
        },
    );
    let sse: Sse<_> = Sse::new(s).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(20))
            .text("ping"),
    );
    Ok(sse.into_response())
}
