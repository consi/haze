//! Compactor - walks a host's chunks and applies its retention policy.
//!
//! For each chunk:
//! - compute `age = now - chunk.end_ts`,
//! - look up the policy's target resolution for that age,
//! - if the chunk's current resolution is already ≥ target, do nothing,
//! - if the policy says "delete" (age past the last tier), unlink the chunk,
//! - otherwise re-encode at the coarser resolution (grouping adjacent G0
//!   chunks per UTC day; rewriting G1+ chunks whole, in place of their span).
//!
//! A chunk is classified by the age of its **newest edge** (`end_ts`), so it
//! is only downsampled or deleted once every sample in it qualifies. Data
//! therefore lingers at the finer resolution up to one chunk-span past a
//! tier boundary (worst case: a yearly G3 bundle waits until its December
//! ages past the boundary). That lag is deliberate: splitting chunks at the
//! moving tier boundary produced a stream of tiny fragment chunks that the
//! rollup layer then had to re-consolidate, which is both churn and the
//! mechanism behind a past data-loss incident.
//!
//! Aggregation uses the same NaN-aware percentile-mean consolidation as the
//! legacy `.hzr` downsampler.
//!
//! Rollup (separate from the retention compactor above) bundles every chunk
//! of a settled UTC day / month / year into one zstd file per resolution,
//! eliminating per-file filesystem block overhead. All rollup phases use a
//! single merge-and-rebundle primitive that decodes every group member
//! (including a pre-existing bundle), merges, rewrites, verifies, and only
//! then deletes the sources - a chunk is never deleted on the assumption
//! that a covering bundle already contains its samples. Monthly/yearly
//! groups are additionally gated on tier finality: a `(resolution, span)`
//! group is only bundled once no finer-tier data can still be downsampled
//! into that resolution for that span (otherwise the bundle would miss the
//! late arrivals). It runs single-threaded so it never piles I/O on the
//! live writer; see [`rollup_settled_days_in_dir`].

use std::{collections::BTreeMap, path::Path};

use chrono::{DateTime, Datelike, TimeZone, Utc};

