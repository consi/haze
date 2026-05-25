// The handler/wire types in this module are intentionally verbose and use
// closures + per-arm match branches throughout. Pedantic noise that fights
// the request/response shape is silenced module-wide rather than at every
// site.
#![allow(
    clippy::needless_pass_by_value,
    clippy::too_many_arguments,
    clippy::implicit_clone,
    clippy::redundant_clone,
    clippy::map_unwrap_or,
    clippy::needless_continue,
    clippy::match_same_arms,
    clippy::manual_let_else,
    clippy::single_match_else,
    clippy::collapsible_if,
    clippy::items_after_statements,
    clippy::used_underscore_binding,
    clippy::too_long_first_doc_paragraph,
    clippy::doc_markdown,
    clippy::redundant_closure_for_method_calls,
    clippy::use_self
)]

//! `/api/v1/replication/*` - cross-instance time-series replication.
//!
//! Split into three role-flavours, all served by this one module:
//!
//! - **Source-side wire endpoints** (`/instance-info`, `/slots*`) are
//!   consumed by a remote *destination* Haze that holds an admin bearer
//!   token for our instance. They expose what we have so the destination
//!   can mirror it locally.
//!
//! - **Destination-side config endpoints** (`/peers*`, `/rules*`) are
//!   browser-facing CRUD used by admins on the *destination* to manage
//!   what we pull from where.
//!
//! - **Inbound observability** (`/inbound*`) is shown on the *source* so an
//!   admin can see which destinations are tailing us and force-remove a
//!   slot if needed.
//!
//! Every endpoint requires `user.role.is_admin()` and returns 403 otherwise.
//! Replication intentionally bypasses `ViewerAccess` / public-mode: it's
//! a trust boundary between instances, not a viewer-facing surface.

use std::{collections::HashSet, convert::Infallible, time::Duration};

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{delete, get, patch, post},
};
use haze_auth::CurrentUser;
use haze_store::{
    Sample,
    repo::{
        groups,
        hosts::{self, GroupFilter},
        replication::{self, NewPeer, PeerPatch, ReplicationError},
    },
};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    ChangeKind,
    error::{ApiError, ApiResult},
    state::AppState,
};

/// Header destinations send on every wire call so the source can refuse
/// loops and store the path. Format: comma-separated list of
/// canonical-form UUIDs ending with the destination's instance UUID.
const PATH_HEADER: &str = "x-replication-path";

/// Header source emits on paginated list responses so the frontend can
/// render "N of M" footers without a second count request.
const TOTAL_HEADER: &str = "x-total-count";

const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 200;

pub fn router() -> Router<AppState> {
    Router::new()
        // ─── Source-side discovery ──────────────────────────────────
        .route("/instance-info", get(instance_info))
        // ─── Destination: peers ─────────────────────────────────────
        .route("/peers", get(list_peers).post(create_peer))
        .route(
            "/peers/{uuid}",
            get(get_peer).patch(update_peer).delete(delete_peer),
        )
        .route("/peers/{uuid}/test", post(test_peer))
        .route("/peers/{uuid}/groups-preview", get(peer_groups_preview))
        // ─── Destination: rules ─────────────────────────────────────
        .route("/rules", get(list_rules).post(create_rule))
        .route("/rules/{uuid}", patch(update_rule).delete(delete_rule))
        // ─── Source-side: inbound observability ─────────────────────
        .route("/inbound", get(list_inbound))
        .route("/inbound/{slot_uuid}", delete(delete_inbound))
        .route("/inbound/{slot_uuid}/unblock", post(unblock_inbound))
        // ─── Source-side: wire endpoints ────────────────────────────
        .route("/slots", post(upsert_slot))
        .route("/slots/{slot_uuid}", delete(delete_slot_route))
        .route("/slots/{slot_uuid}/manifest", get(slot_manifest))
        .route("/slots/{slot_uuid}/range", get(slot_range))
        .route("/slots/{slot_uuid}/ack", post(slot_ack))
        .route("/slots/{slot_uuid}/stream", get(slot_stream))
}

