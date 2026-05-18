//! /api/v1/groups CRUD. All paths address groups by UUID, never the
//! internal DB id.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::get,
};
use haze_auth::CurrentUser;
use haze_store::repo::groups::{self, Group};
use serde::{Deserialize, Deserializer, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    ChangeKind, error::ApiError, error::ApiResult, middleware::ViewerAccess, state::AppState,
};

/// Maximum depth a group is allowed to live at (0 = root). Capped to keep
/// breadcrumbs readable and the materialized path columns bounded.
const MAX_GROUP_DEPTH: i64 = 7; // 8 levels total (depth 0..=7)

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/{uuid}", get(get_one).patch(update).delete(delete))
}

#[derive(Serialize, ToSchema)]
pub(crate) struct GroupResp {
    pub uuid: Uuid,
    pub parent_uuid: Option<Uuid>,
    pub display_name: String,
    pub depth: i64,
    pub created_at: i64,
}

/// Build a `GroupResp` from a repo `Group`.
///
/// The repo loads parents by id so we need a small lookup map to translate
/// `parent_id` to `parent_uuid` for the API.
pub(crate) fn to_resp(g: Group, parents: &std::collections::HashMap<i64, Uuid>) -> GroupResp {
    GroupResp {
        uuid: g.uuid_typed(),
        parent_uuid: g.parent_id.and_then(|pid| parents.get(&pid).copied()),
        display_name: g.display_name,
        depth: g.depth,
        created_at: g.created_at,
    }
}

pub(crate) fn build_parent_map(rows: &[Group]) -> std::collections::HashMap<i64, Uuid> {
    rows.iter().map(|g| (g.id, g.uuid_typed())).collect()
}

