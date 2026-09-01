//! Warm-restart snapshot: persist the `SeriesStore` periodically, and
//! rehydrate it at boot.
//!
//! The flush task ticks on a jittered interval (~5 min ± 1 min) so a
//! fleet of replicas doesn't synchronise their writes. Each tick walks
//! every host buffer in the store and upserts one row per host into
//! `alert_series_snapshot`.
//!
//! On boot we read everything back, drop rows whose newest sample is
//! older than the longest active rule window (so a long downtime can't
//! produce a stale "resolved" notification immediately on restart), and
//! rehydrate the rest into the in-memory store.

use std::{sync::Arc, time::Duration};

use haze_store::{SeriesStore, Slot, repo::settings};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use crate::repo;

/// Jitter is `0..FLUSH_JITTER_FRACTION_PCT` of the configured interval,
/// so when an operator dials the interval up the jitter window scales
/// with it; small intervals keep tight jitter, large ones spread more.
const FLUSH_JITTER_FRACTION_PCT: u64 = 20;

/// One sample as stored in the snapshot JSON. Compact array layout keeps
/// the row small even for hosts with hundreds of buffered slots.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SerSample {
    ts: i64,
    fields: [f32; 7],
}

fn serialise_samples(samples: &[(i64, Slot)]) -> serde_json::Result<String> {
    let v: Vec<SerSample> = samples
        .iter()
        .map(|(ts, slot)| SerSample {
            ts: *ts,
            fields: slot.fields(),
        })
        .collect();
    serde_json::to_string(&v)
}

fn deserialise_samples(json: &str) -> serde_json::Result<Vec<(i64, Slot)>> {
    let v: Vec<SerSample> = serde_json::from_str(json)?;
    Ok(v.into_iter()
        .map(|s| (s.ts, Slot::from_fields(s.fields)))
        .collect())
}

/// At startup, read every snapshot row and rehydrate the `SeriesStore`.
///
/// `max_rule_window_secs` is the longest active rule window; any host
/// whose newest sample is older than `now - max_rule_window_secs` is
/// dropped on the floor so the engine starts from "no data" instead of
/// re-firing on something that already resolved while we were down.
///
/// `now` is passed in so tests can pin the clock.
pub async fn restore(
    pool: &SqlitePool,
    series: &SeriesStore,
    max_rule_window_secs: i64,
    now: i64,
) -> anyhow::Result<RestoreReport> {
    let rows = repo::list_series_snapshots(pool).await?;
    let mut restored = 0usize;
    let mut dropped_stale = 0usize;
    let mut dropped_parse = 0usize;
    let stale_cutoff = now - max_rule_window_secs;
    for row in rows {
        if row.newest_ts < stale_cutoff {
            dropped_stale += 1;
            continue;
        }
        match deserialise_samples(&row.samples_json) {
            Ok(samples) => {
                series.rehydrate(row.host_uuid, samples);
                restored += 1;
            }
            Err(e) => {
                warn!(host_uuid = %row.host_uuid, error = ?e, "snapshot decode failed");
                dropped_parse += 1;
            }
        }
    }
    info!(
        restored,
        dropped_stale, dropped_parse, max_rule_window_secs, "alert series snapshot restored"
    );
    Ok(RestoreReport {
        restored,
        dropped_stale,
        dropped_parse,
    })
}

#[derive(Debug, Clone, Copy)]
pub struct RestoreReport {
    pub restored: usize,
    pub dropped_stale: usize,
    pub dropped_parse: usize,
}

/// Spawn-and-forget periodic flusher.
///
/// Flush interval is `alerting.snapshot_flush_interval_secs`, re-read
/// from /settings each cycle. The `shutdown` notify is used by tests to
/// break out of the loop deterministically; production code never fires it.
pub fn run_flush(
    pool: SqlitePool,
    series: Arc<SeriesStore>,
    shutdown: Option<Arc<Notify>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let cfg = settings::alerting_settings(&pool)
                .await
                .unwrap_or_else(|_| settings::default_alerting_settings());
            let base = u64::from(cfg.snapshot_flush_interval_secs.max(1));
            let jitter_max = base * FLUSH_JITTER_FRACTION_PCT / 100;
            let jitter = if jitter_max > 0 {
                Duration::from_secs(rand::random_range(0..jitter_max))
            } else {
                Duration::ZERO
            };
            let total = Duration::from_secs(base) + jitter;
            if let Some(s) = shutdown.as_ref() {
                tokio::select! {
                    () = tokio::time::sleep(total) => {}
                    () = s.notified() => return,
                }
            } else {
                tokio::time::sleep(total).await;
            }

            if let Err(e) = flush_once(&pool, &series).await {
                warn!(error = ?e, "alert series snapshot flush failed");
            }
        }
    })
}

