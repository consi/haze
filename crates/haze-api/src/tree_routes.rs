//! /api/v1/tree — single round-trip that returns every group and every
//! host the user can see. Frontend sidebar uses this instead of the
//! two-call `listGroups()` + `listHosts()` dance so a tree reload after a
//! mutation is one HTTP request rather than two.

use axum::{Json, Router, extract::State, routing::get};
use haze_store::repo::{
    groups,
    hosts::{self, GroupFilter},
};
use serde::Serialize;
use utoipa::ToSchema;

use crate::{
    error::ApiResult, groups_routes, hosts_routes, middleware::ViewerAccess, state::AppState,
};

pub fn router() -> Router<AppState> {
    Router::new().route("/", get(tree))
}

#[derive(Serialize, ToSchema)]
pub(crate) struct TreeResp {
    pub groups: Vec<groups_routes::GroupResp>,
    pub hosts: Vec<hosts_routes::HostResp>,
}

#[utoipa::path(
    get,
    path = "/api/v1/tree",
    responses(
        (status = 200, body = TreeResp, description = "All groups + all hosts in a single response")
    ),
    tag = "tree"
)]
pub(crate) async fn tree(
    _viewer: ViewerAccess,
    State(state): State<AppState>,
) -> ApiResult<Json<TreeResp>> {
    // Both reads hit the same WAL'd SQLite pool; serializing them is
    // effectively free vs. the network round-trip we just saved.
    let groups_rows = groups::list_all(&state.pool).await?;
    let hosts_rows = hosts::list(&state.pool, GroupFilter::Any, None).await?;
    let parents = groups_routes::build_parent_map(&groups_rows);
    let groups = groups_rows
        .into_iter()
        .map(|g| groups_routes::to_resp(g, &parents))
        .collect();
    let hosts = hosts_rows
        .into_iter()
        .map(hosts_routes::HostResp::from)
        .collect();
    Ok(Json(TreeResp { groups, hosts }))
}
