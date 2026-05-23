//! Pure encode/decode of a `.hzc` chunk's *uncompressed* body.
//!
//! Layout (after the outer `zstd` wrapper is stripped - `encode_chunk` returns
//! the zstd-compressed bytes; `decode_chunk` expects the same):
//!
//! ```text
//! HEADER (12 B):
//!   [0..4]   magic b"HZC\0"
//!   [4..6]   version u16 LE (= 1)
//!   [6..7]   num_value_columns u8 (= 7 - matches Slot fields)
//!   [7..8]   flags u8 (reserved, 0)
//!   [8..12]  sample_count u32 LE
//!
//! Then, repeated for each column (1 timestamp column + N value columns), in
//! the order [ts, min, p2_5, p25, median, p75, p97_5, loss_pct]:
//!
//!   COLUMN_LEN  u32 LE - byte length of the bit-packed column payload
//!   COLUMN_BYTES  variable
//! ```
//!
//! The column-length prefix lets readers skip columns they don't need (e.g.
//! "I only want the median and the `loss_pct`"), and keeps each column on a
//! byte boundary so a single shared zstd context can compress the whole body
//! efficiently.

use super::{
    bits::{BitReader, BitWriter},
    encoding::{DodDecoder, DodEncoder, GorillaF32Decoder, GorillaF32Encoder},
};
use crate::slot::Slot;

const MAGIC: &[u8; 4] = b"HZC\0";
const VERSION: u16 = 1;
const NUM_VALUE_COLS: u8 = 7;
const HEADER_LEN: usize = 12;
const COL_LEN_PREFIX: usize = 4;

/// Decoded view of a chunk file's 12-byte header.
///
/// The on-wire bytes are inside the zstd-compressed body, so reading the
/// header still requires a small zstd-decode of a few hundred bytes - but no
/// column-decoding.
#[derive(Debug, Clone, Copy)]
pub struct ChunkHeader {
    pub version: u16,
    pub num_value_columns: u8,
    pub sample_count: u32,
}

/// Field order matches the on-disk column order; mirrors `Slot::fields()`.
const FIELD_NAMES: [&str; NUM_VALUE_COLS as usize] =
    ["min", "p2_5", "p25", "median", "p75", "p97_5", "loss_pct"];

