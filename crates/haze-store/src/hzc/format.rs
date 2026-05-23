//! Chunk filename scheme - the filesystem is the index.
//!
//! Filenames encode everything a reader needs to range-filter without opening
//! the file: monotonic sequence number, resolution (0 = raw), generation
//! (0 = per-window raw, 1 = daily bundle, etc.), and the `[start, end)`
//! window in epoch seconds. A `readdir` of a host's `chunks/` directory plus
//! `parse_chunk_filename` on each entry is enough to answer "which chunks
//! overlap `[from, to)` at resolution `r` / generation `g`?"
//!
//! Example:
//! ```text
//!   000123_r0_g0_1715846400_1715850000.hzc.zst    ← per-window raw chunk
//!   000124_r0_g0_1715850000_1715853600.hzc.zst
//!   000300_r0_g1_1715817600_1715904000.hzc.zst    ← one-day bundle (g1)
//!   000200_r300_g0_1715420800_1715593600.hzc.zst  ← 5-minute-aggregated
//! ```
//!
//! Legacy four-segment names without the `_g_` segment (written by haze
//! versions before the daily-rollup feature shipped) are still accepted by
//! the parser as `generation = 0`. The migration pass in the compactor
//! renames them to the canonical five-segment form on its first sweep.
//! Underscore-separated so the names are shell-friendly. The `.hzc.zst`
//! extension makes the format obvious to anyone poking the directory with
//! `file(1)`.

use std::hash::Hasher;
use std::path::{Path, PathBuf};

pub const CHUNK_EXTENSION: &str = ".hzc.zst";

/// Top bit reserved for deterministic bundle sequence numbers. G0 seqs come
/// from the writer's `Meta::next_seq` and stay well below this, so the two
/// namespaces never collide.
const BUNDLE_SEQ_FLAG: u64 = 1u64 << 63;

/// Deterministic sequence number for a higher-generation bundle.
///
/// Derived from `(generation, resolution_secs, start_ts)`. Two crash-and-
/// retry passes for the same logical bundle produce the same seq, so the
/// second write hits the same target filename and overwrites cleanly via
/// tmp+rename rather than creating a duplicate.
pub fn bundle_seq(generation: u8, resolution_secs: u32, start_ts: i64) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write_u8(generation);
    h.write_u32(resolution_secs);
    h.write_i64(start_ts);
    (h.finish() & !BUNDLE_SEQ_FLAG) | BUNDLE_SEQ_FLAG
}

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
    /// Compaction generation. `0` = per-window chunk emitted by the live
    /// writer. `1` = daily bundle produced by the rollup pass. Higher
    /// generations cover larger spans and supersede lower ones for the same
    /// `[start, end)` range.
    pub generation: u8,
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
pub fn chunk_filename(
    seq: u64,
    resolution_secs: u32,
    generation: u8,
    start_ts: i64,
    end_ts: i64,
) -> String {
    format!("{seq:06}_r{resolution_secs}_g{generation}_{start_ts}_{end_ts}{CHUNK_EXTENSION}")
}

/// Parse a chunk filename produced by `chunk_filename`. Returns a `ChunkRef`
/// without ever opening the file. The `path` is the input path verbatim.
///
/// Accepts both the canonical 5-segment grammar and the legacy 4-segment
/// grammar (which is treated as `generation = 0`); see the module docstring.
pub fn parse_chunk_filename(path: &Path) -> Result<ChunkRef, FilenameError> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| FilenameError::Malformed(path.display().to_string()))?;

    let stem = name
        .strip_suffix(CHUNK_EXTENSION)
        .ok_or_else(|| FilenameError::NotAChunk(name.to_owned()))?;

    let parts: Vec<&str> = stem.split('_').collect();
    let (seq_s, res_s, gen_opt, start_s, end_s) = match parts.as_slice() {
        [seq, res, g, start, end] => (*seq, *res, Some(*g), *start, *end),
        [seq, res, start, end] => (*seq, *res, None, *start, *end),
        _ => return Err(FilenameError::Malformed(name.to_owned())),
    };

    let seq: u64 = seq_s
        .parse()
        .map_err(|_| FilenameError::Malformed(name.to_owned()))?;
    let resolution_secs: u32 = res_s
        .strip_prefix('r')
        .ok_or_else(|| FilenameError::Malformed(name.to_owned()))?
        .parse()
        .map_err(|_| FilenameError::Malformed(name.to_owned()))?;
    let generation: u8 = match gen_opt {
        None => 0,
        Some(g) => g
            .strip_prefix('g')
            .ok_or_else(|| FilenameError::Malformed(name.to_owned()))?
            .parse()
            .map_err(|_| FilenameError::Malformed(name.to_owned()))?,
    };
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
        generation,
        start_ts,
        end_ts,
        path: path.to_path_buf(),
    })
}

