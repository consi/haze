//! Gorilla XOR encoding for `f32` and delta-of-delta encoding for `i64`
//! timestamps. Adapted from the Facebook `Gorilla` paper for our 32-bit-float
//! columns and second-resolution timestamps.

use super::bits::{BitReader, BitWriter};

// ─── Delta-of-delta for i64 timestamps ────────────────────────────────────

/// Standard `Gorilla` `DoD` cascade. Each bucket covers a signed range biased
/// into unsigned so the bit-packed value fits in `n` bits.
const DOD_BUCKETS: &[(u8, i64)] = &[
    (7, 64),    // marker "10",   range −63..=64       → biased −63..127
    (9, 256),   // marker "110",  range −255..=256     → biased −255..511
    (12, 2048), // marker "1110", range −2047..=2048  → biased −2047..4095
];

pub struct DodEncoder {
    prev_ts: i64,
    prev_delta: i64,
    first: bool,
}

impl Default for DodEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl DodEncoder {
    pub fn new() -> Self {
        Self {
            prev_ts: 0,
            prev_delta: 0,
            first: true,
        }
    }

    pub fn write(&mut self, w: &mut BitWriter, ts: i64) {
        if self.first {
            // First timestamp written raw (64 bits).
            w.write_bits(ts as u64, 64);
            self.prev_ts = ts;
            self.prev_delta = 0;
            self.first = false;
            return;
        }
        let delta = ts - self.prev_ts;
        let dod = delta - self.prev_delta;
        self.prev_ts = ts;
        self.prev_delta = delta;

        if dod == 0 {
            w.write_bit(false);
            return;
        }
        // Bucketed Gorilla DoD.
        for (i, &(nbits, half)) in DOD_BUCKETS.iter().enumerate() {
            if dod >= -(half - 1) && dod <= half {
                // Marker: 1, 10, 110, 1110 - i+1 ones then a 0, except final.
                for _ in 0..=i {
                    w.write_bit(true);
                }
                w.write_bit(false);
                let biased = (dod + (half - 1)) as u64;
                w.write_bits(biased, nbits);
                return;
            }
        }
        // Fallback: "1111" + 32-bit signed (covers ±2³¹ seconds ≈ ±68 years).
        for _ in 0..4 {
            w.write_bit(true);
        }
        w.write_bits((dod as i32) as u32 as u64, 32);
    }
}

pub struct DodDecoder {
    prev_ts: i64,
    prev_delta: i64,
    first: bool,
}

impl Default for DodDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl DodDecoder {
    pub fn new() -> Self {
        Self {
            prev_ts: 0,
            prev_delta: 0,
            first: true,
        }
    }

    pub fn read(&mut self, r: &mut BitReader<'_>) -> Option<i64> {
        if self.first {
            let ts = r.read_bits(64)? as i64;
            self.prev_ts = ts;
            self.prev_delta = 0;
            self.first = false;
            return Some(ts);
        }
        // Count leading 1 bits to identify the bucket.
        let mut ones = 0u8;
        while ones < 4 {
            let b = r.read_bit()?;
            if !b {
                break;
            }
            ones += 1;
        }
        let dod = match ones {
            0 => 0,
            1..=3 => {
                let (nbits, half) = DOD_BUCKETS[(ones - 1) as usize];
                let biased = r.read_bits(nbits)? as i64;
                biased - (half - 1)
            }
            _ => {
                // 32-bit signed fallback
                let raw = r.read_bits(32)? as u32;
                raw as i32 as i64
            }
        };
        let delta = self.prev_delta + dod;
        let ts = self.prev_ts + delta;
        self.prev_ts = ts;
        self.prev_delta = delta;
        Some(ts)
    }
}

// ─── Gorilla XOR encoding for f32 ──────────────────────────────────────────

pub struct GorillaF32Encoder {
    prev_bits: u32,
    prev_leading: u8,
    prev_trailing: u8,
    first: bool,
}

impl Default for GorillaF32Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl GorillaF32Encoder {
    pub fn new() -> Self {
        Self {
            prev_bits: 0,
            prev_leading: 0,
            prev_trailing: 0,
            first: true,
        }
    }

    pub fn write(&mut self, w: &mut BitWriter, value: f32) {
        let bits = value.to_bits();
        if self.first {
            w.write_bits(u64::from(bits), 32);
            self.prev_bits = bits;
            self.prev_leading = 0;
            self.prev_trailing = 0;
            self.first = false;
            return;
        }
        let xor = self.prev_bits ^ bits;
        self.prev_bits = bits;

        if xor == 0 {
            w.write_bit(false);
            return;
        }
        w.write_bit(true);
        let leading = xor.leading_zeros() as u8;
        let trailing = xor.trailing_zeros() as u8;

        if self.prev_leading > 0 && leading >= self.prev_leading && trailing >= self.prev_trailing {
            // Reuse the previous block window - 1 bit overhead.
            w.write_bit(false);
            let sig = 32 - self.prev_leading - self.prev_trailing;
            let masked = (xor >> self.prev_trailing) & ((1u32 << sig) - 1);
            w.write_bits(u64::from(masked), sig);
        } else {
            // New block window - write 5+5 metadata bits then the value.
            w.write_bit(true);
            let sig = 32 - leading - trailing;
            w.write_bits(u64::from(leading), 5);
            w.write_bits(u64::from(sig - 1), 5); // 0..31 maps to length 1..32
            let masked = (xor >> trailing) & ((1u32 << sig) - 1);
            w.write_bits(u64::from(masked), sig);
            self.prev_leading = leading;
            self.prev_trailing = trailing;
        }
    }
}

