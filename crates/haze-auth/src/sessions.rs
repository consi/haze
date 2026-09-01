//! Server-side sessions backed by `SQLite` + secure HTTP-only cookies.
//!
//! Cookie value is 32 random bytes base64url-encoded. The stored session ID is
//! `SHA-256(raw bytes)` so a DB leak does not yield live cookies. Idle timeout
//! 24 h sliding; absolute timeout 30 days.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::user::{CurrentUser, Role, find_by_id};

pub const COOKIE_NAME: &str = "haze_session";

const ID_BYTES: usize = 32;
const IDLE_SECS: i64 = 24 * 60 * 60;
const ABSOLUTE_SECS: i64 = 30 * 24 * 60 * 60;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    User(#[from] crate::user::UserError),
    #[error("invalid cookie")]
    BadCookie,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub user_id: i64,
    pub expires_at: i64,
}

/// Cookie value: opaque random bytes; lookup hashes before DB query.
pub struct SessionStore {
    pub pool: SqlitePool,
}

/// Create a new session for `user_id`; returns the cookie value (raw, urlsafe).
pub async fn create(
    pool: &SqlitePool,
    user_id: i64,
    user_agent: Option<&str>,
    remote_addr: Option<&str>,
) -> Result<String, SessionError> {
    let mut raw = [0u8; ID_BYTES];
    rand::fill(&mut raw);
    let id_hash = sha256(&raw);
    let now = Utc::now().timestamp();
    sqlx::query(
        "INSERT INTO sessions (id, user_id, created_at, expires_at, user_agent, remote_addr) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(&id_hash[..])
    .bind(user_id)
    .bind(now)
    .bind(now + ABSOLUTE_SECS)
    .bind(user_agent)
    .bind(remote_addr)
    .execute(pool)
    .await?;
    Ok(URL_SAFE_NO_PAD.encode(raw))
}

/// Look up the session by cookie; refresh idle expiry; return `(session, user)`.
pub async fn lookup(
    pool: &SqlitePool,
    cookie: &str,
) -> Result<Option<(Session, CurrentUser)>, SessionError> {
    let raw = URL_SAFE_NO_PAD
        .decode(cookie.as_bytes())
        .map_err(|_| SessionError::BadCookie)?;
    if raw.len() != ID_BYTES {
        return Err(SessionError::BadCookie);
    }
    let id_hash = sha256(&raw);

    let row: Option<(i64, i64, i64)> =
        sqlx::query_as("SELECT user_id, created_at, expires_at FROM sessions WHERE id = ?1")
            .bind(&id_hash[..])
            .fetch_optional(pool)
            .await?;
    let Some((user_id, created_at, expires_at)) = row else {
        return Ok(None);
    };

    let now = Utc::now().timestamp();
    if expires_at < now {
        // Absolute expiry; clean up.
        let _ = sqlx::query("DELETE FROM sessions WHERE id = ?1")
            .bind(&id_hash[..])
            .execute(pool)
            .await;
        return Ok(None);
    }
    // Idle check: if last refresh > IDLE_SECS ago, expire.
    if now - created_at > IDLE_SECS && expires_at < now + IDLE_SECS {
        // Refresh the absolute floor - capped at ABSOLUTE_SECS from initial creation.
        let new_expiry = (created_at + ABSOLUTE_SECS).min(now + IDLE_SECS);
        let _ = sqlx::query("UPDATE sessions SET expires_at = ?1 WHERE id = ?2")
            .bind(new_expiry)
            .bind(&id_hash[..])
            .execute(pool)
            .await;
    }

    let user = find_by_id(pool, user_id).await?;
    let Some(user) = user else { return Ok(None) };
    let role = user.role.parse::<Role>().unwrap_or(Role::Disabled);
    // Disabled users don't get a CurrentUser even with a still-valid session
    // cookie. Anyone disabled in-flight is logged out as far as the API
    // surface is concerned, even before the session expires naturally.
    if !role.is_active() {
        return Ok(None);
    }
    Ok(Some((
        Session {
            user_id,
            expires_at,
        },
        CurrentUser {
            id: user.id,
            username: user.username,
            role,
        },
    )))
}

pub async fn destroy(pool: &SqlitePool, cookie: &str) -> Result<(), SessionError> {
    let raw = URL_SAFE_NO_PAD
        .decode(cookie.as_bytes())
        .map_err(|_| SessionError::BadCookie)?;
    let id_hash = sha256(&raw);
    sqlx::query("DELETE FROM sessions WHERE id = ?1")
        .bind(&id_hash[..])
        .execute(pool)
        .await?;
    Ok(())
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

/// Build a `Set-Cookie` header value for a new session.
///
/// `path` is the `Path=` attribute - typically the normalized
/// `HAZE_BASE_URL` (e.g. `/haze`). Empty string is treated as `/` so the
/// root-mode behaviour is byte-identical to the previous hard-coded value.
///
/// `secure` controls the `Secure` attribute: browsers refuse to store a
/// `Secure` cookie sent over plain HTTP, so callers must pass `false` when
/// the request reached the server unencrypted (e.g. local dev) and `true`
/// behind an HTTPS reverse proxy. `SameSite=Lax` does not require `Secure`.
pub fn set_cookie(cookie_value: &str, path: &str, secure: bool) -> String {
    let p = if path.is_empty() { "/" } else { path };
    let secure_attr = if secure { "Secure; " } else { "" };
    format!(
        "{COOKIE_NAME}={cookie_value}; Path={p}; HttpOnly; SameSite=Lax; {secure_attr}Max-Age={ABSOLUTE_SECS}"
    )
}

/// Build a `Set-Cookie` header value that clears the session. The `Path`
/// and `Secure` attributes must match what was used to set the cookie -
/// see [`set_cookie`].
pub fn clear_cookie(path: &str, secure: bool) -> String {
    let p = if path.is_empty() { "/" } else { path };
    let secure_attr = if secure { "Secure; " } else { "" };
    format!("{COOKIE_NAME}=; Path={p}; HttpOnly; SameSite=Lax; {secure_attr}Max-Age=0")
}

/// Configure a background task that deletes expired sessions every hour.
pub async fn run_cleanup_task(pool: SqlitePool) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
    loop {
        tick.tick().await;
        let now = Utc::now().timestamp();
        let res = sqlx::query("DELETE FROM sessions WHERE expires_at < ?1")
            .bind(now)
            .execute(&pool)
            .await;
        if let Err(e) = res {
            tracing::warn!(error = ?e, "session cleanup failed");
        }
    }
}
