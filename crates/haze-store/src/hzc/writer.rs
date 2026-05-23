//! Per-host writer.
//!
//! Owns a host's `data/hzc/<shard>/<uuid>/` directory: holds an fcntl
//! exclusive lock on `lock`, appends incoming samples to a WAL file, and
//! seals the open chunk into a `chunks/<seq>_r0_<start>_<end>.hzc.zst` file
//! on bucket boundaries. Crash recovery happens in `open()`: any orphan
//! WAL (chunk already sealed) is deleted, any live WAL is replayed into the
//! in-memory open chunk buffer.

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use fs2::FileExt;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    chunk::{ChunkEncodeError, ZSTD_LEVEL_G0, encode_chunk},
    format::{chunk_filename, parse_chunk_filename},
    wal,
};
use crate::slot::Slot;

pub const META_FILENAME: &str = "meta.json";
pub const LOCK_FILENAME: &str = "lock";
pub const WAL_DIR: &str = "wal";
pub const CHUNKS_DIR: &str = "chunks";

#[derive(Debug, thiserror::Error)]
pub enum HzcError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("another process holds the writer lock on {0}")]
    LockHeld(PathBuf),
    #[error(transparent)]
    Wal(#[from] wal::WalError),
    #[error(transparent)]
    Encode(#[from] ChunkEncodeError),
    #[error(transparent)]
    Decode(#[from] super::chunk::ChunkDecodeError),
    #[error("malformed chunk filename: {0}")]
    BadFilename(String),
    #[error("malformed meta.json: {0}")]
    BadMeta(String),
}

/// Per-tier rule in a host's retention policy.
///
/// Data older than `max_age_secs` is compacted to `resolution_secs` (or
/// deleted if `resolution_secs == 0` and this is the trailing tier -
/// interpretation lives in the compactor).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct RetentionTier {
    pub max_age_secs: i64,
    /// Target sample resolution in seconds. `0` means raw.
    pub resolution_secs: u32,
}

/// Sensible defaults applied to every host that hasn't been given an override.
/// Tiers must be in ascending `max_age_secs` order and have monotonically
/// non-decreasing `resolution_secs`.
pub fn default_retention_tiers() -> Vec<RetentionTier> {
    vec![
        RetentionTier {
            max_age_secs: 7 * 86_400,
            resolution_secs: 0,
        }, // 7 d raw
        RetentionTier {
            max_age_secs: 30 * 86_400,
            resolution_secs: 300,
        }, // 30 d @ 5 min
        RetentionTier {
            max_age_secs: 180 * 86_400,
            resolution_secs: 1_800,
        }, // 180 d @ 30 min
        RetentionTier {
            max_age_secs: 365 * 86_400,
            resolution_secs: 7_200,
        }, // 1 y @ 2 h
        RetentionTier {
            max_age_secs: 5 * 365 * 86_400,
            resolution_secs: 86_400,
        }, // 5 y @ 1 d
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub host_uuid: Uuid,
    pub created_at: i64,
    pub base_interval_secs: u32,
    pub chunk_window_secs: u32,
    pub next_seq: u64,
    pub retention_tiers: Vec<RetentionTier>,
}

impl Meta {
    fn fresh(host_uuid: Uuid, base_interval_secs: u32, chunk_window_secs: u32) -> Self {
        Self {
            host_uuid,
            created_at: chrono::Utc::now().timestamp(),
            base_interval_secs,
            chunk_window_secs,
            next_seq: 1,
            retention_tiers: default_retention_tiers(),
        }
    }

    fn load(path: &Path) -> Result<Self, HzcError> {
        let bytes = fs::read(path)?;
        let m: Self =
            serde_json::from_slice(&bytes).map_err(|e| HzcError::BadMeta(e.to_string()))?;
        Ok(m)
    }

    fn save_atomic(&self, dir: &Path) -> Result<(), HzcError> {
        let tmp = dir.join(format!("{META_FILENAME}.tmp"));
        let final_path = dir.join(META_FILENAME);
        let s = serde_json::to_vec_pretty(self).expect("meta serialise");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&s)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &final_path)?;
        Ok(())
    }
}

