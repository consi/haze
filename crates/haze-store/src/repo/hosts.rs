//! Hosts (a.k.a. probe targets).
//!
//! Hosts no longer carry a slug; the user only deals with a display name,
//! and the internal identifier is `uuid` (which is also the HZC directory
//! name on disk). Membership in groups is many-to-many via `host_groups`.

use std::collections::HashMap;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::repo::groups;

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("group not found")]
    GroupNotFound,
    #[error("host not found")]
    NotFound,
    #[error("display name must not be empty")]
    InvalidDisplayName,
    #[error("a host with that name already exists")]
    NameTaken,
}

/// Maps the `ux_hosts_display_name` unique-index violation to a typed
/// error so the API can return 409 with a friendly message.
fn map_name_unique(e: sqlx::Error) -> HostError {
    if let sqlx::Error::Database(db) = &e
        && db.is_unique_violation()
    {
        return HostError::NameTaken;
    }
    HostError::Db(e)
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Host {
    /// Internal DB id - retained inside the repo for cheap FK ops on
    /// `host_groups`, but the API layer never serialises this. Use `uuid`.
    #[serde(skip)]
    #[schema(value_type = i64)]
    pub id: i64,
    pub uuid: Vec<u8>,
    pub display_name: String,
    pub probe_type: String,
    pub probe_config: String,
    pub interval_secs: i64,
    pub samples_per_period: i64,
    pub chunk_window_secs: i64,
    pub enabled: i64,
    pub created_at: i64,
    /// UUIDs of the groups this host belongs to. Empty = root-level.
    pub group_uuids: Vec<Uuid>,
    /// `Some(peer_id)` when this host's metadata was created by replication
    /// from a remote peer. The frontend renders the row in dark grey and
    /// refuses probe-parameter edits when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replication_peer_id: Option<i64>,
}

impl Host {
    pub fn uuid_typed(&self) -> Uuid {
        Uuid::from_slice(&self.uuid).unwrap_or(Uuid::nil())
    }
}

#[derive(Debug, Clone)]
pub struct NewHost<'a> {
    pub display_name: &'a str,
    pub probe_type: &'a str,
    pub probe_config: &'a str, // JSON
    pub interval_secs: i64,
    pub samples_per_period: i64,
    /// Per-host chunk window for HZC storage. Captured here AND in
    /// `meta.json` on disk - existing hosts can't change theirs later.
    pub chunk_window_secs: i64,
    /// Group UUIDs to attach the host to. Empty = root-level.
    pub group_uuids: &'a [Uuid],
}

#[derive(sqlx::FromRow)]
struct HostRow {
    id: i64,
    uuid: Vec<u8>,
    display_name: String,
    probe_type: String,
    probe_config: String,
    interval_secs: i64,
    samples_per_period: i64,
    chunk_window_secs: i64,
    enabled: i64,
    created_at: i64,
    replication_peer_id: Option<i64>,
}

impl HostRow {
    fn with_groups(self, group_uuids: Vec<Uuid>) -> Host {
        Host {
            id: self.id,
            uuid: self.uuid,
            display_name: self.display_name,
            probe_type: self.probe_type,
            probe_config: self.probe_config,
            interval_secs: self.interval_secs,
            samples_per_period: self.samples_per_period,
            chunk_window_secs: self.chunk_window_secs,
            enabled: self.enabled,
            created_at: self.created_at,
            group_uuids,
            replication_peer_id: self.replication_peer_id,
        }
    }
}

const SELECT_COLS: &str = "id, uuid, display_name, probe_type, probe_config, interval_secs, \
     samples_per_period, chunk_window_secs, enabled, created_at, replication_peer_id";

/// Group filter values for `list`.
#[derive(Debug, Clone, Copy)]
pub enum GroupFilter {
    /// No filter: return every host.
    Any,
    /// Only hosts that belong to no groups (root-level).
    None,
    /// Only hosts that include this group directly in their memberships.
    Uuid(Uuid),
    /// Hosts in this group or anywhere in its descendant subtree (matched
    /// against the materialised `groups.path` so it works regardless of
    /// the tree's depth).
    Subtree(Uuid),
}