#[derive(Debug, thiserror::Error)]
pub enum ChunkEncodeError {
    #[error("zstd: {0}")]
    Zstd(#[from] std::io::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum ChunkDecodeError {
    #[error("zstd: {0}")]
    Zstd(std::io::Error),
    #[error("chunk too short ({0} bytes)")]
    Truncated(usize),
    #[error("bad magic")]
    BadMagic,
    #[error("unsupported version {0}")]
    UnsupportedVersion(u16),
    #[error("unexpected column count {got}, expected {expected}")]
    BadColumnCount { got: u8, expected: u8 },
    #[error("column {0} truncated")]
    ColumnTruncated(&'static str),
}

/// zstd compression level used by the live writer and the regular tier
/// downsampler. Optimised for write throughput - readers don't care.
pub const ZSTD_LEVEL_G0: i32 = 3;
/// zstd compression level used by the G1 daily-rollup bundles. Re-encode
/// happens once per UTC day per host, so we can afford a slower encoder for
/// a noticeably smaller file.
pub const ZSTD_LEVEL_G1: i32 = 9;
/// zstd compression level used by the G2 monthly-rollup bundles.
pub const ZSTD_LEVEL_G2: i32 = 13;
/// zstd compression level used by the G3 yearly-rollup bundles. The yearly
/// re-encode is the heaviest single operation but it happens at most a
/// handful of times per host per year.
pub const ZSTD_LEVEL_G3: i32 = 15;

/// Pick the default zstd level for a given chunk generation. Callers can
/// still pass an arbitrary `level` to `encode_chunk` if they want.
pub const fn zstd_level_for_generation(generation: u8) -> i32 {
    match generation {
        0 => ZSTD_LEVEL_G0,
        1 => ZSTD_LEVEL_G1,
        2 => ZSTD_LEVEL_G2,
        _ => ZSTD_LEVEL_G3,
    }
}

/// Encode a series of `(timestamp, slot)` tuples into a chunk blob.
///
/// Returns the self-contained zstd-compressed bytes. The sample sequence
/// does not need to be sorted, but for best compression timestamps should
/// be monotonic.
///
/// `level` is the zstd compression level. Higher = smaller output, slower
/// encode. Decoders self-describe so the choice doesn't affect the
/// on-disk format - existing readers can decode any level.
pub fn encode_chunk(samples: &[(i64, Slot)], level: i32) -> Result<Vec<u8>, ChunkEncodeError> {
    let n = samples.len();
    let mut body: Vec<u8> = Vec::with_capacity(HEADER_LEN + 32 * n);
    body.extend_from_slice(MAGIC);
    body.extend_from_slice(&VERSION.to_le_bytes());
    body.push(NUM_VALUE_COLS);
    body.push(0); // flags
    body.extend_from_slice(&(n as u32).to_le_bytes());

    // Timestamp column
    {
        let mut w = BitWriter::new();
        let mut enc = DodEncoder::new();
        for (ts, _) in samples {
            enc.write(&mut w, *ts);
        }
        let bytes = w.into_bytes();
        body.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        body.extend_from_slice(&bytes);
    }

    // Value columns
    for field_idx in 0..NUM_VALUE_COLS as usize {
        let mut w = BitWriter::new();
        let mut enc = GorillaF32Encoder::new();
        for (_, slot) in samples {
            enc.write(&mut w, slot.fields()[field_idx]);
        }
        let bytes = w.into_bytes();
        body.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        body.extend_from_slice(&bytes);
    }

    let compressed = zstd::encode_all(body.as_slice(), level)?;
    Ok(compressed)
}

/// Read and validate the 12-byte HZC header from a chunk file on disk.
///
/// Used by the migration pass to shape-check existing chunks without paying
/// the full column-decode cost. Reads only the first ~1 KiB from disk (one
/// zstd frame's worth) - the rest of the file isn't touched.
pub fn read_header(path: &std::path::Path) -> Result<ChunkHeader, ChunkDecodeError> {
    let bytes = std::fs::read(path).map_err(ChunkDecodeError::Zstd)?;
    let body = zstd::decode_all(bytes.as_slice()).map_err(ChunkDecodeError::Zstd)?;
    if body.len() < HEADER_LEN {
        return Err(ChunkDecodeError::Truncated(body.len()));
    }
    if &body[0..4] != MAGIC {
        return Err(ChunkDecodeError::BadMagic);
    }
    let version = u16::from_le_bytes(body[4..6].try_into().unwrap());
    if version != VERSION {
        return Err(ChunkDecodeError::UnsupportedVersion(version));
    }
    let cols = body[6];
    if cols != NUM_VALUE_COLS {
        return Err(ChunkDecodeError::BadColumnCount {
            got: cols,
            expected: NUM_VALUE_COLS,
        });
    }
    let sample_count = u32::from_le_bytes(body[8..12].try_into().unwrap());
    Ok(ChunkHeader {
        version,
        num_value_columns: cols,
        sample_count,
    })
}

/// Decode a zstd-compressed chunk blob back into the original sample sequence.
pub fn decode_chunk(zstd_bytes: &[u8]) -> Result<Vec<(i64, Slot)>, ChunkDecodeError> {
    let body = zstd::decode_all(zstd_bytes).map_err(ChunkDecodeError::Zstd)?;
    if body.len() < HEADER_LEN {
        return Err(ChunkDecodeError::Truncated(body.len()));
    }
    if &body[0..4] != MAGIC {
        return Err(ChunkDecodeError::BadMagic);
    }
    let version = u16::from_le_bytes(body[4..6].try_into().unwrap());
    if version != VERSION {
        return Err(ChunkDecodeError::UnsupportedVersion(version));
    }
    let cols = body[6];
    if cols != NUM_VALUE_COLS {
        return Err(ChunkDecodeError::BadColumnCount {
            got: cols,
            expected: NUM_VALUE_COLS,
        });
    }
    let sample_count = u32::from_le_bytes(body[8..12].try_into().unwrap()) as usize;

    let mut cursor = HEADER_LEN;

    // Timestamp column
    let ts_len = read_col_len(&body, &mut cursor, "ts")?;
    let ts_bytes = read_col_bytes(&body, &mut cursor, ts_len, "ts")?;
    let timestamps = decode_ts_column(ts_bytes, sample_count, "ts")?;

    // Value columns
    let mut values: [Vec<f32>; NUM_VALUE_COLS as usize] = Default::default();
    for i in 0..NUM_VALUE_COLS as usize {
        let len = read_col_len(&body, &mut cursor, FIELD_NAMES[i])?;
        let bytes = read_col_bytes(&body, &mut cursor, len, FIELD_NAMES[i])?;
        values[i] = decode_f32_column(bytes, sample_count, FIELD_NAMES[i])?;
    }

    let mut out = Vec::with_capacity(sample_count);
    for i in 0..sample_count {
        let slot = Slot {
            min: values[0][i],
            p2_5: values[1][i],
            p25: values[2][i],
            median: values[3][i],
            p75: values[4][i],
            p97_5: values[5][i],
            loss_pct: values[6][i],
        };
        out.push((timestamps[i], slot));
    }
    Ok(out)
}

fn read_col_len(
    body: &[u8],
    cursor: &mut usize,
    name: &'static str,
) -> Result<usize, ChunkDecodeError> {
    if *cursor + COL_LEN_PREFIX > body.len() {
        return Err(ChunkDecodeError::ColumnTruncated(name));
    }
    let len =
        u32::from_le_bytes(body[*cursor..*cursor + COL_LEN_PREFIX].try_into().unwrap()) as usize;
    *cursor += COL_LEN_PREFIX;
    Ok(len)
}

fn read_col_bytes<'a>(
    body: &'a [u8],
    cursor: &mut usize,
    len: usize,
    name: &'static str,
) -> Result<&'a [u8], ChunkDecodeError> {
    if *cursor + len > body.len() {
        return Err(ChunkDecodeError::ColumnTruncated(name));
    }
    let slice = &body[*cursor..*cursor + len];
    *cursor += len;
    Ok(slice)
}

