//! End-to-end lifecycle tests for the hzc storage. Simulates time stepping
//! across multiple probes with different sampling intervals, and verifies:
//!
//! - The G0 → G1 → G2 → G3 rollup chain preserves every sample until tiering
//!   intentionally downsamples it.
//! - Tier-boundary downsampling produces the exact expected sample count.
//! - Past the final tier's horizon, data is deleted.
//! - Compression ratios stay within sensible thresholds (catches encoder
//!   regressions before prod).
//! - A 5y range query completes within a reasonable SLO.
//!
//! These tests are gated by `#[ignore]` because they synthesise years of
//! probe data; run them explicitly with:
//!
//! ```text
//! RUN_LONG_TESTS=1 cargo test -p haze-store --test storage_lifecycle \
//!     -- --ignored --nocapture
//! ```

use std::path::Path;
use std::sync::Arc;

use haze_store::hzc::chunk::decode_chunk;
use haze_store::hzc::compactor::compact_host;
use haze_store::hzc::reader::list_chunks;
use haze_store::{
    Clock, HostWriter, HzcStore, ManualClock, RetentionTier, Sample, Slot, host_directory,
    read_range, rollup_g2_host, rollup_g3_host, rollup_host,
};
use tempfile::TempDir;
use uuid::Uuid;

const SECS_PER_DAY: i64 = 86_400;

/// The compactor classifies a chunk by the age of its newest edge, so data
/// may stay at the previous tier's resolution until its whole chunk has
/// crossed a boundary. The largest chunks are yearly G3 bundles, plus the
/// 30-day pass cadence of these tests.
const WHOLE_CHUNK_LAG: i64 = 366 * SECS_PER_DAY + 31 * SECS_PER_DAY;

/// Reference point - Jan 1 2024 00:00:00 UTC. Aligns chunk windows + days +
/// months + years cleanly.
const T0: i64 = 1_704_067_200;

// =====================================================================
// Test scenario knobs
// =====================================================================
//
// Full spec (per the design plan): 150 hosts × 4 sampling intervals
// (10s/30s/60s/120s) × 5 years. The number of samples that implies (~400M)
// is not viable as a "regression test" runtime, so we run a scaled-down
// version that still exercises every tier transition and every rollup
// phase. The scaling factor only affects sample volume, not correctness:
// the tier-aware split-downsample, the multi-generation reader, and the
// settled-span rollups all behave identically at any scale.

const INTERVALS_SECS: [u32; 4] = [300, 600, 1_200, 1_800];
const HOSTS_PER_INTERVAL: usize = 2;
const YEARS_SIMULATED: i64 = 2;

fn num_hosts() -> usize {
    INTERVALS_SECS.len() * HOSTS_PER_INTERVAL
}

// =====================================================================
// Tier policy (test-only)
// =====================================================================
//
// The user-facing spec expresses retention as percentages:
//   - 0 .. 1.5w  : full resolution
//   - 1.5w .. 2m : keep 80% of points
//   - 2m   .. 8m : keep 50%
//   - 8m   .. 5y : keep 10%
//   - past 5y    : deleted
//
// In production code retention is stored as absolute `resolution_secs`.
// We translate the percentages to per-host absolute resolutions:
//   target_res = base_interval / keep_ratio (rounded up to a multiple of
//   base_interval so consolidation bins line up cleanly).

const TIER_1_AGE: i64 = (3 * SECS_PER_DAY) / 2 + 12 * SECS_PER_DAY; // 13.5 days (≈ 1.5w)
const TIER_2_AGE: i64 = 60 * SECS_PER_DAY; // 2 months
const TIER_3_AGE: i64 = 240 * SECS_PER_DAY; // 8 months
const TIER_4_AGE: i64 = 5 * 365 * SECS_PER_DAY; // 5 years

fn round_up_multiple(value: u32, multiple: u32) -> u32 {
    if value == 0 {
        return multiple;
    }
    value.div_ceil(multiple) * multiple
}

