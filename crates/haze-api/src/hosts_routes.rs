//! /api/v1/hosts CRUD + /api/v1/hosts/{uuid}/series.
//!
//! Every host-scoped path operates on the host's UUID, not the internal
//! DB row id. The id is an implementation detail that never leaves the
//! repo layer.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::get,
};
use haze_auth::CurrentUser;
use haze_probe::{ProbeKind, scheduler::HostSpec};
use haze_store::{
    Sample, Slot, consolidate,
    repo::hosts::{self, GroupFilter, Host, HostPatch, NewHost},
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{error::ApiError, error::ApiResult, state::AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/{uuid}", get(get_one).patch(update).delete(delete))
        .route("/{uuid}/series", get(series))
}

#[derive(Deserialize)]
pub(crate) struct ListQuery {
    /// Filter to hosts in this group (direct membership only).
    group_uuid: Option<Uuid>,
    /// Filter to hosts in this group OR any descendant group. Mutually
    /// exclusive with `group_uuid`; if both are supplied, `subtree_of`
    /// wins because it's the strictly broader query.
    subtree_of: Option<Uuid>,
    /// `true` returns only hosts that belong to no group.
    #[serde(default)]
    ungrouped: bool,
    probe_type: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/v1/hosts",
    params(
        ("group_uuid" = Option<Uuid>, Query, description = "Hosts directly in this group"),
        ("subtree_of" = Option<Uuid>, Query, description = "Hosts anywhere under this group"),
        ("ungrouped" = Option<bool>, Query, description = "Only hosts with no groups"),
        ("probe_type" = Option<String>, Query, description = "Filter by probe type")
    ),
    responses((status = 200, body = Vec<HostResp>, description = "Hosts matching filters")),
    tag = "hosts"
)]
pub(crate) async fn list(
    _user: CurrentUser,
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<HostResp>>> {
    let filter = if let Some(uuid) = q.subtree_of {
        GroupFilter::Subtree(uuid)
    } else if let Some(uuid) = q.group_uuid {
        GroupFilter::Uuid(uuid)
    } else if q.ungrouped {
        GroupFilter::None
    } else {
        GroupFilter::Any
    };
    let rows = hosts::list(&state.pool, filter, q.probe_type.as_deref()).await?;
    Ok(Json(rows.into_iter().map(HostResp::from).collect()))
}

#[utoipa::path(
    get,
    path = "/api/v1/hosts/{uuid}",
    params(("uuid" = Uuid, Path, description = "Host UUID")),
    responses(
        (status = 200, body = HostResp),
        (status = 404, description = "Host not found")
    ),
    tag = "hosts"
)]
pub(crate) async fn get_one(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(uuid): Path<Uuid>,
) -> ApiResult<Json<HostResp>> {
    hosts::get_by_uuid(&state.pool, uuid)
        .await?
        .map(HostResp::from)
        .map(Json)
        .ok_or(ApiError::NotFound)
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct CreateReq {
    /// Group memberships (by UUID). Empty / omitted = root-level.
    #[serde(default)]
    group_uuids: Vec<Uuid>,
    display_name: String,
    probe_type: String,
    /// Probe-specific configuration. Shape depends on `probe_type`; the
    /// frontend builds it from a typed form so callers don't have to know
    /// the schema. Stored as JSON, parsed by the relevant probe at runtime.
    #[schema(value_type = Object, additional_properties = true)]
    probe_config: serde_json::Value,
    #[serde(default = "default_interval")]
    interval_secs: u32,
    #[serde(default = "default_samples")]
    samples_per_period: u32,
    /// HZC chunk window for this host. Captured at creation time and
    /// frozen for the life of the host (the value lands in the host's
    /// `meta.json` on disk). Omit to use the system default (1 h).
    #[serde(default = "default_chunk_window")]
    chunk_window_secs: u32,
}

fn default_interval() -> u32 {
    60
}
fn default_samples() -> u32 {
    20
}
fn default_chunk_window() -> u32 {
    haze_store::DEFAULT_HOST_CHUNK_WINDOW_SECS
}

#[utoipa::path(
    post,
    path = "/api/v1/hosts",
    request_body = CreateReq,
    responses(
        (status = 201, body = HostResp, description = "Host created and scheduled"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Referenced group not found"),
        (status = 422, description = "Validation error")
    ),
    tag = "hosts"
)]
pub(crate) async fn create(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<CreateReq>,
) -> ApiResult<(StatusCode, Json<HostResp>)> {
    if !user.role.can_edit_hosts() {
        return Err(ApiError::Forbidden);
    }
    let kind = parse_kind(&req.probe_type)?;
    if !req.probe_config.is_object() {
        return Err(ApiError::Validation(
            "probe_config must be a JSON object".into(),
        ));
    }
    if req.chunk_window_secs < 60 || req.chunk_window_secs > 86_400 {
        return Err(ApiError::Validation(
            "chunk_window_secs must be between 60 and 86400".into(),
        ));
    }
    let probe_cfg_json = serde_json::to_string(&req.probe_config).expect("json -> string");

    let h = hosts::create(
        &state.pool,
        NewHost {
            display_name: &req.display_name,
            probe_type: &req.probe_type,
            probe_config: &probe_cfg_json,
            interval_secs: i64::from(req.interval_secs),
            samples_per_period: i64::from(req.samples_per_period),
            chunk_window_secs: i64::from(req.chunk_window_secs),
            group_uuids: &req.group_uuids,
        },
    )
    .await?;

    let host_uuid = h.uuid_typed();
    tracing::info!(
        %host_uuid,
        actor = %user.username,
        display_name = %h.display_name,
        probe_type = %h.probe_type,
        interval_secs = h.interval_secs,
        samples_per_period = h.samples_per_period,
        group_count = h.group_uuids.len(),
        "host created"
    );
    state.scheduler.add(HostSpec {
        uuid: host_uuid,
        probe_type: kind,
        probe_config: req.probe_config,
        interval_secs: req.interval_secs,
        samples_per_period: req.samples_per_period,
        chunk_window_secs: req.chunk_window_secs,
    });
    Ok((StatusCode::CREATED, Json(HostResp::from(h))))
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateHostReq {
    /// New display name. Optional: omit to leave unchanged.
    pub display_name: Option<String>,
    /// Replace the host's full set of group memberships. Optional: omit
    /// to leave memberships untouched. Empty array detaches from every
    /// group (host appears at the tree root).
    pub group_uuids: Option<Vec<Uuid>>,
    /// Switch the probe kind. Existing chunks aren't migrated; the
    /// stored time series will hold mixed semantics across the switch
    /// point. Operator's call.
    pub probe_type: Option<String>,
    /// Replace the probe-specific configuration. Shape depends on the
    /// (new) `probe_type`. The probe re-validates on the next attempt.
    #[schema(value_type = Object, additional_properties = true)]
    pub probe_config: Option<serde_json::Value>,
    pub interval_secs: Option<u32>,
    pub samples_per_period: Option<u32>,
}

#[utoipa::path(
    patch,
    path = "/api/v1/hosts/{uuid}",
    params(("uuid" = Uuid, Path, description = "Host UUID")),
    request_body = UpdateHostReq,
    responses(
        (status = 200, body = HostResp, description = "Host updated"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Host or referenced group not found"),
        (status = 422, description = "Validation error")
    ),
    tag = "hosts"
)]
pub(crate) async fn update(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(uuid): Path<Uuid>,
    Json(req): Json<UpdateHostReq>,
) -> ApiResult<Json<HostResp>> {
    if !user.role.can_edit_hosts() {
        return Err(ApiError::Forbidden);
    }

    // Validate any probe-related changes BEFORE touching the DB so a
    // malformed request can't leave half a patch applied.
    if let Some(pt) = req.probe_type.as_deref() {
        parse_kind(pt)?;
    }
    if let Some(cfg) = &req.probe_config {
        if !cfg.is_object() {
            return Err(ApiError::Validation(
                "probe_config must be a JSON object".into(),
            ));
        }
    }
    if let Some(iv) = req.interval_secs {
        if iv == 0 {
            return Err(ApiError::Validation("interval_secs must be > 0".into()));
        }
    }
    if let Some(sp) = req.samples_per_period {
        if sp == 0 || sp > 1000 {
            return Err(ApiError::Validation(
                "samples_per_period must be between 1 and 1000".into(),
            ));
        }
    }

    let probe_cfg_json = req
        .probe_config
        .as_ref()
        .map(|v| serde_json::to_string(v).expect("json -> string"));

    let host = hosts::update_by_uuid(
        &state.pool,
        uuid,
        HostPatch {
            display_name: req.display_name.as_deref(),
            group_uuids: req.group_uuids.as_deref(),
            probe_type: req.probe_type.as_deref(),
            probe_config: probe_cfg_json.as_deref(),
            interval_secs: req.interval_secs.map(i64::from),
            samples_per_period: req.samples_per_period.map(i64::from),
        },
    )
    .await?;

    // Restart the scheduled probe with the post-patch state. Cheap
    // (kills the existing tokio task, spawns a fresh one), and ensures
    // changes to probe_type / probe_config / interval / samples take
    // effect immediately instead of waiting for the next process
    // restart. No-op-ish if only display_name / group_uuids changed,
    // but the cost is one task restart per host edit - acceptable.
    let kind = parse_kind(&host.probe_type)?;
    let probe_config: serde_json::Value =
        serde_json::from_str(&host.probe_config).unwrap_or(serde_json::Value::Null);
    state.scheduler.restart(HostSpec {
        uuid: host.uuid_typed(),
        probe_type: kind,
        probe_config,
        interval_secs: host.interval_secs as u32,
        samples_per_period: host.samples_per_period as u32,
        chunk_window_secs: host.chunk_window_secs as u32,
    });

    tracing::info!(
        host_uuid = %host.uuid_typed(),
        actor = %user.username,
        display_name = %host.display_name,
        probe_type = %host.probe_type,
        interval_secs = host.interval_secs,
        samples_per_period = host.samples_per_period,
        group_count = host.group_uuids.len(),
        "host updated"
    );
    Ok(Json(HostResp::from(host)))
}