fn require_admin(user: &CurrentUser) -> ApiResult<()> {
    if user.role.is_admin() {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

fn parse_path_header(headers: &HeaderMap) -> Vec<Uuid> {
    headers
        .get(PATH_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|raw| {
            raw.split(',')
                .filter_map(|s| Uuid::parse_str(s.trim()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Format a chain as the value for `X-Replication-Path`. Used by the
/// destination-side worker when initiating wire calls to a peer.
pub fn render_path(chain: &[Uuid]) -> String {
    chain
        .iter()
        .map(Uuid::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn clamp_pagination(limit: Option<i64>, offset: Option<i64>) -> (i64, i64) {
    let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset = offset.unwrap_or(0).max(0);
    (limit, offset)
}

fn with_total<T: Serialize>(body: T, total: i64) -> Response {
    let mut resp = Json(body).into_response();
    if let Ok(v) = HeaderValue::from_str(&total.to_string()) {
        resp.headers_mut().insert(TOTAL_HEADER, v);
    }
    resp
}

// ────────────────────────────────────────────────────────────────────────
// /instance-info
// ────────────────────────────────────────────────────────────────────────

#[derive(Serialize, ToSchema)]
pub struct InstanceInfoResp {
    pub instance_uuid: Uuid,
    pub version: String,
    /// Instance UUIDs we ourselves pull from. Returned so the caller can
    /// extend their loop-check with the transitive closure before deciding
    /// to add us as a peer.
    pub upstream_chain: Vec<Uuid>,
}

#[utoipa::path(
    get,
    path = "/api/v1/replication/instance-info",
    responses(
        (status = 200, body = InstanceInfoResp),
        (status = 403, description = "Forbidden - admin role required")
    ),
    tag = "replication"
)]
pub async fn instance_info(
    user: CurrentUser,
    State(state): State<AppState>,
) -> ApiResult<Json<InstanceInfoResp>> {
    require_admin(&user)?;
    // Transitive upstream chain: the union of every locally configured
    // peer's source_instance_uuid + its captured upstream_chain.
    let peers = replication::list_all_peers(&state.pool).await?;
    let mut chain: Vec<Uuid> = Vec::new();
    let mut seen: HashSet<Uuid> = HashSet::new();
    for p in &peers {
        for u in p.source_instance_uuid.iter().chain(p.upstream_chain.iter()) {
            if seen.insert(*u) {
                chain.push(*u);
            }
        }
    }
    Ok(Json(InstanceInfoResp {
        instance_uuid: state.instance_uuid,
        version: env!("CARGO_PKG_VERSION").to_string(),
        upstream_chain: chain,
    }))
}

// ────────────────────────────────────────────────────────────────────────
// Peers (destination side)
// ────────────────────────────────────────────────────────────────────────

#[derive(Serialize, ToSchema)]
pub struct PeerResp {
    pub uuid: Uuid,
    pub name: String,
    pub base_url: String,
    pub source_instance_uuid: Option<Uuid>,
    pub upstream_chain: Vec<Uuid>,
    pub tls_skip_verify: bool,
    pub reconcile_interval_secs: i64,
    pub created_at: i64,
    pub last_contact_at: Option<i64>,
    pub last_error: Option<String>,
    /// Source's `CARGO_PKG_VERSION` from the last successful
    /// `/instance-info`. Surfaced so the Status column can show the
    /// same shape as the manual Test result without an extra click.
    pub source_version: Option<String>,
    /// Wall-clock latency of the last `/instance-info` round trip.
    pub last_latency_ms: Option<i64>,
}

impl From<replication::ReplicationPeer> for PeerResp {
    fn from(p: replication::ReplicationPeer) -> Self {
        Self {
            uuid: p.uuid,
            name: p.name,
            base_url: p.base_url,
            source_instance_uuid: p.source_instance_uuid,
            upstream_chain: p.upstream_chain,
            tls_skip_verify: p.tls_skip_verify,
            reconcile_interval_secs: p.reconcile_interval_secs,
            created_at: p.created_at,
            last_contact_at: p.last_contact_at,
            last_error: p.last_error,
            source_version: p.source_version,
            last_latency_ms: p.last_latency_ms,
        }
    }
}

#[derive(Deserialize)]
pub struct PageQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/api/v1/replication/peers",
    params(
        ("limit" = Option<i64>, Query, description = "Page size (default 20, max 200)"),
        ("offset" = Option<i64>, Query, description = "Row offset (default 0)")
    ),
    responses(
        (status = 200, body = Vec<PeerResp>, description = "Configured peers; X-Total-Count carries the total"),
        (status = 403, description = "Forbidden")
    ),
    tag = "replication"
)]
pub async fn list_peers(
    user: CurrentUser,
    State(state): State<AppState>,
    Query(q): Query<PageQuery>,
) -> ApiResult<Response> {
    require_admin(&user)?;
    let (limit, offset) = clamp_pagination(q.limit, q.offset);
    let (rows, total) = replication::list_peers(&state.pool, limit, offset).await?;
    let body: Vec<PeerResp> = rows.into_iter().map(Into::into).collect();
    Ok(with_total(body, total))
}

#[utoipa::path(
    get,
    path = "/api/v1/replication/peers/{uuid}",
    params(("uuid" = Uuid, Path, description = "Peer UUID")),
    responses(
        (status = 200, body = PeerResp),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Peer not found")
    ),
    tag = "replication"
)]
pub async fn get_peer(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(uuid): Path<Uuid>,
) -> ApiResult<Json<PeerResp>> {
    require_admin(&user)?;
    let p = replication::get_peer_by_uuid(&state.pool, uuid)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(p.into()))
}

#[derive(Deserialize, ToSchema)]
pub struct CreatePeerReq {
    pub name: String,
    /// Base URL of the source Haze, including scheme and port if non-default.
    /// e.g. `https://haze.example.com` or `http://10.0.0.5:8080`.
    pub base_url: String,
    /// Plaintext admin bearer (`hzt_…`) issued on the source. Stored as-is.
    /// Never returned by GET; rotate via PATCH.
    pub api_token: String,
    #[serde(default)]
    pub tls_skip_verify: bool,
    #[serde(default = "default_reconcile_interval")]
    pub reconcile_interval_secs: i64,
}

fn default_reconcile_interval() -> i64 {
    300
}

#[utoipa::path(
    post,
    path = "/api/v1/replication/peers",
    request_body = CreatePeerReq,
    responses(
        (status = 201, body = PeerResp, description = "Peer registered; source identity captured via /instance-info"),
        (status = 403, description = "Forbidden"),
        (status = 409, description = "Name already in use"),
        (status = 422, description = "Validation error or replication loop detected"),
        (status = 502, description = "Source unreachable / token rejected during pairing")
    ),
    tag = "replication"
)]
pub async fn create_peer(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<CreatePeerReq>,
) -> ApiResult<(StatusCode, Json<PeerResp>)> {
    require_admin(&user)?;
    let name = req.name.trim();
    if name.is_empty() {
        return Err(ApiError::Validation("name must not be empty".into()));
    }
    if !(req.base_url.starts_with("http://") || req.base_url.starts_with("https://")) {
        return Err(ApiError::Validation(
            "base_url must include http:// or https://".into(),
        ));
    }
    if !req.api_token.starts_with("hzt_") {
        return Err(ApiError::Validation(
            "api_token must be a hzt_… bearer token from the source".into(),
        ));
    }
    if req.reconcile_interval_secs < 30 || req.reconcile_interval_secs > 86_400 {
        return Err(ApiError::Validation(
            "reconcile_interval_secs must be between 30 and 86400".into(),
        ));
    }

    // Pair up: ask the source who it is so we can capture the chain and
    // refuse a loop right at registration time. Surfaces the most common
    // failure (wrong URL / bad token) as a 502 with the underlying error.
    let info = match fetch_instance_info(&req.base_url, &req.api_token, req.tls_skip_verify).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(actor = %user.username, base_url = %req.base_url, error = %e,
                "replication peer pairing failed");
            return Err(ApiError::BadRequest(format!(
                "could not reach source for pairing: {e}"
            )));
        }
    };
    if info.instance_uuid == state.instance_uuid
        || info.upstream_chain.contains(&state.instance_uuid)
    {
        return Err(ApiError::Validation(format!(
            "would create a replication loop: instance {} is already upstream of {}",
            state.instance_uuid, info.instance_uuid,
        )));
    }

    let chain: Vec<Uuid> = info.upstream_chain.clone();
    let peer = replication::create_peer(
        &state.pool,
        NewPeer {
            name,
            base_url: &req.base_url,
            api_token: &req.api_token,
            source_instance_uuid: Some(info.instance_uuid),
            upstream_chain: &chain,
            tls_skip_verify: req.tls_skip_verify,
            reconcile_interval_secs: req.reconcile_interval_secs,
        },
    )
    .await?;
    tracing::info!(
        actor = %user.username,
        peer_uuid = %peer.uuid,
        name = %peer.name,
        base_url = %peer.base_url,
        source_instance_uuid = %info.instance_uuid,
        chain_len = chain.len(),
        "replication peer added"
    );
    state.notify(ChangeKind::Replication);
    Ok((StatusCode::CREATED, Json(peer.into())))
}

#[derive(Deserialize, ToSchema)]
pub struct UpdatePeerReq {
    pub name: Option<String>,
    pub api_token: Option<String>,
    pub tls_skip_verify: Option<bool>,
    pub reconcile_interval_secs: Option<i64>,
}

