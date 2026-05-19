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
//!
//! Daily rollup (separate from the retention compactor above) bundles every
//! per-window chunk for a settled UTC day into one zstd file, eliminating
//! the per-file filesystem block overhead. It runs single-threaded so it
//! never piles I/O on the live writer; see [`rollup_settled_days_in_dir`].

use std::{collections::BTreeMap, path::Path};

use chrono::{DateTime, Utc};

use crate::hzc::{
    chunk::{decode_chunk, read_header},
    format::{CHUNK_EXTENSION, ChunkRef, is_legacy_chunk_name, parse_chunk_filename},
    reader::list_chunks,
    writer::{CHUNKS_DIR, HzcError, RetentionTier, host_directory, seal_chunk_inline},
};
use crate::{aggregate::consolidate, slot::Slot};
use std::fs;
use uuid::Uuid;

/// Quarantine directory inside `chunks/` for files whose header fails the
/// shape check during migration. Operator-triage zone; files are never
/// auto-deleted from here.
const QUARANTINE_DIR: &str = ".quarantine";

/// Seconds per UTC day.
const SECS_PER_DAY: i64 = 86_400;

/// Default settle margin for the daily rollup: ignore any day whose end is
/// less than this many seconds in the past. One hour is plenty to let the
/// writer finish sealing any chunk that crossed midnight.
pub const DEFAULT_ROLLUP_SETTLED_AFTER_SECS: i64 = 3_600;

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
            0, // generation 0 - downsampler emits per-tier chunks; daily rollup re-bundles them later.
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

// =====================================================================
// Daily rollup + one-time migration
// =====================================================================

#[derive(Debug, Default, Clone)]
pub struct MigrationReport {
    /// Legacy 4-segment chunk files renamed to the 5-segment grammar.
    pub renamed: usize,
    /// Files whose 12-byte header failed validation and were moved into
    /// `.quarantine/` for operator triage.
    pub quarantined: usize,
    /// `.tmp` orphans (interrupted writes or renames) cleaned up.
    pub tmp_removed: usize,
}

impl MigrationReport {
    pub fn touched_anything(&self) -> bool {
        self.renamed > 0 || self.quarantined > 0 || self.tmp_removed > 0
    }
}

#[derive(Debug, Default, Clone)]
pub struct RollupReport {
    /// `(resolution_secs, utc_day_start_ts)` pairs that were successfully bundled.
    pub bundled_days: usize,
    /// Per-window chunks consumed by all bundles in this pass.
    pub source_chunks_consumed: usize,
    /// Groups skipped because the day isn't yet 1 h past UTC midnight.
    pub skipped_unsettled: usize,
    /// Groups skipped because a `g1` bundle already covers the day.
    pub skipped_already_bundled: usize,
    /// Groups skipped because bundling wouldn't shrink the file count.
    pub skipped_singleton: usize,
    /// Groups skipped because the verify-before-delete check failed.
    pub verify_failed: usize,
    /// Total bytes occupied by source files before bundling. Useful for
    /// computing the compression ratio in the per-host log line.
    pub bytes_before: u64,
    /// Total bytes occupied by bundle files after writing.
    pub bytes_after: u64,
}

impl RollupReport {
    pub fn did_work(&self) -> bool {
        self.bundled_days > 0
    }
}

