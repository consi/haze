//! Per-IP rate limiting for anonymous (public-mode) traffic.
//!
//! Two limiter classes - "light" (server-info, tree, groups, hosts list /
//! detail, events) and "series" (`/hosts/{uuid}/series`, which a viewer
//! can hammer while paging through hosts). Authenticated requests bypass
//! the limiter entirely: the middleware short-circuits when the request
//! already carries a `CurrentUser` extension attached by `session_layer`.
//!
//! Limits live in the `public_mode` setting and are rebuilt on save -
//! the middleware reads an `ArcSwap` so the change is lock-free.

use std::{
    net::{IpAddr, SocketAddr},
    num::NonZeroU32,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use arc_swap::ArcSwap;
use axum::{
    extract::{ConnectInfo, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use dashmap::DashMap;
use governor::{
    Quota, RateLimiter,
    clock::{Clock, DefaultClock, QuantaInstant},
    middleware::NoOpMiddleware,
    state::keyed::DefaultKeyedStateStore,
};
use haze_auth::CurrentUser;
use haze_store::PublicModeSettings;

use crate::state::AppState;

/// Per-IP keyed token bucket. One instance per limiter class.
pub type IpRateLimiter = RateLimiter<
    IpAddr,
    DefaultKeyedStateStore<IpAddr>,
    DefaultClock,
    NoOpMiddleware<QuantaInstant>,
>;

/// The two limiter classes wired into the middleware.
pub struct Limiters {
    pub light: IpRateLimiter,
    pub series: IpRateLimiter,
}

/// Lock-free handle the admin settings PUT swaps a fresh `Limiters` into.
pub type LimiterHandle = Arc<ArcSwap<Limiters>>;

/// Build a fresh `Limiters` from the public-mode settings. Called at
/// startup and on every successful `update_public_mode` save.
pub fn build_limiters(s: &PublicModeSettings) -> Limiters {
    Limiters {
        light: RateLimiter::keyed(quota(s.light_per_minute, s.light_burst)),
        series: RateLimiter::keyed(quota(s.series_per_minute, s.series_burst)),
    }
}

pub fn new_handle(settings: &PublicModeSettings) -> LimiterHandle {
    Arc::new(ArcSwap::from_pointee(build_limiters(settings)))
}

fn quota(per_minute: u32, burst: u32) -> Quota {
    // NonZeroU32 is required by governor. Clamp both inputs at 1 so a
    // stored zero (corrupted setting) can't panic.
    let per_min = NonZeroU32::new(per_minute.max(1)).expect("clamped to >= 1");
    let burst = NonZeroU32::new(burst.max(1)).expect("clamped to >= 1");
    Quota::per_minute(per_min).allow_burst(burst)
}

/// Per-IP concurrent-SSE counter. Keyed by client IP, each entry is an
/// atomic that `SseGuard` increments on acquire and decrements on drop.
pub type SsePerIpMap = Arc<DashMap<IpAddr, Arc<AtomicU32>>>;

pub fn new_sse_map() -> SsePerIpMap {
    Arc::new(DashMap::new())
}

/// RAII guard for a single SSE connection slot.
///
/// Holds a reference to the per-IP atomic and decrements it on drop, so a
/// client disconnect automatically frees the slot. `try_acquire` returns
/// `None` if incrementing would exceed `cap`.
pub struct SseGuard {
    counter: Arc<AtomicU32>,
}

impl SseGuard {
    pub fn try_acquire(map: &SsePerIpMap, ip: IpAddr, cap: u32) -> Option<Self> {
        let counter = map
            .entry(ip)
            .or_insert_with(|| Arc::new(AtomicU32::new(0)))
            .clone();
        // CAS loop so concurrent connections can't both observe `cur < cap`
        // and overshoot.
        loop {
            let cur = counter.load(Ordering::Acquire);
            if cur >= cap {
                return None;
            }
            if counter
                .compare_exchange(cur, cur + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(Self { counter });
            }
        }
    }
}

impl Drop for SseGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Apply the per-IP rate limit to anonymous requests.
///
/// Authenticated requests (those carrying a `CurrentUser` extension from
/// `session_layer`) pass straight through. Must run AFTER `session_layer`
/// so the bypass check sees attached users.
pub async fn rate_limit_layer(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    if req.extensions().get::<CurrentUser>().is_some() {
        return next.run(req).await;
    }
    let path = req.uri().path();
    let limiters = state.limiters.load();
    let limiter = pick_limiter(&limiters, path);
    let ip = addr.ip();
    match limiter.check_key(&ip) {
        Ok(()) => next.run(req).await,
        Err(not_until) => {
            let wait = not_until.wait_time_from(DefaultClock::default().now());
            too_many_requests(wait)
        }
    }
}

fn pick_limiter<'a>(limiters: &'a Limiters, path: &str) -> &'a IpRateLimiter {
    // The series endpoint is `/api/v1/hosts/{uuid}/series` - match by
    // suffix so the path is robust to the base-URL prefix.
    if path.ends_with("/series") {
        &limiters.series
    } else {
        &limiters.light
    }
}

fn too_many_requests(wait: Duration) -> Response {
    let secs = wait.as_secs().max(1);
    let mut resp = (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded\n").into_response();
    let _ = resp.headers_mut().insert(
        header::RETRY_AFTER,
        HeaderValue::from_str(&secs.to_string()).unwrap_or_else(|_| HeaderValue::from_static("1")),
    );
    resp
}
