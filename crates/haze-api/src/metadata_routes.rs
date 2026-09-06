//! Route history uses metadata indexes; full paths are loaded only on selection.
use crate::{
    AppState,
    error::{ApiError, ApiResult},
    middleware::ViewerAccess,
};
use axum::{
    Json,
    extract::{Path, Query, State},
};
use haze_store::{MetadataRecord, repo::hosts};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Deserialize)]
pub struct HistoryQuery {
    pub from: i64,
    pub to: i64,
    pub before: Option<String>,
    pub newer: Option<bool>,
    pub at: Option<i64>,
    pub all: Option<bool>,
    pub limit: Option<usize>,
}
#[derive(Clone, Default, Serialize, ToSchema)]
pub struct TimelineBucket {
    pub timestamp: i64,
    pub traces: usize,
    pub changes: usize,
    pub gaps: usize,
    pub loss_pct: f64,
}
#[derive(Serialize, ToSchema)]
pub struct HistoryResponse {
    pub records: Vec<MetadataRecord>,
    pub next: Option<String>,
    pub newer: Option<String>,
    pub timeline: Vec<TimelineBucket>,
    pub total: usize,
    pub support: String,
}
#[derive(Serialize, ToSchema)]
pub struct TraceDetail {
    pub selected: MetadataRecord,
    pub trace: Option<MetadataRecord>,
    pub previous: Option<MetadataRecord>,
}
fn internal(e: impl std::fmt::Display) -> ApiError {
    ApiError::Internal(e.to_string())
}
#[utoipa::path(get,path="/api/v1/hosts/{uuid}/route-history",params(("uuid"=Uuid,Path),("from"=i64,Query),("to"=i64,Query),("before"=Option<String>,Query),("newer"=Option<bool>,Query),("at"=Option<i64>,Query),("all"=Option<bool>,Query),("limit"=Option<usize>,Query)),responses((status=200,body=HistoryResponse)),tag="hosts")]
pub async fn history(
    _viewer: ViewerAccess,
    State(state): State<AppState>,
    Path(uuid): Path<Uuid>,
    Query(q): Query<HistoryQuery>,
) -> ApiResult<Json<HistoryResponse>> {
    let host = hosts::get_by_uuid(&state.pool, uuid)
        .await?
        .ok_or(ApiError::NotFound)?;
    if host.probe_type != "ping" {
        return Err(ApiError::BadRequest(
            "Route history requires an ICMP host".into(),
        ));
    }
    if q.from < 0 || q.to <= q.from || q.to - q.from > 10 * 366 * 86400 {
        return Err(ApiError::BadRequest(
            "Invalid history range (maximum ten years)".into(),
        ));
    }
    let before = if let Some(cursor) = q.before {
        let (ts, id) = cursor
            .split_once(':')
            .ok_or_else(|| ApiError::BadRequest("Invalid history cursor".into()))?;
        Some((
            ts.parse::<i64>()
                .map_err(|_| ApiError::BadRequest("Invalid cursor timestamp".into()))?,
            Uuid::parse_str(id).map_err(|_| ApiError::BadRequest("Invalid cursor ID".into()))?,
        ))
    } else {
        None
    };
    let store = state.hzc.metadata().clone();
    let response = tokio::task::spawn_blocking(move || -> anyhow::Result<HistoryResponse> {
        let index = store.index(uuid, q.from, q.to)?;
        let mut timeline = vec![TimelineBucket::default(); 240];
        let span = q.to - q.from;
        for (i, b) in timeline.iter_mut().enumerate() {
            b.timestamp = q.from + span * i as i64 / 240;
        }
        let mut loss = store
            .predecessor(uuid, q.from, Uuid::nil(), "loss")?
            .and_then(|r| r.data.get("loss_pct").and_then(serde_json::Value::as_f64))
            .unwrap_or(0.0);
        let mut loss_pos = q.from;
        for r in &index {
            if r.kind == "loss" {
                if r.timestamp >= q.from {
                    paint_loss(&mut timeline, q.from, span, loss_pos, r.timestamp, loss);
                }
                loss = r
                    .data
                    .get("loss_pct")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0);
                loss_pos = r.timestamp.max(q.from);
            }
            if r.timestamp < q.from {
                continue;
            }
            let bucket = ((r.timestamp - q.from) * 240 / span).clamp(0, 239) as usize;
            if r.kind == "trace" {
                timeline[bucket].traces += 1;
            }
            if r.event() == "route_changed" {
                timeline[bucket].changes += 1;
            }
            if matches!(r.event(), "trace_failed" | "collection_gap" | "incomplete") {
                timeline[bucket].gaps += 1;
            }
        }
        paint_loss(&mut timeline, q.from, span, loss_pos, q.to, loss);
        let mut records: Vec<_> = index
            .into_iter()
            .filter(|r| {
                matches!(r.kind.as_str(), "trace" | "loss")
                    && r.timestamp >= q.from
                    && (q.all.unwrap_or(false) || !r.event().is_empty())
            })
            .collect();
        let total = records.len();
        records.reverse();
        let limit = q.limit.unwrap_or(100).clamp(1, 200);
        let offset = if let Some(cursor) = before {
            if q.newer.unwrap_or(false) {
                records
                    .partition_point(|r| (r.timestamp, r.id) > cursor)
                    .saturating_sub(limit)
            } else {
                records.partition_point(|r| (r.timestamp, r.id) >= cursor)
            }
        } else if let Some(at) = q.at {
            records
                .iter()
                .enumerate()
                .min_by_key(|(_, r)| r.timestamp.abs_diff(at))
                .map_or(0, |(i, _)| i.saturating_sub(limit / 2))
        } else {
            0
        };
        let end = offset.saturating_add(limit).min(records.len());
        let more = end < records.len();
        records = records[offset..end].to_vec();
        let next = if more {
            records.last().map(|r| format!("{}:{}", r.timestamp, r.id))
        } else {
            None
        };
        let newer = if offset > 0 {
            records.first().map(|r| format!("{}:{}", r.timestamp, r.id))
        } else {
            None
        };
        let support = if host.replication_peer_id.is_some() {
            store
                .read_checkpoint(uuid, "replication-support")?
                .as_str()
                .unwrap_or("pending")
                .to_owned()
        } else {
            "local".into()
        };
        Ok(HistoryResponse {
            records,
            next,
            newer,
            timeline,
            total,
            support,
        })
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;
    Ok(Json(response))
}
fn paint_loss(
    buckets: &mut [TimelineBucket],
    from: i64,
    span: i64,
    start: i64,
    end: i64,
    loss: f64,
) {
    if end < from || start > end || loss <= 0.0 {
        return;
    }
    let a = ((start - from) * 240 / span).clamp(0, 239) as usize;
    let b = ((end - from) * 240 / span).clamp(0, 239) as usize;
    for item in &mut buckets[a..=b] {
        item.loss_pct = item.loss_pct.max(loss);
    }
}
#[utoipa::path(get,path="/api/v1/hosts/{uuid}/route-history/{id}",params(("uuid"=Uuid,Path),("id"=Uuid,Path)),responses((status=200,body=TraceDetail)),tag="hosts")]
pub async fn detail(
    _viewer: ViewerAccess,
    State(state): State<AppState>,
    Path((uuid, id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<TraceDetail>> {
    hosts::get_by_uuid(&state.pool, uuid)
        .await?
        .ok_or(ApiError::NotFound)?;
    let store = state.hzc.metadata().clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<TraceDetail>> {
        let Some(selected) = store.get(uuid, id)? else {
            return Ok(None);
        };
        let trace = if selected.kind == "trace" {
            Some(selected.clone())
        } else {
            store.predecessor(uuid, selected.timestamp, Uuid::max(), "trace")?
        };
        let previous = if let Some(ref trace) = trace {
            comparison_trace(&store, trace)?
        } else {
            None
        };
        Ok(Some(TraceDetail {
            selected,
            trace,
            previous,
        }))
    })
    .await
    .map_err(internal)?
    .map_err(internal)?
    .ok_or(ApiError::NotFound)?;
    Ok(Json(result))
}

// New captures reference the complete checkpoint they were compared against.
// A queue gap or another partial trace must not replace that comparison in UI.
fn comparison_trace(
    store: &haze_store::MetadataStore,
    trace: &MetadataRecord,
) -> anyhow::Result<Option<MetadataRecord>> {
    if let Some(value) = trace.data.get("previous_id") {
        return match value.as_str().and_then(|id| Uuid::parse_str(id).ok()) {
            Some(id) => store.get(trace.host_uuid, id),
            None => Ok(None),
        };
    }
    store.predecessor(trace.host_uuid, trace.timestamp, trace.id, "trace")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn partial_trace_comparison_skips_intervening_queue_gap() {
        let directory = tempfile::tempdir().unwrap();
        let store = haze_store::MetadataStore::new(directory.path().into());
        let host = Uuid::new_v4();
        let complete = MetadataRecord::new(
            host,
            10,
            "trace",
            json!({"hops":[]}),
            json!({"reached":true}),
        );
        let gap = MetadataRecord::new(
            host,
            20,
            "trace",
            json!(null),
            json!({"event":"collection_gap"}),
        );
        store.append_local(complete.clone(), 3600).unwrap();
        store.append_local(gap.clone(), 3600).unwrap();
        let mut partial = MetadataRecord::new(
            host,
            30,
            "trace",
            json!({"hops":[]}),
            json!({"event":"incomplete","previous_id":complete.id}),
        );
        assert_eq!(
            comparison_trace(&store, &partial).unwrap().unwrap().id,
            complete.id
        );
        partial.data["previous_id"] = json!(Uuid::new_v4());
        assert!(comparison_trace(&store, &partial).unwrap().is_none());
        partial.data.as_object_mut().unwrap().remove("previous_id");
        assert_eq!(
            comparison_trace(&store, &partial).unwrap().unwrap().id,
            gap.id
        );
    }
}