/// Migrate a host's `chunks/` directory to the canonical 5-segment grammar.
///
/// Renames legacy 4-segment chunks, validates each one's 12-byte HZC header,
/// and quarantines any that fail. Also sweeps leftover `.tmp` files from a
/// previously-interrupted rollup.
pub fn migrate_and_verify_in_dir(host_dir: &Path) -> Result<MigrationReport, HzcError> {
    let mut report = MigrationReport::default();
    let chunks_dir = host_dir.join(CHUNKS_DIR);
    if !chunks_dir.exists() {
        return Ok(report);
    }

    for entry in fs::read_dir(&chunks_dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };

        // .tmp orphans: a previous run died after creating the temp file but
        // before the atomic rename. The contents are useless either way.
        if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("tmp"))
        {
            let _ = fs::remove_file(&path);
            report.tmp_removed += 1;
            continue;
        }

        if !is_legacy_chunk_name(name) {
            continue;
        }

        match read_header(&path) {
            Ok(header) => {
                tracing::trace!(
                    path = %path.display(),
                    version = header.version,
                    samples = header.sample_count,
                    "hzc legacy chunk header ok"
                );
            }
            Err(e) => {
                let qdir = chunks_dir.join(QUARANTINE_DIR);
                let _ = fs::create_dir_all(&qdir);
                let dest = qdir.join(name);
                if let Err(rename_err) = fs::rename(&path, &dest) {
                    tracing::error!(
                        path = %path.display(),
                        error = ?rename_err,
                        "hzc quarantine rename failed"
                    );
                } else {
                    tracing::warn!(
                        path = %path.display(),
                        moved_to = %dest.display(),
                        reason = %e,
                        "hzc quarantined chunk with bad header"
                    );
                    report.quarantined += 1;
                }
                continue;
            }
        }

        // Parse to extract seq/res/start/end - the legacy filename's
        // grammar guarantees this succeeds because `is_legacy_chunk_name`
        // already vetted it.
        let Ok(cr) = parse_chunk_filename(&path) else {
            continue;
        };
        let new_name = format!(
            "{:06}_r{}_g0_{}_{}{CHUNK_EXTENSION}",
            cr.seq, cr.resolution_secs, cr.start_ts, cr.end_ts
        );
        let dest = chunks_dir.join(&new_name);
        if let Err(e) = fs::rename(&path, &dest) {
            tracing::warn!(
                from = %path.display(),
                to = %dest.display(),
                error = ?e,
                "hzc legacy rename failed"
            );
            continue;
        }
        report.renamed += 1;
    }

    Ok(report)
}

