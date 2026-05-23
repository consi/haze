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

use chrono::{DateTime, Datelike, TimeZone, Utc};

use crate::hzc::{
    chunk::{
        ZSTD_LEVEL_G0, ZSTD_LEVEL_G1, ZSTD_LEVEL_G2, ZSTD_LEVEL_G3, decode_chunk, read_header,
        zstd_level_for_generation,
    },
    format::{CHUNK_EXTENSION, ChunkRef, bundle_seq, is_legacy_chunk_name, parse_chunk_filename},
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

    // Partition by generation. G0 chunks are short-span (default 1h) and
    // many; the legacy grouping pass consolidates a tier's worth of
    // adjacent G0s into one G0-at-coarser-resolution output, which keeps
    // intermediate file count down before the daily rollup picks them up.
    //
    // G1+ chunks already span at least a day, so the per-chunk
    // split-downsample preserves generation and span: a G3 yearly bundle
    // that ages into the next tier becomes a new G3 yearly bundle at the
    // coarser resolution (possibly multiple G3 segments if the year
    // straddles a tier boundary).
    let g0_chunks: Vec<&ChunkRef> = chunks.iter().filter(|c| c.generation == 0).collect();
    let higher_chunks: Vec<&ChunkRef> = chunks.iter().filter(|c| c.generation >= 1).collect();

    compact_g0_grouped(host_dir, &g0_chunks, tiers, now_secs, &mut report)?;
    for c in higher_chunks {
        compact_higher_gen_chunk(host_dir, c, tiers, now_secs, &mut report)?;
    }

    Ok(report)
}

#[derive(Debug)]
struct PendingOutput {
    seq: u64,
    start_ts: i64,
    end_ts: i64,
    resolution_secs: u32,
    samples: Vec<(i64, Slot)>,
    filename: String,
}

/// Existing legacy grouping path for G0 chunks. Chunks past the final tier
/// are deleted; chunks crossing into a coarser tier are merged (across
/// adjacent chunks) and re-encoded at the new resolution. Output is always
/// `generation = 0` so the daily rollup picks it up next.
fn compact_g0_grouped(
    host_dir: &Path,
    chunks: &[&ChunkRef],
    tiers: &[RetentionTier],
    now_secs: i64,
    report: &mut CompactReport,
) -> Result<(), HzcError> {
    let mut to_delete: Vec<&ChunkRef> = Vec::new();
    let mut to_aggregate: Vec<(u32, &ChunkRef)> = Vec::new();
    for c in chunks {
        let age = now_secs - c.end_ts;
        match target_resolution(age, tiers) {
            None => to_delete.push(c),
            Some(target) => {
                if target == 0 || c.resolution_secs >= target {
                    // Already at or below target resolution.
                } else {
                    to_aggregate.push((target, c));
                }
            }
        }
    }

    // Group by (target_res, target_res * 256 window) for batched encode.
    let mut groups: BTreeMap<(u32, i64), Vec<&ChunkRef>> = BTreeMap::new();
    for (target_res, src) in to_aggregate {
        let window_secs = i64::from(target_res) * 256;
        let win_start = src.start_ts.div_euclid(window_secs) * window_secs;
        groups.entry((target_res, win_start)).or_default().push(src);
    }

    for ((target_res, _win_start), srcs) in groups {
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

        // Deterministic seq so a crash+retry overwrites cleanly.
        let seq = bundle_seq(0, target_res, agg_start);
        seal_chunk_inline(
            host_dir,
            seq,
            agg_start,
            agg_end,
            target_res,
            0, // generation 0 - downsampler emits per-tier chunks; daily rollup re-bundles them later.
            ZSTD_LEVEL_G0,
            &aggregated,
        )?;
        report.aggregated_chunks += 1;
        report.source_chunks_consumed += srcs.len();
        for s in srcs {
            let _ = fs::remove_file(&s.path);
        }
    }

    for c in to_delete {
        let _ = fs::remove_file(&c.path);
        report.deleted_chunks += 1;
    }

    Ok(())
}