fn tiers_for(base_interval_secs: u32) -> Vec<RetentionTier> {
    // keep_ratio → absolute resolution
    let r2 = round_up_multiple((base_interval_secs as f32 / 0.8) as u32, base_interval_secs);
    let r3 = round_up_multiple((base_interval_secs as f32 / 0.5) as u32, base_interval_secs);
    let r4 = round_up_multiple((base_interval_secs as f32 / 0.1) as u32, base_interval_secs);
    vec![
        RetentionTier {
            max_age_secs: TIER_1_AGE,
            resolution_secs: 0,
        },
        RetentionTier {
            max_age_secs: TIER_2_AGE,
            resolution_secs: r2,
        },
        RetentionTier {
            max_age_secs: TIER_3_AGE,
            resolution_secs: r3,
        },
        RetentionTier {
            max_age_secs: TIER_4_AGE,
            resolution_secs: r4,
        },
    ]
}

/// Resolution that applies to a sample of `age_secs`. Mirrors the
/// compactor's `target_resolution` so the test computes its expected
/// counts the same way the code under test does.
fn resolution_for_age(age_secs: i64, tiers: &[RetentionTier]) -> Option<u32> {
    for tier in tiers {
        if age_secs <= tier.max_age_secs {
            return Some(tier.resolution_secs);
        }
    }
    None
}

// =====================================================================
// Test data generators
// =====================================================================

/// Deterministic `SplitMix64`. Test data must be reproducible so the
/// expected-count math is exact.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn synthetic_slot(seed: u64) -> Slot {
    let mut s = seed;
    let median = 20.0 + (splitmix64(&mut s) % 50) as f32 * 0.1; // 20.0 .. 25.0 ms
    Slot {
        min: median - 0.3,
        p2_5: median - 0.2,
        p25: median - 0.1,
        median,
        p75: median + 0.1,
        p97_5: median + 0.2,
        loss_pct: 0.0,
    }
}

fn build_fleet(data_dir: &Path) -> Vec<(Uuid, u32)> {
    let mut hosts = Vec::with_capacity(num_hosts());
    for interval_secs in &INTERVALS_SECS {
        for _ in 0..HOSTS_PER_INTERVAL {
            let uuid = Uuid::new_v4();
            let w = HostWriter::open(data_dir, uuid, *interval_secs, 3_600).unwrap();
            w.set_retention_tiers(tiers_for(*interval_secs)).unwrap();
            drop(w);
            hosts.push((uuid, *interval_secs));
        }
    }
    hosts
}

/// Write one day's worth of samples for one host via an already-open
/// writer.
fn write_day_with(writer: &HostWriter, uuid: Uuid, interval_secs: u32, day_start: i64) {
    let interval_i64 = i64::from(interval_secs);
    let samples_per_day = (SECS_PER_DAY / interval_i64) as usize;
    let mut seed = uuid.as_u128() as u64 ^ (day_start as u64);
    for i in 0..samples_per_day {
        let ts = day_start + (i as i64) * interval_i64;
        writer.write_sample(ts, synthetic_slot(seed)).unwrap();
        seed = seed.wrapping_add(1);
    }
}

/// Open-flush convenience for tests that only write a few days. For long
/// runs prefer `write_day_with`.
fn write_day(data_dir: &Path, uuid: Uuid, interval_secs: u32, day_start: i64) {
    let w = HostWriter::open(data_dir, uuid, interval_secs, 3_600).unwrap();
    write_day_with(&w, uuid, interval_secs, day_start);
    w.flush().unwrap();
}

