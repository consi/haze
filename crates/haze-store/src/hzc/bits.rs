//! Bit-level writer / reader used by the Gorilla XOR + delta-of-delta
//! encoders. MSB-first within each byte - matches the original Gorilla paper
//! so produced chunks are byte-compatible with other implementations.

#[derive(Default)]
pub struct BitWriter {
    bytes: Vec<u8>,
    /// Current partial byte being assembled (MSB-first).
    cur: u8,
    /// Number of bits already written into `cur` (0..8).
    nbits: u8,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn write_bit(&mut self, bit: bool) {
        if bit {
            self.cur |= 1 << (7 - self.nbits);
        }
        self.nbits += 1;
        if self.nbits == 8 {
            self.bytes.push(self.cur);
            self.cur = 0;
            self.nbits = 0;
        }
    }

    /// Write the low `n` bits of `value`, MSB-first.
    pub fn write_bits(&mut self, mut value: u64, n: u8) {
        debug_assert!(n <= 64);
        for i in (0..n).rev() {
            let bit = (value >> i) & 1 == 1;
            self.write_bit(bit);
            value &= (1u64 << i).wrapping_sub(1); // optional masking
        }
    }

    /// Consume the writer and return the byte buffer. Pads the trailing byte
    /// with zero bits so the output is byte-aligned.
    pub fn into_bytes(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            self.bytes.push(self.cur);
        }
        self.bytes
    }
}

#[derive(Debug)]
pub struct BitReader<'a> {
    bytes: &'a [u8],
    /// Index of the byte currently being consumed.
    byte_pos: usize,
    /// Number of bits already read from the current byte (0..8).
    bit_pos: u8,
}

impl<'a> BitReader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            byte_pos: 0,
            bit_pos: 0,
        }
    }

    pub fn read_bit(&mut self) -> Option<bool> {
        if self.byte_pos >= self.bytes.len() {
            return None;
        }
        let bit = (self.bytes[self.byte_pos] >> (7 - self.bit_pos)) & 1 == 1;
        self.bit_pos += 1;
        if self.bit_pos == 8 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
        Some(bit)
    }

    pub fn read_bits(&mut self, n: u8) -> Option<u64> {
        debug_assert!(n <= 64);
        let mut out: u64 = 0;
        for _ in 0..n {
            out = (out << 1) | u64::from(self.read_bit()? as u8);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_single_bits() {
        let mut w = BitWriter::new();
        let pattern = [true, false, true, true, false, false, true, false, true];
        for &b in &pattern {
            w.write_bit(b);
        }
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        for &b in &pattern {
            assert_eq!(r.read_bit(), Some(b));
        }
    }

    #[test]
    fn round_trip_bit_groups() {
        let mut w = BitWriter::new();
        w.write_bits(0b1011, 4);
        w.write_bits(0b0100_0001, 8);
        w.write_bits(0b1, 1);
        w.write_bits(0xFFFF_FFFF_FFFF_FFFF, 64);
        let bytes = w.into_bytes();

        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_bits(4), Some(0b1011));
        assert_eq!(r.read_bits(8), Some(0b0100_0001));
        assert_eq!(r.read_bits(1), Some(0b1));
        assert_eq!(r.read_bits(64), Some(0xFFFF_FFFF_FFFF_FFFF));
    }

    #[test]
    fn arbitrary_width_round_trip() {
        let mut w = BitWriter::new();
        for n in 1..=63u8 {
            w.write_bits((1u64 << n) - 1, n);
        }
        let bytes = w.into_bytes();

        let mut r = BitReader::new(&bytes);
        for n in 1..=63u8 {
            assert_eq!(r.read_bits(n), Some((1u64 << n) - 1));
        }
    }
}