/// Per-chunk split-downsample for a G1+ source.
///
/// Decodes the source once and buckets every sample by tier based on its
/// **actual** timestamp age. This correctly handles sparse chunks where
/// the nominal `[start_ts, end_ts)` is wider than the range that actually
/// contains samples (common for monthly G2 / yearly G3 bundles when the
/// source data didn't span the full month / year). Emits one output per
/// non-empty tier bucket at the **same generation** as the source.
fn compact_higher_gen_chunk(
    host_dir: &Path,
    c: &ChunkRef,
    tiers: &[RetentionTier],
    now_secs: i64,
    report: &mut CompactReport,
) -> Result<(), HzcError> {
    // Decode source once. We need the actual sample timestamps to classify
    // - using only the chunk's nominal end_ts misclassifies sparse chunks
    // whose samples are far from the nominal span boundary.
    let bytes = fs::read(&c.path)?;
    let all_samples = decode_chunk(&bytes)?;

    if all_samples.is_empty() {
        // Empty chunk - safe to delete.
        let _ = fs::remove_file(&c.path);
        report.deleted_chunks += 1;
        report.source_chunks_consumed += 1;
        return Ok(());
    }

    // Bucket every sample by tier. Samples whose age exceeds every tier's
    // `max_age_secs` are dropped (past the retention horizon).
    let mut tier_buckets: BTreeMap<usize, Vec<(i64, Slot)>> = BTreeMap::new();
    let mut dropped = 0usize;
    for (ts, slot) in &all_samples {
        let age = now_secs - ts;
        let mut placed = false;
        for (i, tier) in tiers.iter().enumerate() {
            if age <= tier.max_age_secs {
                tier_buckets.entry(i).or_default().push((*ts, *slot));
                placed = true;
                break;
            }
        }
        if !placed {
            dropped += 1;
        }
    }

    // All samples past the horizon - drop the chunk entirely.
    if tier_buckets.is_empty() {
        let _ = fs::remove_file(&c.path);
        report.deleted_chunks += 1;
        report.source_chunks_consumed += 1;
        return Ok(());
    }

    // Fast path: if everything falls in one tier AND that tier's target
    // resolution is already satisfied by the source AND no samples were
    // dropped, the chunk is already correct. Skip without rewriting.
    if dropped == 0 && tier_buckets.len() == 1 {
        let (tier_idx, _samples_in_tier) = tier_buckets.iter().next().unwrap();
        let target_res = tiers[*tier_idx].resolution_secs;
        let satisfied = target_res == 0 || c.resolution_secs >= target_res;
        if satisfied {
            return Ok(());
        }
    }

    // Build outputs - one per tier bucket. Span comes from intersecting
    // the chunk's nominal span with the tier's age-boundary band so that
    // different tier outputs cover disjoint sub-ranges.
    let mut outputs: Vec<PendingOutput> = Vec::with_capacity(tier_buckets.len());

    for (tier_idx, tier_samples) in tier_buckets {
        let target_res = tiers[tier_idx].resolution_secs;

        // Output resolution: target_res == 0 means "raw" / no downsample.
        // Always keep the coarser of (target_res, source).
        let out_res = if target_res == 0 {
            c.resolution_secs
        } else if target_res > c.resolution_secs {
            target_res
        } else {
            c.resolution_secs
        };

        // Downsample if the target is coarser than the source.
        let processed: Vec<(i64, Slot)> = if out_res > c.resolution_secs {
            let res = i64::from(out_res);
            let mut bins: BTreeMap<i64, Vec<Slot>> = BTreeMap::new();
            for (ts, slot) in tier_samples {
                let bucket = ts.div_euclid(res) * res;
                bins.entry(bucket).or_default().push(slot);
            }
            bins.into_iter()
                .map(|(ts, slots)| (ts, consolidate(&slots)))
                .collect()
        } else {
            tier_samples
        };
        if processed.is_empty() {
            continue;
        }

        // Span for this output: intersection of the chunk's nominal span
        // with the tier's age-boundary band.
        let younger_bound = if tier_idx == 0 {
            c.end_ts
        } else {
            // Strictly younger boundary = older-tier's max_age boundary.
            now_secs - tiers[tier_idx - 1].max_age_secs
        };
        let older_bound = now_secs - tiers[tier_idx].max_age_secs;
        let seg_start = c.start_ts.max(older_bound);
        let seg_end = c.end_ts.min(younger_bound);
        if seg_end <= seg_start {
            continue;
        }

        let seq = bundle_seq(c.generation, out_res, seg_start);
        let filename =
            crate::hzc::format::chunk_filename(seq, out_res, c.generation, seg_start, seg_end);
        outputs.push(PendingOutput {
            seq,
            start_ts: seg_start,
            end_ts: seg_end,
            resolution_secs: out_res,
            samples: processed,
            filename,
        });
    }

    // No surviving outputs (all segments empty after binning) → drop source.
    if outputs.is_empty() {
        let _ = fs::remove_file(&c.path);
        report.deleted_chunks += 1;
        report.source_chunks_consumed += 1;
        return Ok(());
    }

    // If the only output exactly matches the source (same span, same
    // resolution, same generation), there's nothing to do.
    if outputs.len() == 1
        && outputs[0].start_ts == c.start_ts
        && outputs[0].end_ts == c.end_ts
        && outputs[0].resolution_secs == c.resolution_secs
    {
        return Ok(());
    }

    // Write each output. Use the same tmp+fsync+rename pattern as
    // `seal_chunk_inline`. Track written paths so we can verify and roll
    // back partial state on failure.
    let mut written_paths: Vec<std::path::PathBuf> = Vec::with_capacity(outputs.len());
    let level = zstd_level_for_generation(c.generation);
    let mut seal_ok = true;
    for o in &outputs {
        if let Err(e) = seal_chunk_inline(
            host_dir,
            o.seq,
            o.start_ts,
            o.end_ts,
            o.resolution_secs,
            c.generation,
            level,
            &o.samples,
        ) {
            tracing::warn!(
                source = %c.path.display(),
                segment_start = o.start_ts,
                segment_end = o.end_ts,
                error = ?e,
                "hzc split-downsample seal failed; source and partial outputs retained"
            );
            seal_ok = false;
            break;
        }
        written_paths.push(host_dir.join(CHUNKS_DIR).join(&o.filename));
    }

    if !seal_ok {
        // Leave partial outputs in place for triage. Source not deleted.
        return Ok(());
    }

    // Verify-before-delete: decode every output and confirm sample counts.
    let mut all_verified = true;
    for (o, path) in outputs.iter().zip(&written_paths) {
        let verified = fs::read(path)
            .ok()
            .and_then(|b| decode_chunk(&b).ok())
            .is_some_and(|d| d.len() == o.samples.len());
        if !verified {
            tracing::warn!(
                path = %path.display(),
                expected_samples = o.samples.len(),
                "hzc split-downsample verify-before-delete mismatch; source and outputs retained"
            );
            all_verified = false;
            break;
        }
    }
    if !all_verified {
        return Ok(());
    }

    // All outputs verified - delete source.
    let _ = fs::remove_file(&c.path);
    report.aggregated_chunks += outputs.len();
    report.source_chunks_consumed += 1;
    Ok(())
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

        let bundle_seq_val = bundle_seq(1, res_secs, day_start_ts);

        // seal_chunk_inline does encode → tmp → fsync → atomic rename.
        if let Err(e) = seal_chunk_inline(
            host_dir,
            bundle_seq_val,
            day_start_ts,
            day_end_ts,
            res_secs,
            1, // generation 1 - daily bundle
            ZSTD_LEVEL_G1,
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
            "{bundle_seq_val:06}_r{res_secs}_g1_{day_start_ts}_{day_end_ts}{CHUNK_EXTENSION}"
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

/// Default settle margin for the G2 monthly rollup.
///
/// Ignore any UTC month whose end is less than this many seconds in the
/// past. One day is enough for the G1 daily rollup to settle every day in
/// the month first.
pub const DEFAULT_ROLLUP_G2_SETTLED_AFTER_SECS: i64 = 86_400;

/// Default settle margin for the G3 yearly rollup.
///
/// Two days lets the G2 pass complete for December before we try to
/// bundle the year.
pub const DEFAULT_ROLLUP_G3_SETTLED_AFTER_SECS: i64 = 2 * 86_400;

// =====================================================================
// G2 (monthly) and G3 (yearly) rollup
// =====================================================================

/// `[start, end)` of the UTC calendar month containing `ts`.
fn utc_month_bounds(ts: i64) -> (i64, i64) {
    let dt =
        DateTime::<Utc>::from_timestamp(ts, 0).unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap());
    let year = dt.year();
    let month = dt.month();
    let start = Utc
        .with_ymd_and_hms(year, month, 1, 0, 0, 0)
        .single()
        .expect("valid month start");
    let next = if month == 12 {
        Utc.with_ymd_and_hms(year + 1, 1, 1, 0, 0, 0)
    } else {
        Utc.with_ymd_and_hms(year, month + 1, 1, 0, 0, 0)
    }
    .single()
    .expect("valid next-month start");
    (start.timestamp(), next.timestamp())
}

/// `[start, end)` of the UTC calendar year containing `ts`.
fn utc_year_bounds(ts: i64) -> (i64, i64) {
    let dt =
        DateTime::<Utc>::from_timestamp(ts, 0).unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap());
    let year = dt.year();
    let start = Utc
        .with_ymd_and_hms(year, 1, 1, 0, 0, 0)
        .single()
        .expect("valid year start");
    let next = Utc
        .with_ymd_and_hms(year + 1, 1, 1, 0, 0, 0)
        .single()
        .expect("valid next-year start");
    (start.timestamp(), next.timestamp())
}

