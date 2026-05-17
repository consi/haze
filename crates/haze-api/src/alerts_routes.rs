//! /api/v1/alerts/* - rule CRUD, current alert states, webhook library.
//!
//! Permission gating:
//!   - Rules + states: `can_see_alerts()` (admin or user) for reads,
//!     `can_edit_alerts()` (admin or user) for writes.
//!   - Webhook library + test fire: admin only - URLs may carry tokens
//!     in query strings, so we treat them like settings, not data.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use haze_alert::{
    repo::{self, AlertRule, AlertState, NewRule, RuleTarget, UpdateRule, Webhook},
    types::{Aggregation, Direction, Metric, Severity, TargetKind},
    webhooks::WebhookClient,
};
use haze_auth::CurrentUser;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{error::ApiError, error::ApiResult, state::AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/rules", get(list_rules).post(create_rule))
        .route(
            "/rules/{uuid}",
            get(get_rule).put(update_rule).delete(delete_rule),
        )
        .route("/states", get(list_states))
        .route("/webhooks", get(list_webhooks).post(create_webhook))
        .route(
            "/webhooks/{uuid}",
            axum::routing::put(update_webhook).delete(delete_webhook),
        )
        .route("/webhooks/{uuid}/test", post(test_webhook))
}

fn require_alert_viewer(u: &CurrentUser) -> ApiResult<()> {
    if u.role.can_see_alerts() {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

fn require_alert_editor(u: &CurrentUser) -> ApiResult<()> {
    if u.role.can_edit_alerts() {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

fn require_admin(u: &CurrentUser) -> ApiResult<()> {
    if u.role.is_admin() {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

// ─── Rule DTOs ──────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub(crate) struct AlertTargetDto {
    pub kind: TargetKind,
    pub uuid: Uuid,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct AlertRuleResp {
    pub uuid: Uuid,
    pub name: String,
    pub enabled: bool,
    pub metric: Metric,
    pub aggregation: Aggregation,
    pub direction: Direction,
    pub warning_threshold: Option<f32>,
    pub critical_threshold: Option<f32>,
    pub window_secs: i64,
    pub targets: Vec<AlertTargetDto>,
    pub webhook_uuids: Vec<Uuid>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<AlertRule> for AlertRuleResp {
    fn from(r: AlertRule) -> Self {
        Self {
            uuid: r.uuid,
            name: r.name,
            enabled: r.enabled,
            metric: r.metric,
            aggregation: r.aggregation,
            direction: r.direction,
            warning_threshold: r.warning_threshold,
            critical_threshold: r.critical_threshold,
            window_secs: r.window_secs,
            targets: r
                .targets
                .into_iter()
                .map(|t| AlertTargetDto {
                    kind: t.kind,
                    uuid: t.uuid,
                })
                .collect(),
            webhook_uuids: r.webhook_uuids,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct AlertRuleReq {
    pub name: String,
    pub enabled: bool,
    pub metric: Metric,
    pub aggregation: Aggregation,
    pub direction: Direction,
    pub warning_threshold: Option<f32>,
    pub critical_threshold: Option<f32>,
    pub window_secs: i64,
    pub targets: Vec<AlertTargetDto>,
    pub webhook_uuids: Vec<Uuid>,
}

async fn validate_rule(pool: &sqlx::SqlitePool, req: &AlertRuleReq) -> ApiResult<()> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err(ApiError::Validation("name is required".into()));
    }
    if name.len() > 200 {
        return Err(ApiError::Validation("name must be <= 200 chars".into()));
    }
    if req.targets.is_empty() {
        return Err(ApiError::Validation(
            "at least one target (host or group) is required".into(),
        ));
    }
    if req.warning_threshold.is_none() && req.critical_threshold.is_none() {
        return Err(ApiError::Validation(
            "at least one of warning_threshold or critical_threshold must be set".into(),
        ));
    }
    // When both are set, critical must be on the "worse" side of warning
    // so the state machine has a coherent ladder. For `above`, critical
    // should be >= warning; for `below`, <= warning.
    if let (Some(w), Some(c)) = (req.warning_threshold, req.critical_threshold) {
        let consistent = match req.direction {
            Direction::Above => c >= w,
            Direction::Below => c <= w,
        };
        if !consistent {
            return Err(ApiError::Validation(format!(
                "with direction = {dir:?}, critical_threshold must be \
                 {cmp} warning_threshold",
                dir = req.direction.as_str(),
                cmp = if matches!(req.direction, Direction::Above) {
                    ">= "
                } else {
                    "<= "
                }
            )));
        }
    }
    let alerting = haze_store::repo::settings::alerting_settings(pool)
        .await
        .unwrap_or_else(|_| haze_store::repo::settings::default_alerting_settings());
    let min = i64::from(alerting.min_window_secs);
    let max = i64::from(alerting.max_window_secs);
    if req.window_secs < min || req.window_secs > max {
        return Err(ApiError::Validation(format!(
            "window_secs must be between {min} and {max} (configurable in \
             /settings/alerting)"
        )));
    }
    Ok(())
}

fn map_create_err(e: haze_alert::repo::CreateRuleError) -> ApiError {
    use haze_alert::repo::{CreateRuleError, RepoError};
    match e {
        CreateRuleError::HostNotFound(u) => {
            ApiError::Validation(format!("target host {u} not found"))
        }
        CreateRuleError::GroupNotFound(u) => {
            ApiError::Validation(format!("target group {u} not found"))
        }
        CreateRuleError::WebhookNotFound(u) => {
            ApiError::Validation(format!("webhook {u} not found"))
        }
        CreateRuleError::Repo(RepoError::RuleNotFound) => ApiError::NotFound,
        CreateRuleError::Repo(RepoError::WebhookNotFound) => {
            ApiError::Validation("referenced webhook not found".into())
        }
        CreateRuleError::Db(e) | CreateRuleError::Repo(RepoError::Db(e)) => ApiError::Db(e),
        CreateRuleError::Repo(RepoError::Decode(d)) => ApiError::Internal(d),
    }
}

fn map_repo_err(e: haze_alert::repo::RepoError) -> ApiError {
    use haze_alert::repo::RepoError;
    match e {
        RepoError::Db(e) => ApiError::Db(e),
        RepoError::RuleNotFound | RepoError::WebhookNotFound => ApiError::NotFound,
        RepoError::Decode(d) => ApiError::Internal(d),
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/alerts/rules",
    responses(
        (status = 200, body = Vec<AlertRuleResp>),
        (status = 403, description = "Insufficient role")
    ),
    tag = "alerts"
)]
pub(crate) async fn list_rules(
    user: CurrentUser,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<AlertRuleResp>>> {
    require_alert_viewer(&user)?;
    let rules = repo::list_rules(&state.pool).await.map_err(map_repo_err)?;
    Ok(Json(rules.into_iter().map(AlertRuleResp::from).collect()))
}

#[utoipa::path(
    get,
    path = "/api/v1/alerts/rules/{uuid}",
    params(("uuid" = Uuid, Path, description = "Rule UUID")),
    responses(
        (status = 200, body = AlertRuleResp),
        (status = 403, description = "Insufficient role"),
        (status = 404, description = "Rule not found")
    ),
    tag = "alerts"
)]
pub(crate) async fn get_rule(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(uuid): Path<Uuid>,
) -> ApiResult<Json<AlertRuleResp>> {
    require_alert_viewer(&user)?;
    let rule = repo::get_rule_by_uuid(&state.pool, uuid)
        .await
        .map_err(map_repo_err)?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(AlertRuleResp::from(rule)))
}

#[utoipa::path(
    post,
    path = "/api/v1/alerts/rules",
    request_body = AlertRuleReq,
    responses(
        (status = 201, body = AlertRuleResp),
        (status = 403, description = "Insufficient role"),
        (status = 422, description = "Validation error")
    ),
    tag = "alerts"
)]
pub(crate) async fn create_rule(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<AlertRuleReq>,
) -> ApiResult<(StatusCode, Json<AlertRuleResp>)> {
    require_alert_editor(&user)?;
    validate_rule(&state.pool, &req).await?;
    let targets: Vec<RuleTarget> = req
        .targets
        .iter()
        .map(|t| RuleTarget {
            kind: t.kind,
            uuid: t.uuid,
        })
        .collect();
    let rule = repo::create_rule(
        &state.pool,
        NewRule {
            name: req.name.trim(),
            enabled: req.enabled,
            metric: req.metric,
            aggregation: req.aggregation,
            direction: req.direction,
            warning_threshold: req.warning_threshold,
            critical_threshold: req.critical_threshold,
            window_secs: req.window_secs,
            targets: &targets,
            webhook_uuids: &req.webhook_uuids,
        },
    )
    .await
    .map_err(map_create_err)?;
    tracing::info!(
        rule_uuid = %rule.uuid,
        actor = %user.username,
        name = %rule.name,
        "alert rule created"
    );
    Ok((StatusCode::CREATED, Json(AlertRuleResp::from(rule))))
}

#[utoipa::path(
    put,
    path = "/api/v1/alerts/rules/{uuid}",
    params(("uuid" = Uuid, Path, description = "Rule UUID")),
    request_body = AlertRuleReq,
    responses(
        (status = 200, body = AlertRuleResp),
        (status = 403, description = "Insufficient role"),
        (status = 404, description = "Rule not found"),
        (status = 422, description = "Validation error")
    ),
    tag = "alerts"
)]
pub(crate) async fn update_rule(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(uuid): Path<Uuid>,
    Json(req): Json<AlertRuleReq>,
) -> ApiResult<Json<AlertRuleResp>> {
    require_alert_editor(&user)?;
    validate_rule(&state.pool, &req).await?;
    let targets: Vec<RuleTarget> = req
        .targets
        .iter()
        .map(|t| RuleTarget {
            kind: t.kind,
            uuid: t.uuid,
        })
        .collect();
    let rule = repo::update_rule(
        &state.pool,
        uuid,
        UpdateRule {
            name: req.name.trim(),
            enabled: req.enabled,
            metric: req.metric,
            aggregation: req.aggregation,
            direction: req.direction,
            warning_threshold: req.warning_threshold,
            critical_threshold: req.critical_threshold,
            window_secs: req.window_secs,
            targets: &targets,
            webhook_uuids: &req.webhook_uuids,
        },
    )
    .await
    .map_err(map_create_err)?;
    tracing::info!(
        rule_uuid = %rule.uuid,
        actor = %user.username,
        name = %rule.name,
        "alert rule updated"
    );
    Ok(Json(AlertRuleResp::from(rule)))
}

#[utoipa::path(
    delete,
    path = "/api/v1/alerts/rules/{uuid}",
    params(("uuid" = Uuid, Path, description = "Rule UUID")),
    responses(
        (status = 204, description = "Rule deleted"),
        (status = 403, description = "Insufficient role"),
        (status = 404, description = "Rule not found")
    ),
    tag = "alerts"
)]
pub(crate) async fn delete_rule(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(uuid): Path<Uuid>,
) -> ApiResult<StatusCode> {
    require_alert_editor(&user)?;
    repo::delete_rule(&state.pool, uuid)
        .await
        .map_err(map_repo_err)?;
    tracing::info!(rule_uuid = %uuid, actor = %user.username, "alert rule deleted");
    Ok(StatusCode::NO_CONTENT)
}