pub async fn list(
    pool: &SqlitePool,
    filter: GroupFilter,
    probe_type: Option<&str>,
) -> Result<Vec<Host>, HostError> {
    let mut sql = format!("SELECT DISTINCT {SELECT_COLS} FROM hosts h WHERE 1=1");
    match filter {
        GroupFilter::Any => {}
        GroupFilter::None => {
            sql.push_str(" AND NOT EXISTS (SELECT 1 FROM host_groups hg WHERE hg.host_id = h.id)");
        }
        GroupFilter::Uuid(_) => sql.push_str(
            " AND EXISTS (SELECT 1 FROM host_groups hg \
                          JOIN groups g ON hg.group_id = g.id \
                          WHERE hg.host_id = h.id AND g.uuid = ?)",
        ),
        GroupFilter::Subtree(_) => sql.push_str(
            // Match any group whose materialized path starts with the
            // root group's path. The scalar subquery resolves the root's
            // path once; LIKE 'path%' covers both the root itself and any
            // descendant since every path ends with '/' and child paths
            // are formed by appending '<child_uuid>/' to the parent's.
            " AND EXISTS (SELECT 1 FROM host_groups hg \
                          JOIN groups g ON hg.group_id = g.id \
                          WHERE hg.host_id = h.id \
                            AND g.path LIKE ( \
                                SELECT path || '%' FROM groups WHERE uuid = ? \
                            ))",
        ),
    }
    if probe_type.is_some() {
        sql.push_str(" AND probe_type = ?");
    }
    sql.push_str(" ORDER BY display_name, id");

    // Only constant SQL clauses/placeholders are interpolated; values are bound.
    let mut q = sqlx::query_as::<_, HostRow>(sqlx::AssertSqlSafe(sql));
    match filter {
        GroupFilter::Uuid(uuid) | GroupFilter::Subtree(uuid) => {
            q = q.bind(uuid.as_bytes().to_vec());
        }
        _ => {}
    }
    if let Some(pt) = probe_type {
        q = q.bind(pt);
    }
    let rows = q.fetch_all(pool).await?;
    attach_groups(pool, rows).await
}

pub async fn get_by_uuid(pool: &SqlitePool, uuid: Uuid) -> Result<Option<Host>, HostError> {
    let row: Option<HostRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {SELECT_COLS} FROM hosts WHERE uuid = ?1"
    )))
    .bind(uuid.as_bytes().to_vec())
    .fetch_optional(pool)
    .await?;
    match row {
        Some(r) => {
            let id = r.id;
            let mut groups = fetch_groups_for(pool, &[id]).await?;
            Ok(Some(r.with_groups(groups.remove(&id).unwrap_or_default())))
        }
        None => Ok(None),
    }
}