/// Bundle every settled g1 daily chunk for the same UTC month into a g2.
///
/// Same crash-safety pattern as the daily rollup: write target first,
/// verify, then delete sources.
pub fn rollup_settled_months_in_dir(
    host_dir: &Path,
    now_secs: i64,
    settled_after_secs: i64,
) -> Result<RollupReport, HzcError> {
    rollup_span_in_dir(
        host_dir,
        now_secs,
        settled_after_secs,
        SourceFilter::Generation(1),
        2,
        ZSTD_LEVEL_G2,
        SpanKind::Month,
    )
}

/// Bundle every settled g2 monthly chunk that belongs to the same UTC year
/// into a single g3 yearly chunk.
pub fn rollup_settled_years_in_dir(
    host_dir: &Path,
    now_secs: i64,
    settled_after_secs: i64,
) -> Result<RollupReport, HzcError> {
    rollup_span_in_dir(
        host_dir,
        now_secs,
        settled_after_secs,
        SourceFilter::Generation(2),
        3,
        ZSTD_LEVEL_G3,
        SpanKind::Year,
    )
}

#[derive(Clone, Copy)]
enum SpanKind {
    Month,
    Year,
}

impl SpanKind {
    fn bounds(self, start_ts: i64) -> (i64, i64) {
        match self {
            Self::Month => utc_month_bounds(start_ts),
            Self::Year => utc_year_bounds(start_ts),
        }
    }

    fn label(self, span_start_ts: i64) -> String {
        let fmt = match self {
            Self::Month => "%Y-%m",
            Self::Year => "%Y",
        };
        DateTime::<Utc>::from_timestamp(span_start_ts, 0).map_or_else(
            || span_start_ts.to_string(),
            |dt| dt.format(fmt).to_string(),
        )
    }

    fn kind_name(self) -> &'static str {
        match self {
            Self::Month => "month",
            Self::Year => "year",
        }
    }
}

#[derive(Clone, Copy)]
enum SourceFilter {
    /// Only consider chunks of this generation as sources.
    Generation(u8),
}

impl SourceFilter {
    fn matches(self, c: &ChunkRef) -> bool {
        match self {
            Self::Generation(g) => c.generation == g,
        }
    }
}