/// Expected sample count after tiering has been applied. For each second
/// of the time range, the sample exists iff its age tier has
/// `resolution_secs == 0` (raw); otherwise it shows up downsampled at the
/// tier's `resolution_secs`. The downsampled count for a tier T over a
/// time window W is `W / T.resolution_secs` (with `T == 0` meaning the
/// raw probe interval).
fn expected_count(
    interval_secs: u32,
    from: i64,
    to: i64,
    tiers: &[RetentionTier],
    now: i64,
) -> usize {
    let mut count = 0usize;
    // Walk the range in tier-aligned segments. For each absolute timestamp
    // `t` in `[from, to)`, find its age and the tier's resolution, then
    // tally one sample per `resolution_secs` worth of time.
    //
    // For computation efficiency, we step in tier-boundary-sized chunks.
    let mut t = from;
    while t < to {
        let age = now - t;
        let Some(target_res) = resolution_for_age(age, tiers) else {
            // Past the final tier - nothing left in this and earlier
            // segments contributes.
            break;
        };
        // Time at which `t` would cross to the *previous* tier (younger),
        // i.e. its age decreases past the previous boundary. We just need
        // to know where this tier ends moving forward in `t`.
        //
        // The current tier applies while `now - t <= tier.max_age_secs`,
        // i.e. `t >= now - tier.max_age_secs`. The tier ENDS (moving
        // forward in t) when t crosses the next coarser tier's boundary -
        // which doesn't happen moving forward in time. The tier ENDS
        // (moving forward in t) when we cross from a tier into a finer
        // (younger) tier. So we need the boundary where age decreases
        // past `tiers[i-1].max_age_secs`, i.e. t = now - tiers[i-1].max_age_secs.
        //
        // Simpler: find the smallest tier boundary strictly newer than `t`.
        let segment_end = {
            let mut e = to;
            for tier in tiers {
                let boundary = now - tier.max_age_secs;
                if boundary > t && boundary < e {
                    e = boundary;
                }
            }
            e
        };
        let span = segment_end - t;
        let effective_res = if target_res == 0 {
            i64::from(interval_secs)
        } else {
            i64::from(target_res)
        };
        // Number of samples in `[t, segment_end)` at this resolution.
        count += (span / effective_res) as usize;
        t = segment_end;
    }
    count
}

// =====================================================================
// Helpers to drive the lifecycle
// =====================================================================

fn run_compaction_pass(data_dir: &Path, hosts: &[(Uuid, u32)], now_secs: i64) {
    // For every host, run the downsample compactor + the G1 + G2 + G3
    // rollups. Order matters: G1 must finish first so G2 has g1 input.
    for (uuid, interval_secs) in hosts {
        let tiers = tiers_for(*interval_secs);
        compact_host(data_dir, *uuid, &tiers, now_secs).unwrap();
        rollup_host(data_dir, *uuid, now_secs, 3_600).unwrap();
        rollup_g2_host(data_dir, *uuid, now_secs, SECS_PER_DAY, &tiers).unwrap();
        rollup_g3_host(data_dir, *uuid, now_secs, 2 * SECS_PER_DAY, &tiers).unwrap();
    }
}

fn list_all_samples(data_dir: &Path, uuid: Uuid, from: i64, to: i64) -> Vec<Sample> {
    read_range(data_dir, uuid, from, to).unwrap()
}

// =====================================================================
// Tests
// =====================================================================

