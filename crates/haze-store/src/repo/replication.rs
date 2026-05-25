// The repo layer mostly mirrors the schema column-for-column; pedantic
// noise on docs and ergonomic closure patterns is silenced at the module
// level rather than at every site.
#![allow(
    clippy::too_long_first_doc_paragraph,
    clippy::doc_markdown,
    clippy::redundant_closure_for_method_calls,
    clippy::use_self
)]

//! Replication state: peers + rules + cursors + slot tracking.
//!
//! Two halves live in the same module because the destination tables
//! (`replication_peers`, `replication_rules`, `replication_cursors`,
//! `replication_group_map`) and the source tables (`replication_slots`,
//! `replication_slot_cursors`) share UUID encoding, pagination helpers,
//! and error types - splitting them would mean duplicating those.
//!
//! `Uuid::nil()` is the sentinel for "root group" in both
//! `source_group_uuid` and `dest_group_uuid` columns - SQLite NULLs are
//! awkward inside `UNIQUE` constraints, and the zero UUID is otherwise
//! unused since `Uuid::new_v4` can't produce it.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum ReplicationError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("replication peer not found")]
    PeerNotFound,
    #[error("replication rule not found")]
    RuleNotFound,
    #[error("replication slot not found")]
    SlotNotFound,
    #[error("a peer with that name already exists")]
    NameTaken,
    #[error("rule already exists for that (peer, source_group, dest_group) triple")]
    RuleDuplicate,
}

fn map_unique(e: sqlx::Error, on_unique: ReplicationError) -> ReplicationError {
    if let sqlx::Error::Database(db) = &e {
        if db.is_unique_violation() {
            return on_unique;
        }
    }
    ReplicationError::Db(e)
}

fn uuid_bytes(u: Uuid) -> Vec<u8> {
    u.as_bytes().to_vec()
}

fn parse_uuid(bytes: &[u8]) -> Uuid {
    Uuid::from_slice(bytes).unwrap_or(Uuid::nil())
}

// ─── Peers (destination side) ──────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ReplicationPeer {
    pub id: i64,
    pub uuid: Uuid,
    pub name: String,
    pub base_url: String,
    /// Plaintext bearer token. Never serialise this out of the repo layer.
    pub api_token: String,
    pub source_instance_uuid: Option<Uuid>,
    pub upstream_chain: Vec<Uuid>,
    pub tls_skip_verify: bool,
    pub reconcile_interval_secs: i64,
    pub created_at: i64,
    pub last_contact_at: Option<i64>,
    pub last_error: Option<String>,
    pub source_version: Option<String>,
    pub last_latency_ms: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct PeerRow {
    id: i64,
    uuid: Vec<u8>,
    name: String,
    base_url: String,
    api_token: String,
    source_instance_uuid: Option<Vec<u8>>,
    upstream_chain: String,
    tls_skip_verify: i64,
    reconcile_interval_secs: i64,
    created_at: i64,
    last_contact_at: Option<i64>,
    last_error: Option<String>,
    source_version: Option<String>,
    last_latency_ms: Option<i64>,
}

impl PeerRow {
    fn into_peer(self) -> Result<ReplicationPeer, ReplicationError> {
        let upstream_chain: Vec<String> = serde_json::from_str(&self.upstream_chain)?;
        let upstream_chain = upstream_chain
            .into_iter()
            .filter_map(|s| Uuid::parse_str(&s).ok())
            .collect();
        Ok(ReplicationPeer {
            id: self.id,
            uuid: parse_uuid(&self.uuid),
            name: self.name,
            base_url: self.base_url,
            api_token: self.api_token,
            source_instance_uuid: self.source_instance_uuid.map(|b| parse_uuid(&b)),
            upstream_chain,
            tls_skip_verify: self.tls_skip_verify != 0,
            reconcile_interval_secs: self.reconcile_interval_secs,
            created_at: self.created_at,
            last_contact_at: self.last_contact_at,
            last_error: self.last_error,
            source_version: self.source_version,
            last_latency_ms: self.last_latency_ms,
        })
    }
}