#[utoipa::path(
    patch,
    path = "/api/v1/replication/peers/{uuid}",
    params(("uuid" = Uuid, Path)),
    request_body = UpdatePeerReq,
    responses(
        (status = 204, description = "Peer updated"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Peer not found"),
        (status = 422, description = "Validation error")
    ),
    tag = "replication"
)]
pub async fn update_peer(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(uuid): Path<Uuid>,
    Json(req): Json<UpdatePeerReq>,
) -> ApiResult<StatusCode> {
    require_admin(&user)?;
    replication::get_peer_by_uuid(&state.pool, uuid)
        .await?
        .ok_or(ApiError::NotFound)?;
    if let Some(token) = &req.api_token {
        if !token.starts_with("hzt_") {
            return Err(ApiError::Validation(
                "api_token must be a hzt_… bearer token".into(),
            ));
        }
    }
    if let Some(s) = req.reconcile_interval_secs {
        if !(30..=86_400).contains(&s) {
            return Err(ApiError::Validation(
                "reconcile_interval_secs must be between 30 and 86400".into(),
            ));
        }
    }
    replication::update_peer(
        &state.pool,
        uuid,
        PeerPatch {
            name: req.name.as_deref(),
            api_token: req.api_token.as_deref(),
            tls_skip_verify: req.tls_skip_verify,
            reconcile_interval_secs: req.reconcile_interval_secs,
            ..Default::default()
        },
    )
    .await?;
    tracing::info!(
        actor = %user.username,
        peer_uuid = %uuid,
        rotated_token = req.api_token.is_some(),
        renamed = req.name.is_some(),
        "replication peer updated"
    );
    state.notify(ChangeKind::Replication);
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete,
    path = "/api/v1/replication/peers/{uuid}",
    params(("uuid" = Uuid, Path)),
    responses(
        (status = 204, description = "Peer removed (rules cascade; replicated hosts/groups detach)"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Peer not found")
    ),
    tag = "replication"
)]
pub async fn delete_peer(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(uuid): Path<Uuid>,
) -> ApiResult<StatusCode> {
    require_admin(&user)?;
    let peer = replication::get_peer_by_uuid(&state.pool, uuid)
        .await?
        .ok_or(ApiError::NotFound)?;
    // Best-effort: tell each of the peer's slot-uuids on the source side to
    // tear down. Done before the local delete so a failed call doesn't
    // leave orphan slots upstream. The worker will redo this on next run
    // if any call fails (because the rules will already be gone).
    if let Ok((rules, _)) = replication::list_rules(&state.pool, Some(peer.id), 10_000, 0).await {
        let client = http_client(peer.tls_skip_verify);
        for r in rules {
            if let Some(slot_uuid) = r.slot_uuid {
                let _ = client
                    .delete(format!(
                        "{}/api/v1/replication/slots/{slot_uuid}",
                        peer.base_url
                    ))
                    .bearer_auth(&peer.api_token)
                    .send()
                    .await;
            }
        }
    }
    replication::delete_peer(&state.pool, uuid).await?;
    tracing::info!(actor = %user.username, peer_uuid = %uuid, name = %peer.name,
        "replication peer removed");
    state.notify(ChangeKind::Replication);
    state.notify(ChangeKind::Tree);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize, ToSchema)]
pub struct PeerTestResp {
    pub ok: bool,
    pub source_instance_uuid: Option<Uuid>,
    pub source_version: Option<String>,
    pub latency_ms: u64,
    pub error: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/v1/replication/peers/{uuid}/test",
    params(("uuid" = Uuid, Path)),
    responses(
        (status = 200, body = PeerTestResp),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Peer not found")
    ),
    tag = "replication"
)]
pub async fn test_peer(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(uuid): Path<Uuid>,
) -> ApiResult<Json<PeerTestResp>> {
    require_admin(&user)?;
    let peer = replication::get_peer_by_uuid(&state.pool, uuid)
        .await?
        .ok_or(ApiError::NotFound)?;
    let started = std::time::Instant::now();
    match fetch_instance_info(&peer.base_url, &peer.api_token, peer.tls_skip_verify).await {
        Ok(info) => {
            tracing::info!(
                actor = %user.username, peer_uuid = %uuid,
                source = %info.instance_uuid, latency_ms = started.elapsed().as_millis() as u64,
                "replication peer test ok"
            );
            Ok(Json(PeerTestResp {
                ok: true,
                source_instance_uuid: Some(info.instance_uuid),
                source_version: Some(info.version),
                latency_ms: started.elapsed().as_millis() as u64,
                error: None,
            }))
        }
        Err(e) => {
            tracing::warn!(actor = %user.username, peer_uuid = %uuid, error = %e,
                "replication peer test failed");
            Ok(Json(PeerTestResp {
                ok: false,
                source_instance_uuid: None,
                source_version: None,
                latency_ms: started.elapsed().as_millis() as u64,
                error: Some(e.to_string()),
            }))
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct GroupPreviewResp {
    pub uuid: Uuid,
    pub parent_uuid: Option<Uuid>,
    pub display_name: String,
    pub depth: i64,
}

#[utoipa::path(
    get,
    path = "/api/v1/replication/peers/{uuid}/groups-preview",
    params(("uuid" = Uuid, Path)),
    responses(
        (status = 200, body = Vec<GroupPreviewResp>),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Peer not found"),
        (status = 502, description = "Source unreachable")
    ),
    tag = "replication"
)]
pub async fn peer_groups_preview(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(uuid): Path<Uuid>,
) -> ApiResult<Json<Vec<GroupPreviewResp>>> {
    require_admin(&user)?;
    let peer = replication::get_peer_by_uuid(&state.pool, uuid)
        .await?
        .ok_or(ApiError::NotFound)?;
    let client = http_client(peer.tls_skip_verify);
    let resp = client
        .get(format!("{}/api/v1/groups", peer.base_url))
        .bearer_auth(&peer.api_token)
        .send()
        .await
        .map_err(|e| ApiError::BadRequest(format!("source unreachable: {e}")))?;
    if !resp.status().is_success() {
        return Err(ApiError::BadRequest(format!(
            "source returned {}",
            resp.status()
        )));
    }
    let raw: Vec<serde_json::Value> = resp
        .json()
        .await
        .map_err(|e| ApiError::BadRequest(format!("source returned bad JSON: {e}")))?;
    let out = raw
        .into_iter()
        .filter_map(|v| {
            Some(GroupPreviewResp {
                uuid: Uuid::parse_str(v.get("uuid")?.as_str()?).ok()?,
                parent_uuid: v
                    .get("parent_uuid")
                    .and_then(|p| p.as_str())
                    .and_then(|s| Uuid::parse_str(s).ok()),
                display_name: v.get("display_name")?.as_str()?.to_string(),
                depth: v
                    .get("depth")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0),
            })
        })
        .collect();
    Ok(Json(out))
}

// ────────────────────────────────────────────────────────────────────────
// Rules (destination side)
// ────────────────────────────────────────────────────────────────────────