struct OpenChunk {
    seq: u64,
    start_ts: i64,
    end_ts: i64,
    samples: Vec<(i64, Slot)>,
    wal: File,
    wal_path: PathBuf,
}

pub struct HostWriter {
    host_dir: PathBuf,
    meta: Mutex<Meta>,
    open: Mutex<Option<OpenChunk>>,
    #[allow(dead_code)]
    lock: File,
}

impl HostWriter {
    /// Open (creating if necessary) the writer for `host_uuid` under `data_dir`.
    /// Holds an exclusive lock for the lifetime of the returned value.
    pub fn open(
        data_dir: &Path,
        host_uuid: Uuid,
        base_interval_secs: u32,
        chunk_window_secs: u32,
    ) -> Result<Self, HzcError> {
        let host_dir = host_directory(data_dir, host_uuid);
        fs::create_dir_all(host_dir.join(CHUNKS_DIR))?;
        fs::create_dir_all(host_dir.join(WAL_DIR))?;

        let lock_path = host_dir.join(LOCK_FILENAME);
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        lock.try_lock_exclusive()
            .map_err(|_| HzcError::LockHeld(lock_path.clone()))?;

        let meta_path = host_dir.join(META_FILENAME);
        let meta = if meta_path.exists() {
            Meta::load(&meta_path)?
        } else {
            let m = Meta::fresh(host_uuid, base_interval_secs, chunk_window_secs);
            m.save_atomic(&host_dir)?;
            m
        };

        // Crash recovery: scan wal/, decide per file what to do.
        let mut open: Option<OpenChunk> = None;
        let wal_dir = host_dir.join(WAL_DIR);
        let mut wal_entries: Vec<(u64, PathBuf)> = Vec::new();
        if let Ok(rd) = fs::read_dir(&wal_dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("wal") {
                    continue;
                }
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .ok_or_else(|| HzcError::BadFilename(format!("{}", path.display())))?;
                let seq: u64 = stem
                    .parse()
                    .map_err(|_| HzcError::BadFilename(format!("{}", path.display())))?;
                wal_entries.push((seq, path));
            }
        }
        wal_entries.sort_by_key(|(s, _)| *s);

        for (seq, path) in &wal_entries {
            // Is there already a chunk for this seq? (Crash after seal but
            // before WAL delete.)
            let chunks_dir = host_dir.join(CHUNKS_DIR);
            let chunk_exists = chunks_for_seq(&chunks_dir, *seq)?.is_some();
            if chunk_exists {
                let _ = fs::remove_file(path);
                continue;
            }
            // Live WAL: replay into open chunk buffer (or seal earlier ones).
            // If replay found corruption past the last good record, truncate
            // the file to the recovered byte count so future appends don't
            // re-trip the same error every restart. Without this the probe
            // loop for this host would log "malformed WAL record" on every
            // open() forever.
            let outcome = wal::replay(path)?;
            let file_len = fs::metadata(path)?.len();
            if outcome.valid_bytes < file_len {
                let discarded = file_len - outcome.valid_bytes;
                tracing::warn!(
                    host_uuid = %meta.host_uuid,
                    seq = *seq,
                    wal_path = %path.display(),
                    discarded_bytes = discarded,
                    recovered_records = outcome.records.len(),
                    "wal corruption: truncating malformed tail and continuing"
                );
                let f = OpenOptions::new().write(true).open(path)?;
                f.set_len(outcome.valid_bytes)?;
                f.sync_all()?;
            }
            let records = outcome.records;
            if records.is_empty() {
                let _ = fs::remove_file(path);
                continue;
            }
            let first_ts = records[0].0;
            let (start, end) = chunk_window_bounds(first_ts, meta.chunk_window_secs);

            if open.is_some() {
                // We already have a live open chunk. Seal this earlier one
                // now - it represents an aborted seal.
                let earlier_wal_seq = *seq;
                seal_chunk_inline(
                    &host_dir,
                    earlier_wal_seq,
                    start,
                    end,
                    0, // raw resolution
                    0, // generation 0 - per-window chunk
                    ZSTD_LEVEL_G0,
                    &records,
                )?;
                let _ = fs::remove_file(path);
            } else {
                let wal_file = wal::open_for_append(path)?;
                open = Some(OpenChunk {
                    seq: *seq,
                    start_ts: start,
                    end_ts: end,
                    samples: records,
                    wal: wal_file,
                    wal_path: path.clone(),
                });
            }
        }

