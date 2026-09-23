//! DELTA and DELTA_OF_DELTA (encoding ids 4/5, SPEC §5): first value(s) raw, remaining
//! differences zigzagged into a FoR block. Signed columns map through the FOR sign-flip.

use crate::segment::error::DecodeError;
use crate::segment::wire::{Cursor, Sink, unzigzag, zigzag};

const SIGN: u64 = 1 << 63;

/// `rows == 0`: nothing at all. `rows == 1`: just `first`. Otherwise `first` plus a FoR
/// block of `rows - 1` zigzagged first differences, all arithmetic wrapping.
fn encode_delta_mapped(x: &[u64], out: &mut Sink) {
    let Some((&first, rest)) = x.split_first() else {
        return;
    };
    out.u64(first);
    if rest.is_empty() {
        return;
    }
    let deltas: Vec<u64> = x
        .windows(2)
        .map(|w| zigzag(w[1].wrapping_sub(w[0]) as i64))
        .collect();
    super::write_for(&deltas, out);
}

fn decode_delta_mapped(cur: &mut Cursor, rows: usize) -> Result<Vec<u64>, DecodeError> {
    if rows == 0 {
        return Ok(Vec::new());
    }
    let first = cur.u64()?;
    if rows == 1 {
        return Ok(vec![first]);
    }
    let deltas = super::read_for(cur, rows - 1)?;
    let mut out = Vec::with_capacity(rows);
    out.push(first);
    let mut prev = first;
    for dz in deltas {
        prev = prev.wrapping_add(unzigzag(dz) as u64);
        out.push(prev);
    }
    Ok(out)
}

/// Second differences: `first` (mapped), svarint `first_delta`, then a FoR block of
/// `rows - 2` zigzagged `delta[i] - delta[i-1]`, all wrapping. `rows <= 1` is plain delta.
fn encode_dod_mapped(x: &[u64], out: &mut Sink) {
    let Some((&first, rest)) = x.split_first() else {
        return;
    };
    out.u64(first);
    let Some((&second, _)) = rest.split_first() else {
        return;
    };
    let first_delta = second.wrapping_sub(first) as i64;
    out.svarint(first_delta);
    // Always emit the FoR block from here on, even empty (rows == 2): decode always reads
    // one, so the two sides must agree unconditionally.
    let mut prev_delta = first_delta;
    let mut seconds: Vec<u64> = Vec::with_capacity(x.len().saturating_sub(2));
    for i in 2..x.len() {
        let delta = x[i].wrapping_sub(x[i - 1]) as i64;
        seconds.push(zigzag(delta.wrapping_sub(prev_delta)));
        prev_delta = delta;
    }
    super::write_for(&seconds, out);
}

fn decode_dod_mapped(cur: &mut Cursor, rows: usize) -> Result<Vec<u64>, DecodeError> {
    if rows == 0 {
        return Ok(Vec::new());
    }
    let first = cur.u64()?;
    if rows == 1 {
        return Ok(vec![first]);
    }
    let first_delta = cur.svarint()?;
    let second = first.wrapping_add(first_delta as u64);
    // Always read the FoR block, even empty (rows == 2): encode always writes one.
    let seconds = super::read_for(cur, rows - 2)?;
    let mut out = Vec::with_capacity(rows);
    out.push(first);
    out.push(second);
    let mut prev_delta = first_delta;
    let mut prev_val = second;
    for dz in seconds {
        let delta = prev_delta.wrapping_add(unzigzag(dz));
        prev_val = prev_val.wrapping_add(delta as u64);
        out.push(prev_val);
        prev_delta = delta;
    }
    Ok(out)
}

fn map_i64(v: &[i64]) -> Vec<u64> {
    v.iter().map(|&x| (x as u64) ^ SIGN).collect()
}

fn unmap_i64(v: Vec<u64>) -> Vec<i64> {
    v.into_iter().map(|u| (u ^ SIGN) as i64).collect()
}

pub(super) fn encode_delta_i64(v: &[i64], out: &mut Sink) {
    encode_delta_mapped(&map_i64(v), out);
}

pub(super) fn decode_delta_i64(cur: &mut Cursor, rows: usize) -> Result<Vec<i64>, DecodeError> {
    Ok(unmap_i64(decode_delta_mapped(cur, rows)?))
}

