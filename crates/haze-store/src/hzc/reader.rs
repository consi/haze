//! Filesystem-backed range reader.
//!
//! Lists a host's `chunks/` directory, parses the filenames into `ChunkRef`s,
//! filters by time-overlap and optional resolution, then decompresses +
//! decodes each matching chunk and concatenates the in-range samples.
//!
//! Self-contained: no `SQLite`, no in-memory index. `readdir + filter` is
//! sub-millisecond for typical host directories (a few hundred chunks).

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use super::{
    chunk::decode_chunk,
    format::{ChunkRef, parse_chunk_filename},
    wal,
    writer::{CHUNKS_DIR, HzcError, WAL_DIR, host_directory},
};
use crate::slot::{Sample, Slot};
use uuid::Uuid;

/// List every chunk in `host_dir/chunks/`. Returns chunk refs sorted by
/// `(resolution_secs ascending, start_ts ascending)`.
pub fn list_chunks(host_dir: &Path) -> Result<Vec<ChunkRef>, HzcError> {
    let dir = host_dir.join(CHUNKS_DIR);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if let Ok(cr) = parse_chunk_filename(&path) {
            out.push(cr);
        }
    }
    out.sort_by(|a, b| {
        a.resolution_secs
            .cmp(&b.resolution_secs)
            .then(a.start_ts.cmp(&b.start_ts))
    });
    Ok(out)
}

/// Read every sample in `[from, to)` for the host, at the best available
/// resolution. The selection rule:
///
/// 1. Collect all chunks that overlap the query range.
/// 2. For each disjoint sub-range, prefer the finest resolution available.
/// 3. Decode + filter + concatenate.
///
/// Returns timestamped samples sorted by `ts`.
pub fn read_range(
    data_dir: &Path,
    host_uuid: Uuid,
    from: i64,
    to: i64,
) -> Result<Vec<Sample>, HzcError> {
    let host_dir = host_directory(data_dir, host_uuid);
    read_range_in_dir(&host_dir, from, to)
}

/// Same as [`read_range`] but takes a host directory directly (handy for tests).
pub fn read_range_in_dir(host_dir: &Path, from: i64, to: i64) -> Result<Vec<Sample>, HzcError> {
    let chunks = list_chunks(host_dir)?;
    let chunk_seqs: HashSet<u64> = chunks.iter().map(|c| c.seq).collect();

    // Filter sealed chunks down to those overlapping the range, then keep only
    // the finest resolution where multiple chunks cover the same span.
    let mut overlapping: Vec<ChunkRef> = chunks
        .into_iter()
        .filter(|c| c.overlaps(from, to))
        .collect();
    overlapping.sort_by(|a, b| {
        a.start_ts
            .cmp(&b.start_ts)
            .then(a.resolution_secs.cmp(&b.resolution_secs))
    });
    let mut chosen: Vec<ChunkRef> = Vec::with_capacity(overlapping.len());
    'outer: for c in overlapping {
        for existing in &chosen {
            if existing.start_ts <= c.start_ts
                && existing.end_ts >= c.end_ts
                && existing.resolution_secs < c.resolution_secs
            {
                continue 'outer;
            }
        }
        chosen.push(c);
    }

    let mut out: Vec<Sample> = Vec::new();
    for cr in chosen {
        let bytes = fs::read(&cr.path)?;
        let decoded = decode_chunk(&bytes)?;
        for (ts, slot) in decoded {
            if ts >= from && ts < to {
                out.push(Sample {
                    timestamp_secs: ts,
                    slot,
                });
            }
        }
    }

    // Include the active (unsealed) chunk by replaying any WAL whose seq has
    // no matching sealed chunk yet. Without this, a host with a long
    // chunk_window (default 1 h) appears to have no data until the first
    // window boundary - even though the WAL has been accumulating samples
    // since the first probe period.
    for wal_path in list_live_wals(host_dir, &chunk_seqs)? {
        // A corrupt or partially-written WAL shouldn't fail the whole query;
        // the operator can still see all sealed chunks.
        let Ok(records) = wal::replay(&wal_path) else {
            continue;
        };
        for (ts, slot) in records {
            if ts >= from && ts < to {
                out.push(Sample {
                    timestamp_secs: ts,
                    slot,
                });
            }
        }
    }

    out.sort_by_key(|s| s.timestamp_secs);
    Ok(out)
}

