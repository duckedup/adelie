//! RLE (encoding id 2, SPEC §5): runs of equal values as `(value, uvarint len)`, run lengths
//! summing to `rows`. Applies to BOOL, INT64, UINT64, TIMESTAMP, DATE.

use crate::exec::Bitmap;
use crate::segment::error::DecodeError;
use crate::segment::wire::{Cursor, Sink};

/// Reads `nruns`, rejecting a count that could not possibly fit in `rows` or in the bytes
/// left (before any run-sized `Vec`/`Bitmap` gets allocated).
fn read_nruns(cur: &mut Cursor, rows: usize) -> Result<usize, DecodeError> {
    let nruns = cur.uvarint()?;
    if nruns > rows as u64 {
        return Err(DecodeError::Malformed("rle: run count exceeds rows"));
    }
    cur.guard_len(nruns, 2)
}

/// A run's length: at least 1, and not pushing the running total past `rows`.
fn read_run_len(cur: &mut Cursor, rows: usize, pos: usize) -> Result<usize, DecodeError> {
    let len = cur.uvarint()?;
    if len == 0 {
        return Err(DecodeError::Malformed("rle: zero-length run"));
    }
    let len = usize::try_from(len).map_err(|_| DecodeError::Malformed("rle: run too long"))?;
    let new_pos = pos
        .checked_add(len)
        .ok_or(DecodeError::Malformed("rle: run too long"))?;
    if new_pos > rows {
        return Err(DecodeError::Malformed("rle: run sum exceeds rows"));
    }
    Ok(len)
}

pub(super) fn encode_bool(bits: &Bitmap, out: &mut Sink) {
    let rows = bits.len();
    let mut runs: Vec<(u8, usize)> = Vec::new();
    for i in 0..rows {
        let v = bits.get(i) as u8;
        match runs.last_mut() {
            Some((last, len)) if *last == v => *len += 1,
            _ => runs.push((v, 1)),
        }
    }
    out.uvarint(runs.len() as u64);
    for (v, len) in runs {
        out.u8(v);
        out.uvarint(len as u64);
    }
}

pub(super) fn decode_bool(cur: &mut Cursor, rows: usize) -> Result<Bitmap, DecodeError> {
    let nruns = read_nruns(cur, rows)?;
    // Grown by `push`, one validated run at a time, rather than allocated at `rows` up
    // front: the claimed row count alone must never drive the allocation size.
    let mut bm = Bitmap::new_null(0);
    let mut pos = 0usize;
    for _ in 0..nruns {
        let v = cur.u8()?;
        if v > 1 {
            return Err(DecodeError::Malformed("rle: bool value not 0 or 1"));
        }
        let len = read_run_len(cur, rows, pos)?;
        for _ in 0..len {
            bm.push(v == 1);
        }
        pos += len;
    }
    if pos != rows {
        return Err(DecodeError::Malformed("rle: run sum does not match rows"));
    }
    Ok(bm)
}

pub(super) fn encode_i64(v: &[i64], out: &mut Sink) {
    let runs = run_lengths(v);
    out.uvarint(runs.len() as u64);
    for (val, len) in runs {
        out.svarint(val);
        out.uvarint(len as u64);
    }
}

pub(super) fn decode_i64(cur: &mut Cursor, rows: usize) -> Result<Vec<i64>, DecodeError> {
    let nruns = read_nruns(cur, rows)?;
    let mut out = Vec::with_capacity(nruns);
    for _ in 0..nruns {
        let v = cur.svarint()?;
        let len = read_run_len(cur, rows, out.len())?;
        out.resize(out.len() + len, v);
    }
    if out.len() != rows {
        return Err(DecodeError::Malformed("rle: run sum does not match rows"));
    }
    Ok(out)
}

pub(super) fn encode_u64(v: &[u64], out: &mut Sink) {
    let runs = run_lengths(v);
    out.uvarint(runs.len() as u64);
    for (val, len) in runs {
        out.uvarint(val);
        out.uvarint(len as u64);
    }
}

