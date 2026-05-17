//! Session middleware: extracts either the `haze_session` cookie or a
//! `Authorization: Bearer hzt_…` API-token header, looks the principal up via
//! `haze-auth`, and attaches `CurrentUser` to the request extensions.

use axum::{
    extract::{Request, State},
    http::header,
    middleware::Next,
    response::Response,
};
use haze_auth::{
    CurrentUser, api_token,
    sessions::{self, COOKIE_NAME},
    user::{self, Role},
};

use crate::state::AppState;

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
