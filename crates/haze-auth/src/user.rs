//! User row + role types.

use std::str::FromStr;

use axum::{
    extract::FromRequestParts,
    http::{StatusCode, request::Parts},
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

#[derive(Debug, thiserror::Error)]
pub enum UserError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("unknown role '{0}'")]
    UnknownRole(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Can do anything.
    Admin,
    /// Everything except `/settings` and the user-management surfaces under it.
    User,
    /// Read-only: cannot mutate hosts/groups/alerts, cannot see alerting or settings UI.
    Reader,
    /// Soft-deleted: cannot log in. Data preserved for re-enablement.
    Disabled,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::User => "user",
            Self::Reader => "reader",
            Self::Disabled => "disabled",
        }
    }

    /// Can the role authenticate? `Disabled` is rejected at login.
    pub fn is_active(self) -> bool {
        !matches!(self, Self::Disabled)
    }

    pub fn is_admin(self) -> bool {
        matches!(self, Self::Admin)
    }

    /// Can see + visit `/settings`.
    pub fn can_see_settings(self) -> bool {
        matches!(self, Self::Admin)
    }

    /// Can see + visit `/alerting`.
    pub fn can_see_alerts(self) -> bool {
        matches!(self, Self::Admin | Self::User)
    }

    /// Can mutate alert rules.
    pub fn can_edit_alerts(self) -> bool {
        matches!(self, Self::Admin | Self::User)
    }

    /// Can create/edit/delete hosts.
    pub fn can_edit_hosts(self) -> bool {
        matches!(self, Self::Admin | Self::User)
    }

    /// Can create/edit/delete groups.
    pub fn can_edit_groups(self) -> bool {
        matches!(self, Self::Admin | Self::User)
    }
}

impl FromStr for Role {
    type Err = UserError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "admin" => Self::Admin,
            "user" => Self::User,
            "reader" => Self::Reader,
            "disabled" => Self::Disabled,
            other => return Err(UserError::UnknownRole(other.into())),
        })
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UserRow {
    pub id: i64,
    pub username: String,
    pub password_hash: Option<String>,
    pub role: String,
    pub created_at: i64,
    pub disabled_at: Option<i64>,
}

/// The authenticated user - attached to the request by the session middleware.
#[derive(Debug, Clone)]
pub struct CurrentUser {
    pub id: i64,
    pub username: String,
    pub role: Role,
}

impl<S> FromRequestParts<S> for CurrentUser
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> {
        std::future::ready(
            parts
                .extensions
                .get::<Self>()
                .cloned()
                .ok_or((StatusCode::UNAUTHORIZED, "not authenticated")),
        )
    }
}

pub async fn find_by_username(
    pool: &SqlitePool,
    username: &str,
) -> Result<Option<UserRow>, UserError> {
    let row: Option<UserRow> = sqlx::query_as(
        "SELECT id, username, password_hash, role, created_at, disabled_at \
         FROM users WHERE username = ?1 AND disabled_at IS NULL",
    )
    .bind(username)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn find_by_id(pool: &SqlitePool, id: i64) -> Result<Option<UserRow>, UserError> {
    let row: Option<UserRow> = sqlx::query_as(
        "SELECT id, username, password_hash, role, created_at, disabled_at \
         FROM users WHERE id = ?1 AND disabled_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Like [`find_by_id`] but also returns disabled users - needed by admin
/// flows that have to look up a user before re-enabling or deleting them.
pub async fn find_by_id_any(pool: &SqlitePool, id: i64) -> Result<Option<UserRow>, UserError> {
    let row: Option<UserRow> = sqlx::query_as(
        "SELECT id, username, password_hash, role, created_at, disabled_at \
         FROM users WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    #[error("username already exists")]
    Conflict,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

pub async fn create(
    pool: &SqlitePool,
    username: &str,
    password_hash: Option<&str>,
    role: Role,
) -> Result<i64, CreateError> {
    let now = chrono::Utc::now().timestamp();
    let res = sqlx::query(
        "INSERT INTO users (username, password_hash, role, created_at) \
         VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(username)
    .bind(password_hash)
    .bind(role.as_str())
    .bind(now)
    .execute(pool)
    .await;
    match res {
        Ok(r) => Ok(r.last_insert_rowid()),
        Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => {
            Err(CreateError::Conflict)
        }
        Err(e) => Err(CreateError::Db(e)),
    }
}

/// All user rows, including any that have been disabled. Sorted by username
/// for stable rendering in the admin UI.
pub async fn list_all(pool: &SqlitePool) -> Result<Vec<UserRow>, UserError> {
    let rows: Vec<UserRow> = sqlx::query_as(
        "SELECT id, username, password_hash, role, created_at, disabled_at \
         FROM users ORDER BY username",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Change a user's role. `Role::Disabled` is treated as a soft-disable -
/// `disabled_at` is set so login checks block them; everything else is just a
/// label update.
pub async fn set_role(pool: &SqlitePool, id: i64, role: Role) -> Result<(), UserError> {
    let now = chrono::Utc::now().timestamp();
    if role == Role::Disabled {
        sqlx::query("UPDATE users SET role = ?1, disabled_at = ?2 WHERE id = ?3")
            .bind(role.as_str())
            .bind(now)
            .bind(id)
            .execute(pool)
            .await?;
    } else {
        sqlx::query("UPDATE users SET role = ?1, disabled_at = NULL WHERE id = ?2")
            .bind(role.as_str())
            .bind(id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// Admin-driven password reset. Also wipes any active sessions so the user is
/// forced to re-authenticate with the new password.
pub async fn set_password_hash(
    pool: &SqlitePool,
    id: i64,
    new_hash: &str,
) -> Result<(), UserError> {
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE users SET password_hash = ?1 WHERE id = ?2")
        .bind(new_hash)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE user_id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Hard-delete the row.
///
/// Foreign keys cascade on `passkey_credentials`, `sessions`, and
/// `api_tokens`; `audit_log.actor_user_id` and `settings.updated_by` go to
/// `NULL`. The caller must enforce "don't delete the last active admin".
pub async fn delete(pool: &SqlitePool, id: i64) -> Result<(), UserError> {
    sqlx::query("DELETE FROM users WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Count remaining active admins. Used to prevent locking the org out of the
/// system by deleting or demoting the last admin.
pub async fn active_admin_count(pool: &SqlitePool) -> Result<i64, UserError> {
    let row: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM users WHERE role = 'admin' AND disabled_at IS NULL")
            .fetch_one(pool)
            .await?;
    Ok(row.0)
}