#[utoipa::path(
    get,
    path = "/api/v1/groups",
    responses((status = 200, body = Vec<GroupResp>, description = "All groups, ordered by display name")),
    tag = "groups"
)]
pub(crate) async fn list(
    _viewer: ViewerAccess,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<GroupResp>>> {
    let rows = groups::list_all(&state.pool).await?;
    let parents = build_parent_map(&rows);
    Ok(Json(
        rows.into_iter().map(|g| to_resp(g, &parents)).collect(),
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/groups/{uuid}",
    params(("uuid" = Uuid, Path, description = "Group UUID")),
    responses(
        (status = 200, body = GroupResp),
        (status = 404, description = "Group not found")
    ),
    tag = "groups"
)]
pub(crate) async fn get_one(
    _viewer: ViewerAccess,
    State(state): State<AppState>,
    Path(uuid): Path<Uuid>,
) -> ApiResult<Json<GroupResp>> {
    let g = groups::get_by_uuid(&state.pool, uuid)
        .await?
        .ok_or(ApiError::NotFound)?;
    // Resolve the parent's UUID directly rather than fetching the whole tree.
    let parent_uuid = match g.parent_id {
        Some(pid) => groups::get(&state.pool, pid).await?.map(|p| p.uuid_typed()),
        None => None,
    };
    Ok(Json(GroupResp {
        uuid: g.uuid_typed(),
        parent_uuid,
        display_name: g.display_name,
        depth: g.depth,
        created_at: g.created_at,
    }))
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateReq {
    /// Parent group UUID. Omit (or pass null) to put the group at the root.
    #[serde(default)]
    parent_uuid: Option<Uuid>,
    /// User-facing name. Need not be unique - the internal UUID is what the
    /// tree is keyed on.
    display_name: String,
}

#[utoipa::path(
    post,
    path = "/api/v1/groups",
    request_body = CreateReq,
    responses(
        (status = 201, body = GroupResp, description = "Group created"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Parent group not found"),
        (status = 422, description = "Validation error (empty name, depth limit)")
    ),
    tag = "groups"
)]
pub(crate) async fn create(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<CreateReq>,
) -> ApiResult<(StatusCode, Json<GroupResp>)> {
    if !user.role.can_edit_groups() {
        return Err(ApiError::Forbidden);
    }
    // Enforce the depth cap before we touch the DB. Checking the parent's
    // depth first means we never insert a group that exceeds the limit.
    let parent_resolved = if let Some(pu) = req.parent_uuid {
        let parent = groups::get_by_uuid(&state.pool, pu)
            .await?
            .ok_or(ApiError::NotFound)?;
        if parent.depth >= MAX_GROUP_DEPTH {
            return Err(ApiError::Validation(format!(
                "group nesting limited to {} levels",
                MAX_GROUP_DEPTH + 1
            )));
        }
        Some(parent)
    } else {
        None
    };
    let g = groups::create(
        &state.pool,
        parent_resolved.as_ref().map(|p| p.id),
        &req.display_name,
    )
    .await?;
    let parent_uuid = parent_resolved
        .as_ref()
        .map(haze_store::repo::groups::Group::uuid_typed);
    tracing::info!(
        group_uuid = %g.uuid_typed(),
        actor = %user.username,
        display_name = %g.display_name,
        depth = g.depth,
        ?parent_uuid,
        "group created"
    );
    state.notify(ChangeKind::Tree);
    Ok((
        StatusCode::CREATED,
        Json(GroupResp {
            uuid: g.uuid_typed(),
            parent_uuid,
            display_name: g.display_name,
            depth: g.depth,
            created_at: g.created_at,
        }),
    ))
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateReq {
    /// New display name. Optional: omit to leave it unchanged.
    display_name: Option<String>,
    /// New parent (by UUID). Tri-state:
    /// - field omitted -> don't change parent
    /// - field set to `null` -> move to the root
    /// - field set to a UUID -> move under that group (path of this group
    ///   and every descendant is rewritten in one transaction)
    #[serde(default, deserialize_with = "deserialize_present_option")]
    #[allow(clippy::option_option)]
    parent_uuid: Option<Option<Uuid>>,
}

#[allow(clippy::option_option)]
fn deserialize_present_option<'de, T, D>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    T: serde::Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

#[utoipa::path(
    patch,
    path = "/api/v1/groups/{uuid}",
    params(("uuid" = Uuid, Path, description = "Group UUID")),
    request_body = UpdateReq,
    responses(
        (status = 204, description = "Group updated"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Group or parent not found"),
        (status = 422, description = "Validation error (cycle, empty name, depth limit)")
    ),
    tag = "groups"
)]
pub(crate) async fn update(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(uuid): Path<Uuid>,
    Json(req): Json<UpdateReq>,
) -> ApiResult<StatusCode> {
    if !user.role.can_edit_groups() {
        return Err(ApiError::Forbidden);
    }
    let target = groups::get_by_uuid(&state.pool, uuid)
        .await?
        .ok_or(ApiError::NotFound)?;
    if let Some(name) = req.display_name {
        groups::update_display_name(&state.pool, target.id, &name).await?;
        tracing::info!(
            group_uuid = %target.uuid_typed(),
            actor = %user.username,
            new_display_name = %name,
            "group renamed"
        );
    }
    if let Some(new_parent_uuid) = req.parent_uuid {
        let new_parent_id = match new_parent_uuid {
            Some(pu) => {
                let parent = groups::get_by_uuid(&state.pool, pu)
                    .await?
                    .ok_or(ApiError::NotFound)?;
                // Depth check uses the subtree's own height too: moving a
                // tall subtree under a parent that's already deep can push
                // descendants past the limit.
                let subtree_height =
                    subtree_max_depth(&state.pool, target.id).await? - target.depth;
                if parent.depth + 1 + subtree_height > MAX_GROUP_DEPTH {
                    return Err(ApiError::Validation(format!(
                        "group nesting limited to {} levels",
                        MAX_GROUP_DEPTH + 1
                    )));
                }
                Some(parent.id)
            }
            None => None,
        };
        match groups::update_parent(&state.pool, target.id, new_parent_id).await {
            Ok(()) => {
                tracing::info!(
                    group_uuid = %target.uuid_typed(),
                    actor = %user.username,
                    new_parent_uuid = ?new_parent_uuid,
                    "group moved"
                );
            }
            Err(groups::MoveError::Cycle) => {
                return Err(ApiError::Validation(
                    "cannot move a group under itself or its descendants".into(),
                ));
            }
            Err(groups::MoveError::Group(e)) => return Err(e.into()),
        }
    }
    state.notify(ChangeKind::Tree);
    Ok(StatusCode::NO_CONTENT)
}

async fn subtree_max_depth(pool: &sqlx::SqlitePool, id: i64) -> ApiResult<i64> {
    let row: Option<(Option<i64>,)> = sqlx::query_as(
        "SELECT MAX(g2.depth) FROM groups g1 JOIN groups g2 ON g2.path LIKE g1.path || '%' \
         WHERE g1.id = ?1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|(d,)| d).unwrap_or(0))
}

#[utoipa::path(
    delete,
    path = "/api/v1/groups/{uuid}",
    params(("uuid" = Uuid, Path, description = "Group UUID")),
    responses(
        (status = 204, description = "Group deleted (cascade on children + host links)"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Group not found")
    ),
    tag = "groups"
)]
pub(crate) async fn delete(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(uuid): Path<Uuid>,
) -> ApiResult<StatusCode> {
    if !user.role.can_edit_groups() {
        return Err(ApiError::Forbidden);
    }
    let target = groups::get_by_uuid(&state.pool, uuid)
        .await?
        .ok_or(ApiError::NotFound)?;
    groups::delete(&state.pool, target.id).await?;
    tracing::info!(
        group_uuid = %target.uuid_typed(),
        actor = %user.username,
        display_name = %target.display_name,
        "group deleted"
    );
    state.notify(ChangeKind::Tree);
    Ok(StatusCode::NO_CONTENT)
}