#[derive(Serialize, ToSchema)]
pub struct RuleResp {
    pub uuid: Uuid,
    pub peer_uuid: Uuid,
    pub peer_name: String,
    /// `nil` UUID means "root".
    pub source_group_uuid: Uuid,
    pub dest_group_uuid: Uuid,
    pub slot_uuid: Option<Uuid>,
    pub enabled: bool,
    pub created_at: i64,
    /// `MAX(last_synced_ts)` across every host this rule has touched.
    /// Frontend uses `now() - latest_ingested_ts` to render a live "lag"
    /// counter next to the rule. `None` when no cursor has advanced yet
    /// (rule just created, or no samples seen since pairing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_ingested_ts: Option<i64>,
    /// Active host count this rule is currently shadowing.
    pub host_count: usize,
    /// Most recent worker error string, if any. Lets the UI surface
    /// "auth failed" / "source unreachable" without an extra endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Deserialize)]
pub struct RulesListQuery {
    pub peer_uuid: Option<Uuid>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/api/v1/replication/rules",
    params(
        ("peer_uuid" = Option<Uuid>, Query, description = "Filter to one peer"),
        ("limit" = Option<i64>, Query),
        ("offset" = Option<i64>, Query)
    ),
    responses(
        (status = 200, body = Vec<RuleResp>),
        (status = 403, description = "Forbidden")
    ),
    tag = "replication"
)]
pub async fn list_rules(
    user: CurrentUser,
    State(state): State<AppState>,
    Query(q): Query<RulesListQuery>,
) -> ApiResult<Response> {
    require_admin(&user)?;
    let (limit, offset) = clamp_pagination(q.limit, q.offset);
    let peer_id = match q.peer_uuid {
        Some(u) => Some(
            replication::get_peer_by_uuid(&state.pool, u)
                .await?
                .ok_or(ApiError::NotFound)?
                .id,
        ),
        None => None,
    };
    let (rules, total) = replication::list_rules(&state.pool, peer_id, limit, offset).await?;
    let peers = replication::list_all_peers(&state.pool).await?;
    let peer_by_id: std::collections::HashMap<i64, (Uuid, String, Option<String>)> = peers
        .into_iter()
        .map(|p| (p.id, (p.uuid, p.name, p.last_error)))
        .collect();
    let mut body: Vec<RuleResp> = Vec::with_capacity(rules.len());
    for r in rules {
        let Some((pu, pn, last_err)) = peer_by_id.get(&r.peer_id) else {
            continue;
        };
        let cursors = replication::list_cursors_for_rule(&state.pool, r.id).await?;
        let active: Vec<&_> = cursors.iter().filter(|c| c.orphaned_at.is_none()).collect();
        let latest_ingested_ts = active.iter().map(|c| c.last_synced_ts).max();
        body.push(RuleResp {
            uuid: r.uuid,
            peer_uuid: *pu,
            peer_name: pn.clone(),
            source_group_uuid: r.source_group_uuid,
            dest_group_uuid: r.dest_group_uuid,
            slot_uuid: r.slot_uuid,
            enabled: r.enabled,
            created_at: r.created_at,
            latest_ingested_ts,
            host_count: active.len(),
            last_error: last_err.clone(),
        });
    }
    Ok(with_total(body, total))
}

#[derive(Deserialize, ToSchema)]
pub struct CreateRuleReq {
    pub peer_uuid: Uuid,
    /// `null` = root group on the source.
    #[serde(default)]
    pub source_group_uuid: Option<Uuid>,
    /// `null` = root group locally.
    #[serde(default)]
    pub dest_group_uuid: Option<Uuid>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

#[utoipa::path(
    post,
    path = "/api/v1/replication/rules",
    request_body = CreateRuleReq,
    responses(
        (status = 201, body = RuleResp),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Peer or local destination group not found"),
        (status = 409, description = "Rule already exists for that mapping")
    ),
    tag = "replication"
)]
pub async fn create_rule(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<CreateRuleReq>,
) -> ApiResult<(StatusCode, Json<RuleResp>)> {
    require_admin(&user)?;
    let peer = replication::get_peer_by_uuid(&state.pool, req.peer_uuid)
        .await?
        .ok_or(ApiError::NotFound)?;
    // Validate dest group exists locally (if not root).
    if let Some(dg) = req.dest_group_uuid {
        groups::get_by_uuid(&state.pool, dg)
            .await?
            .ok_or(ApiError::NotFound)?;
    }
    let src = req.source_group_uuid.unwrap_or(Uuid::nil());
    let dst = req.dest_group_uuid.unwrap_or(Uuid::nil());
    let rule = replication::create_rule(&state.pool, peer.id, src, dst, req.enabled).await?;
    tracing::info!(
        actor = %user.username,
        rule_uuid = %rule.uuid,
        peer_uuid = %peer.uuid,
        peer_name = %peer.name,
        source_group_uuid = %src,
        dest_group_uuid = %dst,
        enabled = rule.enabled,
        "replication rule created; worker will pair on next manager cycle"
    );
    state.notify(ChangeKind::Replication);
    Ok((
        StatusCode::CREATED,
        Json(RuleResp {
            uuid: rule.uuid,
            peer_uuid: peer.uuid,
            peer_name: peer.name,
            source_group_uuid: src,
            dest_group_uuid: dst,
            slot_uuid: None,
            enabled: rule.enabled,
            created_at: rule.created_at,
            latest_ingested_ts: None,
            host_count: 0,
            last_error: None,
        }),
    ))
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateRuleReq {
    pub enabled: bool,
}

#[utoipa::path(
    patch,
    path = "/api/v1/replication/rules/{uuid}",
    params(("uuid" = Uuid, Path)),
    request_body = UpdateRuleReq,
    responses(
        (status = 204),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Rule not found")
    ),
    tag = "replication"
)]
pub async fn update_rule(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(uuid): Path<Uuid>,
    Json(req): Json<UpdateRuleReq>,
) -> ApiResult<StatusCode> {
    require_admin(&user)?;
    replication::set_rule_enabled(&state.pool, uuid, req.enabled).await?;
    tracing::info!(actor = %user.username, rule_uuid = %uuid, enabled = req.enabled,
        "replication rule toggled");
    state.notify(ChangeKind::Replication);
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete,
    path = "/api/v1/replication/rules/{uuid}",
    params(("uuid" = Uuid, Path)),
    responses(
        (status = 204, description = "Rule removed; source slot deletion attempted"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Rule not found")
    ),
    tag = "replication"
)]
pub async fn delete_rule(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(uuid): Path<Uuid>,
) -> ApiResult<StatusCode> {
    require_admin(&user)?;
    let rule = replication::get_rule_by_uuid(&state.pool, uuid)
        .await?
        .ok_or(ApiError::NotFound)?;
    // Best-effort source-side cleanup so the slot doesn't linger.
    if let Some(slot_uuid) = rule.slot_uuid {
        if let Ok(Some(peer)) = replication::get_peer_by_id(&state.pool, rule.peer_id).await {
            let client = http_client(peer.tls_skip_verify);
            let _ = client
                .delete(format!(
                    "{}/api/v1/replication/slots/{slot_uuid}",
                    peer.base_url
                ))
                .bearer_auth(&peer.api_token)
                .send()
                .await;
        }
    }
    replication::delete_rule(&state.pool, uuid).await?;
    tracing::info!(actor = %user.username, rule_uuid = %uuid,
        "replication rule deleted; source slot removal attempted");
    state.notify(ChangeKind::Replication);
    Ok(StatusCode::NO_CONTENT)
}

// ────────────────────────────────────────────────────────────────────────
// Inbound (source side, observability)
// ────────────────────────────────────────────────────────────────────────

