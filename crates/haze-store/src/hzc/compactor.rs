//! Compactor - walks a host's chunks and applies its retention policy.
//!
//! For each chunk:
//! - compute `age = now - chunk.end_ts`,
//! - look up the policy's target resolution for that age,
//! - if the chunk's current resolution is already ≥ target, do nothing,
//! - if the policy says "delete" (age past the last tier), unlink the chunk,
//! - otherwise group adjacent chunks into the new resolution's natural
//!   window and write a single aggregated chunk in their place.
//!
//! Aggregation uses the same NaN-aware percentile-mean consolidation as the
//! legacy `.hzr` downsampler.

use std::{collections::BTreeMap, path::Path};

use crate::hzc::{
    chunk::decode_chunk,
    format::ChunkRef,
    reader::list_chunks,
    writer::{HzcError, RetentionTier, host_directory, seal_chunk_inline},
};
use crate::{aggregate::consolidate, slot::Slot};
use std::fs;
use uuid::Uuid;

#[derive(Debug, Default, Clone)]
pub struct CompactReport {
    pub aggregated_chunks: usize,
    pub deleted_chunks: usize,
    pub source_chunks_consumed: usize,
}

impl CompactReport {
    pub fn merge(&mut self, other: &Self) {
        self.aggregated_chunks += other.aggregated_chunks;
        self.deleted_chunks += other.deleted_chunks;
        self.source_chunks_consumed += other.source_chunks_consumed;
    }
}

/// Determine the target resolution for data of the given age.
///
/// Tiers are ordered ascending by `max_age_secs`. The first tier whose
/// `max_age_secs` is greater than or equal to `age` wins. If age exceeds
/// every tier, the chunk is deleted (returned as `None`).
fn target_resolution(age_secs: i64, tiers: &[RetentionTier]) -> Option<u32> {
    for tier in tiers {
        if age_secs <= tier.max_age_secs {
            return Some(tier.resolution_secs);
        }
    }
    None
}

/// Compact a host directory in-place. `now_secs` lets tests pin wall-clock.
pub fn compact_host(
    data_dir: &Path,
    host_uuid: Uuid,
    tiers: &[RetentionTier],
    now_secs: i64,
) -> Result<CompactReport, HzcError> {
    let host_dir = host_directory(data_dir, host_uuid);
    if !host_dir.exists() {
        return Ok(CompactReport::default());
    }
    compact_in_dir(&host_dir, tiers, now_secs)
}

