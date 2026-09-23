//! XOR / Gorilla (encoding id 6, SPEC §5): FLOAT64 only, bit-exact via `to_bits`. Each value
//! after the first is coded against the previous value's bit pattern.

use crate::segment::error::DecodeError;
use crate::segment::wire::{Cursor, Sink};

fn mask(bits: u32) -> u64 {
    if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    }
}

pub(super) fn encode(v: &[f64], out: &mut Sink) {
    let Some((&first, rest)) = v.split_first() else {
        return;
    };
    let mut bw = super::BitWriter::new();
    bw.write(first.to_bits(), 64);
    let mut prev_bits = first.to_bits();
    let mut window: Option<(u32, u32)> = None; // (leading, length), meaningful bits only
    for &f in rest {
        let bits = f.to_bits();
        let xor = bits ^ prev_bits;
        if xor == 0 {
            bw.write(0, 1);
        } else {
            let leading = xor.leading_zeros().min(63);
            let trailing = xor.trailing_zeros();
            let length = 64 - leading - trailing;
            let fits = window.is_some_and(|(w_lead, w_len)| {
                leading >= w_lead && trailing >= 64 - w_lead - w_len
            });
            if fits {
                let (w_lead, w_len) = window.unwrap();
                let w_trailing = 64 - w_lead - w_len;
                bw.write(1, 1);
                bw.write(0, 1);
                bw.write((xor >> w_trailing) & mask(w_len), w_len as u8);
            } else {
                bw.write(1, 1);
                bw.write(1, 1);
                bw.write(leading as u64, 6);
                bw.write(length as u64, 7);
                bw.write(xor >> trailing, length as u8);
                window = Some((leading, length));
            }
        }
        prev_bits = bits;
    }
    out.raw(&bw.finish());
}

pub(super) fn decode(cur: &mut Cursor, rows: usize) -> Result<Vec<f64>, DecodeError> {
    if rows == 0 {
        return Ok(Vec::new());
    }
    let rows = cur.guard_len(rows as u64, 0)?;
    let bytes = cur.raw(cur.remaining())?;
    let mut br = super::BitReader::new(bytes);
    let mut out = Vec::with_capacity(rows);
    let mut prev_bits = br.read(64)?;
    out.push(f64::from_bits(prev_bits));
    let mut window: Option<(u32, u32)> = None;
    for _ in 1..rows {
        let xor = if br.read(1)? == 0 {
            0u64
        } else if br.read(1)? == 0 {
            let (w_lead, w_len) = window.ok_or(DecodeError::Malformed("xor: no window yet"))?;
            let field = br.read(w_len as u8)?;
            field << (64 - w_lead - w_len)
        } else {
            let leading = br.read(6)? as u32;
            let length = br.read(7)? as u32;
            if length == 0 || length > 64 || leading + length > 64 {
                return Err(DecodeError::Malformed("xor: bad window"));
            }
            let field = br.read(length as u8)?;
            window = Some((leading, length));
            field << (64 - leading - length)
        };
        let bits = xor ^ prev_bits;
        out.push(f64::from_bits(bits));
        prev_bits = bits;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use adelie_harness::rng::SplitMix64;

    fn roundtrip(v: &[f64]) {
        let mut s = Sink::new();
        encode(v, &mut s);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        let got = decode(&mut c, v.len()).unwrap();
        assert_eq!(got.len(), v.len());
        for (a, b) in v.iter().zip(got.iter()) {
            assert_eq!(a.to_bits(), b.to_bits(), "{a} vs {b}");
        }
    }

    #[test]
    fn round_trip_at_boundaries() {
        roundtrip(&[]);
        roundtrip(&[1.0]);
        roundtrip(&[
            f64::NAN,
            -f64::NAN,
            f64::from_bits(0x7ff8_0000_0000_0001),
            -0.0,
            0.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MIN_POSITIVE / 2.0, // subnormal
            1.0,
            -1.0,
            1.0,
            -1.0,
        ]);

        let mut v = Vec::with_capacity(1000);
        let mut x = 100.0f64;
        let mut rng = SplitMix64::new(3);
        for _ in 0..1000 {
            v.push(x);
            x += rng.f64() * 0.01;
        }
        roundtrip(&v);
    }

    #[test]
    fn one_repeated_value_compresses_well() {
        let v = vec![std::f64::consts::PI; 4096];
        let mut s = Sink::new();
        encode(&v, &mut s);
        assert!(s.len() < 600, "encoded len {}", s.len());
    }

    #[test]
    fn hostile_bytes_never_panic() {
        let mut rng = SplitMix64::new(2024);
        let iters = if cfg!(miri) { 30 } else { 1000 };
        for _ in 0..iters {
            let len = rng.range(0, 32) as usize;
            let buf: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
            let mut c = Cursor::new(&buf);
            let _ = decode(&mut c, 16);
        }
    }

    #[test]
    fn max_decode_rows_with_tiny_payload_is_err() {
        let buf = [0xffu8, 0xff, 0xff];
        let mut c = Cursor::new(&buf);
        assert!(decode(&mut c, crate::segment::MAX_DECODE_ROWS).is_err());
    }
}