const PEER_COLS: &str = "id, uuid, name, base_url, api_token, source_instance_uuid, upstream_chain, source_version, last_latency_ms, \
     tls_skip_verify, reconcile_interval_secs, created_at, last_contact_at, last_error";

pub struct NewPeer<'a> {
    pub name: &'a str,
    pub base_url: &'a str,
    pub api_token: &'a str,
    pub source_instance_uuid: Option<Uuid>,
    pub upstream_chain: &'a [Uuid],
    pub tls_skip_verify: bool,
    pub reconcile_interval_secs: i64,
}

pub async fn create_peer(
    pool: &SqlitePool,
    new: NewPeer<'_>,
) -> Result<ReplicationPeer, ReplicationError> {
    let uuid = Uuid::new_v4();
    let chain_json = serde_json::to_string(
        &new.upstream_chain
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>(),
    )?;
    let now = Utc::now().timestamp();
    let res = sqlx::query(
        "INSERT INTO replication_peers (uuid, name, base_url, api_token, source_instance_uuid, \
                                        upstream_chain, tls_skip_verify, reconcile_interval_secs, \
                                        created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(uuid_bytes(uuid))
    .bind(new.name)
    .bind(new.base_url)
    .bind(new.api_token)
    .bind(new.source_instance_uuid.map(uuid_bytes))
    .bind(&chain_json)
    .bind(i64::from(new.tls_skip_verify))
    .bind(new.reconcile_interval_secs)
    .bind(now)
    .execute(pool)
    .await
    .map_err(|e| map_unique(e, ReplicationError::NameTaken))?;
    let id = res.last_insert_rowid();
    Ok(ReplicationPeer {
        id,
        uuid,
        name: new.name.into(),
        base_url: new.base_url.into(),
        api_token: new.api_token.into(),
        source_instance_uuid: new.source_instance_uuid,
        upstream_chain: new.upstream_chain.to_vec(),
        tls_skip_verify: new.tls_skip_verify,
        reconcile_interval_secs: new.reconcile_interval_secs,
        created_at: now,
        last_contact_at: None,
        last_error: None,
        source_version: None,
        last_latency_ms: None,
    })
}

pub async fn list_peers(
    pool: &SqlitePool,
    limit: i64,
    offset: i64,
) -> Result<(Vec<ReplicationPeer>, i64), ReplicationError> {
    let rows: Vec<PeerRow> = sqlx::query_as(&format!(
        "SELECT {PEER_COLS} FROM replication_peers ORDER BY name LIMIT ?1 OFFSET ?2"
    ))
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM replication_peers")
        .fetch_one(pool)
        .await?;
    let peers = rows
        .into_iter()
        .map(|r| r.into_peer())
        .collect::<Result<Vec<_>, _>>()?;
    Ok((peers, total))
}

pub async fn list_all_peers(pool: &SqlitePool) -> Result<Vec<ReplicationPeer>, ReplicationError> {
    let rows: Vec<PeerRow> = sqlx::query_as(&format!(
        "SELECT {PEER_COLS} FROM replication_peers ORDER BY id"
    ))
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(|r| r.into_peer()).collect()
}

pub async fn get_peer_by_uuid(
    pool: &SqlitePool,
    uuid: Uuid,
) -> Result<Option<ReplicationPeer>, ReplicationError> {
    let row: Option<PeerRow> = sqlx::query_as(&format!(
        "SELECT {PEER_COLS} FROM replication_peers WHERE uuid = ?1"
    ))
    .bind(uuid_bytes(uuid))
    .fetch_optional(pool)
    .await?;
    row.map(|r| r.into_peer()).transpose()
}

pub async fn get_peer_by_id(
    pool: &SqlitePool,
    id: i64,
) -> Result<Option<ReplicationPeer>, ReplicationError> {
    let row: Option<PeerRow> = sqlx::query_as(&format!(
        "SELECT {PEER_COLS} FROM replication_peers WHERE id = ?1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    row.map(|r| r.into_peer()).transpose()
}

#[derive(Debug, Default, Clone)]
pub struct PeerPatch<'a> {
    pub name: Option<&'a str>,
    pub api_token: Option<&'a str>,
    pub tls_skip_verify: Option<bool>,
    pub reconcile_interval_secs: Option<i64>,
    pub source_instance_uuid: Option<Option<Uuid>>,
    pub upstream_chain: Option<&'a [Uuid]>,
    pub last_contact_at: Option<Option<i64>>,
    pub last_error: Option<Option<String>>,
    pub source_version: Option<Option<&'a str>>,
    pub last_latency_ms: Option<Option<i64>>,
}

pub async fn update_peer(
    pool: &SqlitePool,
    uuid: Uuid,
    patch: PeerPatch<'_>,
) -> Result<(), ReplicationError> {
    if let Some(name) = patch.name {
        sqlx::query("UPDATE replication_peers SET name = ?1 WHERE uuid = ?2")
            .bind(name)
            .bind(uuid_bytes(uuid))
            .execute(pool)
            .await
            .map_err(|e| map_unique(e, ReplicationError::NameTaken))?;
    }
    if let Some(token) = patch.api_token {
        sqlx::query("UPDATE replication_peers SET api_token = ?1 WHERE uuid = ?2")
            .bind(token)
            .bind(uuid_bytes(uuid))
            .execute(pool)
            .await?;
    }
    if let Some(skip) = patch.tls_skip_verify {
        sqlx::query("UPDATE replication_peers SET tls_skip_verify = ?1 WHERE uuid = ?2")
            .bind(i64::from(skip))
            .bind(uuid_bytes(uuid))
            .execute(pool)
            .await?;
    }
    if let Some(secs) = patch.reconcile_interval_secs {
        sqlx::query("UPDATE replication_peers SET reconcile_interval_secs = ?1 WHERE uuid = ?2")
            .bind(secs)
            .bind(uuid_bytes(uuid))
            .execute(pool)
            .await?;
    }
    if let Some(src) = patch.source_instance_uuid {
        sqlx::query("UPDATE replication_peers SET source_instance_uuid = ?1 WHERE uuid = ?2")
            .bind(src.map(uuid_bytes))
            .bind(uuid_bytes(uuid))
            .execute(pool)
            .await?;
    }
    if let Some(chain) = patch.upstream_chain {
        let json = serde_json::to_string(&chain.iter().map(|u| u.to_string()).collect::<Vec<_>>())?;
        sqlx::query("UPDATE replication_peers SET upstream_chain = ?1 WHERE uuid = ?2")
            .bind(json)
            .bind(uuid_bytes(uuid))
            .execute(pool)
            .await?;
    }
    if let Some(ts) = patch.last_contact_at {
        sqlx::query("UPDATE replication_peers SET last_contact_at = ?1 WHERE uuid = ?2")
            .bind(ts)
            .bind(uuid_bytes(uuid))
            .execute(pool)
            .await?;
    }
    if let Some(v) = patch.source_version {
        sqlx::query("UPDATE replication_peers SET source_version = ?1 WHERE uuid = ?2")
            .bind(v)
            .bind(uuid_bytes(uuid))
            .execute(pool)
            .await?;
    }
    if let Some(ms) = patch.last_latency_ms {
        sqlx::query("UPDATE replication_peers SET last_latency_ms = ?1 WHERE uuid = ?2")
            .bind(ms)
            .bind(uuid_bytes(uuid))
            .execute(pool)
            .await?;
    }
    if let Some(err) = patch.last_error {
        sqlx::query("UPDATE replication_peers SET last_error = ?1 WHERE uuid = ?2")
            .bind(err)
            .bind(uuid_bytes(uuid))
            .execute(pool)
            .await?;
    }
    Ok(())
}

pub async fn delete_peer(pool: &SqlitePool, uuid: Uuid) -> Result<(), ReplicationError> {
    let rows = sqlx::query("DELETE FROM replication_peers WHERE uuid = ?1")
        .bind(uuid_bytes(uuid))
        .execute(pool)
        .await?
        .rows_affected();
    if rows == 0 {
        return Err(ReplicationError::PeerNotFound);
    }
    Ok(())
}

// ─── Rules (destination side) ──────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ReplicationRule {
    pub id: i64,
    pub uuid: Uuid,
    pub peer_id: i64,
    pub source_group_uuid: Uuid,
    pub dest_group_uuid: Uuid,
    pub slot_uuid: Option<Uuid>,
    pub enabled: bool,
    pub created_at: i64,
}

#[derive(sqlx::FromRow)]
struct RuleRow {
    id: i64,
    uuid: Vec<u8>,
    peer_id: i64,
    source_group_uuid: Vec<u8>,
    dest_group_uuid: Vec<u8>,
    slot_uuid: Option<Vec<u8>>,
    enabled: i64,
    created_at: i64,
}

impl From<RuleRow> for ReplicationRule {
    fn from(r: RuleRow) -> Self {
        ReplicationRule {
            id: r.id,
            uuid: parse_uuid(&r.uuid),
            peer_id: r.peer_id,
            source_group_uuid: parse_uuid(&r.source_group_uuid),
            dest_group_uuid: parse_uuid(&r.dest_group_uuid),
            slot_uuid: r.slot_uuid.map(|b| parse_uuid(&b)),
            enabled: r.enabled != 0,
            created_at: r.created_at,
        }
    }
}

const RULE_COLS: &str =
    "id, uuid, peer_id, source_group_uuid, dest_group_uuid, slot_uuid, enabled, created_at";

pub async fn create_rule(
    pool: &SqlitePool,
    peer_id: i64,
    source_group_uuid: Uuid,
    dest_group_uuid: Uuid,
    enabled: bool,
) -> Result<ReplicationRule, ReplicationError> {
    let uuid = Uuid::new_v4();
    let now = Utc::now().timestamp();
    let res = sqlx::query(
        "INSERT INTO replication_rules (uuid, peer_id, source_group_uuid, dest_group_uuid, \
                                        enabled, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(uuid_bytes(uuid))
    .bind(peer_id)
    .bind(uuid_bytes(source_group_uuid))
    .bind(uuid_bytes(dest_group_uuid))
    .bind(i64::from(enabled))
    .bind(now)
    .execute(pool)
    .await
    .map_err(|e| map_unique(e, ReplicationError::RuleDuplicate))?;
    Ok(ReplicationRule {
        id: res.last_insert_rowid(),
        uuid,
        peer_id,
        source_group_uuid,
        dest_group_uuid,
        slot_uuid: None,
        enabled,
        created_at: now,
    })
}

pub async fn list_rules(
    pool: &SqlitePool,
    peer_id: Option<i64>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<ReplicationRule>, i64), ReplicationError> {
    let (rows, total) = if let Some(pid) = peer_id {
        let r: Vec<RuleRow> = sqlx::query_as(&format!(
            "SELECT {RULE_COLS} FROM replication_rules WHERE peer_id = ?1 \
             ORDER BY id LIMIT ?2 OFFSET ?3"
        ))
        .bind(pid)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
        let t: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM replication_rules WHERE peer_id = ?1")
                .bind(pid)
                .fetch_one(pool)
                .await?;
        (r, t)
    } else {
        let r: Vec<RuleRow> = sqlx::query_as(&format!(
            "SELECT {RULE_COLS} FROM replication_rules ORDER BY id LIMIT ?1 OFFSET ?2"
        ))
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
        let t: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM replication_rules")
            .fetch_one(pool)
            .await?;
        (r, t)
    };
    Ok((rows.into_iter().map(Into::into).collect(), total))
}

pub async fn list_enabled_rules(
    pool: &SqlitePool,
) -> Result<Vec<ReplicationRule>, ReplicationError> {
    let rows: Vec<RuleRow> = sqlx::query_as(&format!(
        "SELECT {RULE_COLS} FROM replication_rules WHERE enabled = 1 ORDER BY id"
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

pub async fn get_rule_by_uuid(
    pool: &SqlitePool,
    uuid: Uuid,
) -> Result<Option<ReplicationRule>, ReplicationError> {
    let row: Option<RuleRow> = sqlx::query_as(&format!(
        "SELECT {RULE_COLS} FROM replication_rules WHERE uuid = ?1"
    ))
    .bind(uuid_bytes(uuid))
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

pub async fn set_rule_slot_uuid(
    pool: &SqlitePool,
    rule_id: i64,
    slot_uuid: Uuid,
) -> Result<(), ReplicationError> {
    sqlx::query("UPDATE replication_rules SET slot_uuid = ?1 WHERE id = ?2")
        .bind(uuid_bytes(slot_uuid))
        .bind(rule_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_rule_enabled(
    pool: &SqlitePool,
    uuid: Uuid,
    enabled: bool,
) -> Result<(), ReplicationError> {
    let rows = sqlx::query("UPDATE replication_rules SET enabled = ?1 WHERE uuid = ?2")
        .bind(i64::from(enabled))
        .bind(uuid_bytes(uuid))
        .execute(pool)
        .await?
        .rows_affected();
    if rows == 0 {
        return Err(ReplicationError::RuleNotFound);
    }
    Ok(())
}

pub async fn delete_rule(pool: &SqlitePool, uuid: Uuid) -> Result<(), ReplicationError> {
    let rows = sqlx::query("DELETE FROM replication_rules WHERE uuid = ?1")
        .bind(uuid_bytes(uuid))
        .execute(pool)
        .await?
        .rows_affected();
    if rows == 0 {
        return Err(ReplicationError::RuleNotFound);
    }
    Ok(())
}

// ─── Cursors (destination side) ────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicationCursor {
    pub rule_id: i64,
    pub host_uuid: Uuid,
    pub last_synced_ts: i64,
    pub last_attempt_at: Option<i64>,
    pub last_error: Option<String>,
    pub orphaned_at: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct CursorRow {
    rule_id: i64,
    host_uuid: Vec<u8>,
    last_synced_ts: i64,
    last_attempt_at: Option<i64>,
    last_error: Option<String>,
    orphaned_at: Option<i64>,
}

impl From<CursorRow> for ReplicationCursor {
    fn from(r: CursorRow) -> Self {
        ReplicationCursor {
            rule_id: r.rule_id,
            host_uuid: parse_uuid(&r.host_uuid),
            last_synced_ts: r.last_synced_ts,
            last_attempt_at: r.last_attempt_at,
            last_error: r.last_error,
            orphaned_at: r.orphaned_at,
        }
    }
}

pub async fn list_cursors_for_rule(
    pool: &SqlitePool,
    rule_id: i64,
) -> Result<Vec<ReplicationCursor>, ReplicationError> {
    let rows: Vec<CursorRow> = sqlx::query_as(
        "SELECT rule_id, host_uuid, last_synced_ts, last_attempt_at, last_error, orphaned_at \
         FROM replication_cursors WHERE rule_id = ?1",
    )
    .bind(rule_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

pub async fn get_cursor(
    pool: &SqlitePool,
    rule_id: i64,
    host_uuid: Uuid,
) -> Result<Option<ReplicationCursor>, ReplicationError> {
    let row: Option<CursorRow> = sqlx::query_as(
        "SELECT rule_id, host_uuid, last_synced_ts, last_attempt_at, last_error, orphaned_at \
         FROM replication_cursors WHERE rule_id = ?1 AND host_uuid = ?2",
    )
    .bind(rule_id)
    .bind(uuid_bytes(host_uuid))
    .fetch_optional(pool)
    .await?;
    Ok(row.map(Into::into))
}

pub async fn upsert_cursor(
    pool: &SqlitePool,
    rule_id: i64,
    host_uuid: Uuid,
    last_synced_ts: i64,
    last_error: Option<&str>,
) -> Result<(), ReplicationError> {
    let now = Utc::now().timestamp();
    sqlx::query(
        "INSERT INTO replication_cursors (rule_id, host_uuid, last_synced_ts, last_attempt_at, \
                                          last_error, orphaned_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, NULL) \
         ON CONFLICT(rule_id, host_uuid) DO UPDATE SET \
             last_synced_ts = excluded.last_synced_ts, \
             last_attempt_at = excluded.last_attempt_at, \
             last_error = excluded.last_error, \
             orphaned_at = NULL",
    )
    .bind(rule_id)
    .bind(uuid_bytes(host_uuid))
    .bind(last_synced_ts)
    .bind(now)
    .bind(last_error)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_cursor_orphaned(
    pool: &SqlitePool,
    rule_id: i64,
    host_uuid: Uuid,
) -> Result<(), ReplicationError> {
    let now = Utc::now().timestamp();
    sqlx::query(
        "UPDATE replication_cursors SET orphaned_at = ?1 \
         WHERE rule_id = ?2 AND host_uuid = ?3",
    )
    .bind(now)
    .bind(rule_id)
    .bind(uuid_bytes(host_uuid))
    .execute(pool)
    .await?;
    Ok(())
}

// ─── Group map (destination side) ──────────────────────────────────────────

pub async fn get_group_mapping(
    pool: &SqlitePool,
    rule_id: i64,
    source_group_uuid: Uuid,
) -> Result<Option<Uuid>, ReplicationError> {
    let row: Option<(Vec<u8>,)> = sqlx::query_as(
        "SELECT local_group_uuid FROM replication_group_map \
         WHERE rule_id = ?1 AND source_group_uuid = ?2",
    )
    .bind(rule_id)
    .bind(uuid_bytes(source_group_uuid))
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(b,)| parse_uuid(&b)))
}

pub async fn put_group_mapping(
    pool: &SqlitePool,
    rule_id: i64,
    source_group_uuid: Uuid,
    local_group_uuid: Uuid,
) -> Result<(), ReplicationError> {
    sqlx::query(
        "INSERT INTO replication_group_map (rule_id, source_group_uuid, local_group_uuid) \
         VALUES (?1, ?2, ?3) \
         ON CONFLICT(rule_id, source_group_uuid) DO UPDATE SET local_group_uuid = excluded.local_group_uuid",
    )
    .bind(rule_id)
    .bind(uuid_bytes(source_group_uuid))
    .bind(uuid_bytes(local_group_uuid))
    .execute(pool)
    .await?;
    Ok(())
}

// ─── Slots (source side) ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ReplicationSlot {
    pub id: i64,
    pub slot_uuid: Uuid,
    pub peer_instance_uuid: Uuid,
    pub peer_label: String,
    pub source_group_uuid: Uuid,
    pub replication_path: Vec<Uuid>,
    pub created_at: i64,
    pub last_stream_at: Option<i64>,
    /// When set, source has refused to serve this slot. The row stays
    /// around (preserving the peer's instance UUID so subsequent
    /// upserts can be rejected); the destination's worker sees a 403
    /// on every wire call until an admin unblocks via the Inbound UI.
    pub blocked_at: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct SlotRow {
    id: i64,
    slot_uuid: Vec<u8>,
    peer_instance_uuid: Vec<u8>,
    peer_label: String,
    source_group_uuid: Vec<u8>,
    replication_path: String,
    created_at: i64,
    last_stream_at: Option<i64>,
    blocked_at: Option<i64>,
}

impl SlotRow {
    fn into_slot(self) -> Result<ReplicationSlot, ReplicationError> {
        let path: Vec<String> = serde_json::from_str(&self.replication_path)?;
        let path: Vec<Uuid> = path
            .into_iter()
            .filter_map(|s| Uuid::parse_str(&s).ok())
            .collect();
        Ok(ReplicationSlot {
            id: self.id,
            slot_uuid: parse_uuid(&self.slot_uuid),
            peer_instance_uuid: parse_uuid(&self.peer_instance_uuid),
            peer_label: self.peer_label,
            source_group_uuid: parse_uuid(&self.source_group_uuid),
            replication_path: path,
            created_at: self.created_at,
            last_stream_at: self.last_stream_at,
            blocked_at: self.blocked_at,
        })
    }
}

const SLOT_COLS: &str = "id, slot_uuid, peer_instance_uuid, peer_label, source_group_uuid, \
                         replication_path, created_at, last_stream_at, blocked_at";

/// Upsert a slot keyed by (peer_instance_uuid, source_group_uuid). Returns
/// the (possibly newly-created) slot UUID. The destination is expected to
/// call this on every connect/reconnect; the `(peer_instance, source_group)`
/// uniqueness ensures we never create duplicates.
pub async fn upsert_slot(
    pool: &SqlitePool,
    peer_instance_uuid: Uuid,
    peer_label: &str,
    source_group_uuid: Uuid,
    replication_path: &[Uuid],
) -> Result<ReplicationSlot, ReplicationError> {
    let path_json = serde_json::to_string(
        &replication_path
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>(),
    )?;
    let now = Utc::now().timestamp();
    let existing: Option<SlotRow> = sqlx::query_as(&format!(
        "SELECT {SLOT_COLS} FROM replication_slots \
         WHERE peer_instance_uuid = ?1 AND source_group_uuid = ?2"
    ))
    .bind(uuid_bytes(peer_instance_uuid))
    .bind(uuid_bytes(source_group_uuid))
    .fetch_optional(pool)
    .await?;
    if let Some(row) = existing {
        sqlx::query(
            "UPDATE replication_slots SET peer_label = ?1, replication_path = ?2 \
             WHERE id = ?3",
        )
        .bind(peer_label)
        .bind(&path_json)
        .bind(row.id)
        .execute(pool)
        .await?;
        return Ok(ReplicationSlot {
            id: row.id,
            slot_uuid: parse_uuid(&row.slot_uuid),
            peer_instance_uuid,
            peer_label: peer_label.into(),
            source_group_uuid,
            replication_path: replication_path.to_vec(),
            created_at: row.created_at,
            last_stream_at: row.last_stream_at,
            blocked_at: row.blocked_at,
        });
    }
    let slot_uuid = Uuid::new_v4();
    let res = sqlx::query(
        "INSERT INTO replication_slots (slot_uuid, peer_instance_uuid, peer_label, \
                                        source_group_uuid, replication_path, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(uuid_bytes(slot_uuid))
    .bind(uuid_bytes(peer_instance_uuid))
    .bind(peer_label)
    .bind(uuid_bytes(source_group_uuid))
    .bind(&path_json)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(ReplicationSlot {
        id: res.last_insert_rowid(),
        slot_uuid,
        peer_instance_uuid,
        peer_label: peer_label.into(),
        source_group_uuid,
        replication_path: replication_path.to_vec(),
        created_at: now,
        last_stream_at: None,
        blocked_at: None,
    })
}

/// Mark a slot as administratively blocked.
///
/// Subsequent `POST /replication/slots` and stream/manifest/range calls
/// for this slot's `(peer_instance, source_group)` pair return 403 until
/// `unblock_slot` is called.
pub async fn block_slot(pool: &SqlitePool, slot_uuid: Uuid) -> Result<(), ReplicationError> {
    let now = Utc::now().timestamp();
    let rows = sqlx::query("UPDATE replication_slots SET blocked_at = ?1 WHERE slot_uuid = ?2")
        .bind(now)
        .bind(uuid_bytes(slot_uuid))
        .execute(pool)
        .await?
        .rows_affected();
    if rows == 0 {
        return Err(ReplicationError::SlotNotFound);
    }
    Ok(())
}

/// Clear the block flag on a slot. Idempotent.
pub async fn unblock_slot(pool: &SqlitePool, slot_uuid: Uuid) -> Result<(), ReplicationError> {
    let rows = sqlx::query("UPDATE replication_slots SET blocked_at = NULL WHERE slot_uuid = ?1")
        .bind(uuid_bytes(slot_uuid))
        .execute(pool)
        .await?
        .rows_affected();
    if rows == 0 {
        return Err(ReplicationError::SlotNotFound);
    }
    Ok(())
}

pub async fn list_slots(
    pool: &SqlitePool,
    limit: i64,
    offset: i64,
) -> Result<(Vec<ReplicationSlot>, i64), ReplicationError> {
    let rows: Vec<SlotRow> = sqlx::query_as(&format!(
        "SELECT {SLOT_COLS} FROM replication_slots ORDER BY peer_label, id \
         LIMIT ?1 OFFSET ?2"
    ))
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM replication_slots")
        .fetch_one(pool)
        .await?;
    let slots = rows
        .into_iter()
        .map(|r| r.into_slot())
        .collect::<Result<Vec<_>, _>>()?;
    Ok((slots, total))
}

pub async fn get_slot_by_uuid(
    pool: &SqlitePool,
    slot_uuid: Uuid,
) -> Result<Option<ReplicationSlot>, ReplicationError> {
    let row: Option<SlotRow> = sqlx::query_as(&format!(
        "SELECT {SLOT_COLS} FROM replication_slots WHERE slot_uuid = ?1"
    ))
    .bind(uuid_bytes(slot_uuid))
    .fetch_optional(pool)
    .await?;
    row.map(|r| r.into_slot()).transpose()
}

pub async fn touch_slot_stream(pool: &SqlitePool, slot_id: i64) -> Result<(), ReplicationError> {
    let now = Utc::now().timestamp();
    sqlx::query("UPDATE replication_slots SET last_stream_at = ?1 WHERE id = ?2")
        .bind(now)
        .bind(slot_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_slot(pool: &SqlitePool, slot_uuid: Uuid) -> Result<(), ReplicationError> {
    let rows = sqlx::query("DELETE FROM replication_slots WHERE slot_uuid = ?1")
        .bind(uuid_bytes(slot_uuid))
        .execute(pool)
        .await?
        .rows_affected();
    if rows == 0 {
        return Err(ReplicationError::SlotNotFound);
    }
    Ok(())
}

pub async fn record_slot_ack(
    pool: &SqlitePool,
    slot_id: i64,
    host_uuid: Uuid,
    last_ts: i64,
) -> Result<(), ReplicationError> {
    sqlx::query(
        "INSERT INTO replication_slot_cursors (slot_id, host_uuid, last_acked_ts) \
         VALUES (?1, ?2, ?3) \
         ON CONFLICT(slot_id, host_uuid) DO UPDATE SET last_acked_ts = excluded.last_acked_ts",
    )
    .bind(slot_id)
    .bind(uuid_bytes(host_uuid))
    .bind(last_ts)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct SlotCursorSummary {
    pub host_uuid: Uuid,
    pub last_acked_ts: i64,
}

pub async fn list_slot_cursors(
    pool: &SqlitePool,
    slot_id: i64,
) -> Result<Vec<SlotCursorSummary>, ReplicationError> {
    let rows: Vec<(Vec<u8>, i64)> = sqlx::query_as(
        "SELECT host_uuid, last_acked_ts FROM replication_slot_cursors WHERE slot_id = ?1",
    )
    .bind(slot_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(u, ts)| SlotCursorSummary {
            host_uuid: parse_uuid(&u),
            last_acked_ts: ts,
        })
        .collect())
}