pub struct GorillaF32Decoder {
    prev_bits: u32,
    prev_leading: u8,
    prev_trailing: u8,
    first: bool,
}

impl Default for GorillaF32Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl GorillaF32Decoder {
    pub fn new() -> Self {
        Self {
            prev_bits: 0,
            prev_leading: 0,
            prev_trailing: 0,
            first: true,
        }
    }

    pub fn read(&mut self, r: &mut BitReader<'_>) -> Option<f32> {
        if self.first {
            let bits = r.read_bits(32)? as u32;
            self.prev_bits = bits;
            self.first = false;
            return Some(f32::from_bits(bits));
        }
        let differ = r.read_bit()?;
        if !differ {
            return Some(f32::from_bits(self.prev_bits));
        }
        let new_block = r.read_bit()?;
        let sig = if new_block {
            let leading = r.read_bits(5)? as u8;
            let s = (r.read_bits(5)? as u8) + 1;
            self.prev_leading = leading;
            self.prev_trailing = 32 - leading - s;
            s
        } else {
            32 - self.prev_leading - self.prev_trailing
        };
        let raw = r.read_bits(sig)? as u32;
        let xor = raw << self.prev_trailing;
        let bits = self.prev_bits ^ xor;
        self.prev_bits = bits;
        Some(f32::from_bits(bits))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip_dod(values: &[i64]) {
        let mut w = BitWriter::new();
        let mut enc = DodEncoder::new();
        for &v in values {
            enc.write(&mut w, v);
        }
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let mut dec = DodDecoder::new();
        for &v in values {
            assert_eq!(dec.read(&mut r), Some(v), "value {v}");
        }
    }

    #[test]
    fn dod_periodic_timestamps_are_dense() {
        // 60-second cadence - every dod is 0 after the first two.
        let xs: Vec<i64> = (0..1000).map(|i| 1_700_000_000 + i * 60).collect();
        round_trip_dod(&xs);

        let mut w = BitWriter::new();
        let mut enc = DodEncoder::new();
        for &v in &xs {
            enc.write(&mut w, v);
        }
        let bytes = w.into_bytes();
        // First ts: 64 bits = 8 B. Second ts: bucket "10" + 7 bits = 9 bits.
        // Remaining 998 ts: dod = 0 → 1 bit each = ~125 B. Total ~135 B vs.
        // raw 8 000 B → ~60× compression.
        assert!(
            bytes.len() < 200,
            "{} bytes for 1000 periodic ts",
            bytes.len()
        );
    }

    #[test]
    fn dod_jittery_timestamps() {
        let xs: Vec<i64> = (0..200).map(|i| 1_700_000_000 + i * 60 + (i % 7)).collect();
        round_trip_dod(&xs);
    }

    #[test]
    fn dod_handles_large_jumps() {
        let xs: Vec<i64> = vec![1_700_000_000, 1_700_000_060, 1_705_000_000, 1_705_000_060];
        round_trip_dod(&xs);
    }

    fn round_trip_f32(values: &[f32]) {
        let mut w = BitWriter::new();
        let mut enc = GorillaF32Encoder::new();
        for &v in values {
            enc.write(&mut w, v);
        }
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let mut dec = GorillaF32Decoder::new();
        for &v in values {
            let out = dec.read(&mut r).expect("decode");
            assert!(
                (out.is_nan() && v.is_nan()) || out.to_bits() == v.to_bits(),
                "in {v} out {out}"
            );
        }
    }

    #[test]
    fn gorilla_flat_series_is_dense() {
        let xs = vec![21.5_f32; 1000];
        round_trip_f32(&xs);

        let mut w = BitWriter::new();
        let mut enc = GorillaF32Encoder::new();
        for &v in &xs {
            enc.write(&mut w, v);
        }
        let bytes = w.into_bytes();
        // First sample = 32 bits = 4 bytes, rest = 1 bit each ≈ 125 bytes total.
        // Allow some slack for the trailing-byte padding.
        assert!(bytes.len() < 140, "{} bytes for 1000 flat f32", bytes.len());
    }

    #[test]
    fn gorilla_round_trip_jittery() {
        let xs: Vec<f32> = (0..256)
            .map(|i| 21.0 + (i as f32 / 100.0).sin() * 0.3)
            .collect();
        round_trip_f32(&xs);
    }

    #[test]
    fn gorilla_handles_nan() {
        let xs: Vec<f32> = vec![1.0, 2.0, f32::NAN, 4.0, f32::NAN, f32::NAN, 7.0];
        round_trip_f32(&xs);
    }

    #[test]
    fn gorilla_special_values() {
        let xs: Vec<f32> = vec![
            0.0,
            -0.0,
            1.0,
            -1.0,
            f32::MIN,
            f32::MAX,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
        ];
        round_trip_f32(&xs);
    }
}
