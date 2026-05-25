//! Admin-only system settings - currently just the HZC storage knobs.

use axum::{Json, Router, extract::State, routing::get};
use haze_auth::CurrentUser;
use haze_store::{
    AlertingSettings, HostDefaults, PublicModeSettings, RetentionTier,
    repo::settings::{self, WorkerPools},
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{
    ChangeKind, error::ApiError, error::ApiResult, middleware::ViewerAccess,
    rate_limit::build_limiters, state::AppState,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/storage", get(get_storage).put(update_storage))
        .route("/workers", get(get_workers).put(update_workers))
        .route("/alerting", get(get_alerting).put(update_alerting))
        .route("/hosts", get(get_host_defaults).put(update_host_defaults))
        .route("/public", get(get_public_mode).put(update_public_mode))
}

#[derive(Serialize, ToSchema)]
pub(crate) struct StorageSettingsResp {
    /// Ordered retention tiers. Each tier's `max_age_secs` is the age at which
    /// data should be down-sampled to `resolution_secs`; a `resolution_secs`
    /// of 0 means "keep raw samples".
    pub retention_tiers: Vec<RetentionTier>,
    /// How often (in seconds) the compactor walks every host's chunks.
    /// Reloaded live by the running task on the next cycle.
    pub compactor_interval_secs: u32,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateStorageSettingsReq {
    pub retention_tiers: Vec<RetentionTier>,
    pub compactor_interval_secs: u32,
}

#[utoipa::path(
    get,
    path = "/api/v1/settings/storage",
    responses((status = 200, body = StorageSettingsResp)),
    tag = "settings"
)]
pub(crate) async fn get_storage(
    // ViewerAccess so anonymous public-mode visitors can read the retention
    // tiers - the host and group detail pages use them to compute the
    // "max" preset's lower bound on smoke charts. The data is policy, not
    // sample content, so it's safe to expose to viewers.
    _viewer: ViewerAccess,
    State(state): State<AppState>,
) -> ApiResult<Json<StorageSettingsResp>> {
    let retention_tiers = settings::retention_tiers(&state.pool).await?;
    let compactor_interval_secs = settings::compactor_interval_secs(&state.pool).await?;
    Ok(Json(StorageSettingsResp {
        retention_tiers,
        compactor_interval_secs,
    }))
}

#[utoipa::path(
    put,
    path = "/api/v1/settings/storage",
    request_body = UpdateStorageSettingsReq,
    responses(
        (status = 200, body = StorageSettingsResp),
        (status = 403, description = "Forbidden - admin role required"),
        (status = 422, description = "Validation error")
    ),
    tag = "settings"
)]
pub(crate) async fn update_storage(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<UpdateStorageSettingsReq>,
) -> ApiResult<Json<StorageSettingsResp>> {
    if !user.role.is_admin() {
        return Err(ApiError::Forbidden);
    }
    validate(&req)?;
    settings::set_retention_tiers(&state.pool, &req.retention_tiers, Some(user.id)).await?;
    settings::set_compactor_interval_secs(&state.pool, req.compactor_interval_secs, Some(user.id))
        .await?;
    state.notify(ChangeKind::Settings);
    Ok(Json(StorageSettingsResp {
        retention_tiers: req.retention_tiers,
        compactor_interval_secs: req.compactor_interval_secs,
    }))
}

#[derive(Serialize, ToSchema)]
pub(crate) struct WorkerSettingsResp {
    pub pools: WorkerPools,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateWorkerSettingsReq {
    pub pools: WorkerPools,
}

#[utoipa::path(
    get,
    path = "/api/v1/settings/workers",
    responses((status = 200, body = WorkerSettingsResp)),
    tag = "settings"
)]
pub(crate) async fn get_workers(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> ApiResult<Json<WorkerSettingsResp>> {
    let pools = settings::worker_pools(&state.pool).await?;
    Ok(Json(WorkerSettingsResp { pools }))
}

#[utoipa::path(
    put,
    path = "/api/v1/settings/workers",
    request_body = UpdateWorkerSettingsReq,
    responses(
        (status = 200, body = WorkerSettingsResp),
        (status = 403, description = "Forbidden - admin role required"),
        (status = 422, description = "Validation error")
    ),
    tag = "settings"
)]
pub(crate) async fn update_workers(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<UpdateWorkerSettingsReq>,
) -> ApiResult<Json<WorkerSettingsResp>> {
    if !user.role.is_admin() {
        return Err(ApiError::Forbidden);
    }
    validate_pools(&req.pools)?;
    settings::set_worker_pools(&state.pool, &req.pools, Some(user.id)).await?;
    state.notify(ChangeKind::Settings);
    Ok(Json(WorkerSettingsResp { pools: req.pools }))
}

/// Hard ceiling on the SUM of every pool's permits. The per-field cap of
/// 65 536 stops a single typo from creating a giant semaphore, but with 8
/// fields a malicious / careless edit could still ask for ~524 k permits.
/// Capping the total bounds the worst case at ~32 k in-flight async ops.
const MAX_TOTAL_POOL_BUDGET: u64 = 32_768;