// ─── Alert states ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct AlertStateResp {
    pub rule_uuid: Uuid,
    pub host_uuid: Uuid,
    pub severity: Severity,
    pub since: i64,
    pub last_notified_at: Option<i64>,
    /// Aggregated value at the most recent state transition (what
    /// produced the current severity). `None` if the rule has never
    /// transitioned (severity defaults to `ok`).
    pub last_value: Option<f32>,
    /// Threshold the value was compared against.
    pub last_threshold: Option<f32>,
}

impl From<AlertState> for AlertStateResp {
    fn from(s: AlertState) -> Self {
        Self {
            rule_uuid: s.rule_uuid,
            host_uuid: s.host_uuid,
            severity: s.severity,
            since: s.since,
            last_notified_at: s.last_notified_at,
            last_value: s.last_value,
            last_threshold: s.last_threshold,
        }
    }
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub(crate) struct ListStatesQuery {
    /// Include rows whose severity is currently `ok`. Off by default —
    /// the dashboard only renders firing/warning rows, so dragging every
    /// resolved-but-not-yet-pruned row across the wire is wasted bytes
    /// at large host counts.
    #[serde(default)]
    pub include_ok: bool,
}

#[utoipa::path(
    get,
    path = "/api/v1/alerts/states",
    params(ListStatesQuery),
    responses(
        (status = 200, body = Vec<AlertStateResp>),
        (status = 403, description = "Insufficient role")
    ),
    tag = "alerts"
)]
pub(crate) async fn list_states(
    user: CurrentUser,
    State(state): State<AppState>,
    Query(q): Query<ListStatesQuery>,
) -> ApiResult<Json<Vec<AlertStateResp>>> {
    require_alert_viewer(&user)?;
    let rows = if q.include_ok {
        repo::list_states(&state.pool).await
    } else {
        repo::list_non_ok_state(&state.pool).await
    }
    .map_err(map_repo_err)?;
    Ok(Json(rows.into_iter().map(AlertStateResp::from).collect()))
}

