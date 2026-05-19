//! Session middleware: extracts either the `haze_session` cookie or a
//! `Authorization: Bearer hzt_…` API-token header, looks the principal up via
//! `haze-auth`, and attaches `CurrentUser` to the request extensions.
//!
//! Also exposes the `ViewerAccess` extractor for read endpoints that
//! should be accessible anonymously when public mode is enabled.

use axum::{
    extract::{FromRequestParts, Request, State},
    http::{header, request::Parts},
    middleware::Next,
    response::Response,
};
use haze_auth::{
    CurrentUser, api_token,
    sessions::{self, COOKIE_NAME},
    user::{self, Role},
};
use haze_store::repo::settings;

use crate::{error::ApiError, state::AppState};

pub async fn session_layer(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    // 1. Cookie-backed session (browser).
    let cookie = req
        .headers()
        .get(header::COOKIE)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| extract_cookie(s, COOKIE_NAME));
    if let Some(c) = cookie {
        match sessions::lookup(&state.pool, &c).await {
            Ok(Some((_session, user))) => {
                req.extensions_mut().insert(user);
            }
            Ok(None) => {}
            Err(e) => tracing::debug!(error = ?e, "session lookup failed"),
        }
    }

    // 2. Bearer token (machine clients), only if no cookie session attached.
    if req.extensions().get::<CurrentUser>().is_none() {
        if let Some(bearer) = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer ").map(str::to_owned))
        {
            match api_token::lookup_user(&state.pool, &bearer).await {
                Ok(Some(user_id)) => {
                    if let Ok(Some(row)) = user::find_by_id(&state.pool, user_id).await {
                        let role = row.role.parse::<Role>().unwrap_or(Role::Disabled);
                        // Reject Bearer auth for disabled accounts - same gate
                        // as the login endpoint so tokens stop working when an
                        // account is disabled, without needing to revoke each
                        // token individually.
                        if role.is_active() {
                            req.extensions_mut().insert(CurrentUser {
                                id: row.id,
                                username: row.username,
                                role,
                            });
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::debug!(error = ?e, "bearer token lookup failed"),
            }
        }
    }

    next.run(req).await
}

fn extract_cookie(header_value: &str, name: &str) -> Option<String> {
    for kv in header_value.split(';') {
        let kv = kv.trim();
        if let Some((k, v)) = kv.split_once('=') {
            if k == name {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// True when the request reached the server over HTTPS, judged from
/// `X-Forwarded-Proto` (set by the reverse proxy in production). Absent or
/// non-https header → assume plain HTTP, which is the local-dev case where
/// the `Secure` cookie attribute would prevent the browser from storing
/// the session cookie at all.
pub(crate) fn is_secure_request(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.eq_ignore_ascii_case("https"))
}

/// Extractor for read endpoints that should also work without
/// authentication when public mode is enabled.
///
/// Succeeds with `Some(user)` for authenticated requests, with `None` for
/// anonymous requests *if and only if* public mode is on, and rejects with
/// 401 otherwise. Handlers that don't care which case applies can simply
/// take `_viewer: ViewerAccess` as a gate.
#[derive(Debug, Clone)]
pub struct ViewerAccess {
    pub user: Option<CurrentUser>,
}

impl FromRequestParts<AppState> for ViewerAccess {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        if let Some(user) = parts.extensions.get::<CurrentUser>().cloned() {
            return Ok(Self { user: Some(user) });
        }
        let public = settings::public_mode_settings(&state.pool).await?;
        if public.enabled {
            Ok(Self { user: None })
        } else {
            Err(ApiError::Unauthorized)
        }
    }
}