#[utoipa::path(
    delete,
    path = "/api/v1/hosts/{uuid}",
    params(("uuid" = Uuid, Path, description = "Host UUID")),
    responses(
        (status = 204, description = "Host removed (chunks deleted, scheduler stopped)"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Host not found")
    ),
    tag = "hosts"
)]
pub(crate) async fn delete(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(uuid): Path<Uuid>,
) -> ApiResult<StatusCode> {
    if !user.role.can_edit_hosts() {
        return Err(ApiError::Forbidden);
    }
    let host = hosts::delete_by_uuid(&state.pool, uuid).await?;
    let host_uuid = host.uuid_typed();
    tracing::info!(
        %host_uuid,
        actor = %user.username,
        display_name = %host.display_name,
        probe_type = %host.probe_type,
        "host deleted"
    );
    state.scheduler.remove(host_uuid);
    let _ = state.hzc.delete(host_uuid);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub(crate) struct SeriesQuery {
    from: i64,
    to: i64,
    /// Accepted for backwards compatibility; ignored (the reader picks the
    /// finest resolution available and `max_samples` controls density).
    #[serde(default)]
    resolution: Option<String>,
    /// Soft cap on samples returned. When the raw stream from the chunks is
    /// larger, the server bucket-aggregates down to roughly this count.
    /// Default 2000 keeps payloads bounded for legacy callers.
    #[serde(default = "default_max_samples")]
    max_samples: u32,
}

fn default_max_samples() -> u32 {
    2_000
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SeriesPoint {
    pub ts: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p2_5: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p25: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub median: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p75: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p97_5: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loss_pct: Option<f32>,
}

impl From<Sample> for SeriesPoint {
    fn from(s: Sample) -> Self {
        let nan_to_opt = |v: f32| if v.is_nan() { None } else { Some(v) };
        Self {
            ts: s.timestamp_secs,
            min: nan_to_opt(s.slot.min),
            p2_5: nan_to_opt(s.slot.p2_5),
            p25: nan_to_opt(s.slot.p25),
            median: nan_to_opt(s.slot.median),
            p75: nan_to_opt(s.slot.p75),
            p97_5: nan_to_opt(s.slot.p97_5),
            loss_pct: nan_to_opt(s.slot.loss_pct),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SeriesResp {
    pub host_uuid: Uuid,
    pub resolution_secs: u32,
    pub from: i64,
    pub to: i64,
    pub samples: Vec<SeriesPoint>,
}

#[utoipa::path(
    get,
    path = "/api/v1/hosts/{uuid}/series",
    params(
        ("uuid" = Uuid, Path, description = "Host UUID"),
        ("from" = i64, Query, description = "Window start (epoch seconds)"),
        ("to" = i64, Query, description = "Window end (epoch seconds)"),
        ("max_samples" = Option<u32>, Query, description = "Server-side bucket cap (default 2000)")
    ),
    responses(
        (status = 200, body = SeriesResp, description = "Bucket-aggregated percentile samples"),
        (status = 404, description = "Host not found")
    ),
    tag = "hosts"
)]
pub(crate) async fn series(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(uuid): Path<Uuid>,
    Query(q): Query<SeriesQuery>,
) -> ApiResult<Json<SeriesResp>> {
    let _ = q.resolution; // accepted for backwards compatibility; ignored
    let host = hosts::get_by_uuid(&state.pool, uuid)
        .await?
        .ok_or(ApiError::NotFound)?;
    let host_uuid = host.uuid_typed();
    let raw = haze_store::read_range(&state.data_dir, host_uuid, q.from, q.to)?;
    let max_samples = q.max_samples.max(1);

    let (resolution_secs, samples) = downsample_to_budget(
        raw,
        q.from,
        q.to,
        max_samples,
        u32::try_from(host.interval_secs).unwrap_or(0),
    );

    Ok(Json(SeriesResp {
        host_uuid,
        resolution_secs,
        from: q.from,
        to: q.to,
        samples,
    }))
}

/// Server-side bucket aggregation. The reader returns raw chunk samples; if
/// the count exceeds `max_samples` we collapse adjacent samples into uniform
/// buckets using the same NaN-aware percentile-mean consolidation the
/// compactor uses, so a 90-day view with a 5-second probe interval ships
/// kilobytes instead of megabytes. Returns `(effective_resolution, points)`.
fn downsample_to_budget(
    raw: Vec<Sample>,
    from: i64,
    to: i64,
    max_samples: u32,
    fallback_resolution: u32,
) -> (u32, Vec<SeriesPoint>) {
    if raw.len() <= max_samples as usize {
        let resolution_secs = if raw.len() >= 2 {
            u32::try_from((raw[1].timestamp_secs - raw[0].timestamp_secs).max(1)).unwrap_or(0)
        } else {
            fallback_resolution
        };
        return (
            resolution_secs,
            raw.into_iter().map(SeriesPoint::from).collect(),
        );
    }

    let span = (to - from).max(1);
    // Round bucket width up so we end with at most `max_samples` buckets.
    let bucket_secs = ((span + i64::from(max_samples) - 1) / i64::from(max_samples)).max(1);
    let mut current_bucket_start = from;
    let mut current: Vec<Slot> = Vec::new();
    let mut out: Vec<SeriesPoint> = Vec::new();

    let flush = |bucket_start: i64, slots: &mut Vec<Slot>, out: &mut Vec<SeriesPoint>| {
        if slots.is_empty() {
            return;
        }
        let consolidated = consolidate(slots);
        out.push(SeriesPoint::from(Sample {
            timestamp_secs: bucket_start,
            slot: consolidated,
        }));
        slots.clear();
    };

    for sample in raw {
        let offset = sample.timestamp_secs - from;
        let bucket_index = offset.div_euclid(bucket_secs);
        let bucket_start = from + bucket_index * bucket_secs;
        if bucket_start != current_bucket_start && !current.is_empty() {
            flush(current_bucket_start, &mut current, &mut out);
        }
        current_bucket_start = bucket_start;
        current.push(sample.slot);
    }
    flush(current_bucket_start, &mut current, &mut out);

    let resolution_secs = u32::try_from(bucket_secs).unwrap_or(fallback_resolution);
    (resolution_secs, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use haze_store::Slot;

    fn s(ts: i64, median: f32) -> Sample {
        Sample {
            timestamp_secs: ts,
            slot: Slot {
                min: median - 0.5,
                p2_5: median - 0.25,
                p25: median - 0.1,
                median,
                p75: median + 0.1,
                p97_5: median + 0.25,
                loss_pct: 0.0,
            },
        }
    }

    #[test]
    fn pass_through_when_under_budget() {
        let raw = (0..10).map(|i| s(i * 5, 20.0 + i as f32)).collect();
        let (res, out) = downsample_to_budget(raw, 0, 50, 600, 5);
        assert_eq!(out.len(), 10);
        assert_eq!(res, 5);
    }

    #[test]
    fn buckets_when_over_budget() {
        // 1000 samples spread across 1000 seconds; ask for at most 10.
        let raw: Vec<Sample> = (0..1000).map(|i| s(i, 20.0 + (i % 7) as f32)).collect();
        let (res, out) = downsample_to_budget(raw, 0, 1000, 10, 1);
        assert!(out.len() <= 10, "got {} buckets", out.len());
        // Bucket width is at least span/max, here ceil(1000/10) = 100.
        assert_eq!(res, 100);
        // Bucket starts align to multiples of 100.
        for (i, p) in out.iter().enumerate() {
            assert_eq!(p.ts, i as i64 * 100);
        }
    }

    #[test]
    fn buckets_average_percentile_fields() {
        // Build a bucket where median values are 10, 20, 30 - the consolidated
        // median for that bucket must be 20 (the arithmetic mean).
        let raw = vec![s(0, 10.0), s(10, 20.0), s(20, 30.0), s(1000, 99.0)];
        let (_, out) = downsample_to_budget(raw, 0, 1100, 2, 1);
        // span=1100, max=2 -> bucket=550. First bucket [0,550) holds the
        // three 10/20/30 samples; second bucket holds the 99.
        assert_eq!(out.len(), 2);
        let first = out[0].median.unwrap();
        assert!((first - 20.0).abs() < 0.001, "first median = {first}");
        assert_eq!(out[1].median, Some(99.0));
    }
}

fn parse_kind(s: &str) -> ApiResult<ProbeKind> {
    Ok(match s {
        "ping" => ProbeKind::Ping,
        "dns" => ProbeKind::Dns,
        "tcp_connect" => ProbeKind::TcpConnect,
        "tls_connect" => ProbeKind::TlsConnect,
        "http_ttfb" => ProbeKind::HttpTtfb,
        "http_total" => ProbeKind::HttpTotal,
        other => {
            return Err(ApiError::BadRequest(format!(
                "unknown probe_type '{other}'"
            )));
        }
    })
}

#[derive(Serialize, ToSchema)]
pub(crate) struct HostResp {
    pub uuid: Uuid,
    /// Empty array = root-level (no parent groups). A host can belong to any
    /// number of groups; the tree shows it under each.
    pub group_uuids: Vec<Uuid>,
    pub display_name: String,
    pub probe_type: String,
    #[schema(value_type = Object, additional_properties = true)]
    pub probe_config: serde_json::Value,
    pub interval_secs: i64,
    pub samples_per_period: i64,
    /// Chunk window this host was created with; frozen for life.
    pub chunk_window_secs: i64,
    pub enabled: bool,
    pub created_at: i64,
}

impl From<Host> for HostResp {
    fn from(h: Host) -> Self {
        let probe_config: serde_json::Value =
            serde_json::from_str(&h.probe_config).unwrap_or(serde_json::Value::Null);
        Self {
            uuid: h.uuid_typed(),
            group_uuids: h.group_uuids,
            display_name: h.display_name,
            probe_type: h.probe_type,
            probe_config,
            interval_secs: h.interval_secs,
            samples_per_period: h.samples_per_period,
            chunk_window_secs: h.chunk_window_secs,
            enabled: h.enabled != 0,
            created_at: h.created_at,
        }
    }
}
