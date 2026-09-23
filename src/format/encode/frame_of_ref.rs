//! FOR (encoding id 3, SPEC §5): values mapped to an order-preserving `u64` (signed columns
//! flip the sign bit), then one FoR block (`super::write_for`/`read_for`).

use crate::format::error::DecodeError;
use crate::format::wire::{Cursor, Sink};

const SIGN: u64 = 1 << 63;

pub(super) fn encode_i64(v: &[i64], out: &mut Sink) {
    let mapped: Vec<u64> = v.iter().map(|&x| (x as u64) ^ SIGN).collect();
    super::write_for(&mapped, out);
}

pub(super) fn decode_i64(cur: &mut Cursor, rows: usize) -> Result<Vec<i64>, DecodeError> {
    let mapped = super::read_for(cur, rows)?;
    Ok(mapped.into_iter().map(|u| (u ^ SIGN) as i64).collect())
}

pub(super) fn encode_u64(v: &[u64], out: &mut Sink) {
    super::write_for(v, out);
}

pub(super) fn decode_u64(cur: &mut Cursor, rows: usize) -> Result<Vec<u64>, DecodeError> {
    super::read_for(cur, rows)
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
        roundtrip_i64(&[0]);
        roundtrip_i64(&[3; 10]);
        let alt: Vec<i64> = (0..200)
            .map(|i| if i % 2 == 0 { i64::MIN } else { i64::MAX })
            .collect();
        roundtrip_i64(&alt);
    }

    #[test]
    fn u64_round_trip_at_boundaries() {
        roundtrip_u64(&[]);
        roundtrip_u64(&[0]);
        roundtrip_u64(&[u64::MAX; 10]);
    }

    #[test]
    fn ordering_is_preserved_by_the_map() {
        let mut sorted = [i64::MIN, -5, -1, 0, 1, 5, i64::MAX];
        sorted.sort();
        let mapped: Vec<u64> = sorted.iter().map(|&x| (x as u64) ^ SIGN).collect();
        let mut mapped_sorted = mapped.clone();
        mapped_sorted.sort();
        assert_eq!(mapped, mapped_sorted);
    }

    #[test]
    fn hostile_bytes_never_panic() {
        let mut rng = SplitMix64::new(4242);
        let iters = if cfg!(miri) { 30 } else { 1000 };
        for _ in 0..iters {
            let len = rng.range(0, 32) as usize;
            let buf: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
            let mut c = Cursor::new(&buf);
            let _ = decode_i64(&mut c, 16);
            let mut c = Cursor::new(&buf);
            let _ = decode_u64(&mut c, 16);
        }
    }

    #[test]
    fn max_decode_rows_with_tiny_payload_is_err() {
        let buf = [0xffu8, 0xff, 0xff];
        let mut c = Cursor::new(&buf);
        assert!(decode_i64(&mut c, crate::format::MAX_DECODE_ROWS).is_err());
    }
}