        Ok(Self {
            host_dir,
            meta: Mutex::new(meta),
            open: Mutex::new(open),
            lock,
        })
    }

    pub fn host_dir(&self) -> &Path {
        &self.host_dir
    }

    /// Append one probe-period sample. Sealing of the previous chunk happens
    /// automatically when `ts` falls in a new bucket.
    pub fn write_sample(&self, ts: i64, slot: Slot) -> Result<(), HzcError> {
        let chunk_window_secs = self.meta.lock().chunk_window_secs;
        let (target_start, target_end) = chunk_window_bounds(ts, chunk_window_secs);

        let mut open = self.open.lock();
        let needs_new = match &*open {
            None => true,
            Some(o) => o.start_ts != target_start,
        };
        if needs_new {
            if let Some(prev) = open.take() {
                self.seal_owned(prev)?;
            }
            let seq = {
                let mut m = self.meta.lock();
                let s = m.next_seq;
                m.next_seq += 1;
                m.save_atomic(&self.host_dir)?;
                s
            };
            let wal_path = self.host_dir.join(WAL_DIR).join(format!("{seq}.wal"));
            let wal_file = wal::open_for_append(&wal_path)?;
            *open = Some(OpenChunk {
                seq,
                start_ts: target_start,
                end_ts: target_end,
                samples: Vec::new(),
                wal: wal_file,
                wal_path,
            });
        }

        let oc = open.as_mut().expect("just initialised");
        wal::append(&mut oc.wal, ts, &slot)?;
        // No fsync on the hot path: writeback is left to the kernel. A power
        // loss drops at most the writeback window's worth of WAL records;
        // wal::replay already discards partial trailing records, so recovery
        // is unchanged. Hot-path fsync was previously the dominant cause of
        // tokio-worker stalls under load (parked in fdatasync), which
        // inflated reported probe latencies.
        oc.samples.push((ts, slot));
        Ok(())
    }

    /// Force-seal the current open chunk (if any). Called by the store on
    /// graceful shutdown or by the compactor before walking chunks.
    pub fn flush(&self) -> Result<(), HzcError> {
        let mut open = self.open.lock();
        if let Some(prev) = open.take() {
            self.seal_owned(prev)?;
        }
        Ok(())
    }

    fn seal_owned(&self, mut chunk: OpenChunk) -> Result<(), HzcError> {
        // Drop the WAL file handle so renames are unambiguous on Windows
        // (no-op on UNIX; cheap insurance).
        let _ = wal::sync(&mut chunk.wal);
        drop(chunk.wal);

        let sample_count = chunk.samples.len();
        seal_chunk_inline(
            &self.host_dir,
            chunk.seq,
            chunk.start_ts,
            chunk.end_ts,
            0, // raw resolution
            0, // generation 0 - per-window chunk
            ZSTD_LEVEL_G0,
            &chunk.samples,
        )?;
        let _ = fs::remove_file(&chunk.wal_path);
        let span_secs = chunk.end_ts.saturating_sub(chunk.start_ts);
        tracing::info!(
            host_uuid = %self.meta.lock().host_uuid,
            seq = chunk.seq,
            samples = sample_count,
            start_ts = chunk.start_ts,
            end_ts = chunk.end_ts,
            span_secs,
            "hzc wal sealed into chunk"
        );
        Ok(())
    }

    /// Current retention policy snapshot. Inspected by the compactor.
    pub fn retention_tiers(&self) -> Vec<RetentionTier> {
        self.meta.lock().retention_tiers.clone()
    }

    /// Replace the retention policy. The new policy is persisted to
    /// `meta.json` atomically and used by the next compactor pass.
    pub fn set_retention_tiers(&self, tiers: Vec<RetentionTier>) -> Result<(), HzcError> {
        let mut m = self.meta.lock();
        m.retention_tiers = tiers;
        m.save_atomic(&self.host_dir)?;
        Ok(())
    }
}

