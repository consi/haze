//! /api/v1/auth/* handlers: login, logout, current-user.

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use haze_auth::{self, CurrentUser, password, sessions, user};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{error::ApiError, error::ApiResult, state::AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/me", get(me))
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct LoginReq {
    username: String,
    password: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct LoginResp {
    pub id: i64,
    pub username: String,
    pub role: String,
}

#[utoipa::path(
    post,
    path = "/api/v1/auth/login",
    request_body = LoginReq,
    responses(
        (status = 200, body = LoginResp, description = "Authenticated; session cookie set"),
        (status = 401, description = "Invalid credentials")
    ),
    tag = "auth"
)]
pub(crate) async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<LoginReq>,
) -> ApiResult<Response> {
    let row = user::find_by_username(&state.pool, &req.username)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    let hash = row.password_hash.as_deref().ok_or(ApiError::Unauthorized)?;
    if !password::verify(&req.password, hash)? {
        return Err(ApiError::Unauthorized);
    }
    // Reject disabled accounts at the login boundary so a disabled user never
    // gets a session cookie even with valid credentials.
    let role = row
        .role
        .parse::<user::Role>()
        .unwrap_or(user::Role::Disabled);
    if !role.is_active() {
        return Err(ApiError::Unauthorized);
    }

    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok());
    let cookie_value = sessions::create(&state.pool, row.id, ua, None).await?;
    let set_cookie = sessions::set_cookie(&cookie_value, &state.cookie_path);

    let role_str = role.as_str().to_owned();

    let body = Json(LoginResp {
        id: row.id,
        username: row.username,
        role: role_str,
    });
    let mut resp = body.into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        set_cookie.parse().expect("cookie value is ascii"),
    );
    Ok(resp)
}

#[utoipa::path(
    post,
    path = "/api/v1/auth/logout",
    responses((status = 204, description = "Session destroyed; cookie cleared")),
    tag = "auth"
)]
pub(crate) async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let cookie = headers
        .get(header::COOKIE)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| extract_cookie(s, sessions::COOKIE_NAME));
    if let Some(c) = cookie {
        sessions::destroy(&state.pool, &c).await?;
    }
    let mut resp = StatusCode::NO_CONTENT.into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        sessions::clear_cookie(&state.cookie_path)
            .parse()
            .expect("cookie value is ascii"),
    );
    Ok(resp)
}

#[utoipa::path(
    get,
    path = "/api/v1/auth/me",
    responses(
        (status = 200, body = LoginResp, description = "Authenticated user"),
        (status = 401, description = "Not authenticated")
    ),
    tag = "auth"
)]
pub(crate) async fn me(user: CurrentUser) -> ApiResult<Json<LoginResp>> {
    Ok(Json(LoginResp {
        id: user.id,
        username: user.username,
        role: user.role.as_str().to_owned(),
    }))
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