use crate::hzc::{
    chunk::{
        ZSTD_LEVEL_G0, ZSTD_LEVEL_G1, ZSTD_LEVEL_G2, ZSTD_LEVEL_G3, decode_chunk, read_header,
        zstd_level_for_generation,
    },
    format::{
        CHUNK_EXTENSION, ChunkRef, bundle_seq, chunk_filename, is_legacy_chunk_name,
        parse_chunk_filename,
    },
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
    // many; the grouping pass consolidates one UTC day's worth of adjacent
    // G0s into one G0-at-coarser-resolution output, which keeps
    // intermediate file count down before the daily rollup picks them up.
    //
    // G1+ chunks already span at least a day, so each is rewritten whole
    // (same span, same generation) once its entire span has aged into a
    // coarser tier: a G3 yearly bundle that ages into the next tier
    // becomes a new G3 yearly bundle at the coarser resolution.
    let g0_chunks: Vec<&ChunkRef> = chunks.iter().filter(|c| c.generation == 0).collect();
    let higher_chunks: Vec<&ChunkRef> = chunks.iter().filter(|c| c.generation >= 1).collect();

    compact_g0_grouped(host_dir, &g0_chunks, tiers, now_secs, &mut report)?;
    for c in higher_chunks {
        compact_higher_gen_chunk(host_dir, c, tiers, now_secs, &mut report)?;
    }

    Ok(report)
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

    // Group by (target_res, UTC day) for batched encode. Day alignment
    // keeps the outputs from straddling midnight, so the daily rollup's
    // per-day grouping consumes them cleanly.
    let mut groups: BTreeMap<(u32, i64), Vec<&ChunkRef>> = BTreeMap::new();
    for (target_res, src) in to_aggregate {
        let utc_day = src.start_ts.div_euclid(SECS_PER_DAY);
        groups.entry((target_res, utc_day)).or_default().push(src);
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

/// Whole-chunk downsample for a G1+ source.
///
/// The chunk is classified by the age of its newest edge (`end_ts`), the
/// same rule `compact_g0_grouped` applies. It is therefore only rewritten
/// (or deleted) once **every** sample in it qualifies for the coarser tier
/// (or the horizon), and the rewrite preserves the chunk's span and
/// generation: one input chunk, at most one output chunk. Data lingers at
/// the finer resolution up to one chunk-span past the tier boundary; in
/// exchange there is no per-pass fragment churn at the moving boundary,
/// which the rollup layer would otherwise have to re-consolidate.
fn compact_higher_gen_chunk(
    host_dir: &Path,
    c: &ChunkRef,
    tiers: &[RetentionTier],
    now_secs: i64,
    report: &mut CompactReport,
) -> Result<(), HzcError> {
    let age = now_secs - c.end_ts;
    let Some(target_res) = target_resolution(age, tiers) else {
        // Even the chunk's newest possible sample is past the final
        // tier's horizon - delete.
        let _ = fs::remove_file(&c.path);
        report.deleted_chunks += 1;
        return Ok(());
    };

    // Already raw-tier or already at/below the target resolution: nothing
    // to do, and crucially nothing to decode - this is the steady-state
    // path for every settled bundle on every compactor pass.
    if target_res == 0 || c.resolution_secs >= target_res {
        return Ok(());
    }

    let bytes = fs::read(&c.path)?;
    let all_samples = decode_chunk(&bytes)?;

    if all_samples.is_empty() {
        // Empty chunk (crash artifact) - safe to delete.
        let _ = fs::remove_file(&c.path);
        report.deleted_chunks += 1;
        return Ok(());
    }

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

    // Same span, same generation, coarser resolution. The deterministic
    // seq makes a crash+retry overwrite the same filename.
    let seq = bundle_seq(c.generation, target_res, c.start_ts);
    let level = zstd_level_for_generation(c.generation);
    if let Err(e) = seal_chunk_inline(
        host_dir,
        seq,
        c.start_ts,
        c.end_ts,
        target_res,
        c.generation,
        level,
        &aggregated,
    ) {
        tracing::warn!(
            source = %c.path.display(),
            target_res,
            error = ?e,
            "hzc downsample seal failed; source retained"
        );
        return Ok(());
    }

    // Verify-before-delete: decode the output and confirm the sample count.
    let out_path = host_dir.join(CHUNKS_DIR).join(chunk_filename(
        seq,
        target_res,
        c.generation,
        c.start_ts,
        c.end_ts,
    ));
    let verified = fs::read(&out_path)
        .ok()
        .and_then(|b| decode_chunk(&b).ok())
        .is_some_and(|d| d.len() == aggregated.len());
    if !verified {
        tracing::warn!(
            path = %out_path.display(),
            expected_samples = aggregated.len(),
            "hzc downsample verify-before-delete mismatch; source and output retained"
        );
        return Ok(());
    }

    let _ = fs::remove_file(&c.path);
    report.aggregated_chunks += 1;
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
    /// `(resolution_secs, span_start_ts)` groups that were merged into a
    /// bundle (for the daily rollup the span is a UTC day).
    pub bundled_days: usize,
    /// Source chunks consumed (deleted after verification) by all bundles
    /// in this pass.
    pub source_chunks_consumed: usize,
    /// Groups skipped because the span hasn't settled yet (or, for
    /// monthly/yearly groups, finer-tier data can still arrive at this
    /// resolution).
    pub skipped_unsettled: usize,
    /// Single-member groups whose member is already a bundle.
    pub skipped_already_bundled: usize,
    /// Groups skipped because bundling wouldn't shrink the file count.
    pub skipped_singleton: usize,
    /// Groups skipped because the verify-before-delete check failed.
    pub verify_failed: usize,
    /// Groups where an existing bundle already contained every merged
    /// sample, so the redundant members were removed without a rewrite.
    pub contained_cleanups: usize,
    /// Monthly/yearly groups skipped because their resolution matches no
    /// configured retention tier (left for the compactor to re-resolve).
    pub skipped_unmatched_res: usize,
    /// Monthly/yearly groups skipped because their resolution is the raw
    /// tier of a multi-tier policy: that data will be downsampled into a
    /// coarser tier soon, so bundling it would be churn.
    pub skipped_transitional_res: usize,
    /// Total bytes occupied by source files before bundling. Useful for
    /// computing the compression ratio in the per-host log line.
    pub bytes_before: u64,
    /// Total bytes occupied by bundle files after writing.
    pub bytes_after: u64,
}

impl RollupReport {
    pub fn did_work(&self) -> bool {
        self.bundled_days > 0 || self.contained_cleanups > 0
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

/// Merge every member of a rollup group into one target-generation bundle.
///
/// This is the single primitive behind the daily, monthly, and yearly
/// rollups. It decodes **every** member - including any pre-existing bundle
/// for the span - merges, writes one bundle covering the members' actual
/// data range, verifies it round-trips, and only then deletes the redundant
/// members. A member is therefore never deleted on the assumption that an
/// existing bundle already contains its samples; samples that arrived after
/// a bundle was sealed (late downsampling at a tier boundary, replication
/// backfill) are folded in instead of being swept away.
///
/// The bundle seq is deterministic in `(generation, resolution, start)`, so
/// a crash + retry overwrites the same filename via tmp+fsync+rename, and
/// re-merging an existing bundle with new members rewrites it in place.
#[allow(clippy::too_many_arguments)]
fn merge_group_into_bundle(
    host_dir: &Path,
    members: &[ChunkRef],
    res_secs: u32,
    target_generation: u8,
    target_level: i32,
    span_label: &str,
    span_kind_name: &str,
    report: &mut RollupReport,
) -> Result<(), HzcError> {
    // Decode every member. Any read/decode failure leaves the whole group
    // untouched for this pass.
    let mut decoded: Vec<Vec<(i64, Slot)>> = Vec::with_capacity(members.len());
    let mut bytes_before_group: u64 = 0;
    for c in members {
        let bytes = match fs::read(&c.path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    path = %c.path.display(),
                    error = ?e,
                    kind = span_kind_name,
                    "hzc rollup source read failed; skipping group"
                );
                return Ok(());
            }
        };
        bytes_before_group += bytes.len() as u64;
        match decode_chunk(&bytes) {
            Ok(d) => decoded.push(d),
            Err(e) => {
                tracing::warn!(
                    path = %c.path.display(),
                    error = ?e,
                    kind = span_kind_name,
                    "hzc rollup source decode failed; skipping group"
                );
                return Ok(());
            }
        }
    }

    // Merge. Stable sort + first-wins dedup; at an exact ts collision the
    // values are near-identical (same probe consolidated twice), so the
    // pick doesn't matter beyond determinism.
    let mut merged: Vec<(i64, Slot)> = decoded.iter().flatten().copied().collect();
    merged.sort_by_key(|(ts, _)| *ts);
    merged.dedup_by(|a, b| a.0 == b.0);

    if merged.is_empty() {
        // All-empty members (crash artifacts) - delete them to free blocks.
        for c in members {
            let _ = fs::remove_file(&c.path);
        }
        return Ok(());
    }

    // Containment fast path: if one member is already a bundle whose sample
    // set equals the merged set (every member's samples are a subset of the
    // union, so equal cardinality means equal sets), the other members are
    // verifiably redundant - delete them without rewriting the bundle.
    let container = members
        .iter()
        .zip(&decoded)
        .filter(|(c, d)| c.generation >= target_generation && d.len() == merged.len())
        .max_by_key(|(c, _)| c.generation);
    if let Some((keeper, _)) = container {
        let mut removed = 0usize;
        for c in members {
            if c.path != keeper.path && fs::remove_file(&c.path).is_ok() {
                removed += 1;
            }
        }
        tracing::info!(
            res_secs,
            span = span_label,
            kind = span_kind_name,
            removed,
            "hzc rollup sources contained in existing bundle; removed after decode-verify"
        );
        report.contained_cleanups += 1;
        report.source_chunks_consumed += removed;
        return Ok(());
    }

    // The bundle covers the members' actual data range, never a wider
    // calendar span: a bundle must not claim time it has no data for,
    // because the reader's coverage preferences trust the claimed span.
    let out_start = members.iter().map(|c| c.start_ts).min().unwrap_or(0);
    let out_end = members.iter().map(|c| c.end_ts).max().unwrap_or(0);
    let bundle_seq_val = bundle_seq(target_generation, res_secs, out_start);

    // seal_chunk_inline does encode → tmp → fsync → atomic rename.
    if let Err(e) = seal_chunk_inline(
        host_dir,
        bundle_seq_val,
        out_start,
        out_end,
        res_secs,
        target_generation,
        target_level,
        &merged,
    ) {
        tracing::warn!(
            res_secs,
            span = span_label,
            kind = span_kind_name,
            error = ?e,
            "hzc rollup seal failed; sources retained"
        );
        return Ok(());
    }

    // Verify-before-delete: re-read the freshly-written bundle and confirm
    // the sample count round-trips. This is the last line of defence
    // against an encoder regression silently destroying data; if the count
    // differs, leave bundle and sources in place for an operator to triage.
    let bundle_path = host_dir.join(CHUNKS_DIR).join(chunk_filename(
        bundle_seq_val,
        res_secs,
        target_generation,
        out_start,
        out_end,
    ));
    let bytes_after_group = fs::metadata(&bundle_path).map_or(0, |m| m.len());
    let verify_ok = fs::read(&bundle_path)
        .ok()
        .and_then(|b| decode_chunk(&b).ok())
        .is_some_and(|d| d.len() == merged.len());
    if !verify_ok {
        tracing::warn!(
            path = %bundle_path.display(),
            expected_samples = merged.len(),
            "hzc rollup verify-before-delete mismatch; bundle and sources retained for triage"
        );
        report.verify_failed += 1;
        return Ok(());
    }

    // Delete the now-redundant members. When the merge rewrote an existing
    // bundle in place, that member's path IS the bundle path - skip it.
    let mut consumed = 0usize;
    for c in members {
        if c.path != bundle_path {
            let _ = fs::remove_file(&c.path);
            consumed += 1;
        }
    }

    let ratio = if bytes_after_group > 0 {
        bytes_before_group as f64 / bytes_after_group as f64
    } else {
        0.0
    };
    tracing::info!(
        res_secs,
        span = span_label,
        kind = span_kind_name,
        sources = consumed,
        bytes_before = bytes_before_group,
        bytes_after = bytes_after_group,
        ratio = %format!("{ratio:.1}x"),
        samples = merged.len(),
        target_generation,
        "hzc rolled up group"
    );

    report.bundled_days += 1;
    report.source_chunks_consumed += consumed;
    report.bytes_before += bytes_before_group;
    report.bytes_after += bytes_after_group;
    Ok(())
}

/// Bundle every chunk that belongs to a fully-settled UTC day into a single
/// `g1` chunk, then delete the sources. See the module docstring for
/// crash-safety properties.
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
    // "day" is the day its first sample falls in. Chunks spanning more
    // than a day (monthly/yearly bundles, legacy long-window aggregates)
    // are not day-group members - a wider bundle must never be folded
    // into (or deleted in favour of) a day bundle.
    let mut groups: BTreeMap<(u32, i64), Vec<ChunkRef>> = BTreeMap::new();
    for c in list_chunks(host_dir)? {
        if c.end_ts - c.start_ts > SECS_PER_DAY {
            continue;
        }
        let utc_day = c.start_ts.div_euclid(SECS_PER_DAY);
        groups
            .entry((c.resolution_secs, utc_day))
            .or_default()
            .push(c);
    }

    for ((res_secs, utc_day), members) in groups {
        let day_start_ts = utc_day * SECS_PER_DAY;
        let day_end_ts = day_start_ts + SECS_PER_DAY;

        if day_end_ts + settled_after_secs > now_secs {
            report.skipped_unsettled += 1;
            continue;
        }

        if members.len() <= 1 {
            // Nothing to gain - either zero or one chunk; bundling produces
            // the same file count.
            if members.first().is_some_and(|c| c.generation >= 1) {
                report.skipped_already_bundled += 1;
            } else {
                report.skipped_singleton += 1;
            }
            continue;
        }

        merge_group_into_bundle(
            host_dir,
            &members,
            res_secs,
            1, // generation 1 - daily bundle
            ZSTD_LEVEL_G1,
            &format_utc_day(day_start_ts),
            "day",
            &mut report,
        )?;
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

/// Bundle every settled chunk for the same UTC month into a g2.
///
/// Same crash-safety pattern as the daily rollup: write target first,
/// verify, then delete sources. `tiers` drives the tier-finality gate -
/// see [`rollup_span_in_dir`].
pub fn rollup_settled_months_in_dir(
    host_dir: &Path,
    now_secs: i64,
    settled_after_secs: i64,
    tiers: &[RetentionTier],
) -> Result<RollupReport, HzcError> {
    rollup_span_in_dir(
        host_dir,
        now_secs,
        settled_after_secs,
        tiers,
        2,
        ZSTD_LEVEL_G2,
        SpanKind::Month,
    )
}

/// Bundle every settled chunk that belongs to the same UTC year into a
/// single g3 yearly chunk.
pub fn rollup_settled_years_in_dir(
    host_dir: &Path,
    now_secs: i64,
    settled_after_secs: i64,
    tiers: &[RetentionTier],
) -> Result<RollupReport, HzcError> {
    rollup_span_in_dir(
        host_dir,
        now_secs,
        settled_after_secs,
        tiers,
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

/// Index of the first tier whose resolution matches `res` exactly.
fn tier_index_for_resolution(tiers: &[RetentionTier], res: u32) -> Option<usize> {
    tiers.iter().position(|t| t.resolution_secs == res)
}

/// Generic span-rollup engine used by `rollup_settled_months_in_dir` and
/// `rollup_settled_years_in_dir`.
///
/// A `(resolution, span)` group is only bundled once the span's data is
/// **final** at that resolution - i.e. once every sample of the span must
/// already have been downsampled into the tier whose resolution matches the
/// group's. Data enters tier `i`'s resolution when it ages past
/// `tiers[i-1].max_age_secs`, so the gate is
/// `now >= span_end + tiers[i-1].max_age_secs + settle`. Without this gate
/// a month bundle would be sealed while the month's youngest days are still
/// at a finer resolution, and those days' chunks would only appear in the
/// group later - which is exactly the partial-bundle state that caused a
/// data-loss incident (the late chunks were swept as "stale leftovers").
/// The merge primitive makes late arrivals safe regardless; the gate keeps
/// them from being the common case.
fn rollup_span_in_dir(
    host_dir: &Path,
    now_secs: i64,
    settled_after_secs: i64,
    tiers: &[RetentionTier],
    target_generation: u8,
    target_level: i32,
    span_kind: SpanKind,
) -> Result<RollupReport, HzcError> {
    let mut report = RollupReport::default();
    if !host_dir.join(CHUNKS_DIR).exists() {
        return Ok(report);
    }

    // Group chunks of ANY generation by (resolution_secs, span_start_ts) of
    // their start - an existing bundle for the span is just another merge
    // input, and stray finer-generation chunks (a day that never had
    // siblings to bundle with, late replication backfill) get absorbed
    // instead of stranded. Chunks wider than the span itself (e.g. a yearly
    // bundle whose start falls in this month) are not members.
    let mut groups: BTreeMap<(u32, i64), Vec<ChunkRef>> = BTreeMap::new();
    for c in list_chunks(host_dir)? {
        let (span_start, span_end) = span_kind.bounds(c.start_ts);
        if c.end_ts - c.start_ts > span_end - span_start {
            continue;
        }
        groups
            .entry((c.resolution_secs, span_start))
            .or_default()
            .push(c);
    }

    for ((res_secs, span_start_ts), members) in groups {
        let (span_start_ts, span_end_ts) = span_kind.bounds(span_start_ts);

        if span_end_ts + settled_after_secs > now_secs {
            report.skipped_unsettled += 1;
            continue;
        }

        // Tier-finality gate (see the function docstring).
        match tier_index_for_resolution(tiers, res_secs) {
            None => {
                // No tier produces this resolution (the policy changed).
                // Leave the chunks alone; the compactor will re-resolve
                // them onto a configured tier by age eventually.
                report.skipped_unmatched_res += 1;
                continue;
            }
            Some(0) if tiers.len() > 1 => {
                // Raw tier of a multi-tier policy: this data is guaranteed
                // to be rewritten at the next tier boundary, so a bundle of
                // it would claim a span whose data is about to change.
                report.skipped_transitional_res += 1;
                continue;
            }
            Some(i) => {
                let final_after = if i == 0 { 0 } else { tiers[i - 1].max_age_secs };
                if span_end_ts + final_after + settled_after_secs > now_secs {
                    report.skipped_unsettled += 1;
                    continue;
                }
            }
        }

        if members.len() <= 1 {
            if members
                .first()
                .is_some_and(|c| c.generation >= target_generation)
            {
                report.skipped_already_bundled += 1;
            } else {
                report.skipped_singleton += 1;
            }
            continue;
        }

        merge_group_into_bundle(
            host_dir,
            &members,
            res_secs,
            target_generation,
            target_level,
            &span_kind.label(span_start_ts),
            span_kind.kind_name(),
            &mut report,
        )?;
    }

    Ok(report)
}

/// Run the G2 monthly rollup for one host.
pub fn rollup_g2_host(
    data_dir: &Path,
    host_uuid: Uuid,
    now_secs: i64,
    settled_after_secs: i64,
    tiers: &[RetentionTier],
) -> Result<RollupReport, HzcError> {
    let host_dir = host_directory(data_dir, host_uuid);
    if !host_dir.exists() {
        return Ok(RollupReport::default());
    }
    rollup_settled_months_in_dir(&host_dir, now_secs, settled_after_secs, tiers)
}

/// Run the G3 yearly rollup for one host.
pub fn rollup_g3_host(
    data_dir: &Path,
    host_uuid: Uuid,
    now_secs: i64,
    settled_after_secs: i64,
    tiers: &[RetentionTier],
) -> Result<RollupReport, HzcError> {
    let host_dir = host_directory(data_dir, host_uuid);
    if !host_dir.exists() {
        return Ok(RollupReport::default());
    }
    rollup_settled_years_in_dir(&host_dir, now_secs, settled_after_secs, tiers)
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
        // decode-verify that the bundle contains the leftovers' samples and
        // only then finish the cleanup (containment fast path).
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
        assert_eq!(report.contained_cleanups, 1);
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

    /// Single-tier raw-only policy: raw data is final, so monthly/yearly
    /// rollups of `r0` groups are allowed with no tier-finality wait.
    fn raw_only_tiers() -> Vec<RetentionTier> {
        vec![RetentionTier {
            max_age_secs: 100 * 365 * 86_400,
            resolution_secs: 0,
        }]
    }

    /// The production default 5-tier policy.
    fn default_tiers() -> Vec<RetentionTier> {
        vec![
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
        ]
    }

    /// Synthesize a sealed chunk directly in the host dir.
    #[allow(clippy::too_many_arguments)]
    fn plant_chunk(
        host_dir: &Path,
        generation: u8,
        res: u32,
        start: i64,
        end: i64,
        samples: &[(i64, Slot)],
    ) {
        let seq = bundle_seq(generation, res, start);
        seal_chunk_inline(
            host_dir,
            seq,
            start,
            end,
            res,
            generation,
            ZSTD_LEVEL_G0,
            samples,
        )
        .unwrap();
    }

    /// Create an empty host directory (meta.json etc.) and return it.
    fn empty_host_dir(data_dir: &Path, uuid: Uuid) -> std::path::PathBuf {
        let w = HostWriter::open(data_dir, uuid, 60, 3_600).unwrap();
        drop(w);
        host_directory(data_dir, uuid)
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

        // Step 2: G2 rolls up the month. Raw-only tiers: r0 is final, so
        // the tier-finality gate admits the group immediately.
        let r =
            rollup_settled_months_in_dir(&host_dir, after_jan, 86_400, &raw_only_tiers()).unwrap();
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

        let r = rollup_settled_months_in_dir(&host_dir, now, 86_400, &raw_only_tiers()).unwrap();
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
        rollup_settled_months_in_dir(&host_dir, after_jan, 86_400, &raw_only_tiers()).unwrap();
        let after_first = list_chunks(&host_dir).unwrap();

        // A second pass must not do anything.
        let r =
            rollup_settled_months_in_dir(&host_dir, after_jan, 86_400, &raw_only_tiers()).unwrap();
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
        let r = rollup_settled_months_in_dir(&host_dir, now, 86_400, &raw_only_tiers()).unwrap();
        assert_eq!(r.bundled_days, 12, "expected 12 months bundled into g2");

        // Now G3.
        let r3 =
            rollup_settled_years_in_dir(&host_dir, now, 2 * 86_400, &raw_only_tiers()).unwrap();
        assert_eq!(r3.bundled_days, 1, "expected one year bundled into g3");
        assert_eq!(r3.source_chunks_consumed, 12);
        assert!(r3.bytes_after < r3.bytes_before);

        let after = list_chunks(&host_dir).unwrap();
        let g3 = after
            .iter()
            .find(|c| c.generation == 3)
            .expect("g3 bundle present");
        assert_eq!(g3.start_ts, year_start);
        // Truthful span: the bundle ends where its data ends (each month
        // contributed 2 days, so December's daily bundle ends Dec 3), not
        // at the calendar year end.
        let (_, year_end) = utc_year_bounds(year_start);
        let (dec_start, _) = utc_month_bounds(year_end - 1);
        assert_eq!(g3.end_ts, dec_start + 2 * 86_400);
        assert!(
            after.iter().all(|c| c.generation != 2),
            "all g2 sources removed"
        );

        let samples =
            crate::hzc::reader::read_range_in_dir(&host_dir, year_start, year_end).unwrap();
        assert_eq!(samples.len(), samples_written);
    }

    /// Whole-chunk deletion contract: a sparse G2 monthly bundle whose
    /// samples are all past the retention horizon is kept until its
    /// nominal `end_ts` crosses the horizon too (up to one chunk-span
    /// late, never early), and is then deleted in one pass.
    #[test]
    fn whole_chunk_delete_waits_for_end_ts_past_horizon() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let host_dir = empty_host_dir(dir.path(), uuid);

        // G2-shaped chunk: nominal span one month, samples clustered in a
        // 12-day window inside the month.
        let month_start: i64 = 1_672_531_200; // Jan 1 2023
        let month_end: i64 = 1_675_209_600; // Feb 1 2023
        let samples_start = month_start + 12 * SECS_PER_DAY;
        let samples: Vec<(i64, Slot)> = (0..(12 * 24))
            .map(|i| (samples_start + (i as i64) * 3_600, slot(21.0)))
            .collect();
        plant_chunk(&host_dir, 2, 0, month_start, month_end, &samples);

        let tiers = default_tiers();

        // Every actual sample is past the 5y horizon, but the chunk's
        // newest edge isn't yet: the chunk survives (downsampled to the
        // final tier's resolution at most).
        let now = month_end + 5 * 365 * 86_400 - 86_400; // 1d before the cliff
        assert!(now - samples[0].0 > 5 * 365 * 86_400);
        compact_in_dir(&host_dir, &tiers, now).unwrap();
        let surviving =
            crate::hzc::reader::read_range_in_dir(&host_dir, month_start, month_end).unwrap();
        assert!(
            !surviving.is_empty(),
            "chunk must not be deleted before its end_ts crosses the horizon"
        );

        // Once end_ts crosses, the whole chunk goes.
        let now = month_end + 5 * 365 * 86_400 + 1;
        let report = compact_in_dir(&host_dir, &tiers, now).unwrap();
        assert!(report.deleted_chunks >= 1);
        let after =
            crate::hzc::reader::read_range_in_dir(&host_dir, month_start, month_end).unwrap();
        assert!(after.is_empty());
        assert!(list_chunks(&host_dir).unwrap().is_empty());
    }

    /// Whole-chunk downsampling contract: a G1+ chunk straddling a tier
    /// boundary is left untouched until its entire span has crossed, then
    /// rewritten whole (same span, same generation, coarser resolution) in
    /// a single pass - no per-boundary fragment outputs.
    #[test]
    fn whole_chunk_downsample_waits_for_full_crossing() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let host_dir = empty_host_dir(dir.path(), uuid);

        let day_start: i64 = 1_672_531_200; // Jan 1 2023
        let day_end = day_start + SECS_PER_DAY;
        let samples: Vec<(i64, Slot)> = (0..24)
            .map(|h| (day_start + h * 3_600, slot(20.0)))
            .collect();
        plant_chunk(&host_dir, 1, 0, day_start, day_end, &samples);

        let tiers = default_tiers();
        let before_mtime = std::fs::metadata(&list_chunks(&host_dir).unwrap()[0].path)
            .unwrap()
            .modified()
            .unwrap();

        // Mid-crossing: most of the day is past the 7d raw boundary but
        // the newest hour isn't. The chunk must be untouched.
        let now = day_end + 7 * 86_400 - 100;
        let r = compact_in_dir(&host_dir, &tiers, now).unwrap();
        assert_eq!(r.aggregated_chunks, 0);
        assert_eq!(r.deleted_chunks, 0);
        let chunks = list_chunks(&host_dir).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].resolution_secs, 0);
        assert_eq!(
            std::fs::metadata(&chunks[0].path)
                .unwrap()
                .modified()
                .unwrap(),
            before_mtime,
            "straddling chunk must not be rewritten"
        );

        // Fully crossed: one output, same span, same generation, at the
        // tier's resolution.
        let now = day_end + 7 * 86_400 + 100;
        let r = compact_in_dir(&host_dir, &tiers, now).unwrap();
        assert_eq!(r.aggregated_chunks, 1);
        assert_eq!(r.source_chunks_consumed, 1);
        let chunks = list_chunks(&host_dir).unwrap();
        assert_eq!(chunks.len(), 1, "exactly one output, no fragments");
        assert_eq!(chunks[0].generation, 1);
        assert_eq!(chunks[0].resolution_secs, 300);
        assert_eq!(chunks[0].start_ts, day_start);
        assert_eq!(chunks[0].end_ts, day_end);
        let samples = crate::hzc::reader::read_range_in_dir(&host_dir, day_start, day_end).unwrap();
        assert_eq!(samples.len(), 24, "hourly samples bin 1:1 into 300s");
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

        // A tier policy that wants the chunk downsampled, so the compactor
        // decodes it and discovers it's empty.
        let tiers = vec![
            RetentionTier {
                max_age_secs: 1,
                resolution_secs: 0,
            },
            RetentionTier {
                max_age_secs: 365 * 86_400,
                resolution_secs: 300,
            },
        ];
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

    /// THE regression test for the production data-loss incident.
    ///
    /// Sequence reproduced: a month of hourly data; the first monthly
    /// rollup opportunity arrives one day after month end, while the last
    /// ~6 days of the month are still in the raw tier. The old code sealed
    /// a full-month-span G2 from the already-downsampled days and then
    /// deleted each late day's chunks as "stale leftovers" once the
    /// compactor converted them - destroying May 26 - Jun 1 on every host.
    ///
    /// The fixed pipeline must (a) not create a G2 for the month until no
    /// finer-tier data can still arrive at that resolution, (b) never lose
    /// a sample at any step, and (c) converge to a single G2 bundle
    /// containing the entire month.
    #[test]
    fn month_boundary_partial_month_reproduces_production_sequence() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let jan_start: i64 = 1_672_531_200; // Jan 1 2023
        let feb_start: i64 = 1_675_209_600; // Feb 1 2023
        let w = HostWriter::open(dir.path(), uuid, 60, 3600).unwrap();
        for hour in 0..(31 * 24) {
            w.write_sample(jan_start + hour * 3600, slot(21.0)).unwrap();
        }
        w.flush().unwrap();
        drop(w);

        let host_dir = host_directory(dir.path(), uuid);
        // The production tier shape at the incident's boundary (7d raw,
        // then 5-minute), with the next boundary pushed out so this test's
        // window stays inside the r300 tier and the convergence assertion
        // below can expect a single bundle.
        let tiers = vec![
            RetentionTier {
                max_age_secs: 7 * 86_400,
                resolution_secs: 0,
            },
            RetentionTier {
                max_age_secs: 90 * 86_400,
                resolution_secs: 300,
            },
            RetentionTier {
                max_age_secs: 5 * 365 * 86_400,
                resolution_secs: 1_800,
            },
        ];
        // The (r300, Jan) group is final once the youngest January sample
        // has aged past the raw tier: Feb 1 + 7d, plus the 1d settle.
        let g2_gate = feb_start + 7 * 86_400 + 86_400;

        // Step a day at a time from Feb 2 through Feb 12, running the full
        // pipeline in production order each day.
        for day in 1..=11 {
            let now = feb_start + day * 86_400;
            compact_in_dir(&host_dir, &tiers, now).unwrap();
            fully_rollup_g1(&host_dir, now);
            rollup_settled_months_in_dir(&host_dir, now, 86_400, &tiers).unwrap();

            let g2_count = list_chunks(&host_dir)
                .unwrap()
                .iter()
                .filter(|c| c.generation == 2)
                .count();
            if now < g2_gate {
                assert_eq!(
                    g2_count, 0,
                    "day {day}: no G2 may exist while raw January data remains"
                );
            }

            // The invariant the incident violated: every original sample
            // is still readable after every pass (hourly timestamps bin
            // 1:1 into 300s buckets, so the count is stable at 744).
            let samples =
                crate::hzc::reader::read_range_in_dir(&host_dir, jan_start, feb_start).unwrap();
            assert_eq!(
                samples.len(),
                31 * 24,
                "day {day}: sample count must never drop"
            );
        }

        // Converged: exactly one G2 bundle covering the whole month.
        let chunks = list_chunks(&host_dir).unwrap();
        assert_eq!(chunks.len(), 1, "expected a single chunk, got {chunks:?}");
        assert_eq!(chunks[0].generation, 2);
        assert_eq!(chunks[0].resolution_secs, 300);
        assert_eq!(chunks[0].start_ts, jan_start);
        assert_eq!(chunks[0].end_ts, feb_start);

        // And the pipeline is a no-op from here.
        let now = feb_start + 12 * 86_400;
        compact_in_dir(&host_dir, &tiers, now).unwrap();
        let r1 = rollup_settled_days_in_dir(&host_dir, now, 3_600).unwrap();
        let r2 = rollup_settled_months_in_dir(&host_dir, now, 86_400, &tiers).unwrap();
        assert!(!r1.did_work());
        assert!(!r2.did_work());
        assert_eq!(list_chunks(&host_dir).unwrap().len(), 1);
    }

    /// The exact on-disk state the incident left behind: a G2 bundle
    /// claiming the full month but containing only its first 25 days, plus
    /// later-arriving G1 chunks holding the remaining days. The rollup
    /// must merge them, never delete them.
    #[test]
    fn merge_rebundle_never_deletes_uncontained_sources() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let host_dir = empty_host_dir(dir.path(), uuid);

        let jan_start: i64 = 1_672_531_200;
        let feb_start: i64 = 1_675_209_600;

        // Partial-month bundle: Jan 1-25 hourly at r300, claiming [Jan, Feb).
        let bundle_samples: Vec<(i64, Slot)> = (0..(25 * 24))
            .map(|h| (jan_start + h * 3_600, slot(20.0)))
            .collect();
        plant_chunk(&host_dir, 2, 300, jan_start, feb_start, &bundle_samples);

        // Late G1 dailies: Jan 26-31.
        for d in 25..31 {
            let day_start = jan_start + d * SECS_PER_DAY;
            let day_samples: Vec<(i64, Slot)> = (0..24)
                .map(|h| (day_start + h * 3_600, slot(22.0)))
                .collect();
            plant_chunk(
                &host_dir,
                1,
                300,
                day_start,
                day_start + SECS_PER_DAY,
                &day_samples,
            );
        }

        let now = feb_start + 45 * 86_400; // long past the gate
        let r = rollup_settled_months_in_dir(&host_dir, now, 86_400, &default_tiers()).unwrap();
        assert_eq!(r.bundled_days, 1);
        assert_eq!(r.source_chunks_consumed, 6);

        let chunks = list_chunks(&host_dir).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].generation, 2);
        let samples =
            crate::hzc::reader::read_range_in_dir(&host_dir, jan_start, feb_start).unwrap();
        assert_eq!(samples.len(), 31 * 24, "all 31 days present after merge");
    }

    /// Replication backfill writes OLD samples long after the month's G2
    /// bundle exists. They must end up merged into the bundle, not swept.
    #[test]
    fn replication_backfill_after_bundle_merges_in() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let host_dir = empty_host_dir(dir.path(), uuid);

        let jan_start: i64 = 1_672_531_200;
        let feb_start: i64 = 1_675_209_600;

        // Existing month bundle with a one-day hole at Jan 20.
        let bundle_samples: Vec<(i64, Slot)> = (0..(31 * 24))
            .filter(|h| !(19 * 24..20 * 24).contains(h))
            .map(|h| (jan_start + h * 3_600, slot(20.0)))
            .collect();
        plant_chunk(&host_dir, 2, 300, jan_start, feb_start, &bundle_samples);
        let bundle_path = list_chunks(&host_dir).unwrap()[0].path.clone();

        // Backfill arrives through the writer as raw G0 data.
        let w = HostWriter::open(dir.path(), uuid, 60, 3_600).unwrap();
        for h in 0..24i64 {
            w.write_sample(jan_start + 19 * SECS_PER_DAY + h * 3_600, slot(25.0))
                .unwrap();
        }
        w.flush().unwrap();
        drop(w);

        let tiers = default_tiers();
        // Past the r300 finality gate (Feb 8 + settle) but well before the
        // 30d boundary would drift any of this month into r1800.
        let now = feb_start + 9 * 86_400;
        // Compactor downsamples the old raw chunks to r300 (their day is
        // long past the raw tier)...
        compact_in_dir(&host_dir, &tiers, now).unwrap();
        fully_rollup_g1(&host_dir, now);
        // ...and the monthly rollup folds them into the existing bundle,
        // rewriting it in place (same deterministic filename).
        let r = rollup_settled_months_in_dir(&host_dir, now, 86_400, &tiers).unwrap();
        assert_eq!(r.bundled_days, 1);

        let chunks = list_chunks(&host_dir).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].generation, 2);
        assert_eq!(chunks[0].path, bundle_path, "rewritten in place");
        let samples =
            crate::hzc::reader::read_range_in_dir(&host_dir, jan_start, feb_start).unwrap();
        assert_eq!(samples.len(), 31 * 24, "hole filled by backfill");
    }

    /// The other prod artifact: a full-day-span G1 bundle created from a
    /// partial day, with later same-day chunks sitting next to it. One G1
    /// pass must merge them into a single bundle with each ts exactly once.
    #[test]
    fn g1_partial_day_then_late_fragments_heal() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let host_dir = empty_host_dir(dir.path(), uuid);

        let day_start: i64 = 1_672_531_200;
        let day_end = day_start + SECS_PER_DAY;

        // Day bundle holding only hours 0-11.
        let partial: Vec<(i64, Slot)> = (0..12)
            .map(|h| (day_start + h * 3_600, slot(20.0)))
            .collect();
        plant_chunk(&host_dir, 1, 300, day_start, day_end, &partial);
        // Late hourly chunks for hours 12-23 (overlapping hour 12-13 with
        // nothing; hours are disjoint from the bundle's).
        for h in 12..24i64 {
            let s = day_start + h * 3_600;
            plant_chunk(&host_dir, 1, 300, s, s + 3_600, &[(s, slot(21.0))]);
        }

        let now = day_end + 10 * 86_400;
        let r = rollup_settled_days_in_dir(&host_dir, now, 3_600).unwrap();
        assert_eq!(r.bundled_days, 1);

        let chunks = list_chunks(&host_dir).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].start_ts, day_start);
        assert_eq!(chunks[0].end_ts, day_end);
        let samples = crate::hzc::reader::read_range_in_dir(&host_dir, day_start, day_end).unwrap();
        let ts: Vec<i64> = samples.iter().map(|s| s.timestamp_secs).collect();
        assert_eq!(ts.len(), 24);
        for w in ts.windows(2) {
            assert!(w[0] < w[1], "duplicate ts {} after merge", w[0]);
        }
    }

    /// A lone fragment with no siblings (the only survivor of the incident
    /// on each prod host) must never be deleted by any rollup pass.
    #[test]
    fn g1_singleton_fragment_survives() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let host_dir = empty_host_dir(dir.path(), uuid);

        let day_start: i64 = 1_672_531_200;
        let s = day_start + 23 * 3_600;
        plant_chunk(&host_dir, 2, 300, s, s + 3_600, &[(s, slot(21.0))]);

        let tiers = default_tiers();
        let now = day_start + 90 * 86_400;
        rollup_settled_days_in_dir(&host_dir, now, 3_600).unwrap();
        rollup_settled_months_in_dir(&host_dir, now, 86_400, &tiers).unwrap();
        rollup_settled_years_in_dir(&host_dir, now, 2 * 86_400, &tiers).unwrap();

        let chunks = list_chunks(&host_dir).unwrap();
        assert_eq!(chunks.len(), 1, "singleton fragment must survive");
        let samples =
            crate::hzc::reader::read_range_in_dir(&host_dir, day_start, day_start + SECS_PER_DAY)
                .unwrap();
        assert_eq!(samples.len(), 1);
    }

    /// Crashed-cleanup recovery at G2: leftovers whose samples are already
    /// in the bundle are removed after decode-verify, without rewriting
    /// the bundle.
    #[test]
    fn containment_fast_path_deletes_without_rewrite() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let host_dir = empty_host_dir(dir.path(), uuid);

        let jan_start: i64 = 1_672_531_200;
        let feb_start: i64 = 1_675_209_600;
        let all: Vec<(i64, Slot)> = (0..(31 * 24))
            .map(|h| (jan_start + h * 3_600, slot(20.0)))
            .collect();
        plant_chunk(&host_dir, 2, 300, jan_start, feb_start, &all);
        let bundle = list_chunks(&host_dir).unwrap()[0].clone();
        let before_mtime = std::fs::metadata(&bundle.path).unwrap().modified().unwrap();

        // Two leftover dailies whose samples are subsets of the bundle.
        for d in 0..2usize {
            let day_start = jan_start + (d as i64) * SECS_PER_DAY;
            let day: Vec<(i64, Slot)> = all
                .iter()
                .filter(|(ts, _)| (day_start..day_start + SECS_PER_DAY).contains(ts))
                .copied()
                .collect();
            plant_chunk(&host_dir, 1, 300, day_start, day_start + SECS_PER_DAY, &day);
        }
        assert_eq!(list_chunks(&host_dir).unwrap().len(), 3);

        std::thread::sleep(std::time::Duration::from_millis(20)); // mtime tick
        let now = feb_start + 45 * 86_400;
        let r = rollup_settled_months_in_dir(&host_dir, now, 86_400, &default_tiers()).unwrap();
        assert_eq!(r.contained_cleanups, 1);
        assert_eq!(r.bundled_days, 0);

        let chunks = list_chunks(&host_dir).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            std::fs::metadata(&chunks[0].path)
                .unwrap()
                .modified()
                .unwrap(),
            before_mtime,
            "bundle must not be rewritten when it already contains everything"
        );
    }

    /// Raw groups of a multi-tier policy are never G2-bundled: that data
    /// is guaranteed to be rewritten at the next tier boundary, and an
    /// r0 month bundle is exactly the artifact that fed the incident.
    #[test]
    fn r0_groups_skipped_for_g2_with_multi_tier() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let host_dir = empty_host_dir(dir.path(), uuid);

        let jan_start: i64 = 1_672_531_200;
        for d in 0..2i64 {
            let day_start = jan_start + d * SECS_PER_DAY;
            let day: Vec<(i64, Slot)> = (0..24)
                .map(|h| (day_start + h * 3_600, slot(20.0)))
                .collect();
            plant_chunk(&host_dir, 1, 0, day_start, day_start + SECS_PER_DAY, &day);
        }

        let now = jan_start + 90 * 86_400;
        let r = rollup_settled_months_in_dir(&host_dir, now, 86_400, &default_tiers()).unwrap();
        assert_eq!(r.bundled_days, 0);
        assert!(r.skipped_transitional_res >= 1);
        assert_eq!(list_chunks(&host_dir).unwrap().len(), 2, "chunks untouched");
    }

    /// With a single raw-only tier, raw IS final, so monthly and yearly
    /// bundling of r0 groups proceeds.
    #[test]
    fn single_tier_raw_config_still_bundles_months_and_years() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let host_dir = empty_host_dir(dir.path(), uuid);

        let jan_start: i64 = 1_672_531_200;
        let feb_start: i64 = 1_675_209_600;
        for d in 0..2i64 {
            let day_start = jan_start + d * SECS_PER_DAY;
            let day: Vec<(i64, Slot)> = (0..24)
                .map(|h| (day_start + h * 3_600, slot(20.0)))
                .collect();
            plant_chunk(&host_dir, 1, 0, day_start, day_start + SECS_PER_DAY, &day);
        }

        let tiers = raw_only_tiers();
        let now = jan_start + 400 * 86_400;
        let r2 = rollup_settled_months_in_dir(&host_dir, now, 86_400, &tiers).unwrap();
        assert_eq!(r2.bundled_days, 1);
        let r3 = rollup_settled_years_in_dir(&host_dir, now, 2 * 86_400, &tiers).unwrap();
        // A single g2 in the year group is a singleton - nothing to merge.
        assert_eq!(r3.bundled_days, 0);

        let chunks = list_chunks(&host_dir).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].generation, 2);
        assert_eq!(chunks[0].start_ts, jan_start);
        assert_eq!(
            chunks[0].end_ts,
            jan_start + 2 * SECS_PER_DAY,
            "truthful span"
        );
        let samples =
            crate::hzc::reader::read_range_in_dir(&host_dir, jan_start, feb_start).unwrap();
        assert_eq!(samples.len(), 48);
    }

    /// G3 with the default tiers: year groups bundle per resolution, each
    /// gated on its own tier finality, each with a truthful span - and a
    /// later pass merges a converted G3 with newly-arrived G2 months
    /// instead of shadowing or sweeping them.
    #[test]
    fn g3_gate_produces_truthful_partial_year_bundles_that_converge() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let host_dir = empty_host_dir(dir.path(), uuid);

        let year_start: i64 = 1_672_531_200; // Jan 1 2023
        let (_, year_end) = utc_year_bounds(year_start);

        // Months at mixed resolutions, as whole-chunk aging produces them:
        // Jan + Feb already at r7200, Jul + Aug still at r1800.
        let mut month = year_start;
        let mut planted: Vec<(i64, u32)> = Vec::new(); // (month_start, res)
        for idx in 0..12 {
            let (m_start, m_end) = utc_month_bounds(month);
            if idx == 0 || idx == 1 {
                planted.push((m_start, 7_200));
            } else if idx == 6 || idx == 7 {
                planted.push((m_start, 1_800));
            }
            month = m_end;
        }
        for (m_start, res) in &planted {
            let step = i64::from(*res);
            let samples: Vec<(i64, Slot)> = (0..(2 * SECS_PER_DAY / step))
                .map(|i| (m_start + i * step, slot(20.0)))
                .collect();
            plant_chunk(
                &host_dir,
                2,
                *res,
                *m_start,
                m_start + 2 * SECS_PER_DAY,
                &samples,
            );
        }

        let tiers = default_tiers();

        // Gate check: r1800 becomes final at year_end + 30d (tier 1's max
        // age) + 2d settle; r7200 at year_end + 180d + 2d settle.
        let now = year_end + 33 * 86_400;
        let r = rollup_settled_years_in_dir(&host_dir, now, 2 * 86_400, &tiers).unwrap();
        assert_eq!(r.bundled_days, 1, "only the r1800 group is final");
        assert!(r.skipped_unsettled >= 1, "r7200 group must wait");

        let g3_r1800 = list_chunks(&host_dir)
            .unwrap()
            .into_iter()
            .find(|c| c.generation == 3)
            .expect("r1800 g3 present");
        assert_eq!(g3_r1800.resolution_secs, 1_800);
        let jul_start = planted[2].0;
        let aug_start = planted[3].0;
        assert_eq!(g3_r1800.start_ts, jul_start, "truthful start");
        assert_eq!(
            g3_r1800.end_ts,
            aug_start + 2 * SECS_PER_DAY,
            "truthful end"
        );

        // Later: the r7200 group is final too.
        let now = year_end + 183 * 86_400;
        let r = rollup_settled_years_in_dir(&host_dir, now, 2 * 86_400, &tiers).unwrap();
        assert_eq!(r.bundled_days, 1);

        let chunks = list_chunks(&host_dir).unwrap();
        assert_eq!(chunks.len(), 2, "one g3 per resolution");
        assert!(chunks.iter().all(|c| c.generation == 3));

        // Nothing lost across the whole year.
        let expected: usize = planted
            .iter()
            .map(|(_, res)| (2 * SECS_PER_DAY / i64::from(*res)) as usize)
            .sum();
        let samples =
            crate::hzc::reader::read_range_in_dir(&host_dir, year_start, year_end).unwrap();
        assert_eq!(samples.len(), expected);
    }
}