#[derive(Serialize, ToSchema)]
pub struct InboundSlotResp {
    pub slot_uuid: Uuid,
    pub peer_instance_uuid: Uuid,
    pub peer_label: String,
    pub source_group_uuid: Uuid,
    pub replication_path: Vec<Uuid>,
    pub created_at: i64,
    pub last_stream_at: Option<i64>,
    pub host_count: usize,
    /// When set, this slot has been administratively blocked. The
    /// destination's worker sees 403 on every wire call until an admin
    /// unblocks the slot via `POST /inbound/{slot_uuid}/unblock`.
    pub blocked_at: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/api/v1/replication/inbound",
    params(("limit" = Option<i64>, Query), ("offset" = Option<i64>, Query)),
    responses(
        (status = 200, body = Vec<InboundSlotResp>),
        (status = 403, description = "Forbidden")
    ),
    tag = "replication"
)]
pub async fn list_inbound(
    user: CurrentUser,
    State(state): State<AppState>,
    Query(q): Query<PageQuery>,
) -> ApiResult<Response> {
    require_admin(&user)?;
    let (limit, offset) = clamp_pagination(q.limit, q.offset);
    let (slots, total) = replication::list_slots(&state.pool, limit, offset).await?;
    let mut body: Vec<InboundSlotResp> = Vec::with_capacity(slots.len());
    for s in slots {
        let cursors = replication::list_slot_cursors(&state.pool, s.id).await?;
        body.push(InboundSlotResp {
            slot_uuid: s.slot_uuid,
            peer_instance_uuid: s.peer_instance_uuid,
            peer_label: s.peer_label,
            source_group_uuid: s.source_group_uuid,
            replication_path: s.replication_path,
            created_at: s.created_at,
            last_stream_at: s.last_stream_at,
            host_count: cursors.len(),
            blocked_at: s.blocked_at,
        });
    }
    Ok(with_total(body, total))
}

#[utoipa::path(
    delete,
    path = "/api/v1/replication/inbound/{slot_uuid}",
    params(("slot_uuid" = Uuid, Path)),
    responses(
        (status = 204),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Slot not found")
    ),
    tag = "replication"
)]
pub async fn delete_inbound(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(slot_uuid): Path<Uuid>,
) -> ApiResult<StatusCode> {
    require_admin(&user)?;
    // Mark the slot blocked rather than deleting it - the destination's
    // worker keeps getting 403s on its next wire call (telling the
    // admin on the destination side that they've been refused) and the
    // peer's instance UUID stays on file so reconnects from the same
    // destination remain blocked until an admin unblocks. Deletion-on-
    // unblock is intentionally a separate explicit action.
    replication::block_slot(&state.pool, slot_uuid).await?;
    tracing::info!(actor = %user.username, %slot_uuid,
        "replication inbound slot blocked by operator (will refuse further calls)");
    state.notify(ChangeKind::Replication);
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/api/v1/replication/inbound/{slot_uuid}/unblock",
    params(("slot_uuid" = Uuid, Path)),
    responses(
        (status = 204),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Slot not found")
    ),
    tag = "replication"
)]
pub async fn unblock_inbound(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(slot_uuid): Path<Uuid>,
) -> ApiResult<StatusCode> {
    require_admin(&user)?;
    replication::unblock_slot(&state.pool, slot_uuid).await?;
    tracing::info!(actor = %user.username, %slot_uuid,
        "replication inbound slot unblocked; destination may resume");
    state.notify(ChangeKind::Replication);
    Ok(StatusCode::NO_CONTENT)
}

// ────────────────────────────────────────────────────────────────────────
// Wire endpoints (source side, called by destinations)
// ────────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, ToSchema)]
pub struct UpsertSlotReq {
    pub peer_instance_uuid: Uuid,
    pub peer_label: String,
    /// `null` / nil-uuid means the source's root.
    #[serde(default)]
    pub source_group_uuid: Option<Uuid>,
    /// Chain of instance UUIDs ending at the destination, used for loop
    /// detection. Source rejects if our `instance_uuid` is anywhere in
    /// here. The header is also accepted (and takes precedence) so a
    /// destination's HTTP client can carry the path uniformly.
    #[serde(default)]
    pub replication_path: Vec<Uuid>,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct UpsertSlotResp {
    pub slot_uuid: Uuid,
    /// Chain including our own instance UUID, returned so the destination
    /// can use it for the next hop if it ever serves a downstream peer.
    pub chain: Vec<Uuid>,
}

#[utoipa::path(
    post,
    path = "/api/v1/replication/slots",
    request_body = UpsertSlotReq,
    responses(
        (status = 200, body = UpsertSlotResp, description = "Slot created or refreshed"),
        (status = 403, description = "Forbidden"),
        (status = 422, description = "Loop detected")
    ),
    tag = "replication"
)]
pub async fn upsert_slot(
    user: CurrentUser,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<UpsertSlotReq>,
) -> ApiResult<Json<UpsertSlotResp>> {
    require_admin(&user)?;
    // Prefer the header (uniform across calls); fall back to the body so
    // tooling that can't set headers still works.
    let mut path = parse_path_header(&headers);
    if path.is_empty() {
        path = req.replication_path.clone();
    }
    if path.contains(&state.instance_uuid) {
        return Err(ApiError::Validation(format!(
            "replication loop detected: {} already in path",
            state.instance_uuid
        )));
    }
    let source_group = req.source_group_uuid.unwrap_or(Uuid::nil());
    // Before upserting, look up any existing slot for the same
    // (peer_instance, source_group) - if it was administratively blocked
    // from the Inbound table, refuse this call until an admin unblocks.
    if let Ok((slots, _)) = replication::list_slots(&state.pool, 10_000, 0).await {
        if let Some(existing) = slots.iter().find(|s| {
            s.peer_instance_uuid == req.peer_instance_uuid && s.source_group_uuid == source_group
        }) {
            if existing.blocked_at.is_some() {
                tracing::info!(
                    slot_uuid = %existing.slot_uuid,
                    peer_instance_uuid = %req.peer_instance_uuid,
                    "rejecting POST /slots: slot is administratively blocked"
                );
                return Err(ApiError::Forbidden);
            }
        }
    }
    let slot = replication::upsert_slot(
        &state.pool,
        req.peer_instance_uuid,
        &req.peer_label,
        source_group,
        &path,
    )
    .await?;
    tracing::info!(
        actor = %user.username,
        slot_uuid = %slot.slot_uuid,
        peer_instance_uuid = %req.peer_instance_uuid,
        peer_label = %req.peer_label,
        source_group_uuid = %source_group,
        chain_len = path.len(),
        "replication slot upserted"
    );
    let mut chain = path;
    if !chain.contains(&state.instance_uuid) {
        chain.push(state.instance_uuid);
    }
    state.notify(ChangeKind::Replication);
    Ok(Json(UpsertSlotResp {
        slot_uuid: slot.slot_uuid,
        chain,
    }))
}