/// WAL files for chunks not yet sealed. A `<seq>.wal` whose seq already
/// appears in `chunks/` is an orphan from a crashed seal (the recovery path
/// in [`HostWriter::open`] cleans these up next time the host is opened) -
/// skip it here to avoid double-counting samples.
fn list_live_wals(host_dir: &Path, chunk_seqs: &HashSet<u64>) -> Result<Vec<PathBuf>, HzcError> {
    let dir = host_dir.join(WAL_DIR);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("wal") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(seq) = stem.parse::<u64>() else {
            continue;
        };
        if !chunk_seqs.contains(&seq) {
            out.push(path);
        }
    }
    Ok(out)
}

/// Convenience: dump every sample in `host_dir` regardless of range. Mostly
/// useful for the compactor + tests.
pub fn read_all(host_dir: &Path) -> Result<Vec<(i64, Slot)>, HzcError> {
    let chunks = list_chunks(host_dir)?;
    let mut out: Vec<(i64, Slot)> = Vec::new();
    for cr in chunks {
        let bytes = fs::read(&cr.path)?;
        out.extend(decode_chunk(&bytes)?);
    }
    out.sort_by_key(|(ts, _)| *ts);
    Ok(out)
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
    fn empty_directory_returns_empty() {
        let dir = TempDir::new().unwrap();
        let res = read_range_in_dir(dir.path(), 0, 1_000_000).unwrap();
        assert!(res.is_empty());
    }

    #[test]
    fn round_trip_through_writer() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let w = HostWriter::open(dir.path(), uuid, 30, 60).unwrap();
        for i in 0..120 {
            w.write_sample(i, slot(20.0 + (i as f32 / 30.0))).unwrap();
        }
        w.flush().unwrap();

        let samples = read_range(dir.path(), uuid, 0, 120).unwrap();
        assert_eq!(samples.len(), 120);
        for (i, s) in samples.iter().enumerate() {
            assert_eq!(s.timestamp_secs, i as i64);
        }
    }

    #[test]
    fn reads_live_wal_before_first_seal() {
        // A long chunk window with a probe interval well below it should
        // still return data, because the WAL has the samples even though no
        // chunk has been sealed yet.
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        // 5 s interval, 3600 s chunk window. Write 12 samples (1 min worth).
        let w = HostWriter::open(dir.path(), uuid, 5, 3600).unwrap();
        for i in 0..12 {
            w.write_sample(i * 5, slot(20.0 + i as f32)).unwrap();
        }
        // Deliberately do NOT call flush() so the chunk stays open.
        let samples = read_range(dir.path(), uuid, 0, 60).unwrap();
        assert_eq!(samples.len(), 12);
        assert_eq!(samples[0].timestamp_secs, 0);
        assert_eq!(samples[11].timestamp_secs, 55);
    }

    #[test]
    fn range_filtering_is_inclusive_exclusive() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let w = HostWriter::open(dir.path(), uuid, 30, 60).unwrap();
        for i in 0..120 {
            w.write_sample(i, slot(21.0)).unwrap();
        }
        w.flush().unwrap();
        // Query [50, 70) - should get exactly 20 samples.
        let s = read_range(dir.path(), uuid, 50, 70).unwrap();
        assert_eq!(s.len(), 20);
        assert_eq!(s.first().unwrap().timestamp_secs, 50);
        assert_eq!(s.last().unwrap().timestamp_secs, 69);
    }
}
