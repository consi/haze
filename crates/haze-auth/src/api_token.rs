//! Per-user API tokens for Bearer authentication.
//!
//! Tokens are 32 random bytes, base64url-encoded with a `hzt_` prefix for
//! identification (so curl-leaked tokens can be grepped + revoked). The
//! plaintext is returned once at creation time; only its SHA-256 is stored.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use rand::{RngCore, rngs::OsRng};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

const TOKEN_PREFIX: &str = "hzt_";
const TOKEN_BYTES: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("token not found")]
    NotFound,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ApiTokenRow {
    pub id: i64,
    pub user_id: i64,
    pub name: String,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub last_used_at: Option<i64>,
}

pub async fn create(
    pool: &SqlitePool,
    user_id: i64,
    name: &str,
    expires_at: Option<i64>,
) -> Result<(i64, String), TokenError> {
    let mut raw = [0u8; TOKEN_BYTES];
    OsRng.fill_bytes(&mut raw);
    let plaintext = format!("{TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(raw));
    let hash = sha256(&plaintext);
    let now = Utc::now().timestamp();
    let id = sqlx::query(
        "INSERT INTO api_tokens (user_id, name, token_hash, created_at, expires_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(user_id)
    .bind(name)
    .bind(&hash[..])
    .bind(now)
    .bind(expires_at)
    .execute(pool)
    .await?
    .last_insert_rowid();
    Ok((id, plaintext))
}

pub async fn list_for_user(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<Vec<ApiTokenRow>, TokenError> {
    let rows: Vec<ApiTokenRow> = sqlx::query_as(
        "SELECT id, user_id, name, created_at, expires_at, last_used_at \
         FROM api_tokens WHERE user_id = ?1 ORDER BY created_at DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn delete(pool: &SqlitePool, user_id: i64, token_id: i64) -> Result<(), TokenError> {
    let rows = sqlx::query("DELETE FROM api_tokens WHERE id = ?1 AND user_id = ?2")
        .bind(token_id)
        .bind(user_id)
        .execute(pool)
        .await?
        .rows_affected();
    if rows == 0 {
        return Err(TokenError::NotFound);
    }
    Ok(())
}

/// Look up the `user_id` for a Bearer token. Updates `last_used_at` opportunistically.
pub async fn lookup_user(pool: &SqlitePool, plaintext: &str) -> Result<Option<i64>, TokenError> {
    if !plaintext.starts_with(TOKEN_PREFIX) {
        return Ok(None);
    }
    let hash = sha256(plaintext);
    let row: Option<(i64, Option<i64>)> =
        sqlx::query_as("SELECT user_id, expires_at FROM api_tokens WHERE token_hash = ?1")
            .bind(&hash[..])
            .fetch_optional(pool)
            .await?;
    let Some((user_id, expires_at)) = row else {
        return Ok(None);
    };
    let now = Utc::now().timestamp();
    if expires_at.is_some_and(|exp| exp < now) {
        return Ok(None);
    }
    let _ = sqlx::query("UPDATE api_tokens SET last_used_at = ?1 WHERE token_hash = ?2")
        .bind(now)
        .bind(&hash[..])
        .execute(pool)
        .await;
    Ok(Some(user_id))
}

fn sha256(s: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().into()
}