pub fn compact_in_dir(
    host_dir: &Path,
    tiers: &[RetentionTier],
    now_secs: i64,
) -> Result<CompactReport, HzcError> {
    let mut report = CompactReport::default();
    let chunks = list_chunks(host_dir)?;

    // First pass: classify each chunk.
    let mut to_delete: Vec<&ChunkRef> = Vec::new();
    let mut to_aggregate: Vec<(u32, &ChunkRef)> = Vec::new();
    for c in &chunks {
        let age = now_secs - c.end_ts;
        match target_resolution(age, tiers) {
            None => to_delete.push(c),
            Some(target) => {
                if target == 0 || c.resolution_secs >= target {
                    // Already at or below target resolution (numerically
                    // higher resolution_secs means coarser data).
                } else {
                    to_aggregate.push((target, c));
                }
            }
        }
    }

    // Group aggregations by `(target_res, aggregated_window_start)` -
    // a chunk's "aggregated window" is the natural `target_res * N` window
    // it falls into. We pick N = a few hundred samples worth - using
    // `target_res * 256` as the window size keeps each aggregated chunk
    // around a few KB.
    let mut groups: BTreeMap<(u32, i64), Vec<&ChunkRef>> = BTreeMap::new();
    for (target_res, src) in to_aggregate {
        let window_secs = i64::from(target_res) * 256;
        let win_start = src.start_ts.div_euclid(window_secs) * window_secs;
        groups.entry((target_res, win_start)).or_default().push(src);
    }

    let mut next_seq = chunks.iter().map(|c| c.seq).max().unwrap_or(0) + 1;
    for ((target_res, win_start), srcs) in groups {
        let window_secs = i64::from(target_res) * 256;
        let win_end = win_start + window_secs;
        // Decode every source chunk, collect samples, bucket-bin them.
        let mut all_samples: Vec<(i64, Slot)> = Vec::new();
        for s in &srcs {
            let bytes = fs::read(&s.path)?;
            let decoded = decode_chunk(&bytes)?;
            all_samples.extend(decoded);
        }
        if all_samples.is_empty() {
            for s in &srcs {
                let _ = fs::remove_file(&s.path);
            }
            continue;
        }
        all_samples.sort_by_key(|(ts, _)| *ts);

        // Bin into target_res buckets.
        let res = i64::from(target_res);
        let mut bins: BTreeMap<i64, Vec<Slot>> = BTreeMap::new();
        for (ts, slot) in all_samples {
            let bucket = ts.div_euclid(res) * res;
            bins.entry(bucket).or_default().push(slot);
        }
        let aggregated: Vec<(i64, Slot)> = bins
            .into_iter()
            .map(|(ts, slots)| (ts, consolidate(&slots)))
            .collect();
        if aggregated.is_empty() {
            for s in &srcs {
                let _ = fs::remove_file(&s.path);
            }
            continue;
        }
        let agg_start = aggregated.first().unwrap().0;
        let agg_end = aggregated.last().unwrap().0 + res;
        let _ = win_end; // window framing used only for grouping

        // Write the new aggregated chunk first, then delete sources - readers
        // tolerate "extra" overlapping data while compaction is in flight.
        seal_chunk_inline(
            host_dir,
            next_seq,
            agg_start,
            agg_end,
            target_res,
            &aggregated,
        )?;
        report.aggregated_chunks += 1;
        report.source_chunks_consumed += srcs.len();
        next_seq += 1;
        for s in srcs {
            let _ = fs::remove_file(&s.path);
        }
    }

    // Deletions (chunks older than the retention horizon).
    for c in to_delete {
        let _ = fs::remove_file(&c.path);
        report.deleted_chunks += 1;
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hzc::writer::HostWriter;
    use tempfile::TempDir;

    fn slot(median: f32) -> Slot {
        Slot {
            min: median - 0.2,
            p2_5: median - 0.1,
            p25: median - 0.05,
            median,
            p75: median + 0.05,
            p97_5: median + 0.1,
            loss_pct: 0.0,
        }
    }

    #[test]
    fn empty_directory_is_a_noop() {
        let dir = TempDir::new().unwrap();
        let r = compact_in_dir(dir.path(), &[], 0).unwrap();
        assert_eq!(r.aggregated_chunks, 0);
        assert_eq!(r.deleted_chunks, 0);
    }

    #[test]
    fn deletes_chunks_past_horizon() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let w = HostWriter::open(dir.path(), uuid, 30, 60).unwrap();
        for i in 0..120 {
            w.write_sample(i, slot(21.0)).unwrap();
        }
        w.flush().unwrap();

        // Tiers say: anything ≤ 30 s old is raw; anything older is deleted.
        let tiers = vec![RetentionTier {
            max_age_secs: 30,
            resolution_secs: 0,
        }];
        // Pretend current time is way in the future so every chunk is past horizon.
        let now = 10_000;
        let r = compact_in_dir(&host_directory(dir.path(), uuid), &tiers, now).unwrap();
        assert_eq!(r.deleted_chunks, 2);
        let remaining = list_chunks(&host_directory(dir.path(), uuid)).unwrap();
        assert!(remaining.is_empty());
    }

    #[test]
    fn aggregates_old_chunks_to_coarser_resolution() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let w = HostWriter::open(dir.path(), uuid, 1, 60).unwrap();
        // 120 raw 1-second samples → 2 chunks at 60 s/window.
        for i in 0..120 {
            w.write_sample(i, slot(20.0 + (i as f32) / 100.0)).unwrap();
        }
        w.flush().unwrap();

        // Tier: anything older than 10s gets aggregated to 30-second resolution.
        // Set now far enough that both chunks are eligible.
        let tiers = vec![
            RetentionTier {
                max_age_secs: 10,
                resolution_secs: 0,
            },
            RetentionTier {
                max_age_secs: 10_000,
                resolution_secs: 30,
            },
        ];
        let now = 10_000;
        let host_dir = host_directory(dir.path(), uuid);
        let report = compact_in_dir(&host_dir, &tiers, now).unwrap();
        assert!(report.aggregated_chunks >= 1);
        assert!(report.source_chunks_consumed >= 2);

        // After compaction the directory should hold a single aggregated chunk
        // at resolution 30, no raw chunks left.
        let remaining = list_chunks(&host_dir).unwrap();
        assert!(remaining.iter().all(|c| c.resolution_secs == 30));
        // 120 raw seconds binned into 30 s buckets → 4 aggregated samples.
        // They fit in one chunk (window = 30 * 256 = 7680 s, plenty).
        assert_eq!(remaining.len(), 1);
    }
}
