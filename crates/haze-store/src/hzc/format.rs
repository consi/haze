//! Chunk filename scheme - the filesystem is the index.
//!
//! Filenames encode everything a reader needs to range-filter without opening
//! the file: monotonic sequence number, resolution (0 = raw), and the
//! `[start, end)` window in epoch seconds. A `readdir` of a host's
//! `chunks/` directory plus `parse_chunk_filename` on each entry is enough to
//! answer "which chunks overlap `[from, to)` at resolution `r`?"
//!
//! Example:
//! ```text
//!   000123_r0_1715846400_1715850000.hzc.zst
//!   000124_r0_1715850000_1715853600.hzc.zst
//!   000200_r300_1715420800_1715593600.hzc.zst   ← 5-minute-aggregated
//! ```
//!
//! Underscore-separated so the names are shell-friendly. The `.hzc.zst`
//! extension makes the format obvious to anyone poking the directory with
//! `file(1)`.

use std::path::{Path, PathBuf};

pub const CHUNK_EXTENSION: &str = ".hzc.zst";

#[derive(Debug, thiserror::Error)]
pub enum FilenameError {
    #[error("not a chunk file ({0:?})")]
    NotAChunk(String),
    #[error("malformed chunk filename ({0:?})")]
    Malformed(String),
}

/// All of the metadata about a chunk that's encoded into its filename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkRef {
    pub seq: u64,
    /// Sample resolution in seconds. `0` = raw (whatever the host's probe
    /// interval is). Non-zero values are aggregated tiers.
    pub resolution_secs: u32,
    pub start_ts: i64,
    /// Exclusive upper bound. `end_ts > start_ts`.
    pub end_ts: i64,
    pub path: PathBuf,
}

impl ChunkRef {
    /// Does this chunk's window overlap the half-open range `[from, to)`?
    pub fn overlaps(&self, from: i64, to: i64) -> bool {
        self.end_ts > from && self.start_ts < to
    }
}

/// Construct the conventional filename for a chunk. Pure function - no I/O.
pub fn chunk_filename(seq: u64, resolution_secs: u32, start_ts: i64, end_ts: i64) -> String {
    format!("{seq:06}_r{resolution_secs}_{start_ts}_{end_ts}{CHUNK_EXTENSION}")
}

/// Parse a chunk filename produced by `chunk_filename`. Returns a `ChunkRef`
/// without ever opening the file. The `path` is the input path verbatim.
pub fn parse_chunk_filename(path: &Path) -> Result<ChunkRef, FilenameError> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| FilenameError::Malformed(path.display().to_string()))?;

    let stem = name
        .strip_suffix(CHUNK_EXTENSION)
        .ok_or_else(|| FilenameError::NotAChunk(name.to_owned()))?;

    let mut parts = stem.split('_');
    let seq_s = parts
        .next()
        .ok_or_else(|| FilenameError::Malformed(name.to_owned()))?;
    let res_s = parts
        .next()
        .ok_or_else(|| FilenameError::Malformed(name.to_owned()))?;
    let start_s = parts
        .next()
        .ok_or_else(|| FilenameError::Malformed(name.to_owned()))?;
    let end_s = parts
        .next()
        .ok_or_else(|| FilenameError::Malformed(name.to_owned()))?;
    if parts.next().is_some() {
        return Err(FilenameError::Malformed(name.to_owned()));
    }

    let seq: u64 = seq_s
        .parse()
        .map_err(|_| FilenameError::Malformed(name.to_owned()))?;
    let resolution_secs: u32 = res_s
        .strip_prefix('r')
        .ok_or_else(|| FilenameError::Malformed(name.to_owned()))?
        .parse()
        .map_err(|_| FilenameError::Malformed(name.to_owned()))?;
    let start_ts: i64 = start_s
        .parse()
        .map_err(|_| FilenameError::Malformed(name.to_owned()))?;
    let end_ts: i64 = end_s
        .parse()
        .map_err(|_| FilenameError::Malformed(name.to_owned()))?;
    if end_ts <= start_ts {
        return Err(FilenameError::Malformed(name.to_owned()));
    }
    Ok(ChunkRef {
        seq,
        resolution_secs,
        start_ts,
        end_ts,
        path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let name = chunk_filename(123, 0, 1_715_846_400, 1_715_850_000);
        assert_eq!(name, "000123_r0_1715846400_1715850000.hzc.zst");
        let parsed = parse_chunk_filename(Path::new(&name)).unwrap();
        assert_eq!(parsed.seq, 123);
        assert_eq!(parsed.resolution_secs, 0);
        assert_eq!(parsed.start_ts, 1_715_846_400);
        assert_eq!(parsed.end_ts, 1_715_850_000);
    }

    #[test]
    fn aggregated_resolution() {
        let name = chunk_filename(200, 300, 1_715_420_800, 1_715_593_600);
        assert_eq!(name, "000200_r300_1715420800_1715593600.hzc.zst");
        let parsed = parse_chunk_filename(Path::new(&name)).unwrap();
        assert_eq!(parsed.resolution_secs, 300);
    }

    #[test]
    fn rejects_non_chunk_files() {
        assert!(matches!(
            parse_chunk_filename(Path::new("meta.json")),
            Err(FilenameError::NotAChunk(_))
        ));
        assert!(matches!(
            parse_chunk_filename(Path::new("lock")),
            Err(FilenameError::NotAChunk(_))
        ));
    }

    #[test]
    fn rejects_malformed() {
        for n in [
            "000123_0_1715846400_1715850000.hzc.zst", // missing 'r'
            "abc_r0_1715846400_1715850000.hzc.zst",   // seq not numeric
            "000123_r0_1715846400.hzc.zst",           // missing end
            "000123_r0_1715846400_1715850000_extra.hzc.zst", // extra field
            "000123_r0_1715850000_1715846400.hzc.zst", // end <= start
        ] {
            assert!(
                matches!(
                    parse_chunk_filename(Path::new(n)),
                    Err(FilenameError::Malformed(_))
                ),
                "name {n:?}"
            );
        }
    }

    #[test]
    fn overlap_check() {
        let c = ChunkRef {
            seq: 1,
            resolution_secs: 0,
            start_ts: 100,
            end_ts: 200,
            path: PathBuf::from("x"),
        };
        assert!(c.overlaps(150, 250));
        assert!(c.overlaps(50, 150));
        assert!(c.overlaps(50, 300));
        assert!(c.overlaps(120, 180));
        assert!(!c.overlaps(0, 100)); // half-open: ends at start
        assert!(!c.overlaps(200, 300));
    }
}