pub(super) fn encode_delta_u64(v: &[u64], out: &mut Sink) {
    encode_delta_mapped(v, out);
}

pub(super) fn decode_delta_u64(cur: &mut Cursor, rows: usize) -> Result<Vec<u64>, DecodeError> {
    decode_delta_mapped(cur, rows)
}

pub(super) fn encode_dod_i64(v: &[i64], out: &mut Sink) {
    encode_dod_mapped(&map_i64(v), out);
}

pub(super) fn decode_dod_i64(cur: &mut Cursor, rows: usize) -> Result<Vec<i64>, DecodeError> {
    Ok(unmap_i64(decode_dod_mapped(cur, rows)?))
}

pub(super) fn encode_dod_u64(v: &[u64], out: &mut Sink) {
    encode_dod_mapped(v, out);
}

pub(super) fn decode_dod_u64(cur: &mut Cursor, rows: usize) -> Result<Vec<u64>, DecodeError> {
    decode_dod_mapped(cur, rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use adelie_harness::rng::SplitMix64;

    fn roundtrip_delta_i64(v: &[i64]) {
        let mut s = Sink::new();
        encode_delta_i64(v, &mut s);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert_eq!(decode_delta_i64(&mut c, v.len()).unwrap(), v);
    }

    fn roundtrip_dod_i64(v: &[i64]) {
        let mut s = Sink::new();
        encode_dod_i64(v, &mut s);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert_eq!(decode_dod_i64(&mut c, v.len()).unwrap(), v);
    }

    #[test]
    fn delta_round_trip_at_boundaries() {
        roundtrip_delta_i64(&[]);
        roundtrip_delta_i64(&[42]);
        let alt: Vec<i64> = (0..200)
            .map(|i| if i % 2 == 0 { i64::MIN } else { i64::MAX })
            .collect();
        roundtrip_delta_i64(&alt);

        let mut s = Sink::new();
        encode_delta_u64(&[u64::MAX, 0, u64::MAX], &mut s);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert_eq!(
            decode_delta_u64(&mut c, 3).unwrap(),
            vec![u64::MAX, 0, u64::MAX]
        );
    }

    #[test]
    fn dod_round_trip_at_boundaries() {
        roundtrip_dod_i64(&[]);
        roundtrip_dod_i64(&[42]);
        roundtrip_dod_i64(&[1, 2]);

        // Monotone timestamps at a 1s step with jitter.
        let mut t = 1_700_000_000_000_000_000i64;
        let mut rng = SplitMix64::new(77);
        let mut ts = Vec::with_capacity(1000);
        for _ in 0..1000 {
            ts.push(t);
            t = t.wrapping_add(1_000_000_000 + (rng.range(0, 3) as i64 - 1));
        }
        roundtrip_dod_i64(&ts);
    }

    #[test]
    fn dod_timestamps_compress_well() {
        let ts: Vec<i64> = (0..4096)
            .map(|i| 1_700_000_000_000_000_000i64 + i * 1_000_000_000)
            .collect();
        let mut s = Sink::new();
        encode_dod_i64(&ts, &mut s);
        assert!(s.len() < 64, "encoded len {}", s.len());
    }

    #[test]
    fn hostile_bytes_never_panic() {
        let mut rng = SplitMix64::new(555);
        let iters = if cfg!(miri) { 30 } else { 1000 };
        for _ in 0..iters {
            let len = rng.range(0, 32) as usize;
            let buf: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
            for rows in [0usize, 1, 2, 16] {
                let mut c = Cursor::new(&buf);
                let _ = decode_delta_i64(&mut c, rows);
                let mut c = Cursor::new(&buf);
                let _ = decode_dod_i64(&mut c, rows);
            }
        }
    }

    #[test]
    fn max_decode_rows_with_tiny_payload_is_err() {
        let buf = [0xffu8, 0xff, 0xff];
        let mut c = Cursor::new(&buf);
        assert!(decode_delta_i64(&mut c, crate::segment::MAX_DECODE_ROWS).is_err());
        let mut c = Cursor::new(&buf);
        assert!(decode_dod_i64(&mut c, crate::segment::MAX_DECODE_ROWS).is_err());
    }
}