/// True when `name` is a chunk filename in the legacy 4-segment grammar
/// (no `_g_` segment). Used by the migration pass to decide whether the
/// file needs renaming.
pub fn is_legacy_chunk_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(CHUNK_EXTENSION) else {
        return false;
    };
    let parts: Vec<&str> = stem.split('_').collect();
    if parts.len() != 4 {
        return false;
    }
    parts[0].parse::<u64>().is_ok()
        && parts[1].starts_with('r')
        && parts[1][1..].parse::<u32>().is_ok()
        && parts[2].parse::<i64>().is_ok()
        && parts[3].parse::<i64>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_g0() {
        let name = chunk_filename(123, 0, 0, 1_715_846_400, 1_715_850_000);
        assert_eq!(name, "000123_r0_g0_1715846400_1715850000.hzc.zst");
        let parsed = parse_chunk_filename(Path::new(&name)).unwrap();
        assert_eq!(parsed.seq, 123);
        assert_eq!(parsed.resolution_secs, 0);
        assert_eq!(parsed.generation, 0);
        assert_eq!(parsed.start_ts, 1_715_846_400);
        assert_eq!(parsed.end_ts, 1_715_850_000);
    }

    #[test]
    fn round_trip_g1_daily_bundle() {
        let name = chunk_filename(300, 0, 1, 1_715_817_600, 1_715_904_000);
        assert_eq!(name, "000300_r0_g1_1715817600_1715904000.hzc.zst");
        let parsed = parse_chunk_filename(Path::new(&name)).unwrap();
        assert_eq!(parsed.generation, 1);
    }

    #[test]
    fn aggregated_resolution() {
        let name = chunk_filename(200, 300, 0, 1_715_420_800, 1_715_593_600);
        assert_eq!(name, "000200_r300_g0_1715420800_1715593600.hzc.zst");
        let parsed = parse_chunk_filename(Path::new(&name)).unwrap();
        assert_eq!(parsed.resolution_secs, 300);
        assert_eq!(parsed.generation, 0);
    }

    #[test]
    fn legacy_four_segment_parses_as_g0() {
        // Files written before the daily-rollup feature shipped use the
        // older 4-segment grammar. The parser must accept them so existing
        // deployments keep working until the migration pass renames them.
        let parsed =
            parse_chunk_filename(Path::new("000123_r0_1715846400_1715850000.hzc.zst")).unwrap();
        assert_eq!(parsed.seq, 123);
        assert_eq!(parsed.resolution_secs, 0);
        assert_eq!(parsed.generation, 0);
        assert_eq!(parsed.start_ts, 1_715_846_400);
        assert_eq!(parsed.end_ts, 1_715_850_000);
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
            "000123_0_g0_1715846400_1715850000.hzc.zst", // missing 'r'
            "000123_r0_x0_1715846400_1715850000.hzc.zst", // bad gen prefix
            "000123_r0_g_1715846400_1715850000.hzc.zst", // 'g' without number
            "abc_r0_g0_1715846400_1715850000.hzc.zst",   // seq not numeric
            "000123_r0_g0_1715846400.hzc.zst",           // missing end
            "000123_r0_g0_1715846400_1715850000_extra.hzc.zst", // extra field
            "000123_r0_g0_1715850000_1715846400.hzc.zst", // end <= start
            "000123_r0_1715850000_1715846400.hzc.zst",   // legacy with end <= start
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
    fn detects_legacy_names() {
        assert!(is_legacy_chunk_name(
            "000123_r0_1715846400_1715850000.hzc.zst"
        ));
        assert!(is_legacy_chunk_name(
            "000200_r300_1715420800_1715593600.hzc.zst"
        ));
        assert!(!is_legacy_chunk_name(
            "000123_r0_g0_1715846400_1715850000.hzc.zst"
        ));
        assert!(!is_legacy_chunk_name(
            "000300_r0_g1_1715817600_1715904000.hzc.zst"
        ));
        assert!(!is_legacy_chunk_name("meta.json"));
        assert!(!is_legacy_chunk_name(
            "abc_r0_1715846400_1715850000.hzc.zst"
        ));
    }

    #[test]
    fn overlap_check() {
        let c = ChunkRef {
            seq: 1,
            resolution_secs: 0,
            generation: 0,
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