// ─── Webhooks ───────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub(crate) struct WebhookHeader {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct WebhookResp {
    pub uuid: Uuid,
    pub name: String,
    pub url: String,
    pub headers: Vec<WebhookHeader>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<Webhook> for WebhookResp {
    fn from(w: Webhook) -> Self {
        Self {
            uuid: w.uuid,
            name: w.name,
            url: w.url,
            headers: w
                .headers
                .into_iter()
                .map(|(name, value)| WebhookHeader { name, value })
                .collect(),
            created_at: w.created_at,
            updated_at: w.updated_at,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct WebhookReq {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<WebhookHeader>,
}

// Header names: ASCII printable, no separators. RFC 7230 token chars.
// reqwest re-validates these too, but we surface the error earlier with
// a better message instead of leaking a reqwest error string.
const HEADER_NAME_INVALID: &[char] = &[
    ' ', '\t', '"', '(', ')', ',', '/', ':', ';', '<', '=', '>', '?', '@', '[', '\\', ']', '{', '}',
];

fn validate_webhook(req: &WebhookReq) -> ApiResult<()> {
    let name = req.name.trim();
    if name.is_empty() || name.len() > 200 {
        return Err(ApiError::Validation(
            "name is required (max 200 chars)".into(),
        ));
    }
    let url = req.url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(ApiError::Validation(
            "url must start with http:// or https://".into(),
        ));
    }
    if url.len() > 2048 {
        return Err(ApiError::Validation("url must be <= 2048 chars".into()));
    }
    let mut seen: Vec<String> = Vec::with_capacity(req.headers.len());
    for h in &req.headers {
        let hn = h.name.trim();
        if hn.is_empty() || hn.len() > 200 {
            return Err(ApiError::Validation(
                "header name must be 1..=200 chars".into(),
            ));
        }
        if hn
            .bytes()
            .any(|b| !b.is_ascii() || !(0x21..=0x7e).contains(&b))
            || hn.chars().any(|c| HEADER_NAME_INVALID.contains(&c))
        {
            return Err(ApiError::Validation(format!(
                "header name '{hn}' contains illegal characters"
            )));
        }
        if h.value.len() > 4096 {
            return Err(ApiError::Validation(
                "header value must be <= 4096 chars".into(),
            ));
        }
        let key = hn.to_ascii_lowercase();
        if seen.contains(&key) {
            return Err(ApiError::Validation(format!(
                "duplicate header '{hn}' (header names are case-insensitive)"
            )));
        }
        seen.push(key);
    }
    Ok(())
}

fn headers_to_pairs(headers: &[WebhookHeader]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|h| (h.name.trim().to_string(), h.value.clone()))
        .collect()
}

#[utoipa::path(
    get,
    path = "/api/v1/alerts/webhooks",
    responses(
        (status = 200, body = Vec<WebhookResp>),
        (status = 403, description = "Admin role required")
    ),
    tag = "alerts"
)]
pub(crate) async fn list_webhooks(
    user: CurrentUser,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<WebhookResp>>> {
    require_admin(&user)?;
    let rows = repo::list_webhooks(&state.pool)
        .await
        .map_err(map_repo_err)?;
    Ok(Json(rows.into_iter().map(WebhookResp::from).collect()))
}

#[utoipa::path(
    post,
    path = "/api/v1/alerts/webhooks",
    request_body = WebhookReq,
    responses(
        (status = 201, body = WebhookResp),
        (status = 403, description = "Admin role required"),
        (status = 422, description = "Validation error")
    ),
    tag = "alerts"
)]
pub(crate) async fn create_webhook(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<WebhookReq>,
) -> ApiResult<(StatusCode, Json<WebhookResp>)> {
    require_admin(&user)?;
    validate_webhook(&req)?;
    let headers = headers_to_pairs(&req.headers);
    let w = repo::create_webhook(&state.pool, req.name.trim(), req.url.trim(), &headers)
        .await
        .map_err(map_repo_err)?;
    tracing::info!(webhook_uuid = %w.uuid, actor = %user.username, "webhook created");
    Ok((StatusCode::CREATED, Json(WebhookResp::from(w))))
}