pub async fn create(pool: &SqlitePool, new: NewHost<'_>) -> Result<Host, HostError> {
    let display_name = new.display_name.trim();
    if display_name.is_empty() {
        return Err(HostError::InvalidDisplayName);
    }
    let uuid = Uuid::new_v4();
    let uuid_bytes = uuid.as_bytes().to_vec();
    let now = Utc::now().timestamp();

    // Resolve group UUIDs to internal ids up front so an unknown group fails
    // before we've written anything. Dedup first so repeated input doesn't
    // collide on the host_groups PK.
    let mut unique: Vec<Uuid> = Vec::with_capacity(new.group_uuids.len());
    for gu in new.group_uuids {
        if !unique.contains(gu) {
            unique.push(*gu);
        }
    }
    let mut resolved: Vec<(i64, Uuid)> = Vec::with_capacity(unique.len());
    for gu in &unique {
        let id = groups::resolve_id(pool, *gu)
            .await
            .map_err(|_| HostError::GroupNotFound)?
            .ok_or(HostError::GroupNotFound)?;
        resolved.push((id, *gu));
    }

    let mut tx = pool.begin().await?;
    let id = sqlx::query(
        "INSERT INTO hosts (uuid, display_name, probe_type, probe_config, \
                            interval_secs, samples_per_period, chunk_window_secs, \
                            enabled, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8)",
    )
    .bind(&uuid_bytes)
    .bind(display_name)
    .bind(new.probe_type)
    .bind(new.probe_config)
    .bind(new.interval_secs)
    .bind(new.samples_per_period)
    .bind(new.chunk_window_secs)
    .bind(now)
    .execute(&mut *tx)
    .await
    .map_err(map_name_unique)?
    .last_insert_rowid();

    for (gid, _) in &resolved {
        sqlx::query("INSERT INTO host_groups (host_id, group_id) VALUES (?1, ?2)")
            .bind(id)
            .bind(gid)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;

    Ok(Host {
        id,
        uuid: uuid_bytes,
        display_name: display_name.into(),
        probe_type: new.probe_type.into(),
        probe_config: new.probe_config.into(),
        interval_secs: new.interval_secs,
        samples_per_period: new.samples_per_period,
        chunk_window_secs: new.chunk_window_secs,
        enabled: 1,
        created_at: now,
        group_uuids: resolved.into_iter().map(|(_, u)| u).collect(),
        replication_peer_id: None,
    })
}

/// Create a replication-owned host (caller supplies the source UUID).
///
/// Identical to `create` except (a) the caller supplies the host UUID
/// (preserved from the source so cross-instance references remain
/// stable) and (b) `replication_peer_id` is set so the UI / API treat
/// the row as non-editable apart from `display_name`.
///
/// Returns `NameTaken` if the same `uuid` or `display_name` already
/// exists locally. Both situations are operator-recoverable (rename or
/// delete the colliding row, or remove the rule).
pub async fn create_replicated(
    pool: &SqlitePool,
    uuid: Uuid,
    new: NewHost<'_>,
    peer_id: i64,
) -> Result<Host, HostError> {
    let display_name = new.display_name.trim();
    if display_name.is_empty() {
        return Err(HostError::InvalidDisplayName);
    }
    let uuid_bytes = uuid.as_bytes().to_vec();
    let now = Utc::now().timestamp();

    let mut unique: Vec<Uuid> = Vec::with_capacity(new.group_uuids.len());
    for gu in new.group_uuids {
        if !unique.contains(gu) {
            unique.push(*gu);
        }
    }
    let mut resolved: Vec<(i64, Uuid)> = Vec::with_capacity(unique.len());
    for gu in &unique {
        let id = groups::resolve_id(pool, *gu)
            .await
            .map_err(|_| HostError::GroupNotFound)?
            .ok_or(HostError::GroupNotFound)?;
        resolved.push((id, *gu));
    }

    let mut tx = pool.begin().await?;
    let id = sqlx::query(
        "INSERT INTO hosts (uuid, display_name, probe_type, probe_config, \
                            interval_secs, samples_per_period, chunk_window_secs, \
                            enabled, created_at, replication_peer_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?9)",
    )
    .bind(&uuid_bytes)
    .bind(display_name)
    .bind(new.probe_type)
    .bind(new.probe_config)
    .bind(new.interval_secs)
    .bind(new.samples_per_period)
    .bind(new.chunk_window_secs)
    .bind(now)
    .bind(peer_id)
    .execute(&mut *tx)
    .await
    .map_err(map_name_unique)?
    .last_insert_rowid();

    for (gid, _) in &resolved {
        sqlx::query("INSERT INTO host_groups (host_id, group_id) VALUES (?1, ?2)")
            .bind(id)
            .bind(gid)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;

    Ok(Host {
        id,
        uuid: uuid_bytes,
        display_name: display_name.into(),
        probe_type: new.probe_type.into(),
        probe_config: new.probe_config.into(),
        interval_secs: new.interval_secs,
        samples_per_period: new.samples_per_period,
        chunk_window_secs: new.chunk_window_secs,
        enabled: 1,
        created_at: now,
        group_uuids: resolved.into_iter().map(|(_, u)| u).collect(),
        replication_peer_id: Some(peer_id),
    })
}

/// Partial host update.
///
/// Each `Some` field is applied; `None` leaves the existing value alone.
/// `chunk_window_secs` is intentionally NOT patchable - it's baked into
/// the host's HZC meta.json at creation time and migrating existing
/// chunks isn't supported.
#[derive(Debug, Default, Clone)]
pub struct HostPatch<'a> {
    pub display_name: Option<&'a str>,
    pub group_uuids: Option<&'a [Uuid]>,
    pub probe_type: Option<&'a str>,
    /// Already-serialised JSON. The API validates it's an object before
    /// reaching here.
    pub probe_config: Option<&'a str>,
    pub interval_secs: Option<i64>,
    pub samples_per_period: Option<i64>,
}

pub async fn update_by_uuid(
    pool: &SqlitePool,
    uuid: Uuid,
    patch: HostPatch<'_>,
) -> Result<Host, HostError> {
    let existing = get_by_uuid(pool, uuid).await?.ok_or(HostError::NotFound)?;

    if let Some(name) = patch.display_name {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(HostError::InvalidDisplayName);
        }
        sqlx::query("UPDATE hosts SET display_name = ?1 WHERE id = ?2")
            .bind(trimmed)
            .bind(existing.id)
            .execute(pool)
            .await
            .map_err(map_name_unique)?;
    }

    if let Some(pt) = patch.probe_type {
        sqlx::query("UPDATE hosts SET probe_type = ?1 WHERE id = ?2")
            .bind(pt)
            .bind(existing.id)
            .execute(pool)
            .await?;
    }

    if let Some(cfg) = patch.probe_config {
        sqlx::query("UPDATE hosts SET probe_config = ?1 WHERE id = ?2")
            .bind(cfg)
            .bind(existing.id)
            .execute(pool)
            .await?;
    }

    if let Some(iv) = patch.interval_secs {
        sqlx::query("UPDATE hosts SET interval_secs = ?1 WHERE id = ?2")
            .bind(iv)
            .bind(existing.id)
            .execute(pool)
            .await?;
    }

    if let Some(sp) = patch.samples_per_period {
        sqlx::query("UPDATE hosts SET samples_per_period = ?1 WHERE id = ?2")
            .bind(sp)
            .bind(existing.id)
            .execute(pool)
            .await?;
    }

    if let Some(group_uuids) = patch.group_uuids {
        set_groups(pool, uuid, group_uuids).await?;
    }

    get_by_uuid(pool, uuid).await?.ok_or(HostError::NotFound)
}