fn validate_pools(p: &WorkerPools) -> ApiResult<()> {
    let fields = [
        ("probe_ping", p.probe_ping),
        ("probe_dns", p.probe_dns),
        ("probe_tcp_connect", p.probe_tcp_connect),
        ("probe_tls_connect", p.probe_tls_connect),
        ("probe_http_ttfb", p.probe_http_ttfb),
        ("probe_http_total", p.probe_http_total),
        ("compactor", p.compactor),
        ("alert_eval", p.alert_eval),
        ("replication", p.replication),
    ];
    let mut total: u64 = 0;
    for (name, v) in fields {
        if v == 0 {
            return Err(ApiError::Validation(format!("{name} must be > 0")));
        }
        if v > 16_384 {
            return Err(ApiError::Validation(format!(
                "{name} must be <= 16384 per pool"
            )));
        }
        total = total.saturating_add(u64::from(v));
    }
    if total > MAX_TOTAL_POOL_BUDGET {
        return Err(ApiError::Validation(format!(
            "total worker pool size {total} exceeds budget of {MAX_TOTAL_POOL_BUDGET}; \
             trim individual pools so they sum below the cap"
        )));
    }
    Ok(())
}

// ─── Alerting tunables ─────────────────────────────────────────────────────

#[derive(Serialize, ToSchema)]
pub(crate) struct AlertingSettingsResp {
    pub settings: AlertingSettings,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateAlertingSettingsReq {
    pub settings: AlertingSettings,
}

#[utoipa::path(
    get,
    path = "/api/v1/settings/alerting",
    responses((status = 200, body = AlertingSettingsResp)),
    tag = "settings"
)]
pub(crate) async fn get_alerting(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> ApiResult<Json<AlertingSettingsResp>> {
    let settings = settings::alerting_settings(&state.pool).await?;
    Ok(Json(AlertingSettingsResp { settings }))
}

#[utoipa::path(
    put,
    path = "/api/v1/settings/alerting",
    request_body = UpdateAlertingSettingsReq,
    responses(
        (status = 200, body = AlertingSettingsResp),
        (status = 403, description = "Forbidden - admin role required"),
        (status = 422, description = "Validation error")
    ),
    tag = "settings"
)]
pub(crate) async fn update_alerting(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<UpdateAlertingSettingsReq>,
) -> ApiResult<Json<AlertingSettingsResp>> {
    if !user.role.is_admin() {
        return Err(ApiError::Forbidden);
    }
    validate_alerting(&req.settings)?;
    settings::set_alerting_settings(&state.pool, &req.settings, Some(user.id)).await?;
    state.notify(ChangeKind::Settings);
    Ok(Json(AlertingSettingsResp {
        settings: req.settings,
    }))
}

fn validate_alerting(s: &AlertingSettings) -> ApiResult<()> {
    if !(5..=3600).contains(&s.eval_interval_secs) {
        return Err(ApiError::Validation(
            "eval_interval_secs must be between 5 and 3600 (1 hour)".into(),
        ));
    }
    if !(1..=120).contains(&s.webhook_timeout_secs) {
        return Err(ApiError::Validation(
            "webhook_timeout_secs must be between 1 and 120".into(),
        ));
    }
    if !(30..=86_400).contains(&s.snapshot_flush_interval_secs) {
        return Err(ApiError::Validation(
            "snapshot_flush_interval_secs must be between 30 and 86400 (24h)".into(),
        ));
    }
    if s.min_window_secs == 0 || s.min_window_secs > 86_400 {
        return Err(ApiError::Validation(
            "min_window_secs must be between 1 and 86400 (24h)".into(),
        ));
    }
    if s.max_window_secs <= s.min_window_secs {
        return Err(ApiError::Validation(
            "max_window_secs must be greater than min_window_secs".into(),
        ));
    }
    if s.max_window_secs > 30 * 86_400 {
        return Err(ApiError::Validation(
            "max_window_secs must be <= 30 days".into(),
        ));
    }
    Ok(())
}

// ─── Host defaults ─────────────────────────────────────────────────────────

#[derive(Serialize, ToSchema)]
pub(crate) struct HostDefaultsResp {
    pub defaults: HostDefaults,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateHostDefaultsReq {
    pub defaults: HostDefaults,
}

#[utoipa::path(
    get,
    path = "/api/v1/settings/hosts",
    responses((status = 200, body = HostDefaultsResp)),
    tag = "settings"
)]
pub(crate) async fn get_host_defaults(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> ApiResult<Json<HostDefaultsResp>> {
    let defaults = settings::host_defaults(&state.pool).await?;
    Ok(Json(HostDefaultsResp { defaults }))
}

#[utoipa::path(
    put,
    path = "/api/v1/settings/hosts",
    request_body = UpdateHostDefaultsReq,
    responses(
        (status = 200, body = HostDefaultsResp),
        (status = 403, description = "Forbidden - admin role required"),
        (status = 422, description = "Validation error")
    ),
    tag = "settings"
)]
pub(crate) async fn update_host_defaults(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<UpdateHostDefaultsReq>,
) -> ApiResult<Json<HostDefaultsResp>> {
    if !user.role.is_admin() {
        return Err(ApiError::Forbidden);
    }
    validate_host_defaults(req.defaults)?;
    settings::set_host_defaults(&state.pool, &req.defaults, Some(user.id)).await?;
    state.notify(ChangeKind::Settings);
    Ok(Json(HostDefaultsResp {
        defaults: req.defaults,
    }))
}