/// Compute the chunk window `[start, end)` containing timestamp `ts`.
pub fn chunk_window_bounds(ts: i64, window_secs: u32) -> (i64, i64) {
    let w = i64::from(window_secs);
    let start = ts.div_euclid(w) * w;
    (start, start + w)
}

/// Build the host's on-disk directory path. Sharded by the first two hex
/// chars of the UUID so any single shard directory stays small.
pub fn host_directory(data_dir: &Path, host_uuid: Uuid) -> PathBuf {
    let s = host_uuid.simple().to_string();
    let shard = &s[0..2];
    data_dir.join("hzc").join(shard).join(s)
}

fn chunks_for_seq(chunks_dir: &Path, seq: u64) -> Result<Option<PathBuf>, HzcError> {
    if !chunks_dir.exists() {
        return Ok(None);
    }
    for entry in fs::read_dir(chunks_dir)? {
        let entry = entry?;
        let path = entry.path();
        if let Ok(cr) = parse_chunk_filename(&path) {
            // A WAL at `<seq>.wal` was sealed by a `g0` chunk at the same
            // seq - that's the crash-recovery short-circuit. A bundle (g≥1)
            // happens to use seqs from the same numeric range but does NOT
            // represent the seal of any single WAL; ignore it here, otherwise
            // we'd delete the live writer's open WAL on restart.
            if cr.seq == seq && cr.generation == 0 {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}

/// Encode + atomically rename in a chunk file. Shared by the writer's
/// normal seal path, the crash-recovery path, and the compactor - including
/// the rollup passes that emit higher-generation bundles. `level` is the
/// zstd compression level; use `zstd_level_for_generation(generation)` for
/// the default per-generation policy.
#[allow(clippy::too_many_arguments)]
pub(super) fn seal_chunk_inline(
    host_dir: &Path,
    seq: u64,
    start_ts: i64,
    end_ts: i64,
    resolution_secs: u32,
    generation: u8,
    level: i32,
    samples: &[(i64, Slot)],
) -> Result<(), HzcError> {
    let bytes = encode_chunk(samples, level)?;
    let filename = chunk_filename(seq, resolution_secs, generation, start_ts, end_ts);
    let chunks_dir = host_dir.join(CHUNKS_DIR);
    fs::create_dir_all(&chunks_dir)?;
    let tmp = chunks_dir.join(format!("{filename}.tmp"));
    let final_path = chunks_dir.join(&filename);
    {
        let mut f = File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &final_path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn writes_and_seals_on_window_boundary() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let w = HostWriter::open(dir.path(), uuid, 30, 60).unwrap();
        // Window 0: [0, 60); window 1: [60, 120).
        w.write_sample(10, slot(21.0)).unwrap();
        w.write_sample(30, slot(22.0)).unwrap();
        // Crossing into next window seals chunk 1.
        w.write_sample(60, slot(23.0)).unwrap();
        w.flush().unwrap();

        // Two chunks should now exist.
        let chunks_dir = w.host_dir().join(CHUNKS_DIR);
        let entries: Vec<_> = fs::read_dir(&chunks_dir).unwrap().collect();
        assert_eq!(entries.len(), 2, "expected 2 chunks, got {}", entries.len());

        // No WAL files left.
        let wal_dir = w.host_dir().join(WAL_DIR);
        let wals: Vec<_> = fs::read_dir(&wal_dir).unwrap().collect();
        assert_eq!(wals.len(), 0);
    }

    #[test]
    fn lock_is_exclusive_across_writers() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        let _w1 = HostWriter::open(dir.path(), uuid, 30, 60).unwrap();
        let r = HostWriter::open(dir.path(), uuid, 30, 60);
        assert!(matches!(r, Err(HzcError::LockHeld(_))));
    }

    #[test]
    fn crash_recovery_replays_wal() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        {
            let w = HostWriter::open(dir.path(), uuid, 30, 60).unwrap();
            w.write_sample(10, slot(21.0)).unwrap();
            w.write_sample(20, slot(22.0)).unwrap();
            // Drop without flushing - simulates crash.
        }
        // Should have a WAL file, no chunks yet.
        let host_dir = host_directory(dir.path(), uuid);
        let chunks_count = fs::read_dir(host_dir.join(CHUNKS_DIR)).unwrap().count();
        assert_eq!(chunks_count, 0);
        let wals_count = fs::read_dir(host_dir.join(WAL_DIR)).unwrap().count();
        assert_eq!(wals_count, 1);

        // Reopen: WAL should replay into the open chunk, no chunk yet.
        let w = HostWriter::open(dir.path(), uuid, 30, 60).unwrap();
        // Adding a sample in the SAME window should not seal.
        w.write_sample(40, slot(23.0)).unwrap();
        // Adding a sample in the NEXT window should seal what we replayed +
        // the new one.
        w.write_sample(60, slot(24.0)).unwrap();
        w.flush().unwrap();

        let chunks_count = fs::read_dir(host_dir.join(CHUNKS_DIR)).unwrap().count();
        assert_eq!(chunks_count, 2);
    }

    #[test]
    fn crash_recovery_truncates_malformed_wal() {
        use std::io::Write;
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        // Phase 1: write two clean samples, then drop the writer without
        // sealing (simulates crash mid-window).
        {
            let w = HostWriter::open(dir.path(), uuid, 30, 60).unwrap();
            w.write_sample(10, slot(21.0)).unwrap();
            w.write_sample(20, slot(22.0)).unwrap();
        }
        let host_dir = host_directory(dir.path(), uuid);
        let wal_path = {
            let mut entries = fs::read_dir(host_dir.join(WAL_DIR))
                .unwrap()
                .collect::<Vec<_>>();
            assert_eq!(entries.len(), 1);
            entries.pop().unwrap().unwrap().path()
        };
        let pre_corruption_len = fs::metadata(&wal_path).unwrap().len();

        // Phase 2: simulate WAL corruption - write a record-sized run of
        // zeros (length header reads as 0, the exact prod symptom).
        {
            let mut f = OpenOptions::new().append(true).open(&wal_path).unwrap();
            f.write_all(&[0u8; 40]).unwrap();
            f.sync_all().unwrap();
        }
        assert_eq!(
            fs::metadata(&wal_path).unwrap().len(),
            pre_corruption_len + 40
        );

        // Phase 3: reopen the writer. Recovery must drop the corrupt tail,
        // shrink the WAL back to the clean size, and let new appends succeed.
        let w = HostWriter::open(dir.path(), uuid, 30, 60).unwrap();
        assert_eq!(fs::metadata(&wal_path).unwrap().len(), pre_corruption_len);

        w.write_sample(40, slot(23.0)).unwrap();
        w.write_sample(60, slot(24.0)).unwrap(); // seals window [0,60)
        w.flush().unwrap();

        // Across all sealed chunks we must see the two recovered samples
        // (10, 20), the one written into the recovered window (40), and the
        // one that crossed the boundary and triggered the seal (60).
        let chunks_dir = host_dir.join(CHUNKS_DIR);
        let mut all_ts: Vec<i64> = Vec::new();
        for entry in fs::read_dir(&chunks_dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("zst") {
                continue;
            }
            let bytes = fs::read(&path).unwrap();
            for (ts, _) in crate::hzc::chunk::decode_chunk(&bytes).unwrap() {
                all_ts.push(ts);
            }
        }
        all_ts.sort_unstable();
        assert_eq!(
            all_ts,
            vec![10, 20, 40, 60],
            "expected all four samples to survive recovery + new appends"
        );
    }

    #[test]
    fn meta_persists_next_seq() {
        let dir = TempDir::new().unwrap();
        let uuid = Uuid::new_v4();
        {
            let w = HostWriter::open(dir.path(), uuid, 30, 60).unwrap();
            w.write_sample(10, slot(21.0)).unwrap();
            w.write_sample(60, slot(22.0)).unwrap(); // seals 1, opens 2
            w.write_sample(120, slot(23.0)).unwrap(); // seals 2, opens 3
            w.flush().unwrap();
        }
        let host_dir = host_directory(dir.path(), uuid);
        let meta = Meta::load(&host_dir.join(META_FILENAME)).unwrap();
        // 3 chunks were opened (seqs 1, 2, 3) so next_seq is now 4.
        assert_eq!(meta.next_seq, 4);
    }
}
