//! Write-ahead log for in-progress chunk samples.
//!
//! Each open chunk has its own WAL file `wal/<seq>.wal`. Samples are appended
//! length-prefixed; on chunk seal the WAL file is deleted. Crash recovery:
//! at startup, scan `wal/` - any `<seq>.wal` whose corresponding chunk file
//! `chunks/<seq>_*.hzc.zst` is missing replays into the open buffer; any
//! whose chunk already exists is an orphan from a crash after seal+before
//! delete, and gets deleted.
//!
//! Record format (40 bytes, fixed-size since `Slot` is fixed):
//!
//! ```text
//!  0..4   record length u32 LE (= 36)
//!  4..12  timestamp_secs i64 LE
//! 12..16  min   f32 LE
//! 16..20  p2_5  f32 LE
//! 20..24  p25   f32 LE
//! 24..28  median f32 LE
//! 28..32  p75   f32 LE
//! 32..36  p97_5 f32 LE
//! 36..40  loss_pct f32 LE
//! ```

use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

use crate::slot::Slot;

pub const RECORD_PAYLOAD: usize = 36; // 8 (ts) + 7*4 (fields)
pub const RECORD_TOTAL: usize = 4 + RECORD_PAYLOAD;

#[derive(Debug, thiserror::Error)]
pub enum WalError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Outcome of replaying a WAL file.
///
/// `records` holds every well-formed sample up to the first framing failure.
/// `valid_bytes` is the byte offset past the last well-formed record - anything
/// beyond it is either a benign trailing partial write or active corruption,
/// and the caller is expected to truncate the file to `valid_bytes` before
/// resuming appends.
#[derive(Debug, Clone)]
pub struct ReplayOutcome {
    pub records: Vec<(i64, Slot)>,
    pub valid_bytes: u64,
}

/// Append a single sample to an open WAL file. Caller is responsible for
/// fsync if they need crash-safety stronger than "lose the last sample".
pub fn append(wal: &mut File, ts: i64, slot: &Slot) -> Result<(), WalError> {
    let mut buf = [0u8; RECORD_TOTAL];
    buf[0..4].copy_from_slice(&(RECORD_PAYLOAD as u32).to_le_bytes());
    buf[4..12].copy_from_slice(&ts.to_le_bytes());
    let fields = slot.fields();
    for (i, v) in fields.iter().enumerate() {
        let off = 12 + i * 4;
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    wal.write_all(&buf)?;
    Ok(())
}

/// Force durability of all appended records.
pub fn sync(wal: &mut File) -> Result<(), WalError> {
    wal.sync_data()?;
    Ok(())
}

/// Read every well-formed record from a WAL file.
///
/// A trailing partial record (e.g. from a crash mid-write) is silently
/// ignored - the next caller will just rewrite the lost sample on the next
/// probe period. A malformed length header in the interior is treated the
/// same way: replay stops and the returned `valid_bytes` lets the caller
/// truncate the corrupt tail so a future append can resume cleanly. Losing
/// a probe loop forever to one bad byte-range is worse than dropping the
/// unreadable suffix.
pub fn replay(path: &Path) -> Result<ReplayOutcome, WalError> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len() as usize;
    let mut out = Vec::with_capacity(file_len / RECORD_TOTAL);
    let mut hdr = [0u8; 4];
    let mut payload = [0u8; RECORD_PAYLOAD];
    let mut pos: usize = 0;
    while pos + RECORD_TOTAL <= file_len {
        file.seek(SeekFrom::Start(pos as u64))?;
        if file.read_exact(&mut hdr).is_err() {
            break;
        }
        let len = u32::from_le_bytes(hdr);
        if len as usize != RECORD_PAYLOAD {
            tracing::warn!(
                wal_path = %path.display(),
                offset = pos,
                bad_length = len,
                "wal corruption detected; stopping replay at first malformed record"
            );
            break;
        }
        if file.read_exact(&mut payload).is_err() {
            // Partial trailing record - stop, no error.
            break;
        }
        let ts = i64::from_le_bytes(payload[0..8].try_into().unwrap());
        let mut fields = [0f32; 7];
        for (i, slot_field) in fields.iter_mut().enumerate() {
            let off = 8 + i * 4;
            *slot_field = f32::from_le_bytes(payload[off..off + 4].try_into().unwrap());
        }
        out.push((ts, Slot::from_fields(fields)));
        pos += RECORD_TOTAL;
    }
    Ok(ReplayOutcome {
        records: out,
        valid_bytes: pos as u64,
    })
}

/// Open (or create) a WAL file for append.
pub fn open_for_append(path: &Path) -> Result<File, WalError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let f = OpenOptions::new().create(true).append(true).open(path)?;
    Ok(f)
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
    fn append_and_replay() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.wal");
        let mut f = open_for_append(&path).unwrap();
        append(&mut f, 1_700_000_000, &slot(21.0)).unwrap();
        append(&mut f, 1_700_000_030, &slot(22.0)).unwrap();
        append(&mut f, 1_700_000_060, &Slot::NAN).unwrap();
        drop(f);

        let outcome = replay(&path).unwrap();
        assert_eq!(outcome.records.len(), 3);
        assert_eq!(outcome.valid_bytes, 3 * RECORD_TOTAL as u64);
        assert_eq!(outcome.records[0].0, 1_700_000_000);
        assert_eq!(outcome.records[1].0, 1_700_000_030);
        assert!(outcome.records[2].1.is_nan());
        // Round-trip exact f32 bits
        assert_eq!(outcome.records[0].1.median.to_bits(), 21.0_f32.to_bits());
    }

    #[test]
    fn partial_trailing_record_ignored() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.wal");
        {
            let mut f = open_for_append(&path).unwrap();
            append(&mut f, 1_700_000_000, &slot(21.0)).unwrap();
            // Append a partial record manually (only header, no payload).
            f.write_all(&(RECORD_PAYLOAD as u32).to_le_bytes()).unwrap();
        }
        let outcome = replay(&path).unwrap();
        assert_eq!(outcome.records.len(), 1);
        // The 4-byte partial header is shorter than RECORD_TOTAL, so the
        // outer `pos + RECORD_TOTAL <= file_len` check exits the loop before
        // we'd even try to parse it - valid_bytes stops at the last full
        // record boundary.
        assert_eq!(outcome.valid_bytes, RECORD_TOTAL as u64);
        assert_eq!(outcome.records[0].0, 1_700_000_000);
    }

    #[test]
    fn replay_stops_at_malformed_record() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.wal");
        {
            let mut f = open_for_append(&path).unwrap();
            append(&mut f, 1_700_000_000, &slot(21.0)).unwrap();
            append(&mut f, 1_700_000_030, &slot(22.0)).unwrap();
            // 40 bytes of zeros - the length header reads as 0, which is the
            // exact prod symptom ("malformed WAL record (length 0)").
            f.write_all(&[0u8; RECORD_TOTAL]).unwrap();
        }
        let outcome = replay(&path).unwrap();
        assert_eq!(outcome.records.len(), 2);
        assert_eq!(outcome.valid_bytes, 2 * RECORD_TOTAL as u64);
        assert_eq!(outcome.records[0].0, 1_700_000_000);
        assert_eq!(outcome.records[1].0, 1_700_000_030);
    }

    #[test]
    fn replay_zero_length_at_start() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.wal");
        {
            let mut f = open_for_append(&path).unwrap();
            f.write_all(&[0u8; RECORD_TOTAL]).unwrap();
        }
        let outcome = replay(&path).unwrap();
        assert!(outcome.records.is_empty());
        assert_eq!(outcome.valid_bytes, 0);
    }
}
