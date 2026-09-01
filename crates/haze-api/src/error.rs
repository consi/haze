//! Uniform API error type: RFC 9457 problem+json.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    #[allow(dead_code)]
    Conflict(String),
    #[error("validation: {0}")]
    Validation(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Storage(#[from] haze_store::HzcError),
    #[error(transparent)]
    GroupRepo(#[from] haze_store::repo::groups::GroupError),
    #[error(transparent)]
    HostRepo(#[from] haze_store::repo::hosts::HostError),
    #[error(transparent)]
    Settings(#[from] haze_store::repo::settings::SettingsError),
    #[error(transparent)]
    Replication(#[from] haze_store::repo::replication::ReplicationError),
    #[error(transparent)]
    Password(#[from] haze_auth::PasswordError),
    #[error(transparent)]
    Session(#[from] haze_auth::SessionError),
    #[error(transparent)]
    User(#[from] haze_auth::user::UserError),
    #[error("internal: {0}")]
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let request_id = Uuid::new_v4();
        let (status, title) = match &self {
            Self::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad request"),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            Self::NotFound
            | Self::GroupRepo(haze_store::repo::groups::GroupError::NotFound)
            | Self::HostRepo(haze_store::repo::hosts::HostError::NotFound)
            | Self::Replication(
                haze_store::repo::replication::ReplicationError::PeerNotFound
                | haze_store::repo::replication::ReplicationError::RuleNotFound
                | haze_store::repo::replication::ReplicationError::SlotNotFound,
            ) => (StatusCode::NOT_FOUND, "not found"),
            Self::Conflict(_)
            | Self::GroupRepo(haze_store::repo::groups::GroupError::NameTaken)
            | Self::HostRepo(haze_store::repo::hosts::HostError::NameTaken)
            | Self::Replication(
                haze_store::repo::replication::ReplicationError::NameTaken
                | haze_store::repo::replication::ReplicationError::RuleDuplicate,
            ) => (StatusCode::CONFLICT, "conflict"),
            Self::Validation(_)
            | Self::GroupRepo(haze_store::repo::groups::GroupError::InvalidDisplayName)
            | Self::HostRepo(haze_store::repo::hosts::HostError::InvalidDisplayName) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "validation error")
            }
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
        };

        // Log full detail; surface only the safe message to the client.
        if status.is_server_error() {
            tracing::error!(%request_id, error = ?self, "API error");
        } else {
            tracing::debug!(%request_id, error = ?self, "API error");
        }

        let body = json!({
            "type": "about:blank",
            "title": title,
            "status": status.as_u16(),
            "detail": self.to_string(),
            "request_id": request_id.to_string(),
        });
        (status, Json(body)).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
