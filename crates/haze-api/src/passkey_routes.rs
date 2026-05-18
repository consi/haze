//! `WebAuthn` passkey endpoints - registration + authentication.

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use haze_auth::{CurrentUser, sessions, user::Role};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;
use webauthn_rs::prelude::{PublicKeyCredential, RegisterPublicKeyCredential};

use crate::{ChangeKind, error::ApiError, error::ApiResult, state::AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/register/begin", post(register_begin))
        .route("/register/finish", post(register_finish))
        .route("/login/begin", post(login_begin))
        .route("/login/finish", post(login_finish))
}

#[derive(Serialize, ToSchema)]
pub(crate) struct BeginResp {
    pub token: Uuid,
    /// Opaque `WebAuthn` challenge JSON - pass to `navigator.credentials.{create,get}`.
    #[schema(value_type = Object, additional_properties = true)]
    pub challenge: serde_json::Value,
}

#[utoipa::path(
    post,
    path = "/api/v1/auth/passkey/register/begin",
    responses(
        (status = 200, body = BeginResp, description = "Registration challenge"),
        (status = 401, description = "Not authenticated"),
        (status = 500, description = "Passkey service not configured (set HAZE_ORIGIN)")
    ),
    tag = "passkeys"
)]
pub(crate) async fn register_begin(
    user: CurrentUser,
    State(state): State<AppState>,
) -> ApiResult<Json<BeginResp>> {
    let svc = state.passkeys.as_ref().ok_or_else(|| {
        ApiError::Internal(
            "passkey service not configured - set HAZE_ORIGIN to the URL the browser sees \
                 (e.g. HAZE_ORIGIN=https://haze.example.com, or HAZE_ORIGIN=http://localhost:5173 \
                 for the Vite dev server) and restart the daemon"
                .into(),
        )
    })?;
    let (token, challenge) = svc
        .begin_register(&state.pool, user.id, &user.username)
        .await
        .map_err(map_pkerr)?;
    let challenge_json = serde_json::to_value(challenge)
        .map_err(|e| ApiError::Internal(format!("challenge serialise: {e}")))?;
    Ok(Json(BeginResp {
        token,
        challenge: challenge_json,
    }))
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct RegisterFinishReq {
    token: Uuid,
    #[schema(value_type = Object, additional_properties = true)]
    credential: RegisterPublicKeyCredential,
    #[serde(default)]
    label: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/v1/auth/passkey/register/finish",
    request_body = RegisterFinishReq,
    responses(
        (status = 204, description = "Passkey registered"),
        (status = 401, description = "Bad challenge or invalid credential")
    ),
    tag = "passkeys"
)]
pub(crate) async fn register_finish(
    _user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<RegisterFinishReq>,
) -> ApiResult<StatusCode> {
    let svc = state.passkeys.as_ref().ok_or_else(|| {
        ApiError::Internal(
            "passkey service not configured - set HAZE_ORIGIN to the URL the browser sees \
                 (e.g. HAZE_ORIGIN=https://haze.example.com, or HAZE_ORIGIN=http://localhost:5173 \
                 for the Vite dev server) and restart the daemon"
                .into(),
        )
    })?;
    svc.finish_register(&state.pool, req.token, req.credential, req.label.as_deref())
        .await
        .map_err(map_pkerr)?;
    state.notify(ChangeKind::Users);
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/api/v1/auth/passkey/login/begin",
    responses(
        (status = 200, body = BeginResp, description = "Discoverable-credential authentication challenge")
    ),
    tag = "passkeys"
)]
pub(crate) async fn login_begin(State(state): State<AppState>) -> ApiResult<Json<BeginResp>> {
    let svc = state.passkeys.as_ref().ok_or_else(|| {
        ApiError::Internal(
            "passkey service not configured - set HAZE_ORIGIN to the URL the browser sees \
                 (e.g. HAZE_ORIGIN=https://haze.example.com, or HAZE_ORIGIN=http://localhost:5173 \
                 for the Vite dev server) and restart the daemon"
                .into(),
        )
    })?;
    let (token, challenge) = svc.begin_discoverable().map_err(map_pkerr)?;
    let challenge_json = serde_json::to_value(challenge)
        .map_err(|e| ApiError::Internal(format!("challenge serialise: {e}")))?;
    Ok(Json(BeginResp {
        token,
        challenge: challenge_json,
    }))
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct LoginFinishReq {
    token: Uuid,
    #[schema(value_type = Object, additional_properties = true)]
    credential: PublicKeyCredential,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct LoginFinishResp {
    pub id: i64,
    pub username: String,
    pub role: String,
}

#[utoipa::path(
    post,
    path = "/api/v1/auth/passkey/login/finish",
    request_body = LoginFinishReq,
    responses(
        (status = 200, body = LoginFinishResp, description = "Authenticated; session cookie set"),
        (status = 401, description = "Invalid credential or expired challenge")
    ),
    tag = "passkeys"
)]
pub(crate) async fn login_finish(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<LoginFinishReq>,
) -> ApiResult<Response> {
    let svc = state.passkeys.as_ref().ok_or_else(|| {
        ApiError::Internal(
            "passkey service not configured - set HAZE_ORIGIN to the URL the browser sees \
                 (e.g. HAZE_ORIGIN=https://haze.example.com, or HAZE_ORIGIN=http://localhost:5173 \
                 for the Vite dev server) and restart the daemon"
                .into(),
        )
    })?;
    let user_id = svc
        .finish_discoverable(&state.pool, req.token, req.credential)
        .await
        .map_err(map_pkerr)?;
    let user = haze_auth::user::find_by_id(&state.pool, user_id)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    let role = user.role.parse::<Role>().unwrap_or(Role::Disabled);
    if !role.is_active() {
        return Err(ApiError::Unauthorized);
    }
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok());
    let cookie = sessions::create(&state.pool, user.id, ua, None).await?;
    let role_str = role.as_str().to_owned();
    let body = Json(LoginFinishResp {
        id: user.id,
        username: user.username,
        role: role_str,
    });
    let mut resp = body.into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        sessions::set_cookie(&cookie)
            .parse()
            .expect("cookie value is ascii"),
    );
    Ok(resp)
}

fn map_pkerr(e: haze_auth::PasskeyError) -> ApiError {
    use haze_auth::PasskeyError;
    match e {
        PasskeyError::ChallengeNotFound
        | PasskeyError::BadState
        | PasskeyError::Webauthn(_)
        | PasskeyError::UserNotFound => ApiError::Unauthorized,
        PasskeyError::Db(e) => ApiError::Db(e),
        other => ApiError::Internal(other.to_string()),
    }
}
