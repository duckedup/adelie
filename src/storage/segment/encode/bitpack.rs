//! Bit-packing (SPEC §5, D0008): LSB-first `BitWriter`/`BitReader`, and the FoR block shared
//! by FOR, DELTA and DELTA_OF_DELTA (`frame_of_ref.rs`/`delta.rs`, group 2 siblings).

use crate::storage::segment::error::DecodeError;
use crate::storage::segment::wire::{Cursor, Sink};

/// `bits` in `0..=64`, safe against the shift-overflow a literal `1u64 << 64` would cause.
fn mask(bits: u8) -> u64 {
    if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    }
}

/// Appends fixed-width fields LSB-first, packing across byte boundaries as it goes.
pub(crate) struct BitWriter {
    buf: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    pub(crate) fn new() -> Self {
        BitWriter {
            buf: Vec::new(),
            bits: 0,
        }
    }

    /// Writes the low `bits` bits of `value` (masked first), `bits` in `0..=64`.
    pub(crate) fn write(&mut self, value: u64, bits: u8) {
        let mut value = value & mask(bits);
        let mut remaining = bits;
        while remaining > 0 {
            let bit_off = (self.bits % 8) as u8;
            if bit_off == 0 {
                self.buf.push(0);
            }
            let take = remaining.min(8 - bit_off);
            let chunk = (value & mask(take)) as u8;
            *self
                .buf
                .last_mut()
                .expect("just pushed or already had a byte") |= chunk << bit_off;
            value >>= take;
            remaining -= take;
            self.bits += take as usize;
        }
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.buf
    }
}

/// Reads fixed-width fields LSB-first from a borrowed byte slice; never panics.
pub(crate) struct BitReader<'a> {
    buf: &'a [u8],
    bits: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        BitReader {
            buf: bytes,
            bits: 0,
        }
    }

    /// Reads `bits` bits (`0..=64`); `Truncated` if fewer remain.
    pub(crate) fn read(&mut self, bits: u8) -> Result<u64, DecodeError> {
        if self.bits + bits as usize > self.buf.len() * 8 {
            return Err(DecodeError::Truncated);
        }
        let mut result: u64 = 0;
        let mut got: u8 = 0;
        while got < bits {
            let byte_idx = self.bits / 8;
            let bit_off = (self.bits % 8) as u8;
            let take = (bits - got).min(8 - bit_off);
            let bits_here = (self.buf[byte_idx] >> bit_off) & mask(take) as u8;
            result |= (bits_here as u64) << got;
            self.bits += take as usize;
            got += take;
        }
        Ok(result)
    }
}

/// A FoR block: `u64 min, u8 width, bytes(packed)`, `width = 64 - (max - min).leading_zeros()`.
pub(crate) fn write_for(values: &[u64], out: &mut Sink) {
    let min = values.iter().copied().min().unwrap_or(0);
    let max = values.iter().copied().max().unwrap_or(0);
    let width = (64 - (max - min).leading_zeros()) as u8;
    out.u64(min);
    out.u8(width);
    let mut bw = BitWriter::new();
    for &v in values {
        bw.write(v.wrapping_sub(min), width);
    }
    out.bytes(&bw.finish());
}

/// Inverse of `write_for`. Rejects `width > 64` and a packed length that does not match
/// `ceil(rows * width / 8)`; guards `rows` before allocating the output.
pub(crate) fn read_for(cur: &mut Cursor, rows: usize) -> Result<Vec<u64>, DecodeError> {
    let min = cur.u64()?;
    let width = cur.u8()?;
    if width > 64 {
        return Err(DecodeError::Malformed("for width exceeds 64"));
    }
    let n = cur.guard_len(rows as u64, 0)?;
    let expected = (n as u64 * width as u64).div_ceil(8) as usize;
    let packed = cur.bytes()?;
    if packed.len() != expected {
        return Err(DecodeError::Malformed("for packed length mismatch"));
    }
    let mut br = BitReader::new(packed);
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(min.wrapping_add(br.read(width)?));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_widths_round_trip_at_boundaries() {
        for &bits in &[0u8, 1, 7, 63, 64] {
            let values: Vec<u64> = (0..37)
                .map(|i| (i as u64).wrapping_mul(0x9e37) & mask(bits))
                .collect();
            let mut bw = BitWriter::new();
            for &v in &values {
                bw.write(v, bits);
            }
            let buf = bw.finish();
            let mut br = BitReader::new(&buf);
            for &v in &values {
                assert_eq!(br.read(bits).unwrap(), v, "bits={bits}");
            }
        }
    }

    #[test]
    fn for_block_round_trips() {
        let values = vec![10u64, 20, 15, 10, 10_000];
        let mut s = Sink::new();
        write_for(&values, &mut s);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert_eq!(read_for(&mut c, values.len()).unwrap(), values);
    }

    #[test]
    fn for_block_handles_zero_rows() {
        let mut s = Sink::new();
        write_for(&[], &mut s);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert_eq!(read_for(&mut c, 0).unwrap(), Vec::<u64>::new());
    }

    #[test]
    fn read_for_rejects_width_over_64() {
        let mut s = Sink::new();
        s.u64(0);
        s.u8(65);
        s.bytes(&[]);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert!(matches!(
            read_for(&mut c, 0),
            Err(DecodeError::Malformed(_))
        ));
    }

    #[test]
    fn read_for_rejects_short_buffer() {
        let mut s = Sink::new();
        s.u64(0);
        s.u8(8);
        s.bytes(&[1, 2]); // 3 rows at width 8 need 3 bytes, not 2
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert!(matches!(
            read_for(&mut c, 3),
            Err(DecodeError::Malformed(_))
        ));
    }
}