#[utoipa::path(
    delete,
    path = "/api/v1/replication/slots/{slot_uuid}",
    params(("slot_uuid" = Uuid, Path)),
    responses(
        (status = 204),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Slot not found")
    ),
    tag = "replication"
)]
pub async fn delete_slot_route(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(slot_uuid): Path<Uuid>,
) -> ApiResult<StatusCode> {
    require_admin(&user)?;
    replication::delete_slot(&state.pool, slot_uuid).await?;
    tracing::info!(actor = %user.username, %slot_uuid, "replication slot deleted by destination");
    state.notify(ChangeKind::Replication);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct ManifestHost {
    pub uuid: Uuid,
    pub display_name: String,
    pub probe_type: String,
    pub interval_secs: i64,
    pub samples_per_period: i64,
    pub chunk_window_secs: i64,
    pub group_uuids: Vec<Uuid>,
    pub earliest_sample_ts: Option<i64>,
    pub latest_sample_ts: Option<i64>,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct ManifestGroup {
    pub uuid: Uuid,
    pub parent_uuid: Option<Uuid>,
    pub display_name: String,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct ManifestResp {
    pub slot_uuid: Uuid,
    pub source_group_uuid: Uuid,
    pub groups: Vec<ManifestGroup>,
    pub hosts: Vec<ManifestHost>,
}

#[utoipa::path(
    get,
    path = "/api/v1/replication/slots/{slot_uuid}/manifest",
    params(("slot_uuid" = Uuid, Path)),
    responses(
        (status = 200, body = ManifestResp),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Slot not found")
    ),
    tag = "replication"
)]
pub async fn slot_manifest(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(slot_uuid): Path<Uuid>,
) -> ApiResult<Json<ManifestResp>> {
    require_admin(&user)?;
    let slot = replication::get_slot_by_uuid(&state.pool, slot_uuid)
        .await?
        .ok_or(ApiError::NotFound)?;
    if slot.blocked_at.is_some() {
        return Err(ApiError::Forbidden);
    }
    let source_group = slot.source_group_uuid;

    // Resolve the source group to either "root" (Uuid::nil) or a concrete
    // groups row, then walk the subtree using the existing materialized
    // `path LIKE prefix%` filter.
    let (group_filter, parent_path, root_depth) = if source_group.is_nil() {
        (GroupFilter::Any, String::new(), -1i64)
    } else {
        let g = groups::get_by_uuid(&state.pool, source_group)
            .await?
            .ok_or(ApiError::NotFound)?;
        (GroupFilter::Subtree(source_group), g.path.clone(), g.depth)
    };

    // Hosts in scope. For root scope we return everything in /hosts; for a
    // subtree we use the helper that joins through host_groups.
    let host_rows = hosts::list(&state.pool, group_filter, None).await?;

    // Groups: every group whose path begins with the parent's path, OR all
    // groups when at root. Drop replicated-from-elsewhere groups (their
    // `replication_peer_id` is set) - we don't redistribute someone else's
    // mirror downstream by default; same for replicated hosts. (Cascade
    // replication is allowed, but only of hosts whose probe is local.)
    let all_groups = groups::list_all(&state.pool).await?;
    let groups_filtered: Vec<haze_store::repo::groups::Group> = all_groups
        .into_iter()
        .filter(|g| {
            if parent_path.is_empty() {
                true
            } else {
                g.path.starts_with(&parent_path)
            }
        })
        .collect();
    // Map group id -> uuid for parent_uuid resolution.
    let parent_map: std::collections::HashMap<i64, Uuid> = groups_filtered
        .iter()
        .map(|g| (g.id, g.uuid_typed()))
        .collect();
    let manifest_groups: Vec<ManifestGroup> = groups_filtered
        .iter()
        .map(|g| ManifestGroup {
            uuid: g.uuid_typed(),
            parent_uuid: g.parent_id.and_then(|pid| parent_map.get(&pid).copied()),
            display_name: g.display_name.clone(),
        })
        .collect();
    // ManifestGroup currently inherits everyone, even the group's own
    // ancestors above `parent_path`. Trim to the subtree (root scope keeps all).
    let manifest_groups: Vec<ManifestGroup> = if parent_path.is_empty() {
        manifest_groups
    } else {
        manifest_groups
            .into_iter()
            .zip(groups_filtered.iter())
            .filter(|(_, g)| g.depth >= root_depth)
            .map(|(m, _)| m)
            .collect()
    };

    // Earliest/latest sample timestamp per host. Reads chunk file names
    // only (no decode), so it scales with chunk count rather than samples.
    // Replicated hosts are included too - cascading replication
    // (A -> B -> C) is supported: B forwards A's data to C, and the
    // `replication_path` header carries the chain of instance UUIDs end
    // to end so loops are still caught at peer creation.
    let mut hosts_out = Vec::with_capacity(host_rows.len());
    for h in host_rows {
        let host_uuid = h.uuid_typed();
        let (earliest, latest) = chunk_time_bounds(&state.data_dir, host_uuid);
        hosts_out.push(ManifestHost {
            uuid: host_uuid,
            display_name: h.display_name,
            probe_type: h.probe_type,
            interval_secs: h.interval_secs,
            samples_per_period: h.samples_per_period,
            chunk_window_secs: h.chunk_window_secs,
            group_uuids: h.group_uuids,
            earliest_sample_ts: earliest,
            latest_sample_ts: latest,
        });
    }

    tracing::debug!(
        %slot_uuid, hosts = hosts_out.len(), groups = manifest_groups.len(),
        "replication manifest served"
    );
    Ok(Json(ManifestResp {
        slot_uuid,
        source_group_uuid: source_group,
        groups: manifest_groups,
        hosts: hosts_out,
    }))
}

fn chunk_time_bounds(data_dir: &std::path::Path, host_uuid: Uuid) -> (Option<i64>, Option<i64>) {
    let dir = haze_store::host_directory(data_dir, host_uuid).join("chunks");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return (None, None);
    };
    let mut earliest: Option<i64> = None;
    let mut latest: Option<i64> = None;
    // Filename format: {seq}_r{res}_{start}_{end}.hzc.zst — parse the
    // 3rd and 4th underscore-separated fields. Anything malformed is
    // skipped silently.
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.ends_with(".hzc.zst") {
            continue;
        }
        let parts: Vec<&str> = name.trim_end_matches(".hzc.zst").split('_').collect();
        if parts.len() < 4 {
            continue;
        }
        let (Ok(start), Ok(end)) = (parts[2].parse::<i64>(), parts[3].parse::<i64>()) else {
            continue;
        };
        earliest = Some(earliest.map_or(start, |e| e.min(start)));
        latest = Some(latest.map_or(end, |e| e.max(end)));
    }
    (earliest, latest)
}

#[derive(Deserialize)]
pub struct RangeQuery {
    pub host: Uuid,
    pub from: i64,
    pub to: i64,
    /// Cap on returned samples; source paginates by truncating at the cap
    /// and reporting `exhausted=false` so destination can request the next
    /// window. Defaults to 5000 - one minute of 12k-host fanout under
    /// 1Hz cadence is roughly that.
    #[serde(default = "default_range_max")]
    pub max: i64,
}

