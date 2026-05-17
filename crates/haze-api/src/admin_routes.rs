//! Admin-only user management: list / change role / reset password / delete.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use haze_auth::{CurrentUser, Role, hash, user};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{error::ApiError, error::ApiResult, state::AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/users", get(list_users).post(create_user))
        .route(
            "/users/{id}",
            axum::routing::patch(update_user).delete(delete_user),
        )
        .route("/users/{id}/password", post(reset_password))
        .route("/restart", post(restart))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/restart",
    responses(
        (status = 202, description = "Server will exit shortly; supervisor / cargo-watch should bring it back"),
        (status = 403, description = "Admin role required")
    ),
    tag = "admin"
)]
pub(crate) async fn restart(user_in: CurrentUser) -> ApiResult<StatusCode> {
    require_admin(&user_in)?;
    // Schedule the exit on a detached task so the HTTP response can flush
    // first. Process exit with code 0 plays nicely with systemd
    // `Restart=always` / `cargo watch` / any external supervisor that picks
    // up the binary again.
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        tracing::info!("admin-triggered restart: exiting");
        std::process::exit(0);
    });
    Ok(StatusCode::ACCEPTED)
}

#[derive(Serialize, ToSchema)]
pub(crate) struct AdminUserResp {
    pub id: i64,
    pub username: String,
    pub role: String,
    pub has_password: bool,
    pub created_at: i64,
    pub disabled_at: Option<i64>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateUserReq {
    pub username: String,
    pub password: String,
    pub role: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateUserReq {
    pub role: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct ResetPasswordReq {
    pub new_password: String,
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/users",
    responses(
        (status = 200, body = Vec<AdminUserResp>),
        (status = 403, description = "Admin role required")
    ),
    tag = "admin"
)]
pub(crate) async fn list_users(
    user_in: CurrentUser,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<AdminUserResp>>> {
    require_admin(&user_in)?;
    let rows = user::list_all(&state.pool).await?;
    Ok(Json(rows.into_iter().map(user_to_resp).collect()))
}

fn user_to_resp(r: haze_auth::user::UserRow) -> AdminUserResp {
    AdminUserResp {
        id: r.id,
        username: r.username,
        role: r.role,
        has_password: r.password_hash.is_some(),
        created_at: r.created_at,
        disabled_at: r.disabled_at,
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/users",
    request_body = CreateUserReq,
    responses(
        (status = 201, body = AdminUserResp, description = "User created"),
        (status = 403, description = "Admin role required"),
        (status = 409, description = "Username already exists"),
        (status = 422, description = "Validation error")
    ),
    tag = "admin"
)]
pub(crate) async fn create_user(
    user_in: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<CreateUserReq>,
) -> ApiResult<(StatusCode, Json<AdminUserResp>)> {
    require_admin(&user_in)?;
    let username = req.username.trim();
    if username.is_empty() {
        return Err(ApiError::Validation("username is required".into()));
    }
    if req.password.len() < 8 {
        return Err(ApiError::Validation(
            "password must be at least 8 characters".into(),
        ));
    }
    let role: Role = req
        .role
        .parse()
        .map_err(|_| ApiError::Validation(format!("unknown role '{}'", req.role)))?;
    let hashed = hash(&req.password)?;
    let id = match user::create(&state.pool, username, Some(&hashed), role).await {
        Ok(id) => id,
        Err(user::CreateError::Conflict) => {
            return Err(ApiError::Conflict("username already exists".into()));
        }
        Err(user::CreateError::Db(e)) => return Err(ApiError::Db(e)),
    };
    let row = user::find_by_id_any(&state.pool, id)
        .await?
        .ok_or_else(|| ApiError::Internal("created user vanished".into()))?;
    Ok((StatusCode::CREATED, Json(user_to_resp(row))))
}

#[utoipa::path(
    patch,
    path = "/api/v1/admin/users/{id}",
    params(("id" = i64, Path, description = "User id")),
    request_body = UpdateUserReq,
    responses(
        (status = 204, description = "Role updated"),
        (status = 403, description = "Admin role required"),
        (status = 404, description = "User not found"),
        (status = 409, description = "Would leave no active admins"),
        (status = 422, description = "Unknown role")
    ),
    tag = "admin"
)]
pub(crate) async fn update_user(
    user_in: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateUserReq>,
) -> ApiResult<StatusCode> {
    require_admin(&user_in)?;
    let new_role: Role = req
        .role
        .parse()
        .map_err(|_| ApiError::Validation(format!("unknown role '{}'", req.role)))?;

    let Some(target) = user::find_by_id_any(&state.pool, id).await? else {
        return Err(ApiError::NotFound);
    };
    let was_admin = target.role == "admin" && target.disabled_at.is_none();
    let stays_admin = matches!(new_role, Role::Admin);

    if was_admin && !stays_admin {
        guard_last_admin(&state, id).await?;
    }
    user::set_role(&state.pool, id, new_role).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/users/{id}/password",
    params(("id" = i64, Path, description = "User id")),
    request_body = ResetPasswordReq,
    responses(
        (status = 204, description = "Password reset"),
        (status = 403, description = "Admin role required"),
        (status = 404, description = "User not found"),
        (status = 422, description = "Password too short")
    ),
    tag = "admin"
)]
pub(crate) async fn reset_password(
    user_in: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<ResetPasswordReq>,
) -> ApiResult<StatusCode> {
    require_admin(&user_in)?;
    if req.new_password.len() < 8 {
        return Err(ApiError::Validation(
            "password must be at least 8 characters".into(),
        ));
    }
    if user::find_by_id_any(&state.pool, id).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    let hashed = hash(&req.new_password)?;
    user::set_password_hash(&state.pool, id, &hashed).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete,
    path = "/api/v1/admin/users/{id}",
    params(("id" = i64, Path, description = "User id")),
    responses(
        (status = 204, description = "User deleted"),
        (status = 400, description = "Cannot delete self"),
        (status = 403, description = "Admin role required"),
        (status = 404, description = "User not found"),
        (status = 409, description = "Would leave no active admins")
    ),
    tag = "admin"
)]
pub(crate) async fn delete_user(
    user_in: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    require_admin(&user_in)?;
    if id == user_in.id {
        return Err(ApiError::BadRequest(
            "cannot delete your own account".into(),
        ));
    }
    let Some(target) = user::find_by_id_any(&state.pool, id).await? else {
        return Err(ApiError::NotFound);
    };
    if target.role == "admin" && target.disabled_at.is_none() {
        guard_last_admin(&state, id).await?;
    }
    user::delete(&state.pool, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn require_admin(u: &CurrentUser) -> ApiResult<()> {
    if u.role.is_admin() {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

/// Refuse the operation if `id` is the sole remaining active admin.
async fn guard_last_admin(state: &AppState, _id: i64) -> ApiResult<()> {
    let count = user::active_admin_count(&state.pool).await?;
    if count <= 1 {
        return Err(ApiError::Conflict(
            "refusing to remove the last active admin".into(),
        ));
    }
    Ok(())
}
