//! /api/v1/user/* - self-management for the currently-authenticated user:
//! password change, passkey listing/deletion, API-token CRUD.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use haze_auth::{CurrentUser, api_token, password, user};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{ChangeKind, error::ApiError, error::ApiResult, state::AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/password", post(change_password))
        .route("/passkeys", get(list_passkeys))
        .route("/passkeys/{id}", axum::routing::delete(delete_passkey))
        .route("/tokens", get(list_tokens).post(create_token))
        .route("/tokens/{id}", axum::routing::delete(delete_token))
}

// ─── Password ──────────────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub(crate) struct ChangePasswordReq {
    current_password: String,
    new_password: String,
}

#[utoipa::path(
    post,
    path = "/api/v1/user/password",
    request_body = ChangePasswordReq,
    responses(
        (status = 204, description = "Password updated"),
        (status = 401, description = "Current password incorrect"),
        (status = 422, description = "New password fails policy (min 8 chars)")
    ),
    tag = "user"
)]
pub(crate) async fn change_password(
    auth_user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<ChangePasswordReq>,
) -> ApiResult<StatusCode> {
    if req.new_password.len() < 8 {
        return Err(ApiError::Validation(
            "password must be at least 8 characters".into(),
        ));
    }
    let row = user::find_by_id(&state.pool, auth_user.id)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    let stored = row.password_hash.as_deref().ok_or(ApiError::Unauthorized)?;
    if !password::verify(&req.current_password, stored)? {
        return Err(ApiError::Unauthorized);
    }
    let new_hash = password::hash(&req.new_password)?;
    sqlx::query("UPDATE users SET password_hash = ?1 WHERE id = ?2")
        .bind(&new_hash)
        .bind(auth_user.id)
        .execute(&state.pool)
        .await?;
    state.notify(ChangeKind::Users);
    Ok(StatusCode::NO_CONTENT)
}

// ─── Passkeys ──────────────────────────────────────────────────────────────

#[derive(Serialize, ToSchema, sqlx::FromRow)]
pub(crate) struct PasskeyResp {
    pub id: i64,
    pub label: Option<String>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/api/v1/user/passkeys",
    responses(
        (status = 200, body = Vec<PasskeyResp>, description = "All passkeys registered for the user")
    ),
    tag = "user"
)]
pub(crate) async fn list_passkeys(
    user: CurrentUser,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<PasskeyResp>>> {
    let rows: Vec<PasskeyResp> = sqlx::query_as(
        "SELECT id, label, created_at, last_used_at \
         FROM passkey_credentials WHERE user_id = ?1 ORDER BY created_at DESC",
    )
    .bind(user.id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(rows))
}

#[utoipa::path(
    delete,
    path = "/api/v1/user/passkeys/{id}",
    params(("id" = i64, Path, description = "Passkey credential ID")),
    responses(
        (status = 204, description = "Passkey deleted"),
        (status = 404, description = "Passkey not found or doesn't belong to caller")
    ),
    tag = "user"
)]
pub(crate) async fn delete_passkey(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    let rows = sqlx::query("DELETE FROM passkey_credentials WHERE id = ?1 AND user_id = ?2")
        .bind(id)
        .bind(user.id)
        .execute(&state.pool)
        .await?
        .rows_affected();
    if rows == 0 {
        return Err(ApiError::NotFound);
    }
    state.notify(ChangeKind::Users);
    Ok(StatusCode::NO_CONTENT)
}

// ─── API tokens ────────────────────────────────────────────────────────────

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateTokenReq {
    name: String,
    /// Optional absolute expiry, epoch seconds. `None` = never expires.
    #[serde(default)]
    expires_at: Option<i64>,
    /// When `true`, the token is only accepted on paths under
    /// `/api/v1/replication`. Lets an admin issue a token to another
    /// Haze instance for cross-instance pulls without granting full
    /// admin authority over this one. Only admins may set this; ignored
    /// for non-admin users (their tokens never have this scope anyway).
    #[serde(default)]
    replication_only: bool,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct CreateTokenResp {
    pub id: i64,
    pub name: String,
    /// Plaintext token. **Shown only once** - store it client-side immediately.
    pub plaintext: String,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub replication_only: bool,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct TokenResp {
    pub id: i64,
    pub name: String,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub last_used_at: Option<i64>,
    pub replication_only: bool,
}

#[utoipa::path(
    get,
    path = "/api/v1/user/tokens",
    responses(
        (status = 200, body = Vec<TokenResp>, description = "API tokens belonging to the user (metadata only)")
    ),
    tag = "user"
)]
pub(crate) async fn list_tokens(
    user: CurrentUser,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<TokenResp>>> {
    let rows = api_token::list_for_user(&state.pool, user.id)
        .await
        .map_err(map_token_err)?;
    Ok(Json(
        rows.into_iter()
            .map(|r| TokenResp {
                id: r.id,
                name: r.name,
                created_at: r.created_at,
                expires_at: r.expires_at,
                last_used_at: r.last_used_at,
                replication_only: r.replication_only != 0,
            })
            .collect(),
    ))
}

#[utoipa::path(
    post,
    path = "/api/v1/user/tokens",
    request_body = CreateTokenReq,
    responses(
        (status = 201, body = CreateTokenResp, description = "Plaintext token shown once")
    ),
    tag = "user"
)]
pub(crate) async fn create_token(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<CreateTokenReq>,
) -> ApiResult<(StatusCode, Json<CreateTokenResp>)> {
    if req.name.trim().is_empty() {
        return Err(ApiError::Validation("name is required".into()));
    }
    // Only admins can mint `replication_only=true` tokens. The flag itself
    // is a permission grant ("this token is allowed past the per-instance
    // admin gate on /replication"), and admins are the only role that
    // already has that grant - so it makes sense to keep mint authority
    // there. Non-admin attempts silently fall back to a normal token.
    let replication_only = req.replication_only && user.role.is_admin();
    let (id, plaintext) = api_token::create(
        &state.pool,
        user.id,
        &req.name,
        req.expires_at,
        replication_only,
    )
    .await
    .map_err(map_token_err)?;
    let now = chrono::Utc::now().timestamp();
    state.notify(ChangeKind::Users);
    Ok((
        StatusCode::CREATED,
        Json(CreateTokenResp {
            id,
            name: req.name,
            plaintext,
            created_at: now,
            expires_at: req.expires_at,
            replication_only,
        }),
    ))
}

#[utoipa::path(
    delete,
    path = "/api/v1/user/tokens/{id}",
    params(("id" = i64, Path, description = "Token ID")),
    responses(
        (status = 204, description = "Token revoked"),
        (status = 404, description = "Token not found or doesn't belong to caller")
    ),
    tag = "user"
)]
pub(crate) async fn delete_token(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    api_token::delete(&state.pool, user.id, id)
        .await
        .map_err(|e| match e {
            haze_auth::TokenError::NotFound => ApiError::NotFound,
            haze_auth::TokenError::Db(e) => ApiError::Db(e),
        })?;
    state.notify(ChangeKind::Users);
    Ok(StatusCode::NO_CONTENT)
}

fn map_token_err(e: haze_auth::TokenError) -> ApiError {
    match e {
        haze_auth::TokenError::NotFound => ApiError::NotFound,
        haze_auth::TokenError::Db(e) => ApiError::Db(e),
    }
}