fn default_range_max() -> i64 {
    5000
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct RangeSample {
    pub ts: i64,
    pub min: f32,
    pub p2_5: f32,
    pub p25: f32,
    pub median: f32,
    pub p75: f32,
    pub p97_5: f32,
    pub loss_pct: f32,
}

impl From<Sample> for RangeSample {
    fn from(s: Sample) -> Self {
        Self {
            ts: s.timestamp_secs,
            min: s.slot.min,
            p2_5: s.slot.p2_5,
            p25: s.slot.p25,
            median: s.slot.median,
            p75: s.slot.p75,
            p97_5: s.slot.p97_5,
            loss_pct: s.slot.loss_pct,
        }
    }
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct RangeResp {
    pub host_uuid: Uuid,
    pub samples: Vec<RangeSample>,
    /// Earliest sample timestamp source currently retains for this host.
    /// Destination clamps its cursor against this on backlog truncation.
    pub earliest_available: Option<i64>,
    /// `true` when the response carries every sample in `[from, to]`. When
    /// `false`, destination resumes with `from = samples.last().ts + 1`.
    pub exhausted: bool,
    /// Set when `from` was older than `earliest_available`; tells the
    /// destination to advance its cursor and log a retention-gap warning.
    pub truncated_to: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/api/v1/replication/slots/{slot_uuid}/range",
    params(
        ("slot_uuid" = Uuid, Path),
        ("host" = Uuid, Query),
        ("from" = i64, Query),
        ("to" = i64, Query),
        ("max" = Option<i64>, Query, description = "Max samples per response (default 5000)")
    ),
    responses(
        (status = 200, body = RangeResp),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Slot or host not found")
    ),
    tag = "replication"
)]
pub async fn slot_range(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(slot_uuid): Path<Uuid>,
    Query(q): Query<RangeQuery>,
) -> ApiResult<Json<RangeResp>> {
    require_admin(&user)?;
    let _slot = replication::get_slot_by_uuid(&state.pool, slot_uuid)
        .await?
        .ok_or(ApiError::NotFound)?;
    if _slot.blocked_at.is_some() {
        return Err(ApiError::Forbidden);
    }
    // Replicated hosts can be re-served downstream; this is what makes
    // cascading replication (A -> B -> C) work. The chain is tracked via
    // the path header on every wire call so cycles are still refused.
    let host = hosts::get_by_uuid(&state.pool, q.host)
        .await?
        .ok_or(ApiError::NotFound)?;
    let host_uuid = host.uuid_typed();
    let (earliest, _latest) = chunk_time_bounds(&state.data_dir, host_uuid);
    let truncated_to = match earliest {
        Some(e) if e > q.from => Some(e),
        _ => None,
    };
    let from = truncated_to.unwrap_or(q.from);
    let raw = haze_store::read_range(&state.data_dir, host_uuid, from, q.to)?;
    let limit = q.max.clamp(1, 50_000) as usize;
    let exhausted = raw.len() <= limit;
    let mut samples: Vec<RangeSample> = raw.into_iter().take(limit).map(Into::into).collect();
    // Keep monotonic so the destination's cursor advances cleanly.
    samples.sort_by_key(|s| s.ts);
    tracing::debug!(
        %slot_uuid, %host_uuid, from, to = q.to, count = samples.len(), exhausted,
        ?truncated_to, "replication range served"
    );
    Ok(Json(RangeResp {
        host_uuid,
        samples,
        earliest_available: earliest,
        exhausted,
        truncated_to,
    }))
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct AckEntry {
    pub host_uuid: Uuid,
    pub last_ts: i64,
}

#[utoipa::path(
    post,
    path = "/api/v1/replication/slots/{slot_uuid}/ack",
    params(("slot_uuid" = Uuid, Path)),
    request_body = Vec<AckEntry>,
    responses(
        (status = 204),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Slot not found")
    ),
    tag = "replication"
)]
pub async fn slot_ack(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(slot_uuid): Path<Uuid>,
    Json(req): Json<Vec<AckEntry>>,
) -> ApiResult<StatusCode> {
    require_admin(&user)?;
    let slot = replication::get_slot_by_uuid(&state.pool, slot_uuid)
        .await?
        .ok_or(ApiError::NotFound)?;
    if slot.blocked_at.is_some() {
        return Err(ApiError::Forbidden);
    }
    for entry in &req {
        replication::record_slot_ack(&state.pool, slot.id, entry.host_uuid, entry.last_ts).await?;
    }
    replication::touch_slot_stream(&state.pool, slot.id).await?;
    tracing::debug!(%slot_uuid, count = req.len(), "replication ack recorded");
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get,
    path = "/api/v1/replication/slots/{slot_uuid}/stream",
    params(("slot_uuid" = Uuid, Path)),
    responses(
        (status = 200, description = "SSE: sample / manifest-changed / ping"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Slot not found")
    ),
    tag = "replication"
)]
pub async fn slot_stream(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(slot_uuid): Path<Uuid>,
) -> Result<Response, ApiError> {
    require_admin(&user)?;
    let slot = replication::get_slot_by_uuid(&state.pool, slot_uuid)
        .await?
        .ok_or(ApiError::NotFound)?;
    if slot.blocked_at.is_some() {
        return Err(ApiError::Forbidden);
    }
    // Snapshot the host set in scope so the stream can filter sample
    // events without re-querying the DB on every event. Refreshed when a
    // `manifest-changed` is dispatched.
    let host_filter = compute_host_filter(&state, slot.source_group_uuid).await?;
    let host_filter = std::sync::Arc::new(std::sync::RwLock::new(host_filter));
    let samples_rx = state.samples.subscribe();
    let events_rx = state.events.subscribe();
    let shutdown = state.shutdown.clone();
    let state_for_refresh = state.clone();
    let source_group_uuid = slot.source_group_uuid;
    let host_filter_writer = host_filter.clone();
    let slot_uuid_for_check = slot.slot_uuid;

    let stream = futures::stream::unfold(
        StreamState {
            samples_rx,
            events_rx,
            shutdown,
            host_filter,
        },
        move |mut s| {
            let state_for_refresh = state_for_refresh.clone();
            let host_filter_writer = host_filter_writer.clone();
            async move {
                loop {
                    tokio::select! {
                        biased;
                        () = s.shutdown.notified() => return None,
                        sample = s.samples_rx.recv() => match sample {
                            Ok(ev) => {
                                let hit = s
                                    .host_filter
                                    .read()
                                    .map(|set| set.contains(&ev.host_uuid))
                                    .unwrap_or(false);
                                if !hit { continue; }
                                let payload = serde_json::json!({
                                    "host_uuid": ev.host_uuid,
                                    "ts": ev.timestamp_secs,
                                    "slot": {
                                        "min": ev.slot.min, "p2_5": ev.slot.p2_5,
                                        "p25": ev.slot.p25, "median": ev.slot.median,
                                        "p75": ev.slot.p75, "p97_5": ev.slot.p97_5,
                                        "loss_pct": ev.slot.loss_pct,
                                    }
                                });
                                let event = Event::default()
                                    .event("sample")
                                    .data(payload.to_string());
                                return Some((Ok::<_, Infallible>(event), s));
                            }
                            Err(RecvError::Lagged(skipped)) => {
                                tracing::warn!(skipped, "replication stream subscriber lagged");
                                let event = Event::default()
                                    .event("lagged")
                                    .data(skipped.to_string());
                                return Some((Ok::<_, Infallible>(event), s));
                            }
                            Err(RecvError::Closed) => return None,
                        },
                        ev = s.events_rx.recv() => match ev {
                            Ok(ChangeKind::Tree) => {
                                // Tree changed; recompute host filter and
                                // tell the destination to re-fetch manifest.
                                if let Ok(new_set) =
                                    compute_host_filter(&state_for_refresh, source_group_uuid).await
                                {
                                    if let Ok(mut w) = host_filter_writer.write() {
                                        *w = new_set;
                                    }
                                }
                                let event = Event::default().event("manifest-changed").data("");
                                return Some((Ok::<_, Infallible>(event), s));
                            }
                            Ok(ChangeKind::Replication) => {
                                // Could be a block; re-read the slot row.
                                // On block_at being set we drop the stream
                                // so the destination's worker switches to
                                // its reconnect-with-backoff path and starts
                                // seeing 403s immediately.
                                if let Ok(Some(slot)) = replication::get_slot_by_uuid(
                                    &state_for_refresh.pool, slot_uuid_for_check
                                ).await {
                                    if slot.blocked_at.is_some() {
                                        tracing::info!(
                                            %slot_uuid_for_check,
                                            "closing live SSE stream because slot was blocked"
                                        );
                                        return None;
                                    }
                                }
                                continue;
                            }
                            Ok(_) => continue,
                            Err(RecvError::Lagged(_)) => continue,
                            Err(RecvError::Closed) => return None,
                        },
                    }
                }
            }
        },
    );

    tracing::info!(%slot_uuid, "replication stream opened");
    let sse: Sse<_> = Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    );
    Ok(sse.into_response())
}