/// Bundle every per-window chunk that belongs to a fully-settled UTC day
/// into a single `g1` chunk, then delete the sources. See the module
/// docstring for crash-safety properties.
pub fn rollup_settled_days_in_dir(
    host_dir: &Path,
    now_secs: i64,
    settled_after_secs: i64,
) -> Result<RollupReport, HzcError> {
    let mut report = RollupReport::default();
    if !host_dir.join(CHUNKS_DIR).exists() {
        return Ok(report);
    }

    // Group chunks by (resolution_secs, utc_day_of_start_ts). A chunk's
    // "day" is the day its first sample falls in.
    let mut groups: BTreeMap<(u32, i64), Vec<ChunkRef>> = BTreeMap::new();
    for c in list_chunks(host_dir)? {
        let utc_day = c.start_ts.div_euclid(SECS_PER_DAY);
        groups
            .entry((c.resolution_secs, utc_day))
            .or_default()
            .push(c);
    }

    // Seed `next_seq` once before the loop. Each bundle bumps it locally and
    // we re-derive only if we run out of safety distance (we won't - every
    // bundled seq is strictly greater than every on-disk seq at the moment
    // we observed them).
    let mut next_seq = groups
        .values()
        .flat_map(|g| g.iter().map(|c| c.seq))
        .max()
        .unwrap_or(0)
        + 1;

    for ((res_secs, utc_day), srcs) in groups {
        let day_start_ts = utc_day * SECS_PER_DAY;
        let day_end_ts = day_start_ts + SECS_PER_DAY;

        if day_end_ts + settled_after_secs > now_secs {
            report.skipped_unsettled += 1;
            continue;
        }

        let has_bundle_covering_day = srcs
            .iter()
            .any(|c| c.generation >= 1 && c.start_ts <= day_start_ts && c.end_ts >= day_end_ts);
        if has_bundle_covering_day {
            // Clean up any lingering g0 sources from a crashed previous pass.
            let mut swept = 0usize;
            for c in &srcs {
                if c.generation == 0 {
                    if let Err(e) = fs::remove_file(&c.path) {
                        tracing::debug!(
                            path = %c.path.display(),
                            error = ?e,
                            "hzc rollup post-bundle cleanup remove failed"
                        );
                    } else {
                        swept += 1;
                    }
                }
            }
            if swept > 0 {
                tracing::info!(
                    res_secs,
                    day = %format_utc_day(day_start_ts),
                    swept,
                    "hzc rollup swept stale g0 sources after existing bundle"
                );
            }
            report.skipped_already_bundled += 1;
            continue;
        }

        if srcs.len() <= 1 {
            // Nothing to gain - either zero or one chunk; bundling produces
            // the same file count.
            report.skipped_singleton += 1;
            continue;
        }

        // Decode every source and merge.
        let mut samples: Vec<(i64, Slot)> = Vec::new();
        let mut bytes_before_group: u64 = 0;
        let mut decode_failed = false;
        for c in &srcs {
            let bytes = match fs::read(&c.path) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(
                        path = %c.path.display(),
                        error = ?e,
                        "hzc rollup source read failed; skipping day"
                    );
                    decode_failed = true;
                    break;
                }
            };
            bytes_before_group += bytes.len() as u64;
            match decode_chunk(&bytes) {
                Ok(decoded) => samples.extend(decoded),
                Err(e) => {
                    tracing::warn!(
                        path = %c.path.display(),
                        error = ?e,
                        "hzc rollup source decode failed; skipping day"
                    );
                    decode_failed = true;
                    break;
                }
            }
        }
        if decode_failed {
            continue;
        }

        if samples.is_empty() {
            // All-empty sources can happen if a chunk represented an empty
            // window. Just delete them to free their blocks.
            for c in &srcs {
                let _ = fs::remove_file(&c.path);
            }
            continue;
        }

        // Sort by timestamp; dedupe exact ts collisions (raw beats aggregated,
        // but we already grouped by resolution so this only matters if two
        // chunks at the same resolution overlap - unusual but cheap to handle).
        samples.sort_by_key(|(ts, _)| *ts);
        samples.dedup_by(|a, b| a.0 == b.0);

        let bundle_seq = next_seq;
        next_seq += 1;

        // seal_chunk_inline does encode → tmp → fsync → atomic rename.
        if let Err(e) = seal_chunk_inline(
            host_dir,
            bundle_seq,
            day_start_ts,
            day_end_ts,
            res_secs,
            1, // generation 1 - daily bundle
            &samples,
        ) {
            tracing::warn!(
                res_secs,
                day = %format_utc_day(day_start_ts),
                error = ?e,
                "hzc rollup seal failed; sources retained"
            );
            continue;
        }

        // Verify-before-delete: re-read the freshly-written bundle and
        // confirm the sample count round-trips. This is the last line of
        // defence against an encoder regression silently destroying a day of
        // data; if the count differs, leave both bundle and sources in place
        // for an operator to triage.
        let bundle_path = host_dir.join(CHUNKS_DIR).join(format!(
            "{bundle_seq:06}_r{res_secs}_g1_{day_start_ts}_{day_end_ts}{CHUNK_EXTENSION}"
        ));
        let bytes_after_group = match fs::metadata(&bundle_path) {
            Ok(m) => m.len(),
            Err(_) => 0,
        };
        let verify_ok = match fs::read(&bundle_path) {
            Ok(b) => match decode_chunk(&b) {
                Ok(d) => d.len() == samples.len(),
                Err(e) => {
                    tracing::warn!(
                        path = %bundle_path.display(),
                        error = ?e,
                        "hzc rollup bundle decode-verify failed"
                    );
                    false
                }
            },
            Err(e) => {
                tracing::warn!(
                    path = %bundle_path.display(),
                    error = ?e,
                    "hzc rollup bundle re-read failed"
                );
                false
            }
        };
        if !verify_ok {
            tracing::warn!(
                path = %bundle_path.display(),
                expected_samples = samples.len(),
                "hzc rollup verify-before-delete mismatch; bundle and sources retained for triage"
            );
            report.verify_failed += 1;
            continue;
        }

        // Delete sources now that the bundle is durable and verified.
        let src_count = srcs.len();
        for c in &srcs {
            let _ = fs::remove_file(&c.path);
        }

        let ratio = if bytes_after_group > 0 {
            bytes_before_group as f64 / bytes_after_group as f64
        } else {
            0.0
        };
        tracing::info!(
            res_secs,
            day = %format_utc_day(day_start_ts),
            sources = src_count,
            bytes_before = bytes_before_group,
            bytes_after = bytes_after_group,
            ratio = %format!("{ratio:.1}x"),
            samples = samples.len(),
            "hzc rolled up day"
        );

        report.bundled_days += 1;
        report.source_chunks_consumed += src_count;
        report.bytes_before += bytes_before_group;
        report.bytes_after += bytes_after_group;
    }

    Ok(report)
}

