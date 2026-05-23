//! One-shot measurement of per-generation compression and query latency
//! against synthetic data. Prints a markdown summary table on stderr.
//!
//! Run with:
//!
//! ```text
//! cargo test -p haze-store --test measure_compression --release \
//!     -- --ignored --nocapture
//! ```

use std::time::Instant;

use haze_store::Slot;
use haze_store::hzc::chunk::{
    ZSTD_LEVEL_G0, ZSTD_LEVEL_G1, ZSTD_LEVEL_G2, ZSTD_LEVEL_G3, encode_chunk,
};
use haze_store::hzc::compactor::{
    compact_host, rollup_settled_months_in_dir, rollup_settled_years_in_dir,
};
use haze_store::hzc::reader::list_chunks;
use haze_store::{
    HostWriter, RetentionTier, default_retention_tiers, host_directory, read_range, rollup_host,
};
use tempfile::TempDir;
use uuid::Uuid;

const SECS_PER_DAY: i64 = 86_400;
const T0: i64 = 1_704_067_200; // Jan 1 2024 UTC

fn synth_slot(i: i64) -> Slot {
    let m = 20.0 + ((i % 50) as f32) * 0.1;
    Slot {
        min: m - 0.3,
        p2_5: m - 0.2,
        p25: m - 0.1,
        median: m,
        p75: m + 0.1,
        p97_5: m + 0.2,
        loss_pct: 0.0,
    }
}

#[test]
#[ignore = "measurement test; run via --ignored"]
fn measure_per_generation_compression() {
    // 7 days of 60s samples = 10080 samples
    let interval = 60i64;
    let samples: Vec<(i64, Slot)> = (0..(7 * 24 * 60))
        .map(|i| (T0 + i * interval, synth_slot(i)))
        .collect();
    let n = samples.len();
    let raw_bytes = n * (8 + 7 * 4); // ts + 7 f32 fields

    let g0 = encode_chunk(&samples, ZSTD_LEVEL_G0).unwrap();
    let g1 = encode_chunk(&samples, ZSTD_LEVEL_G1).unwrap();
    let g2 = encode_chunk(&samples, ZSTD_LEVEL_G2).unwrap();
    let g3 = encode_chunk(&samples, ZSTD_LEVEL_G3).unwrap();

    eprintln!(
        "\n## Compression by generation (7 days × 60s = {n} samples, raw = {raw_bytes} bytes)\n"
    );
    eprintln!("| Generation | zstd level | Bytes | B/sample | Ratio vs raw |");
    eprintln!("|---|---|---|---|---|");
    for (label, level, bytes) in [
        ("G0 (live writer)", ZSTD_LEVEL_G0, g0.len()),
        ("G1 (daily bundle)", ZSTD_LEVEL_G1, g1.len()),
        ("G2 (monthly bundle)", ZSTD_LEVEL_G2, g2.len()),
        ("G3 (yearly bundle)", ZSTD_LEVEL_G3, g3.len()),
    ] {
        let bps = bytes as f64 / n as f64;
        let ratio = raw_bytes as f64 / bytes as f64;
        eprintln!("| {label} | {level} | {bytes} | {bps:.2} | {ratio:.1}× |");
    }
}

#[test]
#[ignore = "measurement test; run via --ignored"]
fn measure_query_latency_after_lifecycle() {
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path();
    let uuid = Uuid::new_v4();
    let interval: u32 = 60;
    let w = HostWriter::open(data_dir, uuid, interval, 3_600).unwrap();
    // 90 days of 60s samples = 129,600 samples.
    let total_days: i64 = 90;
    let interval_i64 = i64::from(interval);
    for day in 0..total_days {
        let ds = T0 + day * SECS_PER_DAY;
        for i in 0..(SECS_PER_DAY / interval_i64) {
            w.write_sample(ds + i * interval_i64, synth_slot(i))
                .unwrap();
        }
    }
    w.flush().unwrap();
    drop(w);

    let now = T0 + total_days * SECS_PER_DAY + 3 * SECS_PER_DAY;
    let tiers = default_retention_tiers();

    // Pre-rollup query latency
    let q0_start = Instant::now();
    let samples_pre = read_range(data_dir, uuid, T0, now).unwrap();
    let q0_ms = q0_start.elapsed().as_millis();
    let file_count_pre = list_chunks(&host_directory(data_dir, uuid)).unwrap().len();

    compact_host(data_dir, uuid, &tiers, now).unwrap();
    rollup_host(data_dir, uuid, now, 3_600).unwrap();
    rollup_settled_months_in_dir(&host_directory(data_dir, uuid), now, 86_400).unwrap();
    rollup_settled_years_in_dir(&host_directory(data_dir, uuid), now, 2 * 86_400).unwrap();

    let q1_start = Instant::now();
    let samples_post = read_range(data_dir, uuid, T0, now).unwrap();
    let q1_ms = q1_start.elapsed().as_millis();
    let file_count_post = list_chunks(&host_directory(data_dir, uuid)).unwrap().len();

    let host_dir = host_directory(data_dir, uuid);
    let mut total_bytes_post: u64 = 0;
    let mut by_gen: std::collections::BTreeMap<u8, (u64, usize, u64)> =
        std::collections::BTreeMap::new(); // gen -> (bytes, files, samples)
    for c in list_chunks(&host_dir).unwrap() {
        let bytes = std::fs::metadata(&c.path).map_or(0, |m| m.len());
        total_bytes_post += bytes;
        let s = haze_store::hzc::chunk::decode_chunk(&std::fs::read(&c.path).unwrap())
            .map_or(0, |v| v.len()) as u64;
        let entry = by_gen.entry(c.generation).or_default();
        entry.0 += bytes;
        entry.1 += 1;
        entry.2 += s;
    }

    eprintln!("\n## End-to-end query latency (90d × 60s = 129 600 samples)\n");
    eprintln!("| Stage | Files | Bytes on disk | Samples | Query [T0, now] (ms) |");
    eprintln!("|---|---|---|---|---|");
    eprintln!(
        "| Pre-rollup (G0 only) | {} | n/a | {} | {} |",
        file_count_pre,
        samples_pre.len(),
        q0_ms
    );
    eprintln!(
        "| Post-rollup (G0/G1/G2/G3) | {} | {} | {} | {} |",
        file_count_post,
        total_bytes_post,
        samples_post.len(),
        q1_ms
    );

    eprintln!("\n## Files by generation after full rollup\n");
    eprintln!("| Gen | Files | Bytes | Samples | B/sample |");
    eprintln!("|---|---|---|---|---|");
    for (generation, (bytes, files, samples)) in &by_gen {
        let bps = if *samples > 0 {
            *bytes as f64 / *samples as f64
        } else {
            0.0
        };
        eprintln!("| G{generation} | {files} | {bytes} | {samples} | {bps:.2} |");
    }
}

fn _avoid_unused(_: &[RetentionTier]) {}