/// Generic span-rollup engine used by `rollup_settled_months_in_dir` and
/// `rollup_settled_years_in_dir`.
fn rollup_span_in_dir(
    host_dir: &Path,
    now_secs: i64,
    settled_after_secs: i64,
    source_filter: SourceFilter,
    target_generation: u8,
    target_level: i32,
    span_kind: SpanKind,
) -> Result<RollupReport, HzcError> {
    let mut report = RollupReport::default();
    if !host_dir.join(CHUNKS_DIR).exists() {
        return Ok(report);
    }

    // Group ALL chunks (any generation) by (resolution_secs, span_start_ts).
    // We need higher-gen chunks to detect "already bundled" and source-gen
    // chunks as the bundling input. Sub-span chunks (those whose start_ts
    // falls in the span but whose end_ts may stretch slightly past, e.g. a
    // legacy chunk that crossed the month boundary) are grouped by their
    // start.
    let mut groups: BTreeMap<(u32, i64), Vec<ChunkRef>> = BTreeMap::new();
    for c in list_chunks(host_dir)? {
        let (span_start, _) = span_kind.bounds(c.start_ts);
        groups
            .entry((c.resolution_secs, span_start))
            .or_default()
            .push(c);
    }

    for ((res_secs, span_start_ts), chunks_in_span) in groups {
        let (span_start_ts, span_end_ts) = span_kind.bounds(span_start_ts);

        if span_end_ts + settled_after_secs > now_secs {
            report.skipped_unsettled += 1;
            continue;
        }

        let has_target_or_higher = chunks_in_span.iter().any(|c| {
            c.generation >= target_generation
                && c.start_ts <= span_start_ts
                && c.end_ts >= span_end_ts
        });
        if has_target_or_higher {
            // A target-gen (or higher) bundle already covers the span. If a
            // previous pass crashed mid-cleanup, sweep any lingering
            // source-gen chunks here.
            let mut swept = 0usize;
            for c in &chunks_in_span {
                if source_filter.matches(c) {
                    if let Err(e) = std::fs::remove_file(&c.path) {
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
                    span = %span_kind.label(span_start_ts),
                    kind = span_kind.kind_name(),
                    swept,
                    target_generation,
                    "hzc rollup swept stale source chunks after existing bundle"
                );
            }
            report.skipped_already_bundled += 1;
            continue;
        }

        // Sources are chunks of the configured generation that fall inside
        // the span. Any chunk at a lower generation than the configured
        // source means a prior rollup phase didn't finish - skip the span
        // for this pass and let the lower-phase rollup catch up next time.
        let srcs: Vec<&ChunkRef> = chunks_in_span
            .iter()
            .filter(|c| source_filter.matches(c))
            .collect();
        if srcs.len() <= 1 {
            report.skipped_singleton += 1;
            continue;
        }

        // Decode every source and merge.
        let mut samples: Vec<(i64, crate::slot::Slot)> = Vec::new();
        let mut bytes_before_group: u64 = 0;
        let mut decode_failed = false;
        for c in &srcs {
            let bytes = match std::fs::read(&c.path) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(
                        path = %c.path.display(),
                        error = ?e,
                        kind = span_kind.kind_name(),
                        "hzc rollup source read failed; skipping span"
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
                        kind = span_kind.kind_name(),
                        "hzc rollup source decode failed; skipping span"
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
            for c in &srcs {
                let _ = std::fs::remove_file(&c.path);
            }
            continue;
        }

        samples.sort_by_key(|(ts, _)| *ts);
        samples.dedup_by(|a, b| a.0 == b.0);

        let bundle_seq_val = bundle_seq(target_generation, res_secs, span_start_ts);

        if let Err(e) = seal_chunk_inline(
            host_dir,
            bundle_seq_val,
            span_start_ts,
            span_end_ts,
            res_secs,
            target_generation,
            target_level,
            &samples,
        ) {
            tracing::warn!(
                res_secs,
                span = %span_kind.label(span_start_ts),
                kind = span_kind.kind_name(),
                error = ?e,
                "hzc rollup seal failed; sources retained"
            );
            continue;
        }

        // Verify-before-delete: re-read the freshly-written bundle and
        // confirm the sample count round-trips.
        let bundle_filename = format!(
            "{bundle_seq_val:06}_r{res_secs}_g{target_generation}_{span_start_ts}_{span_end_ts}{CHUNK_EXTENSION}"
        );
        let bundle_path = host_dir.join(CHUNKS_DIR).join(&bundle_filename);
        let bytes_after_group = std::fs::metadata(&bundle_path).map_or(0, |m| m.len());
        let verify_ok = match std::fs::read(&bundle_path) {
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

        let src_count = srcs.len();
        for c in &srcs {
            let _ = std::fs::remove_file(&c.path);
        }

        let ratio = if bytes_after_group > 0 {
            bytes_before_group as f64 / bytes_after_group as f64
        } else {
            0.0
        };
        tracing::info!(
            res_secs,
            span = %span_kind.label(span_start_ts),
            kind = span_kind.kind_name(),
            sources = src_count,
            bytes_before = bytes_before_group,
            bytes_after = bytes_after_group,
            ratio = %format!("{ratio:.1}x"),
            samples = samples.len(),
            target_generation,
            "hzc rolled up span"
        );

        report.bundled_days += 1;
        report.source_chunks_consumed += src_count;
        report.bytes_before += bytes_before_group;
        report.bytes_after += bytes_after_group;
    }

    Ok(report)
}

/// Run the G2 monthly rollup for one host.
pub fn rollup_g2_host(
    data_dir: &Path,
    host_uuid: Uuid,
    now_secs: i64,
    settled_after_secs: i64,
) -> Result<RollupReport, HzcError> {
    let host_dir = host_directory(data_dir, host_uuid);
    if !host_dir.exists() {
        return Ok(RollupReport::default());
    }
    rollup_settled_months_in_dir(&host_dir, now_secs, settled_after_secs)
}

/// Run the G3 yearly rollup for one host.
pub fn rollup_g3_host(
    data_dir: &Path,
    host_uuid: Uuid,
    now_secs: i64,
    settled_after_secs: i64,
) -> Result<RollupReport, HzcError> {
    let host_dir = host_directory(data_dir, host_uuid);
    if !host_dir.exists() {
        return Ok(RollupReport::default());
    }
    rollup_settled_years_in_dir(&host_dir, now_secs, settled_after_secs)
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
    fn live_wal_survives_bundle_creation() {
        // Regression coverage for the original `max(seq)+1` seq scheme that
        // could collide with the live writer's WAL seq. Bundle seqs are now
        // deterministic (`bundle_seq`) with the top bit set, so a numeric
        // collision is impossible. We still verify the reader's "skip WAL
        // whose seq has a sealed chunk" rule doesn't accidentally drop the
        // live writer's open chunk when a bundle exists for an earlier day.
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        {
            let w = HostWriter::open(dir.path(), uuid, 60, 3_600).unwrap();
            for h in 0..24 {
                w.write_sample(h * 3_600, slot(20.0)).unwrap();
            }
            w.flush().unwrap();
        }
        // Day 1: open a new writer and write one sample without flushing.
        let w = HostWriter::open(dir.path(), uuid, 60, 3_600).unwrap();
        w.write_sample(86_400 + 60, slot(22.0)).unwrap();

        let host_dir = host_directory(dir.path(), uuid);
        assert_eq!(list_chunks(&host_dir).unwrap().len(), 24);
        assert!(host_dir.join("wal").join("25.wal").exists());

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
        // because a bundle exists. The writer should replay WAL 25 into a
        // fresh open chunk.
        let w2 = HostWriter::open(dir.path(), uuid, 60, 3_600).unwrap();
        assert!(host_dir.join("wal").join("25.wal").exists());
        w2.flush().unwrap();
        let after = list_chunks(&host_dir).unwrap();
        let has_g0_25 = after.iter().any(|c| c.seq == 25 && c.generation == 0);
        let has_g1_for_day0 = after.iter().any(|c| c.generation == 1 && c.start_ts == 0);
        assert!(
            has_g0_25,
            "writer's restart should have sealed WAL 25 into a g0 chunk"
        );
        assert!(
            has_g1_for_day0,
            "the day-0 bundle should still be present at g1"
        );
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

    /// Helper: walk a host directory and run G1 daily rollup for every day
    /// present, until no day is bundled. Used in the G2/G3 tests below.
    fn fully_rollup_g1(host_dir: &Path, now_secs: i64) {
        loop {
            let r = rollup_settled_days_in_dir(host_dir, now_secs, 1).unwrap();
            if !r.did_work() {
                break;
            }
        }
    }

    #[test]
    fn g2_rollup_bundles_a_settled_month() {
        // January 2023: 31 daily g1 bundles -> one g2 monthly bundle.
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        // Jan 1 2023 00:00:00 UTC.
        let jan_start: i64 = 1_672_531_200;
        let w = HostWriter::open(dir.path(), uuid, 60, 3600).unwrap();
        // 31 days × 24 hours = 744 hourly samples.
        for hour in 0..(31 * 24) {
            w.write_sample(jan_start + hour * 3600, slot(20.0 + (hour as f32) * 0.001))
                .unwrap();
        }
        w.flush().unwrap();
        drop(w);

        let host_dir = host_directory(dir.path(), uuid);

        // Step 1: settle all of January with the daily rollup.
        let after_jan = jan_start + 31 * 86_400 + 2 * 86_400; // Feb 2 2023
        fully_rollup_g1(&host_dir, after_jan);
        let after = list_chunks(&host_dir).unwrap();
        let g1_count = after.iter().filter(|c| c.generation == 1).count();
        assert_eq!(g1_count, 31, "expected 31 daily g1 bundles for January");

        // Step 2: G2 rolls up the month.
        let r = rollup_settled_months_in_dir(&host_dir, after_jan, 86_400).unwrap();
        assert_eq!(r.bundled_days, 1, "expected one month bundled");
        assert_eq!(r.source_chunks_consumed, 31);
        assert!(r.bytes_after < r.bytes_before);

        let after = list_chunks(&host_dir).unwrap();
        let g2 = after
            .iter()
            .find(|c| c.generation == 2)
            .expect("g2 bundle present");
        assert_eq!(g2.start_ts, jan_start);
        assert_eq!(g2.end_ts, jan_start + 31 * 86_400);
        assert!(
            after.iter().all(|c| c.generation != 1),
            "all g1 sources removed"
        );

        // Reading the bundle returns every original sample.
        let samples =
            crate::hzc::reader::read_range_in_dir(&host_dir, jan_start, g2.end_ts).unwrap();
        assert_eq!(samples.len(), 31 * 24);
    }

    #[test]
    fn g2_rollup_skips_unsettled_month() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let jan_start: i64 = 1_672_531_200;
        let w = HostWriter::open(dir.path(), uuid, 60, 3600).unwrap();
        // Only 5 days of data, all in January.
        for hour in 0..(5 * 24) {
            w.write_sample(jan_start + hour * 3600, slot(21.0)).unwrap();
        }
        w.flush().unwrap();
        drop(w);

        let host_dir = host_directory(dir.path(), uuid);
        // now = Jan 10 - the month is still in progress.
        let now = jan_start + 10 * 86_400;
        fully_rollup_g1(&host_dir, now);

        let r = rollup_settled_months_in_dir(&host_dir, now, 86_400).unwrap();
        assert_eq!(r.bundled_days, 0);
        assert!(r.skipped_unsettled >= 1);
    }

    #[test]
    fn g2_rollup_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let jan_start: i64 = 1_672_531_200;
        let w = HostWriter::open(dir.path(), uuid, 60, 3600).unwrap();
        for hour in 0..(31 * 24) {
            w.write_sample(jan_start + hour * 3600, slot(21.0)).unwrap();
        }
        w.flush().unwrap();
        drop(w);

        let host_dir = host_directory(dir.path(), uuid);
        let after_jan = jan_start + 31 * 86_400 + 2 * 86_400;
        fully_rollup_g1(&host_dir, after_jan);
        rollup_settled_months_in_dir(&host_dir, after_jan, 86_400).unwrap();
        let after_first = list_chunks(&host_dir).unwrap();

        // A second pass must not do anything.
        let r = rollup_settled_months_in_dir(&host_dir, after_jan, 86_400).unwrap();
        assert_eq!(r.bundled_days, 0);
        let after_second = list_chunks(&host_dir).unwrap();
        assert_eq!(
            after_first.iter().map(|c| &c.path).collect::<Vec<_>>(),
            after_second.iter().map(|c| &c.path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn g3_rollup_bundles_a_settled_year() {
        // Year 2023: 12 g2 monthly bundles -> one g3 yearly bundle.
        // Write 2 days per actual calendar month (using utc_month_bounds to
        // walk variable-length months precisely), bundle days into g1,
        // months into g2, and finally year into g3.
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let year_start: i64 = 1_672_531_200; // Jan 1 2023

        let w = HostWriter::open(dir.path(), uuid, 60, 3600).unwrap();
        // Walk the calendar by computing the start of each month from the
        // previous month's end, so leap years and 28/30/31-day months are
        // handled exactly.
        let mut m_start = year_start;
        let mut samples_written = 0;
        for _ in 0..12 {
            let (start, end) = utc_month_bounds(m_start);
            for day_offset in 0..2i64 {
                for hour in 0..24 {
                    let ts = start + day_offset * 86_400 + hour * 3600;
                    w.write_sample(ts, slot(20.0)).unwrap();
                    samples_written += 1;
                }
            }
            m_start = end;
        }
        w.flush().unwrap();
        drop(w);

        let host_dir = host_directory(dir.path(), uuid);
        let now = year_start + 365 * 86_400 + 3 * 86_400; // ~Jan 4 2024
        fully_rollup_g1(&host_dir, now);
        let r = rollup_settled_months_in_dir(&host_dir, now, 86_400).unwrap();
        assert_eq!(r.bundled_days, 12, "expected 12 months bundled into g2");

        // Now G3.
        let r3 = rollup_settled_years_in_dir(&host_dir, now, 2 * 86_400).unwrap();
        assert_eq!(r3.bundled_days, 1, "expected one year bundled into g3");
        assert_eq!(r3.source_chunks_consumed, 12);
        assert!(r3.bytes_after < r3.bytes_before);

        let after = list_chunks(&host_dir).unwrap();
        let g3 = after
            .iter()
            .find(|c| c.generation == 3)
            .expect("g3 bundle present");
        assert_eq!(g3.start_ts, year_start);
        let (_, year_end) = utc_year_bounds(year_start);
        assert_eq!(g3.end_ts, year_end);
        assert!(
            after.iter().all(|c| c.generation != 2),
            "all g2 sources removed"
        );

        let samples =
            crate::hzc::reader::read_range_in_dir(&host_dir, year_start, g3.end_ts).unwrap();
        assert_eq!(samples.len(), samples_written);
    }

    /// Edge case from the prod-data run: a sparse G2 monthly bundle.
    ///
    /// The bundle's nominal span (e.g. Mar 1 - Apr 1) is wider than its
    /// actual samples (e.g. Mar 13-19). At `now` = `source_now` + 5y +
    /// 30d, the chunk's nominal `end_ts` is JUST inside tier 4 (under 5y
    /// old) but every actual sample has age > 5y and must be dropped.
    /// The old logic that classified by `end_ts - 1` age would have kept
    /// the chunk downsampled-to-daily; the per-sample bucketing must
    /// delete it.
    #[test]
    fn split_downsample_drops_sparse_chunk_when_all_samples_past_horizon() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let host_dir = host_directory(dir.path(), uuid);
        std::fs::create_dir_all(host_dir.join(CHUNKS_DIR)).unwrap();
        std::fs::create_dir_all(host_dir.join("wal")).unwrap();
        // Bootstrap minimum meta.json so list_chunks etc work.
        let w = HostWriter::open(dir.path(), uuid, 60, 3_600).unwrap();
        drop(w);

        // Synthesise a g2-shaped chunk whose nominal span is one month
        // but whose samples are clustered in a 6-day window inside the
        // month.
        let month_start: i64 = 1_672_531_200; // Jan 1 2023
        let month_end: i64 = 1_675_209_600; // Feb 1 2023
        let samples_start = month_start + 12 * SECS_PER_DAY;
        let samples_count = 12 * 24; // 12 days × 24 hourly samples
        let samples: Vec<(i64, Slot)> = (0..samples_count)
            .map(|i| (samples_start + (i as i64) * 3_600, slot(21.0)))
            .collect();
        let seq = crate::hzc::format::bundle_seq(2, 0, month_start);
        seal_chunk_inline(
            &host_dir,
            seq,
            month_start,
            month_end,
            0,
            2, // g2
            ZSTD_LEVEL_G2,
            &samples,
        )
        .unwrap();
        assert_eq!(list_chunks(&host_dir).unwrap().len(), 1);

        // Define a 5y tier policy and pick `now` so the chunk's nominal
        // end_ts is JUST inside tier 4 (~ 1822 days old) but its actual
        // samples (Jan 13-25) are JUST past tier 4 (~ 1825+ days old).
        let tiers = vec![
            RetentionTier {
                max_age_secs: 7 * 86_400,
                resolution_secs: 0,
            },
            RetentionTier {
                max_age_secs: 30 * 86_400,
                resolution_secs: 300,
            },
            RetentionTier {
                max_age_secs: 180 * 86_400,
                resolution_secs: 1_800,
            },
            RetentionTier {
                max_age_secs: 365 * 86_400,
                resolution_secs: 7_200,
            },
            RetentionTier {
                max_age_secs: 5 * 365 * 86_400,
                resolution_secs: 86_400,
            },
        ];
        // last sample ts ≈ month_start + 24 days; pick now so that age >
        // 5y for every actual sample but age < 5y for chunk.end_ts.
        let now = month_end + 5 * 365 * 86_400 - 86_400; // 1d before tier 4 cliff
        // sanity:
        let oldest_age = now - samples[0].0;
        let chunk_end_age = now - month_end;
        assert!(oldest_age > 5 * 365 * 86_400);
        assert!(chunk_end_age < 5 * 365 * 86_400);

        let report = compact_in_dir(&host_dir, &tiers, now).unwrap();
        assert!(
            report.deleted_chunks + report.source_chunks_consumed > 0,
            "compactor must touch the sparse chunk"
        );

        let after = list_chunks(&host_dir).unwrap();
        // The chunk is sparse: all samples are past horizon, so it must
        // be deleted (or replaced with nothing).
        let samples =
            crate::hzc::reader::read_range_in_dir(&host_dir, month_start, month_end + 86_400)
                .unwrap();
        assert!(
            samples.is_empty(),
            "all samples were past 5y horizon but {} survived",
            samples.len()
        );
        // No outputs should have been written at the chunk's nominal span
        // because the per-sample bucketing found zero samples in any tier.
        assert!(
            after
                .iter()
                .all(|c| c.generation != 2 || c.start_ts != month_start),
            "stale g2 must be replaced or removed"
        );
    }

    /// Edge case: a sparse G2 month whose samples straddle a tier
    /// boundary. The per-sample bucketing must split the samples into two
    /// outputs (one per tier) using each sample's actual age, not the
    /// chunk's nominal `end_ts` age.
    #[test]
    fn split_downsample_buckets_sparse_chunk_per_sample() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let host_dir = host_directory(dir.path(), uuid);
        std::fs::create_dir_all(host_dir.join(CHUNKS_DIR)).unwrap();
        std::fs::create_dir_all(host_dir.join("wal")).unwrap();
        let w = HostWriter::open(dir.path(), uuid, 60, 3_600).unwrap();
        drop(w);

        let month_start: i64 = 1_672_531_200; // Jan 1 2023
        let month_end: i64 = 1_675_209_600; // Feb 1 2023
        // Samples cover Jan 10 - Jan 20 hourly.
        let samples_start = month_start + 9 * SECS_PER_DAY;
        let samples: Vec<(i64, Slot)> = (0..(10 * 24))
            .map(|i| (samples_start + (i as i64) * 3_600, slot(20.0)))
            .collect();
        let last_sample_ts = samples.last().unwrap().0;
        let seq = crate::hzc::format::bundle_seq(2, 0, month_start);
        seal_chunk_inline(
            &host_dir,
            seq,
            month_start,
            month_end,
            0,
            2,
            ZSTD_LEVEL_G2,
            &samples,
        )
        .unwrap();

        // Tier policy: tier 0 (raw) up to 5 days, tier 1 (300s) up to 30d,
        // tier 2 (1800s) up to 100d. Pick `now` so the boundary at
        // `now - 5 days` falls inside the sample range (e.g. Jan 15).
        let tiers = vec![
            RetentionTier {
                max_age_secs: 5 * 86_400,
                resolution_secs: 0,
            },
            RetentionTier {
                max_age_secs: 30 * 86_400,
                resolution_secs: 300,
            },
            RetentionTier {
                max_age_secs: 100 * 86_400,
                resolution_secs: 1_800,
            },
        ];
        // now picked so that Jan 15 = now - 5d, i.e. now = Jan 20.
        let now = month_start + 19 * SECS_PER_DAY;
        let tier_boundary = now - 5 * 86_400; // = Jan 15
        // Sanity: tier_boundary is between first and last sample.
        assert!(tier_boundary > samples[0].0);
        assert!(tier_boundary < last_sample_ts);

        compact_in_dir(&host_dir, &tiers, now).unwrap();

        let after = list_chunks(&host_dir).unwrap();
        // Expect exactly two outputs: one at raw resolution (tier 0,
        // young samples) and one at 300s (tier 1, older samples). Both
        // are G2 because downsampling preserves generation.
        let g2s: Vec<_> = after.iter().filter(|c| c.generation == 2).collect();
        assert_eq!(g2s.len(), 2, "expected two G2 segments after split");
        let raw_seg = g2s.iter().find(|c| c.resolution_secs == 0).unwrap();
        let downsampled = g2s.iter().find(|c| c.resolution_secs == 300).unwrap();
        // raw segment is the YOUNGER portion (samples newer than tier_boundary).
        assert!(raw_seg.start_ts >= tier_boundary || raw_seg.end_ts == month_end);
        assert!(raw_seg.end_ts > tier_boundary);
        // downsampled segment is the OLDER portion.
        assert!(downsampled.start_ts <= tier_boundary || downsampled.start_ts == month_start);
        assert!(downsampled.end_ts <= tier_boundary);

        // Total sample count across the two outputs:
        //   - raw portion: roughly 5 days × 24 hours = 120 samples
        //   - downsampled portion: ~5 days at 300s resolution
        //
        // We just want to confirm we didn't lose data: every sample maps
        // either to a raw passthrough or to a 300s bucket.
        let total_samples =
            crate::hzc::reader::read_range_in_dir(&host_dir, month_start, month_end).unwrap();
        // Samples must be monotonic with no exact-ts duplicates.
        for w in total_samples.windows(2) {
            assert!(
                w[0].timestamp_secs < w[1].timestamp_secs,
                "non-monotone or duplicate at {}",
                w[0].timestamp_secs
            );
        }
        // At least the young 5-day window's samples are present at raw
        // resolution.
        let young_count = total_samples
            .iter()
            .filter(|s| s.timestamp_secs >= tier_boundary)
            .count();
        assert!(
            (24 * 4..=24 * 6).contains(&young_count),
            "young raw portion expected ~120 samples, got {young_count}"
        );
    }

    /// Edge case: an empty chunk (zero samples) must be deleted without
    /// errors. Defensive coverage for crash-recovery scenarios where an
    /// in-progress flush left an empty bundle behind.
    #[test]
    fn compact_higher_gen_drops_empty_chunk() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let host_dir = host_directory(dir.path(), uuid);
        std::fs::create_dir_all(host_dir.join(CHUNKS_DIR)).unwrap();
        std::fs::create_dir_all(host_dir.join("wal")).unwrap();
        let w = HostWriter::open(dir.path(), uuid, 60, 3_600).unwrap();
        drop(w);

        let start: i64 = 1_672_531_200;
        let end: i64 = start + 86_400;
        let seq = crate::hzc::format::bundle_seq(2, 0, start);
        seal_chunk_inline(&host_dir, seq, start, end, 0, 2, ZSTD_LEVEL_G2, &[]).unwrap();
        assert_eq!(list_chunks(&host_dir).unwrap().len(), 1);

        let tiers = vec![RetentionTier {
            max_age_secs: 86_400,
            resolution_secs: 0,
        }];
        let now = end + 10;
        let r = compact_in_dir(&host_dir, &tiers, now).unwrap();
        assert!(r.deleted_chunks >= 1);
        assert!(list_chunks(&host_dir).unwrap().is_empty());
    }

    /// Edge case: a chunk whose samples are already at the target tier's
    /// resolution and entirely inside one tier must NOT be re-encoded.
    /// This guards against wasteful rewrites on every compaction pass.
    #[test]
    fn compact_higher_gen_skips_chunk_already_at_target_resolution() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let host_dir = host_directory(dir.path(), uuid);
        std::fs::create_dir_all(host_dir.join(CHUNKS_DIR)).unwrap();
        std::fs::create_dir_all(host_dir.join("wal")).unwrap();
        let w = HostWriter::open(dir.path(), uuid, 60, 3_600).unwrap();
        drop(w);

        // G2 bundle already at 300s resolution.
        let start: i64 = 1_672_531_200;
        let end: i64 = start + 86_400;
        let samples: Vec<(i64, Slot)> = (0..288).map(|i| (start + i * 300, slot(21.0))).collect();
        let seq = crate::hzc::format::bundle_seq(2, 300, start);
        seal_chunk_inline(&host_dir, seq, start, end, 300, 2, ZSTD_LEVEL_G2, &samples).unwrap();
        let before = std::fs::metadata(&list_chunks(&host_dir).unwrap()[0].path).unwrap();
        let before_mtime = before.modified().unwrap();

        // Tiers say: data 0..30d → 300s. The chunk is 0..1d old → tier
        // applies; target res 300 equals source res 300 → no work.
        let tiers = vec![RetentionTier {
            max_age_secs: 30 * 86_400,
            resolution_secs: 300,
        }];
        let now = end + 86_400;
        std::thread::sleep(std::time::Duration::from_millis(20)); // ensure mtime tick
        let r = compact_in_dir(&host_dir, &tiers, now).unwrap();
        assert_eq!(r.aggregated_chunks, 0);
        assert_eq!(r.deleted_chunks, 0);

        let chunks = list_chunks(&host_dir).unwrap();
        assert_eq!(chunks.len(), 1);
        let after = std::fs::metadata(&chunks[0].path).unwrap();
        assert_eq!(
            after.modified().unwrap(),
            before_mtime,
            "chunk was rewritten when it shouldn't have been"
        );
    }

    #[test]
    fn split_downsample_splits_g2_at_tier_boundary() {
        // Build a G2 monthly bundle, then run the compactor with a tier
        // policy whose boundary falls mid-month. Expect two G2 outputs at
        // different resolutions covering disjoint sub-spans of the month.
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let jan_start: i64 = 1_672_531_200; // Jan 1 2023
        // Write 31 days of hourly samples + run G1+G2 to produce one G2
        // monthly bundle covering January.
        let w = HostWriter::open(dir.path(), uuid, 60, 3600).unwrap();
        for hour in 0..(31 * 24) {
            w.write_sample(jan_start + hour * 3600, slot(21.0)).unwrap();
        }
        w.flush().unwrap();
        drop(w);
        let host_dir = host_directory(dir.path(), uuid);
        let after_jan = jan_start + 31 * 86_400 + 2 * 86_400;
        fully_rollup_g1(&host_dir, after_jan);
        rollup_settled_months_in_dir(&host_dir, after_jan, 86_400).unwrap();
        let before = list_chunks(&host_dir).unwrap();
        let g2_count_before = before.iter().filter(|c| c.generation == 2).count();
        assert_eq!(g2_count_before, 1, "starting state: one G2 month");

        // Tier policy: anything older than 5 days from `compaction_now`
        // gets aggregated to 7200s resolution. Set `compaction_now` so the
        // boundary lands ~15 days into January.
        let boundary_in_month = jan_start + 16 * 86_400; // Jan 17 2023
        // Tier max_age = 5 days, so now = boundary + 5 days.
        let compaction_now = boundary_in_month + 5 * 86_400;
        let tiers = vec![
            RetentionTier {
                max_age_secs: 5 * 86_400,
                resolution_secs: 0,
            },
            RetentionTier {
                max_age_secs: 365 * 86_400,
                resolution_secs: 7_200,
            },
        ];

        let report = compact_in_dir(&host_dir, &tiers, compaction_now).unwrap();
        assert!(
            report.aggregated_chunks >= 2,
            "expected at least 2 outputs from split, got {}",
            report.aggregated_chunks
        );
        assert_eq!(
            report.source_chunks_consumed, 1,
            "expected one G2 source consumed"
        );

        let after = list_chunks(&host_dir).unwrap();
        let g2s: Vec<_> = after.iter().filter(|c| c.generation == 2).collect();
        // Two G2 segments expected: one older (downsampled to 7200s), one
        // younger (still at raw resolution).
        assert_eq!(g2s.len(), 2, "expected two G2 segments after split");
        let older = g2s.iter().find(|c| c.start_ts == jan_start).unwrap();
        let younger = g2s
            .iter()
            .find(|c| c.end_ts > jan_start + 30 * 86_400)
            .unwrap();
        // The older segment was downsampled.
        assert_eq!(older.resolution_secs, 7_200, "older segment downsampled");
        // The younger segment stayed at the source's resolution (0 = raw).
        assert_eq!(younger.resolution_secs, 0, "younger segment stayed raw");
        // The split point matches the tier boundary.
        assert_eq!(older.end_ts, boundary_in_month);
        assert_eq!(younger.start_ts, boundary_in_month);

        // Full readback should still return all samples (downsampled for
        // the older portion).
        let samples =
            crate::hzc::reader::read_range_in_dir(&host_dir, jan_start, jan_start + 31 * 86_400)
                .unwrap();
        assert!(!samples.is_empty());
        let older_samples = samples
            .iter()
            .filter(|s| s.timestamp_secs < boundary_in_month)
            .count();
        let younger_samples = samples
            .iter()
            .filter(|s| s.timestamp_secs >= boundary_in_month)
            .count();
        // Older segment: ~16 days × 24 hours = 384 raw → binned into 7200s
        // (2h) buckets ≈ 192 samples.
        assert!(
            older_samples < 200 && older_samples > 150,
            "older expected ~192 samples after downsample, got {older_samples}"
        );
        // Younger segment: 31 - 16 = 15 days × 24 hours = 360 raw samples.
        assert_eq!(younger_samples, 15 * 24);
    }

    #[test]
    fn g2_rollup_cleans_up_stale_g1_after_existing_bundle() {
        // Simulate a crashed previous G2 pass: the g2 bundle was written and
        // renamed, but some g1 sources were not yet removed. The next pass
        // must detect "bundle already covers month" and finish cleanup.
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let jan_start: i64 = 1_672_531_200;
        let w = HostWriter::open(dir.path(), uuid, 60, 3600).unwrap();
        for hour in 0..(31 * 24) {
            w.write_sample(jan_start + hour * 3600, slot(21.0)).unwrap();
        }
        w.flush().unwrap();
        drop(w);

        let host_dir = host_directory(dir.path(), uuid);
        let after_jan = jan_start + 31 * 86_400 + 2 * 86_400;
        fully_rollup_g1(&host_dir, after_jan);
        rollup_settled_months_in_dir(&host_dir, after_jan, 86_400).unwrap();
        let chunks_dir = host_dir.join(CHUNKS_DIR);
        let g2_bundle = list_chunks(&host_dir)
            .unwrap()
            .into_iter()
            .find(|c| c.generation == 2)
            .expect("g2 present");

        // Re-create some legacy g1s for the month to simulate a partial
        // cleanup.
        for d in 0..3 {
            let day_start = jan_start + d * 86_400;
            let day_end = day_start + 86_400;
            let fake = chunks_dir.join(format!(
                "{:06}_r0_g1_{}_{}.hzc.zst",
                g2_bundle.seq + 100 + d as u64,
                day_start,
                day_end
            ));
            std::fs::write(&fake, std::fs::read(&g2_bundle.path).unwrap()).unwrap();
        }

        let r = rollup_settled_months_in_dir(&host_dir, after_jan, 86_400).unwrap();
        assert!(r.skipped_already_bundled >= 1);
        let remaining = list_chunks(&host_dir).unwrap();
        assert!(remaining.iter().filter(|c| c.generation == 1).count() == 0);
        assert_eq!(remaining.iter().filter(|c| c.generation == 2).count(), 1);
    }
}