/// Run migration then daily rollup for one host. The single-threaded
/// rollup scheduler calls this once per host per pass.
pub fn rollup_host(
    data_dir: &Path,
    host_uuid: Uuid,
    now_secs: i64,
    settled_after_secs: i64,
) -> Result<(MigrationReport, RollupReport), HzcError> {
    let host_dir = host_directory(data_dir, host_uuid);
    if !host_dir.exists() {
        return Ok((MigrationReport::default(), RollupReport::default()));
    }
    let migration = migrate_and_verify_in_dir(&host_dir)?;
    let rollup = rollup_settled_days_in_dir(&host_dir, now_secs, settled_after_secs)?;
    Ok((migration, rollup))
}

fn format_utc_day(day_start_ts: i64) -> String {
    DateTime::<Utc>::from_timestamp(day_start_ts, 0).map_or_else(
        || day_start_ts.to_string(),
        |dt| dt.format("%Y-%m-%d").to_string(),
    )
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
    fn rollup_bundles_a_settled_day_into_one_chunk() {
        // Day D at the UNIX epoch (Jan 1 1970) - start_ts 0, end_ts 86_400.
        // Writer emits 60-second-window chunks so a full day = 1 440 chunks.
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let w = HostWriter::open(dir.path(), uuid, 60, 60).unwrap();
        for window in 0..1_440 {
            let ts = window * 60;
            w.write_sample(ts, slot(20.0 + (window as f32) / 1_440.0))
                .unwrap();
        }
        w.flush().unwrap();
        drop(w);

        let host_dir = host_directory(dir.path(), uuid);
        assert_eq!(list_chunks(&host_dir).unwrap().len(), 1_440);

        // now = 25 h past day-end so the day is settled.
        let now = 86_400 + 25 * 3_600;
        let report = rollup_settled_days_in_dir(&host_dir, now, 3_600).unwrap();
        assert_eq!(report.bundled_days, 1);
        assert_eq!(report.source_chunks_consumed, 1_440);
        assert!(report.bytes_after < report.bytes_before);

        let remaining = list_chunks(&host_dir).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].generation, 1);
        assert_eq!(remaining[0].start_ts, 0);
        assert_eq!(remaining[0].end_ts, 86_400);

        // Reading the bundle should return every original sample.
        let samples = crate::hzc::reader::read_range_in_dir(&host_dir, 0, 86_400).unwrap();
        assert_eq!(samples.len(), 1_440);
    }

    #[test]
    fn rollup_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let w = HostWriter::open(dir.path(), uuid, 60, 60).unwrap();
        for window in 0..1_440 {
            w.write_sample(window * 60, slot(21.0)).unwrap();
        }
        w.flush().unwrap();
        drop(w);

        let host_dir = host_directory(dir.path(), uuid);
        let now = 86_400 + 25 * 3_600;
        rollup_settled_days_in_dir(&host_dir, now, 3_600).unwrap();
        let after_first = list_chunks(&host_dir).unwrap();

        let second = rollup_settled_days_in_dir(&host_dir, now, 3_600).unwrap();
        assert_eq!(second.bundled_days, 0);
        assert_eq!(second.source_chunks_consumed, 0);
        let after_second = list_chunks(&host_dir).unwrap();
        assert_eq!(
            after_first.iter().map(|c| &c.path).collect::<Vec<_>>(),
            after_second.iter().map(|c| &c.path).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn rollup_leaves_unsettled_day_alone() {
        // Two days of data: day 0 settled, day 1 in-progress (now is just
        // past the start of day 1).
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let w = HostWriter::open(dir.path(), uuid, 60, 60).unwrap();
        for window in 0..2_880 {
            w.write_sample(window * 60, slot(21.0)).unwrap();
        }
        w.flush().unwrap();
        drop(w);

        let host_dir = host_directory(dir.path(), uuid);
        // now = 1h into day 1. settled_after = 3600. So day 0's settle
        // boundary is 86_400 + 3_600 = 90_000, day 1's is 172_800 + 3_600.
        // Day 0 is settled (now=90_000 ≥ 90_000)... we want strictly past so
        // pick now = 90_100.
        let now = 90_100;
        let report = rollup_settled_days_in_dir(&host_dir, now, 3_600).unwrap();
        assert_eq!(report.bundled_days, 1, "expected day 0 bundled");
        assert_eq!(report.skipped_unsettled, 1, "expected day 1 skipped");

        let remaining = list_chunks(&host_dir).unwrap();
        // 1 g1 bundle for day 0 plus all g0 chunks for day 1.
        let g1_count = remaining.iter().filter(|c| c.generation == 1).count();
        let g0_count = remaining.iter().filter(|c| c.generation == 0).count();
        assert_eq!(g1_count, 1);
        assert_eq!(g0_count, 1_440);
    }

    #[test]
    fn rollup_cleans_up_stale_g0_after_existing_bundle() {
        // Simulate a crashed previous rollup: the g1 bundle was written and
        // renamed, but only some g0 sources got deleted. The next pass must
        // detect "bundle already covers day" and finish the cleanup.
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let w = HostWriter::open(dir.path(), uuid, 60, 60).unwrap();
        for window in 0..1_440 {
            w.write_sample(window * 60, slot(21.0)).unwrap();
        }
        w.flush().unwrap();
        drop(w);

        let host_dir = host_directory(dir.path(), uuid);
        let now = 86_400 + 25 * 3_600;
        // First pass: produces the bundle and removes all g0s.
        rollup_settled_days_in_dir(&host_dir, now, 3_600).unwrap();
        // Re-create some legacy g0s to simulate a partial cleanup.
        let chunks_dir = host_dir.join(CHUNKS_DIR);
        let bundle = list_chunks(&host_dir)
            .unwrap()
            .into_iter()
            .find(|c| c.generation == 1)
            .unwrap();
        for window in 0..3 {
            // Hand-craft a tiny chunk in the same day; seq is unique.
            let tmp = chunks_dir.join(format!(
                "{:06}_r0_g0_{}_{}.hzc.zst.tmp",
                bundle.seq + 10 + window,
                window * 60,
                window * 60 + 60
            ));
            std::fs::write(&tmp, std::fs::read(&bundle.path).unwrap()).unwrap();
            std::fs::rename(
                &tmp,
                chunks_dir.join(format!(
                    "{:06}_r0_g0_{}_{}.hzc.zst",
                    bundle.seq + 10 + window,
                    window * 60,
                    window * 60 + 60
                )),
            )
            .unwrap();
        }
        assert_eq!(list_chunks(&host_dir).unwrap().len(), 4);

        let report = rollup_settled_days_in_dir(&host_dir, now, 3_600).unwrap();
        assert_eq!(report.skipped_already_bundled, 1);
        let remaining = list_chunks(&host_dir).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].generation, 1);
    }

    #[test]
    fn migration_renames_legacy_chunks() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let w = HostWriter::open(dir.path(), uuid, 60, 60).unwrap();
        for window in 0..10 {
            w.write_sample(window * 60, slot(21.0)).unwrap();
        }
        w.flush().unwrap();
        drop(w);

        let host_dir = host_directory(dir.path(), uuid);
        let chunks_dir = host_dir.join(CHUNKS_DIR);

        // Rename every 5-segment file back to the legacy 4-segment form so
        // the migration has something to do.
        let entries: Vec<_> = std::fs::read_dir(&chunks_dir)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        for entry in entries {
            let name = entry.file_name().into_string().unwrap();
            if let Some(stem) = name.strip_suffix(CHUNK_EXTENSION) {
                // 000001_r0_g0_60_120 → 000001_r0_60_120
                let parts: Vec<&str> = stem.split('_').collect();
                if parts.len() == 5 && parts[2].starts_with('g') {
                    let legacy = format!(
                        "{}_{}_{}_{}{CHUNK_EXTENSION}",
                        parts[0], parts[1], parts[3], parts[4]
                    );
                    std::fs::rename(entry.path(), chunks_dir.join(legacy)).unwrap();
                }
            }
        }
        // Sanity: now every name is legacy.
        for entry in std::fs::read_dir(&chunks_dir).unwrap().flatten() {
            let name = entry.file_name().into_string().unwrap();
            assert!(is_legacy_chunk_name(&name), "{name} should be legacy");
        }

        let report = migrate_and_verify_in_dir(&host_dir).unwrap();
        assert_eq!(report.renamed, 10);
        assert_eq!(report.quarantined, 0);

        // Idempotency: a second pass does nothing.
        let again = migrate_and_verify_in_dir(&host_dir).unwrap();
        assert_eq!(again.renamed, 0);
        assert_eq!(again.quarantined, 0);
    }

    #[test]
    fn migration_quarantines_bad_header() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let w = HostWriter::open(dir.path(), uuid, 60, 60).unwrap();
        w.write_sample(0, slot(21.0)).unwrap();
        w.flush().unwrap();
        drop(w);

        let host_dir = host_directory(dir.path(), uuid);
        let chunks_dir = host_dir.join(CHUNKS_DIR);

        // Plant a legacy-named file whose contents are not a valid HZC chunk.
        let bogus = chunks_dir.join("000999_r0_0_60.hzc.zst");
        std::fs::write(&bogus, b"not a zstd stream").unwrap();
        assert!(is_legacy_chunk_name(
            bogus.file_name().unwrap().to_str().unwrap()
        ));

        let report = migrate_and_verify_in_dir(&host_dir).unwrap();
        assert_eq!(report.quarantined, 1);
        // The bogus file should now be in .quarantine/ and not in chunks/
        // top-level any more.
        assert!(!bogus.exists());
        let q_path = chunks_dir
            .join(QUARANTINE_DIR)
            .join("000999_r0_0_60.hzc.zst");
        assert!(q_path.exists());
    }

    #[test]
    fn writer_and_rollup_coexist() {
        // The writer is creating fresh chunks for "today" while the rollup
        // pass is bundling "yesterday". They must not race on shared state;
        // the readback must return every sample written without duplication.
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let w = HostWriter::open(dir.path(), uuid, 60, 60).unwrap();
        // Yesterday: 1 440 minute chunks.
        for window in 0..1_440 {
            w.write_sample(window * 60, slot(20.0)).unwrap();
        }
        // Today (a few hours in): a handful of minute chunks.
        let today_base = 86_400;
        for window in 0..120 {
            w.write_sample(today_base + window * 60, slot(22.0))
                .unwrap();
        }
        w.flush().unwrap();

        let host_dir = host_directory(dir.path(), uuid);
        // now = 25 h past midnight UTC of "today". Yesterday's day-end was at
        // 86_400; with settle margin 3_600 the boundary is 90_000. now=90_100
        // settles yesterday but leaves today's in-progress chunks alone.
        let now = 90_100;
        let _ = rollup_settled_days_in_dir(&host_dir, now, 3_600).unwrap();

        let remaining = list_chunks(&host_dir).unwrap();
        let g1 = remaining.iter().filter(|c| c.generation == 1).count();
        assert_eq!(g1, 1);
        // Today's per-window chunks survived.
        let today_chunks = remaining
            .iter()
            .filter(|c| c.start_ts >= today_base)
            .count();
        assert_eq!(today_chunks, 120);

        // Full readback yields 1 560 samples without duplication.
        let samples =
            crate::hzc::reader::read_range_in_dir(&host_dir, 0, today_base + 120 * 60).unwrap();
        assert_eq!(samples.len(), 1_440 + 120);
    }

    #[test]
    fn live_wal_survives_bundle_seq_collision() {
        // Regression: the rollup pass picks seq = max(chunk seqs)+1, which
        // collides with the live writer's next-WAL seq. The reader's
        // "skip WAL whose seq has a sealed chunk" rule used to misfire on
        // the bundle and silently drop the open chunk's samples.
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        // 1 hour chunk windows; 24 hourly samples = 24 sealed chunks for day 0.
        {
            let w = HostWriter::open(dir.path(), uuid, 60, 3_600).unwrap();
            for h in 0..24 {
                w.write_sample(h * 3_600, slot(20.0)).unwrap();
            }
            w.flush().unwrap();
        }
        // Day 1: open a new writer and write one sample without flushing.
        // The host's next_seq is now 25; the open chunk + WAL both use seq 25.
        let w = HostWriter::open(dir.path(), uuid, 60, 3_600).unwrap();
        w.write_sample(86_400 + 60, slot(22.0)).unwrap();

        let host_dir = host_directory(dir.path(), uuid);
        // Sanity: 24 sealed chunks for day 0, no chunk yet for seq 25, WAL 25 exists.
        assert_eq!(list_chunks(&host_dir).unwrap().len(), 24);
        assert!(host_dir.join("wal").join("25.wal").exists());

        // Rollup with now ≥ day 0's settle horizon. Bundle is allocated as
        // seq=max(1..24)+1=25 - same seq the live WAL is using.
        let now = 86_400 + 25 * 3_600;
        let report = rollup_settled_days_in_dir(&host_dir, now, 3_600).unwrap();
        assert_eq!(report.bundled_days, 1);

        // The live writer's open WAL must still be visible to readers
        // covering day 1.
        let samples = crate::hzc::reader::read_range_in_dir(&host_dir, 0, 86_400 + 7_200).unwrap();
        assert_eq!(
            samples.len(),
            25,
            "expected 24 sealed-chunk samples + 1 live-WAL sample, got {}",
            samples.len()
        );
        assert!(samples.iter().any(|s| s.timestamp_secs == 86_400 + 60));

        drop(w);

        // Reopening the host writer must not delete the live WAL just
        // because a bundle exists at seq 25. The writer should replay the
        // WAL into a fresh open chunk.
        let w2 = HostWriter::open(dir.path(), uuid, 60, 3_600).unwrap();
        assert!(host_dir.join("wal").join("25.wal").exists());
        // Flushing now seals the WAL into a g0 chunk; the bundle remains too.
        w2.flush().unwrap();
        let after = list_chunks(&host_dir).unwrap();
        let g0_25 = after.iter().any(|c| c.seq == 25 && c.generation == 0);
        let g1_25 = after.iter().any(|c| c.seq == 25 && c.generation == 1);
        assert!(
            g0_25,
            "writer's restart should have sealed WAL 25 into a g0 chunk"
        );
        assert!(g1_25, "the day-0 bundle should still be present at g1");
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
