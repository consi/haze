//! `WebAuthn` passkey registration + authentication.
//!
//! Challenge state lives in an in-memory `DashMap` with a short TTL since
//! Haze is single-instance for v0.1; if/when we go HA, this moves into `SQLite`.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::Utc;
use dashmap::DashMap;
use sqlx::SqlitePool;
use uuid::Uuid;
use webauthn_rs::{
    Webauthn, WebauthnBuilder,
    prelude::{
        CreationChallengeResponse, CredentialID, DiscoverableAuthentication, DiscoverableKey,
        Passkey, PasskeyAuthentication, PasskeyRegistration, PublicKeyCredential,
        RegisterPublicKeyCredential, RequestChallengeResponse, Url,
    },
};

const CHALLENGE_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, thiserror::Error)]
pub enum PasskeyError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Webauthn(#[from] webauthn_rs::prelude::WebauthnError),
    #[error("invalid rp configuration: {0}")]
    BadRp(String),
    #[error("challenge expired or unknown")]
    ChallengeNotFound,
    #[error("invalid challenge state")]
    BadState,
    #[error("user not found")]
    UserNotFound,
    #[error(transparent)]
    Url(#[from] url::ParseError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub struct PasskeyService {
    webauthn: Webauthn,
    pending_reg: DashMap<Uuid, (Instant, PasskeyRegistration, i64)>,
    pending_auth: DashMap<Uuid, (Instant, PasskeyAuthentication)>,
    pending_disc: DashMap<Uuid, (Instant, DiscoverableAuthentication)>,
}

impl PasskeyService {
    /// `rp_id` is the registrable domain (e.g. `haze.example.com` or `localhost`
    /// for dev). `origin` is the served URL the browser sees.
    pub fn new(rp_id: &str, rp_name: &str, origin: &str) -> Result<Arc<Self>, PasskeyError> {
        let origin_url = Url::parse(origin)?;
        let builder = WebauthnBuilder::new(rp_id, &origin_url)
            .map_err(|e| PasskeyError::BadRp(e.to_string()))?
            .rp_name(rp_name);
        let webauthn = builder
            .build()
            .map_err(|e| PasskeyError::BadRp(e.to_string()))?;
        Ok(Arc::new(Self {
            webauthn,
            pending_reg: DashMap::new(),
            pending_auth: DashMap::new(),
            pending_disc: DashMap::new(),
        }))
    }

    /// Begin a discoverable (username-less) authentication ceremony. The
    /// challenge has empty `allowCredentials`; the browser presents all
    /// resident passkeys for the relying-party origin and the user picks one.
    pub fn begin_discoverable(&self) -> Result<(Uuid, RequestChallengeResponse), PasskeyError> {
        let (rcr, state) = self.webauthn.start_discoverable_authentication()?;
        let token = Uuid::new_v4();
        self.gc();
        self.pending_disc.insert(token, (Instant::now(), state));
        Ok((token, rcr))
    }

    /// Finish a discoverable authentication. We don't know which user we're
    /// dealing with up front - identify the credential ID from the response,
    /// look it up in the DB to find the owner + stored `Passkey`, then verify.
    pub async fn finish_discoverable(
        &self,
        pool: &SqlitePool,
        token: Uuid,
        credential: PublicKeyCredential,
    ) -> Result<i64, PasskeyError> {
        let (_, state) = self
            .pending_disc
            .remove(&token)
            .map(|(_, v)| v)
            .ok_or(PasskeyError::ChallengeNotFound)?;

        let (_user_uuid, cred_id) = self
            .webauthn
            .identify_discoverable_authentication(&credential)?;
        let cred_id_bytes: Vec<u8> = cred_id.as_ref().to_vec();

        let row: Option<(i64, String)> = sqlx::query_as(
            "SELECT user_id, passkey_json FROM passkey_credentials WHERE credential_id = ?1",
        )
        .bind(&cred_id_bytes)
        .fetch_optional(pool)
        .await?;
        let (user_id, passkey_json) = row.ok_or(PasskeyError::BadState)?;
        let passkey: Passkey = serde_json::from_str(&passkey_json)?;
        let discoverable_keys: Vec<DiscoverableKey> = vec![(&passkey).into()];

        let _result = self.webauthn.finish_discoverable_authentication(
            &credential,
            state,
            &discoverable_keys,
        )?;

        let now = Utc::now().timestamp();
        sqlx::query("UPDATE passkey_credentials SET last_used_at = ?1 WHERE credential_id = ?2")
            .bind(now)
            .bind(&cred_id_bytes)
            .execute(pool)
            .await?;
        Ok(user_id)
    }

    /// Begin a registration ceremony. Returns the challenge for the browser
    /// and an opaque token the caller passes back on `finish_register`.
    pub async fn begin_register(
        &self,
        pool: &SqlitePool,
        user_id: i64,
        username: &str,
    ) -> Result<(Uuid, CreationChallengeResponse), PasskeyError> {
        // Re-derive a stable per-user UUID so each user has one webauthn handle
        // across multiple credentials. Format: namespace + user_id bytes.
        let user_uuid = stable_user_uuid(user_id);

        // Exclude already-registered credentials so the user doesn't double-bind
        // the same authenticator.
        let exclude = list_credential_ids(pool, user_id).await?;
        let (challenge, state) = self.webauthn.start_passkey_registration(
            user_uuid,
            username,
            username,
            Some(exclude),
        )?;
        let token = Uuid::new_v4();
        self.gc();
        self.pending_reg
            .insert(token, (Instant::now(), state, user_id));
        Ok((token, challenge))
    }

    pub async fn finish_register(
        &self,
        pool: &SqlitePool,
        token: Uuid,
        credential: RegisterPublicKeyCredential,
        label: Option<&str>,
    ) -> Result<(), PasskeyError> {
        let (_, state, user_id) = self
            .pending_reg
            .remove(&token)
            .map(|(_, v)| v)
            .ok_or(PasskeyError::ChallengeNotFound)?;
        let passkey = self
            .webauthn
            .finish_passkey_registration(&credential, &state)?;
        let cred_id: Vec<u8> = passkey.cred_id().as_ref().to_vec();
        let json = serde_json::to_string(&passkey)?;
        let now = Utc::now().timestamp();
        sqlx::query(
            "INSERT INTO passkey_credentials (user_id, credential_id, passkey_json, label, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(user_id)
        .bind(&cred_id)
        .bind(&json)
        .bind(label)
        .bind(now)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Begin an authentication ceremony for the given user. Returns a token
    /// + challenge.
    pub async fn begin_authenticate(
        &self,
        pool: &SqlitePool,
        user_id: i64,
    ) -> Result<(Uuid, RequestChallengeResponse), PasskeyError> {
        let passkeys = load_passkeys(pool, user_id).await?;
        let (challenge, state) = self.webauthn.start_passkey_authentication(&passkeys)?;
        let token = Uuid::new_v4();
        self.gc();
        self.pending_auth.insert(token, (Instant::now(), state));
        Ok((token, challenge))
    }

    /// Verify the authentication response. On success returns the user id.
    pub async fn finish_authenticate(
        &self,
        pool: &SqlitePool,
        token: Uuid,
        credential: PublicKeyCredential,
    ) -> Result<i64, PasskeyError> {
        let (_, state) = self
            .pending_auth
            .remove(&token)
            .map(|(_, v)| v)
            .ok_or(PasskeyError::ChallengeNotFound)?;
        let auth = self
            .webauthn
            .finish_passkey_authentication(&credential, &state)?;
        // Look up the credential to find which user owns it.
        let cred_id_bytes: Vec<u8> = auth.cred_id().as_ref().to_vec();
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT user_id FROM passkey_credentials WHERE credential_id = ?1")
                .bind(&cred_id_bytes)
                .fetch_optional(pool)
                .await?;
        let user_id = row.ok_or(PasskeyError::BadState)?.0;
        // Update counter + last-used.
        let now = Utc::now().timestamp();
        sqlx::query("UPDATE passkey_credentials SET last_used_at = ?1 WHERE credential_id = ?2")
            .bind(now)
            .bind(&cred_id_bytes)
            .execute(pool)
            .await?;
        Ok(user_id)
    }

    fn gc(&self) {
        let now = Instant::now();
        self.pending_reg
            .retain(|_, (t, _, _)| now.duration_since(*t) < CHALLENGE_TTL);
        self.pending_auth
            .retain(|_, (t, _)| now.duration_since(*t) < CHALLENGE_TTL);
        self.pending_disc
            .retain(|_, (t, _)| now.duration_since(*t) < CHALLENGE_TTL);
    }
}

fn stable_user_uuid(user_id: i64) -> Uuid {
    // Deterministic v5 UUID from the user_id; namespace is arbitrary fixed bytes.
    const NS: Uuid = Uuid::from_bytes([
        0x68, 0x61, 0x7a, 0x65, 0xff, 0xff, 0x40, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x01,
    ]);
    Uuid::new_v5(&NS, &user_id.to_be_bytes())
}

async fn list_credential_ids(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<Vec<CredentialID>, PasskeyError> {
    let rows: Vec<(Vec<u8>,)> =
        sqlx::query_as("SELECT credential_id FROM passkey_credentials WHERE user_id = ?1")
            .bind(user_id)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|(b,)| CredentialID::from(b)).collect())
}

async fn load_passkeys(pool: &SqlitePool, user_id: i64) -> Result<Vec<Passkey>, PasskeyError> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT passkey_json FROM passkey_credentials WHERE user_id = ?1")
            .bind(user_id)
            .fetch_all(pool)
            .await?;
    let mut out = Vec::with_capacity(rows.len());
    for (json,) in rows {
        let pk: Passkey = serde_json::from_str(&json)?;
        out.push(pk);
    }
    Ok(out)
}