pub(super) fn decode_u64(cur: &mut Cursor, rows: usize) -> Result<Vec<u64>, DecodeError> {
    let nruns = read_nruns(cur, rows)?;
    let mut out = Vec::with_capacity(nruns);
    for _ in 0..nruns {
        let v = cur.uvarint()?;
        let len = read_run_len(cur, rows, out.len())?;
        out.resize(out.len() + len, v);
    }
    if out.len() != rows {
        return Err(DecodeError::Malformed("rle: run sum does not match rows"));
    }
    Ok(out)
}

/// Maximal equal-value runs over `v`, in order; every run's length is at least 1.
fn run_lengths<T: PartialEq + Copy>(v: &[T]) -> Vec<(T, usize)> {
    let mut out: Vec<(T, usize)> = Vec::new();
    for &x in v {
        match out.last_mut() {
            Some((last, len)) if *last == x => *len += 1,
            _ => out.push((x, 1)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use adelie_harness::rng::SplitMix64;

    fn roundtrip_i64(v: &[i64]) {
        let mut s = Sink::new();
        encode_i64(v, &mut s);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert_eq!(decode_i64(&mut c, v.len()).unwrap(), v);
    }

    fn roundtrip_u64(v: &[u64]) {
        let mut s = Sink::new();
        encode_u64(v, &mut s);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert_eq!(decode_u64(&mut c, v.len()).unwrap(), v);
    }

    #[test]
    fn i64_round_trip_at_boundaries() {
        roundtrip_i64(&[]);
        roundtrip_i64(&[42]);
        roundtrip_i64(&[7; 100]);
        let alt: Vec<i64> = (0..200)
            .map(|i| if i % 2 == 0 { i64::MIN } else { i64::MAX })
            .collect();
        roundtrip_i64(&alt);
    }

    #[test]
    fn u64_round_trip_at_boundaries() {
        roundtrip_u64(&[]);
        roundtrip_u64(&[u64::MAX]);
        roundtrip_u64(&[u64::MAX; 50]);
    }

    #[test]
    fn bool_round_trip() {
        for len in [0usize, 1, 2, 100] {
            let mut bm = Bitmap::new_null(len);
            for i in 0..len {
                bm.set(i, i.is_multiple_of(3));
            }
            let mut s = Sink::new();
            encode_bool(&bm, &mut s);
            let buf = s.into_vec();
            let mut c = Cursor::new(&buf);
            assert_eq!(decode_bool(&mut c, len).unwrap(), bm);
        }
    }

    #[test]
    fn equal_values_compress_well() {
        let v = vec![9i64; 4096];
        let mut s = Sink::new();
        encode_i64(&v, &mut s);
        assert!(s.len() < 16, "encoded len {}", s.len());
    }

    #[test]
    fn decode_rejects_zero_length_run() {
        let mut s = Sink::new();
        s.uvarint(1);
        s.svarint(5);
        s.uvarint(0);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert!(decode_i64(&mut c, 4).is_err());
    }

    #[test]
    fn decode_rejects_run_sum_mismatch() {
        let mut s = Sink::new();
        s.uvarint(1);
        s.svarint(5);
        s.uvarint(3);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert!(decode_i64(&mut c, 4).is_err());
    }

    #[test]
    fn decode_rejects_nruns_over_rows() {
        let mut s = Sink::new();
        s.uvarint(5);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert!(decode_i64(&mut c, 2).is_err());
    }

    #[test]
    fn hostile_bytes_never_panic() {
        let mut rng = SplitMix64::new(9001);
        let iters = if cfg!(miri) { 30 } else { 1000 };
        for _ in 0..iters {
            let len = rng.range(0, 32) as usize;
            let buf: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
            let mut c = Cursor::new(&buf);
            let _ = decode_i64(&mut c, 16);
            let mut c = Cursor::new(&buf);
            let _ = decode_u64(&mut c, 16);
            let mut c = Cursor::new(&buf);
            let _ = decode_bool(&mut c, 16);
        }
    }

    #[test]
    fn max_decode_rows_with_tiny_payload_is_err() {
        let buf = [0xffu8, 0xff, 0xff];
        let mut c = Cursor::new(&buf);
        assert!(decode_i64(&mut c, crate::segment::MAX_DECODE_ROWS).is_err());
    }
}