struct StreamState {
    samples_rx: tokio::sync::broadcast::Receiver<haze_store::SampleEvent>,
    events_rx: tokio::sync::broadcast::Receiver<ChangeKind>,
    shutdown: std::sync::Arc<tokio::sync::Notify>,
    host_filter: std::sync::Arc<std::sync::RwLock<HashSet<Uuid>>>,
}

async fn compute_host_filter(
    state: &AppState,
    source_group_uuid: Uuid,
) -> ApiResult<HashSet<Uuid>> {
    let filter = if source_group_uuid.is_nil() {
        GroupFilter::Any
    } else {
        GroupFilter::Subtree(source_group_uuid)
    };
    let hosts_in = hosts::list(&state.pool, filter, None).await?;
    // Include replicated hosts in the SSE filter so cascading
    // replication (A -> B -> C) forwards every event from A through B
    // to C. Cycle prevention is enforced at peer-creation time + on
    // every wire call via the `X-Replication-Path` chain, so forwarding
    // here is safe.
    Ok(hosts_in.into_iter().map(|h| h.uuid_typed()).collect())
}

// ────────────────────────────────────────────────────────────────────────
// Outbound HTTP client used by destination handlers (peer creation/test,
// peer deletion cascade, rule deletion cascade, groups-preview).
// ────────────────────────────────────────────────────────────────────────

fn http_client(skip_tls_verify: bool) -> reqwest::Client {
    // No request `.timeout()` here - the same client is used for
    // long-lived SSE streams (`/slots/{id}/stream`) which would otherwise
    // be aborted after the timeout window, killing replication every
    // ~15 s and forcing a noisy reconnect cycle. `connect_timeout`
    // still bounds the initial handshake so unreachable peers fail
    // fast; once connected the stream is allowed to run indefinitely.
    let mut b = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .user_agent(concat!("haze-replication/", env!("CARGO_PKG_VERSION")));
    if skip_tls_verify {
        b = b.danger_accept_invalid_certs(true);
    }
    b.build().unwrap_or_else(|_| reqwest::Client::new())
}

#[derive(Deserialize)]
struct WireInstanceInfo {
    instance_uuid: Uuid,
    version: String,
    #[serde(default)]
    upstream_chain: Vec<Uuid>,
}

async fn fetch_instance_info(
    base_url: &str,
    token: &str,
    skip_tls_verify: bool,
) -> Result<WireInstanceInfo, String> {
    let client = http_client(skip_tls_verify);
    let resp = client
        .get(format!("{base_url}/api/v1/replication/instance-info"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;
    let status = resp.status();
    // 404 means the source either isn't a Haze instance at all, or it's
    // an older version that predates replication endpoints. Surface that
    // explicitly so the operator knows to upgrade the source first.
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(
            "source did not respond to /api/v1/replication/instance-info \
             (the source Haze instance is older than the replication feature; \
             upgrade the source before pairing)"
                .into(),
        );
    }
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(
            "token rejected by source (401). Confirm the token starts with `hzt_` \
             and was created on the source - not on this destination"
                .into(),
        );
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        return Err(
            "token is not authorised to access replication (403). The token must \
             belong to an admin user on the source"
                .into(),
        );
    }
    if !status.is_success() {
        return Err(format!("source returned HTTP {status}"));
    }
    resp.json::<WireInstanceInfo>().await.map_err(|e| {
        format!(
            "source returned a response we couldn't parse \
                 (possibly an HTML error page or an unexpected API version): {e}"
        )
    })
}

// Helpers re-exported for the worker so it can share the wire types and
// build outgoing requests in lock-step with the server-side handlers.
pub mod wire {
    pub use super::{
        AckEntry, ManifestGroup, ManifestHost, ManifestResp, RangeResp, RangeSample, UpsertSlotReq,
        UpsertSlotResp, fetch_instance_info_for_worker as fetch_instance_info,
        http_client_for_worker as http_client, path_header_name,
    };
}

/// The HTTP header destinations carry on every wire call. Exposed so the
/// worker uses the exact same constant as the request parser on the source.
pub fn path_header_name() -> &'static str {
    PATH_HEADER
}

pub async fn fetch_instance_info_for_worker(
    base_url: &str,
    token: &str,
    skip_tls_verify: bool,
) -> Result<(Uuid, String, Vec<Uuid>), String> {
    let info = fetch_instance_info(base_url, token, skip_tls_verify).await?;
    Ok((info.instance_uuid, info.version, info.upstream_chain))
}

pub fn http_client_for_worker(skip_tls_verify: bool) -> reqwest::Client {
    http_client(skip_tls_verify)
}

// Some `ReplicationError::Db` paths can return `NotFound` via the SQLite
// error code on missing rows; surface as 404 for paths that already
// pre-fetch but bubble up otherwise. Kept here so the conversion lives next
// to the routes that depend on it.
#[allow(dead_code)]
fn map_repo_err(e: ReplicationError) -> ApiError {
    e.into()
}