pub async fn delete_by_uuid(pool: &SqlitePool, uuid: Uuid) -> Result<Host, HostError> {
    let host = get_by_uuid(pool, uuid).await?.ok_or(HostError::NotFound)?;
    sqlx::query("DELETE FROM hosts WHERE id = ?1")
        .bind(host.id)
        .execute(pool)
        .await?;
    Ok(host)
}

/// Replace the host's group membership with the given list. Empty list =
/// detach the host from every group (it appears at the tree root).
#[allow(dead_code)]
pub async fn set_groups(
    pool: &SqlitePool,
    host_uuid: Uuid,
    group_uuids: &[Uuid],
) -> Result<(), HostError> {
    let host_id = get_by_uuid(pool, host_uuid)
        .await?
        .ok_or(HostError::NotFound)?
        .id;
    let mut unique: Vec<Uuid> = Vec::with_capacity(group_uuids.len());
    for u in group_uuids {
        if !unique.contains(u) {
            unique.push(*u);
        }
    }
    let mut resolved: Vec<i64> = Vec::with_capacity(unique.len());
    for u in &unique {
        let gid = groups::resolve_id(pool, *u)
            .await
            .map_err(|_| HostError::GroupNotFound)?
            .ok_or(HostError::GroupNotFound)?;
        resolved.push(gid);
    }

    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM host_groups WHERE host_id = ?1")
        .bind(host_id)
        .execute(&mut *tx)
        .await?;
    for gid in &resolved {
        sqlx::query("INSERT INTO host_groups (host_id, group_id) VALUES (?1, ?2)")
            .bind(host_id)
            .bind(gid)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn attach_groups(pool: &SqlitePool, rows: Vec<HostRow>) -> Result<Vec<Host>, HostError> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let mut groups = fetch_groups_for(pool, &ids).await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let g = groups.remove(&r.id).unwrap_or_default();
            r.with_groups(g)
        })
        .collect())
}

async fn fetch_groups_for(
    pool: &SqlitePool,
    host_ids: &[i64],
) -> Result<HashMap<i64, Vec<Uuid>>, HostError> {
    if host_ids.is_empty() {
        return Ok(HashMap::new());
    }
    // Only generated question-mark placeholders are interpolated; IDs are bound.
    let placeholders = std::iter::repeat_n("?", host_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT hg.host_id, g.uuid FROM host_groups hg \
         JOIN groups g ON g.id = hg.group_id \
         WHERE hg.host_id IN ({placeholders})"
    );
    // Only constant SQL clauses/placeholders are interpolated; values are bound.
    let mut q = sqlx::query_as::<_, (i64, Vec<u8>)>(sqlx::AssertSqlSafe(sql));
    for id in host_ids {
        q = q.bind(id);
    }
    let pairs: Vec<(i64, Vec<u8>)> = q.fetch_all(pool).await?;
    let mut out: HashMap<i64, Vec<Uuid>> = HashMap::new();
    for (host_id, uuid_bytes) in pairs {
        if let Ok(u) = Uuid::from_slice(&uuid_bytes) {
            out.entry(host_id).or_default().push(u);
        }
    }
    Ok(out)
}
