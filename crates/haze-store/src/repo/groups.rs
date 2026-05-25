//! Group tree: materialized path using opaque UUIDs.
//!
//! Display names are purely cosmetic; the user can rename freely or have
//! duplicates at any level. The materialized `path` is built from per-group
//! UUIDs, so subtree queries (`path LIKE '/abc.../'`) survive renames and
//! never collide.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum GroupError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("parent group not found")]
    ParentNotFound,
    #[error("group not found")]
    NotFound,
    #[error("display name must not be empty")]
    InvalidDisplayName,
    #[error("a sibling group with that name already exists")]
    NameTaken,
}

/// Tag the unique-index violation we expect from
/// `ux_groups_sibling_name`. Anything else is a real DB error.
fn map_name_unique(e: sqlx::Error) -> GroupError {
    if let sqlx::Error::Database(db) = &e {
        if db.is_unique_violation() {
            return GroupError::NameTaken;
        }
    }
    GroupError::Db(e)
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct Group {
    pub id: i64,
    pub uuid: Vec<u8>,
    pub parent_id: Option<i64>,
    pub display_name: String,
    pub path: String,
    pub depth: i64,
    pub created_at: i64,
    /// `Some(peer_id)` when this group was materialised by replication
    /// from a remote peer. Frontend renders dark grey and refuses any
    /// edit other than `display_name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replication_peer_id: Option<i64>,
}

impl Group {
    pub fn uuid_typed(&self) -> Uuid {
        Uuid::from_slice(&self.uuid).unwrap_or(Uuid::nil())
    }
}

pub async fn list_all(pool: &SqlitePool) -> Result<Vec<Group>, GroupError> {
    let rows: Vec<Group> = sqlx::query_as(
        "SELECT id, uuid, parent_id, display_name, path, depth, created_at, replication_peer_id \
         FROM groups ORDER BY display_name, id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get(pool: &SqlitePool, id: i64) -> Result<Option<Group>, GroupError> {
    let row: Option<Group> = sqlx::query_as(
        "SELECT id, uuid, parent_id, display_name, path, depth, created_at, replication_peer_id \
         FROM groups WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn get_by_uuid(pool: &SqlitePool, uuid: Uuid) -> Result<Option<Group>, GroupError> {
    let row: Option<Group> = sqlx::query_as(
        "SELECT id, uuid, parent_id, display_name, path, depth, created_at, replication_peer_id \
         FROM groups WHERE uuid = ?1",
    )
    .bind(uuid.as_bytes().to_vec())
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Internal helper: resolve a UUID to a row id.
///
/// Returns `None` when the group doesn't exist. Used by the hosts repo when
/// wiring up `host_groups` rows so the FK columns can stay integer-typed for
/// cheap joins.
pub async fn resolve_id(pool: &SqlitePool, uuid: Uuid) -> Result<Option<i64>, GroupError> {
    let row: Option<(i64,)> = sqlx::query_as("SELECT id FROM groups WHERE uuid = ?1")
        .bind(uuid.as_bytes().to_vec())
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(id,)| id))
}

pub async fn create_with_parent_uuid(
    pool: &SqlitePool,
    parent_uuid: Option<Uuid>,
    display_name: &str,
) -> Result<Group, GroupError> {
    let parent_id = match parent_uuid {
        Some(u) => Some(
            resolve_id(pool, u)
                .await?
                .ok_or(GroupError::ParentNotFound)?,
        ),
        None => None,
    };
    create(pool, parent_id, display_name).await
}

pub async fn create(
    pool: &SqlitePool,
    parent_id: Option<i64>,
    display_name: &str,
) -> Result<Group, GroupError> {
    let trimmed = display_name.trim();
    if trimmed.is_empty() {
        return Err(GroupError::InvalidDisplayName);
    }
    let uuid = Uuid::new_v4();
    // `simple()` gives 32 hex chars with no dashes - shorter and safer for
    // shell/URL/grep usage than the canonical form.
    let segment = uuid.simple().to_string();
    let uuid_bytes = uuid.as_bytes().to_vec();
    let (path, depth) = match parent_id {
        Some(pid) => {
            let parent = get(pool, pid).await?.ok_or(GroupError::ParentNotFound)?;
            (format!("{}{segment}/", parent.path), parent.depth + 1)
        }
        None => (format!("/{segment}/"), 0),
    };
    let now = Utc::now().timestamp();
    sqlx::query(
        "INSERT INTO groups (uuid, parent_id, display_name, path, depth, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(&uuid_bytes)
    .bind(parent_id)
    .bind(trimmed)
    .bind(&path)
    .bind(depth)
    .bind(now)
    .execute(pool)
    .await
    .map_err(map_name_unique)?;
    let id = sqlx::query_scalar::<_, i64>("SELECT last_insert_rowid()")
        .fetch_one(pool)
        .await?;
    Ok(Group {
        id,
        uuid: uuid_bytes,
        parent_id,
        display_name: trimmed.into(),
        path,
        depth,
        created_at: now,
        replication_peer_id: None,
    })
}

/// Sibling-name lookup for merge-by-name during replication ingest.
///
/// Returns the group whose `(parent_id, display_name COLLATE NOCASE)`
/// matches, or `None`. Bypasses the `ux_groups_sibling_name` unique-index
/// violation by detecting the dup before insertion.
pub async fn find_sibling_by_name(
    pool: &SqlitePool,
    parent_id: Option<i64>,
    display_name: &str,
) -> Result<Option<Group>, GroupError> {
    let row: Option<Group> = sqlx::query_as(
        "SELECT id, uuid, parent_id, display_name, path, depth, created_at, replication_peer_id \
         FROM groups \
         WHERE COALESCE(parent_id, -1) = COALESCE(?1, -1) \
           AND display_name = ?2 COLLATE NOCASE \
         LIMIT 1",
    )
    .bind(parent_id)
    .bind(display_name)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Create a replication-owned group.
///
/// Identical to `create` except the new row has `replication_peer_id`
/// set so the UI / API treats it as non-editable (apart from
/// `display_name`) and can render it differently.
pub async fn create_replicated(
    pool: &SqlitePool,
    parent_id: Option<i64>,
    display_name: &str,
    peer_id: i64,
) -> Result<Group, GroupError> {
    let trimmed = display_name.trim();
    if trimmed.is_empty() {
        return Err(GroupError::InvalidDisplayName);
    }
    let uuid = Uuid::new_v4();
    let segment = uuid.simple().to_string();
    let uuid_bytes = uuid.as_bytes().to_vec();
    let (path, depth) = match parent_id {
        Some(pid) => {
            let parent = get(pool, pid).await?.ok_or(GroupError::ParentNotFound)?;
            (format!("{}{segment}/", parent.path), parent.depth + 1)
        }
        None => (format!("/{segment}/"), 0),
    };
    let now = Utc::now().timestamp();
    sqlx::query(
        "INSERT INTO groups (uuid, parent_id, display_name, path, depth, created_at, replication_peer_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(&uuid_bytes)
    .bind(parent_id)
    .bind(trimmed)
    .bind(&path)
    .bind(depth)
    .bind(now)
    .bind(peer_id)
    .execute(pool)
    .await
    .map_err(map_name_unique)?;
    let id = sqlx::query_scalar::<_, i64>("SELECT last_insert_rowid()")
        .fetch_one(pool)
        .await?;
    Ok(Group {
        id,
        uuid: uuid_bytes,
        parent_id,
        display_name: trimmed.into(),
        path,
        depth,
        created_at: now,
        replication_peer_id: Some(peer_id),
    })
}

pub async fn update_display_name(
    pool: &SqlitePool,
    id: i64,
    display_name: &str,
) -> Result<(), GroupError> {
    let trimmed = display_name.trim();
    if trimmed.is_empty() {
        return Err(GroupError::InvalidDisplayName);
    }
    let rows = sqlx::query("UPDATE groups SET display_name = ?1 WHERE id = ?2")
        .bind(trimmed)
        .bind(id)
        .execute(pool)
        .await
        .map_err(map_name_unique)?
        .rows_affected();
    if rows == 0 {
        return Err(GroupError::NotFound);
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum MoveError {
    #[error(transparent)]
    Group(GroupError),
    #[error("cannot move a group under itself or one of its descendants")]
    Cycle,
}

impl From<GroupError> for MoveError {
    fn from(e: GroupError) -> Self {
        Self::Group(e)
    }
}

impl From<sqlx::Error> for MoveError {
    fn from(e: sqlx::Error) -> Self {
        Self::Group(GroupError::Db(e))
    }
}

/// Reparent a group.
///
/// Rewrites the materialized path of every descendant in a single
/// transaction. Passing `None` for `new_parent_id` moves the group to the
/// root. Cycles (moving X under one of X's descendants) are rejected.
pub async fn update_parent(
    pool: &SqlitePool,
    id: i64,
    new_parent_id: Option<i64>,
) -> Result<(), MoveError> {
    let group = get(pool, id).await?.ok_or(GroupError::NotFound)?;
    if Some(id) == new_parent_id {
        return Err(MoveError::Cycle);
    }

    let old_path = group.path.clone();
    let segment = old_path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string();
    let (new_path, new_depth) = match new_parent_id {
        Some(pid) => {
            let parent = get(pool, pid).await?.ok_or(GroupError::ParentNotFound)?;
            // Reject if the proposed new parent is within this group's subtree.
            if parent.path.starts_with(&old_path) {
                return Err(MoveError::Cycle);
            }
            (format!("{}{segment}/", parent.path), parent.depth + 1)
        }
        None => (format!("/{segment}/"), 0),
    };

    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE groups SET parent_id = ?1, path = ?2, depth = ?3 WHERE id = ?4")
        .bind(new_parent_id)
        .bind(&new_path)
        .bind(new_depth)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| MoveError::Group(map_name_unique(e)))?;

    // Rewrite descendants in a single UPDATE: any group whose path begins
    // with `old_path` (and isn't this group itself) has its prefix swapped
    // for `new_path`. The depth column shifts by the change in this group's
    // depth.
    let depth_delta = new_depth - group.depth;
    let pattern = format!("{old_path}%");
    sqlx::query(
        "UPDATE groups SET \
             path = ?1 || SUBSTR(path, ?2), \
             depth = depth + ?3 \
         WHERE id != ?4 AND path LIKE ?5",
    )
    .bind(&new_path)
    .bind((old_path.len() + 1) as i64)
    .bind(depth_delta)
    .bind(id)
    .bind(&pattern)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

pub async fn delete(pool: &SqlitePool, id: i64) -> Result<(), GroupError> {
    let rows = sqlx::query("DELETE FROM groups WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    if rows == 0 {
        return Err(GroupError::NotFound);
    }
    Ok(())
}
