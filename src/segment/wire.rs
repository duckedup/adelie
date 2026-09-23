//! Wire primitives (SPEC §5, D0008): little-endian fixed-width ints, LEB128 varints,
//! length-prefixed bytes/records. `Cursor` never panics; every read is checked.

use super::MAX_DECODE_ROWS;
use super::error::DecodeError;

/// Zigzag-encodes a signed value so small magnitudes (either sign) stay small unsigned.
pub(crate) fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

pub(crate) fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// An append-only byte builder for the encodings above.
pub(crate) struct Sink {
    buf: Vec<u8>,
}

impl Sink {
    pub(crate) fn new() -> Self {
        Sink { buf: Vec::new() }
    }

    pub(crate) fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub(crate) fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub(crate) fn i128(&mut self, v: i128) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// LEB128: 7 value bits per byte, high bit set while more bytes follow.
    pub(crate) fn uvarint(&mut self, v: u64) {
        let mut v = v;
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.buf.push(byte);
                break;
            }
            self.buf.push(byte | 0x80);
        }
    }

    pub(crate) fn svarint(&mut self, v: i64) {
        self.uvarint(zigzag(v));
    }

    pub(crate) fn bytes(&mut self, b: &[u8]) {
        self.uvarint(b.len() as u64);
        self.raw(b);
    }

    pub(crate) fn raw(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    pub(crate) fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }

    /// Writes `f`'s output as a length-prefixed body: a reader ignores trailing fields it
    /// does not know, which is what makes new fields additive (SPEC §5).
    pub(crate) fn record(&mut self, f: impl FnOnce(&mut Sink)) {
        let mut inner = Sink::new();
        f(&mut inner);
        self.bytes(&inner.buf);
    }

    pub(crate) fn len(&self) -> usize {
        self.buf.len()
    }

    pub(crate) fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

/// A checked read cursor over a borrowed byte slice. Every method returns `Err` rather than
/// panicking, so a malformed segment can never crash a reader.
pub(crate) struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Cursor { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if n > self.remaining() {
            return Err(DecodeError::Truncated);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub(crate) fn i32(&mut self) -> Result<i32, DecodeError> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub(crate) fn i64(&mut self) -> Result<i64, DecodeError> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub(crate) fn i128(&mut self) -> Result<i128, DecodeError> {
        Ok(i128::from_le_bytes(self.take(16)?.try_into().unwrap()))
    }

    /// A uvarint longer than 10 bytes, or whose 10th byte is above 1 (the only bit of a u64
    /// it can still hold), is `Malformed`.
    pub(crate) fn uvarint(&mut self) -> Result<u64, DecodeError> {
        let mut result: u64 = 0;
        for i in 0..10u32 {
            let byte = self.u8()?;
            if i == 9 && byte > 1 {
                return Err(DecodeError::Malformed("varint overflow"));
            }
            result |= ((byte & 0x7f) as u64) << (7 * i);
            if byte & 0x80 == 0 {
                return Ok(result);
            }
        }
        Err(DecodeError::Malformed("varint overflow"))
    }

    pub(crate) fn svarint(&mut self) -> Result<i64, DecodeError> {
        Ok(unzigzag(self.uvarint()?))
    }

    pub(crate) fn bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let len = self.uvarint()?;
        let len = usize::try_from(len).map_err(|_| DecodeError::Truncated)?;
        self.take(len)
    }

    pub(crate) fn raw(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        self.take(n)
    }

    pub(crate) fn str(&mut self) -> Result<&'a str, DecodeError> {
        std::str::from_utf8(self.bytes()?).map_err(|_| DecodeError::Malformed("invalid utf-8"))
    }

    /// A sub-cursor over one record's body; the outer cursor advances past the whole body
    /// regardless of how much of it the sub-cursor's caller actually reads.
    pub(crate) fn record(&mut self) -> Result<Cursor<'a>, DecodeError> {
        Ok(Cursor::new(self.bytes()?))
    }

    pub(crate) fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Rejects a claimed count before it drives a `Vec::with_capacity`: either it can't
    /// possibly fit the bytes left, or (rows specifically) it exceeds `MAX_DECODE_ROWS`.
    pub(crate) fn guard_len(&self, n: u64, min_bytes_each: usize) -> Result<usize, DecodeError> {
        if n > MAX_DECODE_ROWS as u64 {
            return Err(DecodeError::Malformed("row count exceeds MAX_DECODE_ROWS"));
        }
        let need = n
            .checked_mul(min_bytes_each as u64)
            .ok_or(DecodeError::Truncated)?;
        if need > self.remaining() as u64 {
            return Err(DecodeError::Truncated);
        }
        Ok(n as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_width_round_trip_at_boundaries() {
        let mut s = Sink::new();
        s.u64(0);
        s.u64(u64::MAX);
        s.i64(0);
        s.i64(i64::MIN);
        s.i32(i32::MIN);
        s.i128(i128::MIN);
        s.u32(u32::MAX);
        s.u16(u16::MAX);
        s.u8(1);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert_eq!(c.u64().unwrap(), 0);
        assert_eq!(c.u64().unwrap(), u64::MAX);
        assert_eq!(c.i64().unwrap(), 0);
        assert_eq!(c.i64().unwrap(), i64::MIN);
        assert_eq!(c.i32().unwrap(), i32::MIN);
        assert_eq!(c.i128().unwrap(), i128::MIN);
        assert_eq!(c.u32().unwrap(), u32::MAX);
        assert_eq!(c.u16().unwrap(), u16::MAX);
        assert_eq!(c.u8().unwrap(), 1);
        assert!(c.is_empty());
    }

    #[test]
    fn varint_round_trips_at_boundaries() {
        for v in [0u64, 1, u64::MAX] {
            let mut s = Sink::new();
            s.uvarint(v);
            let buf = s.into_vec();
            let mut c = Cursor::new(&buf);
            assert_eq!(c.uvarint().unwrap(), v);
        }
        for v in [0i64, 1, -1, i64::MIN, i64::MAX] {
            let mut s = Sink::new();
            s.svarint(v);
            let buf = s.into_vec();
            let mut c = Cursor::new(&buf);
            assert_eq!(c.svarint().unwrap(), v);
        }
    }

    #[test]
    fn overlong_varint_is_rejected() {
        let buf = [0x80u8; 11];
        let mut c = Cursor::new(&buf);
        assert_eq!(c.uvarint(), Err(DecodeError::Malformed("varint overflow")));
    }

    #[test]
    fn record_skips_trailing_bytes() {
        let mut s = Sink::new();
        s.record(|r| {
            r.u8(7);
            r.u32(0xdead_beef);
        });
        s.u8(9);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        let mut body = c.record().unwrap();
        assert_eq!(body.u8().unwrap(), 7);
        assert_eq!(c.u8().unwrap(), 9);
    }

    #[test]
    fn guard_len_rejects_huge_count() {
        let buf = [0u8; 8];
        let c = Cursor::new(&buf);
        assert!(c.guard_len(u64::MAX, 1).is_err());
    }

    #[test]
    fn guard_len_checks_bytes_available() {
        let buf = [0u8; 4];
        let c = Cursor::new(&buf);
        assert_eq!(c.guard_len(4, 1).unwrap(), 4);
        assert!(c.guard_len(5, 1).is_err());
        assert_eq!(c.guard_len(1_000_000, 0).unwrap(), 1_000_000);
    }
}