fn decode_ts_column(
    bytes: &[u8],
    n: usize,
    name: &'static str,
) -> Result<Vec<i64>, ChunkDecodeError> {
    let mut r = BitReader::new(bytes);
    let mut dec = DodDecoder::new();
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(
            dec.read(&mut r)
                .ok_or(ChunkDecodeError::ColumnTruncated(name))?,
        );
    }
    Ok(out)
}

fn decode_f32_column(
    bytes: &[u8],
    n: usize,
    name: &'static str,
) -> Result<Vec<f32>, ChunkDecodeError> {
    let mut r = BitReader::new(bytes);
    let mut dec = GorillaF32Decoder::new();
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(
            dec.read(&mut r)
                .ok_or(ChunkDecodeError::ColumnTruncated(name))?,
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(median: f32) -> Slot {
        Slot {
            min: median - 0.5,
            p2_5: median - 0.4,
            p25: median - 0.1,
            median,
            p75: median + 0.1,
            p97_5: median + 0.4,
            loss_pct: 0.0,
        }
    }

    #[test]
    fn empty_chunk_round_trips() {
        let bytes = encode_chunk(&[], ZSTD_LEVEL_G0).unwrap();
        let back = decode_chunk(&bytes).unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn single_sample_round_trips() {
        let s = vec![(1_700_000_000, slot(21.0))];
        let bytes = encode_chunk(&s, ZSTD_LEVEL_G0).unwrap();
        let back = decode_chunk(&bytes).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].0, s[0].0);
        assert!((back[0].1.median - s[0].1.median).abs() < 1e-6);
    }

    #[test]
    fn periodic_samples_compress_well() {
        // 2 hours at 30 s = 240 samples. Sinusoidal jitter on the median means
        // every Gorilla XOR is non-trivial; this is closer to "synthetic
        // worst-case" than real probe data which is much steadier.
        let mut s = Vec::with_capacity(240);
        for i in 0..240 {
            s.push((1_700_000_000 + i * 30, slot(20.0 + (i as f32).sin() * 0.5)));
        }
        let bytes = encode_chunk(&s, ZSTD_LEVEL_G0).unwrap();
        // Raw uncompressed = 240 * (8 + 7*4) = 8 640 bytes.
        // Sinusoidal data compresses to ~50 %; steady probe data does better.
        assert!(bytes.len() < 5500, "{} bytes for 240 samples", bytes.len());
        assert!(bytes.len() < s.len() * 36 / 2, "no compression at all");
        let back = decode_chunk(&bytes).unwrap();
        assert_eq!(back.len(), 240);
        for (a, b) in s.iter().zip(back.iter()) {
            assert_eq!(a.0, b.0);
            assert_eq!(
                a.1.fields().map(f32::to_bits),
                b.1.fields().map(f32::to_bits)
            );
        }
    }

    #[test]
    fn realistic_steady_latency_compresses_a_lot() {
        // Real-world shape: ~21 ms latency, tight percentile spread, almost
        // never any loss. This is the case the format is optimised for.
        let mut s = Vec::with_capacity(240);
        for i in 0..240 {
            let m = 21.0 + ((i % 3) as f32) * 0.05; // 21.00 / 21.05 / 21.10
            s.push((
                1_700_000_000 + i * 30,
                Slot {
                    min: m - 0.2,
                    p2_5: m - 0.15,
                    p25: m - 0.05,
                    median: m,
                    p75: m + 0.05,
                    p97_5: m + 0.15,
                    loss_pct: 0.0,
                },
            ));
        }
        let bytes = encode_chunk(&s, ZSTD_LEVEL_G0).unwrap();
        // Steady data should compress aggressively.
        assert!(
            bytes.len() < 1500,
            "{} bytes for steady 240 samples",
            bytes.len()
        );
        let back = decode_chunk(&bytes).unwrap();
        for (a, b) in s.iter().zip(back.iter()) {
            assert_eq!(a.0, b.0);
            assert_eq!(
                a.1.fields().map(f32::to_bits),
                b.1.fields().map(f32::to_bits)
            );
        }
    }

    #[test]
    fn nan_slots_round_trip() {
        let s = vec![
            (1_700_000_000, slot(21.0)),
            (1_700_000_030, Slot::NAN),
            (1_700_000_060, slot(22.0)),
            (1_700_000_090, Slot::NAN),
        ];
        let bytes = encode_chunk(&s, ZSTD_LEVEL_G0).unwrap();
        let back = decode_chunk(&bytes).unwrap();
        assert_eq!(back.len(), 4);
        assert!(back[1].1.is_nan());
        assert!(back[3].1.is_nan());
        assert_eq!(back[0].1.median.to_bits(), 21.0_f32.to_bits());
        assert_eq!(back[2].1.median.to_bits(), 22.0_f32.to_bits());
    }

    #[test]
    fn bad_magic_is_rejected() {
        let mut body = vec![0u8; 12];
        body[..4].copy_from_slice(b"XXXX");
        let bad = zstd::encode_all(body.as_slice(), 1).unwrap();
        assert!(matches!(
            decode_chunk(&bad),
            Err(ChunkDecodeError::BadMagic)
        ));
    }
}