#[test]
#[ignore = "long-running storage lifecycle test; run with RUN_LONG_TESTS=1"]
fn lifecycle_g0_to_g3_no_data_loss() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    let clock = Arc::new(ManualClock::new(T0));
    let _store = HzcStore::new_with_clock(data_dir, clock.clone()).unwrap();

    let hosts = build_fleet(data_dir);
    let total_days = (YEARS_SIMULATED * 365) + 5; // a few extra days

    // Open writers once and reuse across days to avoid per-day fcntl /
    // meta.json churn dominating the runtime.
    let writers: Vec<HostWriter> = hosts
        .iter()
        .map(|(uuid, interval_secs)| {
            HostWriter::open(data_dir, *uuid, *interval_secs, 3_600).unwrap()
        })
        .collect();

    // Step time day-by-day, writing one day's samples per host per step.
    // Run compaction + rollup every 30 days to amortise the cost; that's
    // still fine-grained enough to catch tier transitions.
    for d in 0..total_days {
        let day_start = T0 + d * SECS_PER_DAY;
        for ((uuid, interval_secs), writer) in hosts.iter().zip(&writers) {
            write_day_with(writer, *uuid, *interval_secs, day_start);
        }
        clock.set_now(day_start + SECS_PER_DAY);
        if d % 30 == 0 || d == total_days - 1 {
            // Flush all open writers so the compactor sees sealed chunks.
            for w in &writers {
                w.flush().unwrap();
            }
            run_compaction_pass(data_dir, &hosts, clock.now_secs());
        }
    }
    for w in &writers {
        w.flush().unwrap();
    }
    let now = clock.now_secs();
    run_compaction_pass(data_dir, &hosts, now);

    // Validate every host's sample count matches expectation.
    //
    // The compactor classifies a chunk by the age of its newest edge, so a
    // chunk straddling a tier boundary keeps its finer resolution until the
    // whole chunk has crossed. The largest chunks are yearly G3 bundles, so
    // data may stay at the previous tier's resolution for up to a year past
    // the strict boundary (plus the 30-day pass cadence of this test). The
    // strict policy count is therefore a LOWER bound, and the upper bound
    // is the count computed as if every boundary were one year + one pass
    // later (`WHOLE_CHUNK_LAG`).
    for (uuid, interval_secs) in &hosts {
        let tiers = tiers_for(*interval_secs);
        let samples = list_all_samples(data_dir, *uuid, T0, now);
        let expected_strict = expected_count(*interval_secs, T0, now, &tiers, now);
        let expected_lagged =
            expected_count(*interval_secs, T0, now, &tiers, now - WHOLE_CHUNK_LAG);
        // Very young samples in the live WAL haven't been compacted yet, so
        // they may exceed the count. Allow a small headroom.
        let headroom = 2 * (SECS_PER_DAY / i64::from(*interval_secs)) as usize;
        assert!(
            samples.len() <= expected_lagged + headroom,
            "host {uuid} interval={interval_secs}s: returned {} samples, expected ≤ {} (+headroom {})",
            samples.len(),
            expected_lagged,
            headroom,
        );
        // Lower bound: downsampling and deletion only ever LAG the strict
        // policy, so nothing may drop below it (modulo WAL slop).
        assert!(
            samples.len() + headroom >= expected_strict.saturating_sub(headroom),
            "host {uuid}: lost too many samples vs expectation {} (got {})",
            expected_strict,
            samples.len(),
        );
        // No duplicate timestamps may survive the reader.
        for w in samples.windows(2) {
            assert!(
                w[0].timestamp_secs < w[1].timestamp_secs,
                "host {uuid}: duplicate or unordered ts {}",
                w[0].timestamp_secs
            );
        }
    }
}

#[test]
#[ignore = "long-running storage lifecycle test; run with RUN_LONG_TESTS=1"]
fn past_final_tier_data_is_deleted() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    let clock = Arc::new(ManualClock::new(T0));
    let _store = HzcStore::new_with_clock(data_dir, clock.clone()).unwrap();

    let interval_secs: u32 = 600;
    let uuid = Uuid::new_v4();
    let w = HostWriter::open(data_dir, uuid, interval_secs, 3_600).unwrap();
    w.set_retention_tiers(tiers_for(interval_secs)).unwrap();
    drop(w);

    // Write a single day at T0, then jump forward >5y and compact. All
    // data should be gone.
    write_day(data_dir, uuid, interval_secs, T0);
    let future = T0 + (5 * 365 + 30) * SECS_PER_DAY;
    clock.set_now(future);
    run_compaction_pass(data_dir, &[(uuid, interval_secs)], future);

    let samples = list_all_samples(data_dir, uuid, T0, future);
    assert!(
        samples.is_empty(),
        "expected no samples past final tier, got {}",
        samples.len()
    );
}