#[utoipa::path(
    put,
    path = "/api/v1/alerts/webhooks/{uuid}",
    params(("uuid" = Uuid, Path, description = "Webhook UUID")),
    request_body = WebhookReq,
    responses(
        (status = 200, body = WebhookResp),
        (status = 403, description = "Admin role required"),
        (status = 404, description = "Webhook not found"),
        (status = 422, description = "Validation error")
    ),
    tag = "alerts"
)]
pub(crate) async fn update_webhook(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(uuid): Path<Uuid>,
    Json(req): Json<WebhookReq>,
) -> ApiResult<Json<WebhookResp>> {
    require_admin(&user)?;
    validate_webhook(&req)?;
    let headers = headers_to_pairs(&req.headers);
    let w = repo::update_webhook(&state.pool, uuid, req.name.trim(), req.url.trim(), &headers)
        .await
        .map_err(map_repo_err)?;
    tracing::info!(webhook_uuid = %w.uuid, actor = %user.username, "webhook updated");
    Ok(Json(WebhookResp::from(w)))
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct WebhookDeleteConflictResp {
    pub rules: Vec<String>,
}

#[utoipa::path(
    delete,
    path = "/api/v1/alerts/webhooks/{uuid}",
    params(("uuid" = Uuid, Path, description = "Webhook UUID")),
    responses(
        (status = 204, description = "Webhook deleted"),
        (status = 403, description = "Admin role required"),
        (status = 404, description = "Webhook not found"),
        (status = 409, body = WebhookDeleteConflictResp, description = "Webhook is referenced by alert rules")
    ),
    tag = "alerts"
)]
pub(crate) async fn delete_webhook(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(uuid): Path<Uuid>,
) -> ApiResult<StatusCode> {
    require_admin(&user)?;
    let w = repo::get_webhook_by_uuid(&state.pool, uuid)
        .await
        .map_err(map_repo_err)?
        .ok_or(ApiError::NotFound)?;
    let referencing = repo::rules_referencing_webhook(&state.pool, w.id)
        .await
        .map_err(map_repo_err)?;
    if !referencing.is_empty() {
        // 409 + the list of rule names so the UI can surface them.
        return Err(ApiError::Conflict(format!(
            "in use by alert rule(s): {}",
            referencing.join(", ")
        )));
    }
    repo::delete_webhook(&state.pool, uuid)
        .await
        .map_err(map_repo_err)?;
    tracing::info!(webhook_uuid = %uuid, actor = %user.username, "webhook deleted");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct WebhookTestResp {
    pub status: Option<u16>,
    pub detail: String,
}

#[utoipa::path(
    post,
    path = "/api/v1/alerts/webhooks/{uuid}/test",
    params(("uuid" = Uuid, Path, description = "Webhook UUID")),
    responses(
        (status = 200, body = WebhookTestResp),
        (status = 403, description = "Admin role required"),
        (status = 404, description = "Webhook not found")
    ),
    tag = "alerts"
)]
pub(crate) async fn test_webhook(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(uuid): Path<Uuid>,
) -> ApiResult<Json<WebhookTestResp>> {
    require_admin(&user)?;
    let w = repo::get_webhook_by_uuid(&state.pool, uuid)
        .await
        .map_err(map_repo_err)?
        .ok_or(ApiError::NotFound)?;
    // Use the same timeout the engine uses live so a test fail matches
    // what an actual delivery would see.
    let cfg = haze_store::repo::settings::alerting_settings(&state.pool)
        .await
        .unwrap_or_else(|_| haze_store::repo::settings::default_alerting_settings());
    let client = WebhookClient::with_timeout(cfg.webhook_timeout_secs);
    let (status, detail) = client.test(&w.url, &w.headers).await;
    Ok(Json(WebhookTestResp { status, detail }))
}