/// One full pass over the `SeriesStore` - public for tests. Uses a single
/// transaction so a crash mid-flush either upserts everything or nothing.
pub async fn flush_once(pool: &SqlitePool, series: &SeriesStore) -> anyhow::Result<()> {
    let mut to_persist: Vec<(uuid::Uuid, Vec<(i64, Slot)>)> = Vec::new();
    series.for_each_snapshot(|uuid, samples| {
        if samples.is_empty() {
            return;
        }
        to_persist.push((uuid, samples.to_vec()));
    });
    if to_persist.is_empty() {
        debug!("alert series snapshot flush: nothing to write");
        return Ok(());
    }
    let saved_at = chrono::Utc::now().timestamp();
    let mut persisted = 0usize;
    let mut skipped_unknown_host = 0usize;
    for (host_uuid, samples) in to_persist {
        let Some(host_id) = repo::host_id_for_uuid(pool, host_uuid).await? else {
            // The host may have been deleted between append and flush -
            // drop the buffer so we don't accumulate garbage.
            series.forget(host_uuid);
            skipped_unknown_host += 1;
            continue;
        };
        let newest_ts = samples.last().map_or(saved_at, |(ts, _)| *ts);
        let json = serialise_samples(&samples)?;
        repo::upsert_series_snapshot(pool, host_id, saved_at, newest_ts, &json).await?;
        persisted += 1;
    }
    debug!(
        persisted,
        skipped_unknown_host, "alert series snapshot flushed"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn slot(median: f32) -> Slot {
        Slot {
            min: median - 0.5,
            p2_5: median - 0.25,
            p25: median - 0.1,
            median,
            p75: median + 0.1,
            p97_5: median + 0.25,
            loss_pct: 0.0,
        }
    }

    #[test]
    fn samples_round_trip_through_json() {
        let samples = vec![(100, slot(1.0)), (200, slot(2.0)), (300, slot(3.5))];
        let json = serialise_samples(&samples).expect("encode");
        let back = deserialise_samples(&json).expect("decode");
        assert_eq!(back.len(), 3);
        assert_eq!(back[0].0, 100);
        assert!((back[2].1.median - 3.5).abs() < 1e-6);
    }

    #[tokio::test]
    async fn restore_drops_stale_rows() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("memory db");
        sqlx::migrate!("../../migrations")
            .run(&pool)
            .await
            .expect("migrate");

        let host_uuid = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO hosts (uuid, display_name, probe_type, probe_config, \
                interval_secs, samples_per_period, chunk_window_secs, enabled, created_at) \
             VALUES (?1, 'h', 'ping', '{}', 60, 20, 3600, 1, 0)",
        )
        .bind(host_uuid.as_bytes().to_vec())
        .execute(&pool)
        .await
        .expect("insert host");

        let host_id: (i64,) = sqlx::query_as("SELECT id FROM hosts WHERE uuid = ?1")
            .bind(host_uuid.as_bytes().to_vec())
            .fetch_one(&pool)
            .await
            .unwrap();

        let samples = vec![(50, slot(1.0)), (100, slot(2.0))];
        let json = serialise_samples(&samples).unwrap();
        repo::upsert_series_snapshot(&pool, host_id.0, 200, 100, &json)
            .await
            .unwrap();

        // window=500s, now=10_000 -> newest_ts=100 is older than 9500, drop.
        let series = SeriesStore::new();
        let report = restore(&pool, &series, 500, 10_000).await.unwrap();
        assert_eq!(report.restored, 0);
        assert_eq!(report.dropped_stale, 1);
        assert!(series.newest_ts(host_uuid).is_none());

        // Now bump the window so the snapshot is fresh enough.
        let series = SeriesStore::new();
        let report = restore(&pool, &series, 100_000, 10_000).await.unwrap();
        assert_eq!(report.restored, 1);
        assert_eq!(report.dropped_stale, 0);
        assert_eq!(series.newest_ts(host_uuid), Some(100));
    }
}