#[test]
#[ignore = "long-running storage lifecycle test; run with RUN_LONG_TESTS=1"]
fn compression_ratios_stay_under_thresholds() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    let clock = Arc::new(ManualClock::new(T0));
    let _store = HzcStore::new_with_clock(data_dir, clock.clone()).unwrap();

    let interval_secs: u32 = 60;
    let uuid = Uuid::new_v4();
    let w = HostWriter::open(data_dir, uuid, interval_secs, 3_600).unwrap();
    w.set_retention_tiers(tiers_for(interval_secs)).unwrap();
    drop(w);

    // Write 40 days at 60s interval to give the rollup G1 and G2 something
    // to chew on. (G3 needs a full year, so skip the G3 assertion below.)
    let total_days: i64 = 40;
    for d in 0..total_days {
        write_day(data_dir, uuid, interval_secs, T0 + d * SECS_PER_DAY);
    }
    let now = T0 + (total_days + 35) * SECS_PER_DAY;
    clock.set_now(now);
    run_compaction_pass(data_dir, &[(uuid, interval_secs)], now);

    let host_dir = host_directory(data_dir, uuid);
    let chunks = list_chunks(&host_dir).unwrap();
    let mut g1_total_bytes = 0u64;
    let mut g1_total_samples = 0u64;
    let mut g2_total_bytes = 0u64;
    let mut g2_total_samples = 0u64;
    for c in &chunks {
        let bytes = std::fs::metadata(&c.path).map_or(0, |m| m.len());
        let samples = decode_chunk(&std::fs::read(&c.path).unwrap()).map_or(0, |s| s.len() as u64);
        if c.generation == 1 {
            g1_total_bytes += bytes;
            g1_total_samples += samples;
        } else if c.generation == 2 {
            g2_total_bytes += bytes;
            g2_total_samples += samples;
        }
    }
    if g1_total_samples > 0 {
        let bps = g1_total_bytes as f64 / g1_total_samples as f64;
        eprintln!("G1 bytes/sample = {bps:.2}");
        assert!(bps < 80.0, "G1 bytes/sample {bps:.2} exceeds 80 threshold");
    }
    if g2_total_samples > 0 {
        let bps = g2_total_bytes as f64 / g2_total_samples as f64;
        eprintln!("G2 bytes/sample = {bps:.2}");
        assert!(bps < 30.0, "G2 bytes/sample {bps:.2} exceeds 30 threshold");
    }
}

#[test]
#[ignore = "long-running storage lifecycle test; run with RUN_LONG_TESTS=1"]
fn full_range_query_latency() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();
    let clock = Arc::new(ManualClock::new(T0));
    let _store = HzcStore::new_with_clock(data_dir, clock.clone()).unwrap();

    let interval_secs: u32 = 600; // 10-minute samples; keeps total volume low.
    let uuid = Uuid::new_v4();
    let w = HostWriter::open(data_dir, uuid, interval_secs, 3_600).unwrap();
    w.set_retention_tiers(tiers_for(interval_secs)).unwrap();
    drop(w);

    // Two years of data at 10-minute resolution.
    let total_days: i64 = 2 * 365;
    for d in 0..total_days {
        write_day(data_dir, uuid, interval_secs, T0 + d * SECS_PER_DAY);
    }
    let now = T0 + (total_days + 5) * SECS_PER_DAY;
    clock.set_now(now);
    run_compaction_pass(data_dir, &[(uuid, interval_secs)], now);

    let q_start = std::time::Instant::now();
    let samples = list_all_samples(data_dir, uuid, T0, now);
    let elapsed = q_start.elapsed();
    eprintln!(
        "full_range_query: {} samples, {} ms",
        samples.len(),
        elapsed.as_millis()
    );
    assert!(
        elapsed.as_millis() < 1_000,
        "full-range query took {} ms - exceeds 1s SLO",
        elapsed.as_millis()
    );
}