fn validate_host_defaults(d: HostDefaults) -> ApiResult<()> {
    if !(1..=86_400).contains(&d.interval_secs) {
        return Err(ApiError::Validation(
            "interval_secs must be between 1 and 86400 (24h)".into(),
        ));
    }
    if !(1..=1_000).contains(&d.samples_per_period) {
        return Err(ApiError::Validation(
            "samples_per_period must be between 1 and 1000".into(),
        ));
    }
    Ok(())
}

// ─── Public mode + anonymous rate limits ──────────────────────────────────

#[derive(Serialize, ToSchema)]
pub(crate) struct PublicModeSettingsResp {
    pub settings: PublicModeSettings,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdatePublicModeSettingsReq {
    pub settings: PublicModeSettings,
}

#[utoipa::path(
    get,
    path = "/api/v1/settings/public",
    responses((status = 200, body = PublicModeSettingsResp)),
    tag = "settings"
)]
pub(crate) async fn get_public_mode(
    _user: CurrentUser,
    State(state): State<AppState>,
) -> ApiResult<Json<PublicModeSettingsResp>> {
    let settings = settings::public_mode_settings(&state.pool).await?;
    Ok(Json(PublicModeSettingsResp { settings }))
}

#[utoipa::path(
    put,
    path = "/api/v1/settings/public",
    request_body = UpdatePublicModeSettingsReq,
    responses(
        (status = 200, body = PublicModeSettingsResp),
        (status = 403, description = "Forbidden - admin role required"),
        (status = 422, description = "Validation error")
    ),
    tag = "settings"
)]
pub(crate) async fn update_public_mode(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<UpdatePublicModeSettingsReq>,
) -> ApiResult<Json<PublicModeSettingsResp>> {
    if !user.role.is_admin() {
        return Err(ApiError::Forbidden);
    }
    validate_public_mode(&req.settings)?;
    settings::set_public_mode_settings(&state.pool, &req.settings, Some(user.id)).await?;
    // Hot-swap the running limiters so the new caps apply immediately
    // without a server restart. Subsequent anonymous requests see the
    // fresh buckets on the next `state.limiters.load()`.
    state
        .limiters
        .store(std::sync::Arc::new(build_limiters(&req.settings)));
    state.notify(ChangeKind::Settings);
    Ok(Json(PublicModeSettingsResp {
        settings: req.settings,
    }))
}

fn validate_public_mode(s: &PublicModeSettings) -> ApiResult<()> {
    if !(1..=100_000).contains(&s.light_per_minute) {
        return Err(ApiError::Validation(
            "light_per_minute must be between 1 and 100000".into(),
        ));
    }
    if !(1..=s.light_per_minute).contains(&s.light_burst) {
        return Err(ApiError::Validation(
            "light_burst must be between 1 and light_per_minute".into(),
        ));
    }
    if !(1..=1_000_000).contains(&s.series_per_minute) {
        return Err(ApiError::Validation(
            "series_per_minute must be between 1 and 1000000".into(),
        ));
    }
    if !(1..=s.series_per_minute).contains(&s.series_burst) {
        return Err(ApiError::Validation(
            "series_burst must be between 1 and series_per_minute".into(),
        ));
    }
    if !(1..=64).contains(&s.sse_max_per_ip) {
        return Err(ApiError::Validation(
            "sse_max_per_ip must be between 1 and 64".into(),
        ));
    }
    Ok(())
}

fn validate(req: &UpdateStorageSettingsReq) -> ApiResult<()> {
    if req.compactor_interval_secs < 60 {
        return Err(ApiError::Validation(
            "compactor_interval_secs must be >= 60 (one minute)".into(),
        ));
    }
    if req.compactor_interval_secs > 86_400 {
        return Err(ApiError::Validation(
            "compactor_interval_secs must be <= 86400 (one day)".into(),
        ));
    }
    if req.retention_tiers.is_empty() {
        return Err(ApiError::Validation(
            "retention_tiers must not be empty".into(),
        ));
    }
    let mut prev_age = 0i64;
    let mut prev_res = 0u32;
    for (i, tier) in req.retention_tiers.iter().enumerate() {
        if tier.max_age_secs <= prev_age {
            return Err(ApiError::Validation(format!(
                "retention_tiers[{i}].max_age_secs must be strictly increasing",
            )));
        }
        if tier.resolution_secs < prev_res {
            return Err(ApiError::Validation(format!(
                "retention_tiers[{i}].resolution_secs must be non-decreasing",
            )));
        }
        prev_age = tier.max_age_secs;
        prev_res = tier.resolution_secs;
    }
    Ok(())
}
