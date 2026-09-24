//! Validity section (SPEC §5, D0008): tag 0 (no bitmap), tag 1 (raw words) or tag 2 (runs),
//! whichever of 1/2 is smaller, tie going to 1 so a `Some` bitmap always round-trips as `Some`.

use crate::exec::Bitmap;

use crate::storage::segment::error::DecodeError;
use crate::storage::segment::wire::{Cursor, Sink};

pub(super) fn encode_validity(v: Option<&Bitmap>, rows: usize, out: &mut Sink) {
    let Some(bm) = v else {
        out.u8(0);
        return;
    };
    debug_assert_eq!(bm.len(), rows, "validity bitmap length must match rows");
    let tag1 = words_payload(bm);
    let tag2 = runs_payload(bm);
    if tag1.len() <= tag2.len() {
        out.u8(1);
        out.raw(&tag1);
    } else {
        out.u8(2);
        out.raw(&tag2);
    }
}

pub(super) fn decode_validity(
    cur: &mut Cursor,
    rows: usize,
) -> Result<Option<Bitmap>, DecodeError> {
    match cur.u8()? {
        0 => Ok(None),
        1 => {
            let n = cur.guard_len(rows.div_ceil(64) as u64, 8)?;
            let mut words = Vec::with_capacity(n);
            for _ in 0..n {
                words.push(cur.u64()?);
            }
            Bitmap::from_words(words, rows)
                .map(Some)
                .ok_or(DecodeError::Malformed("bad validity bitmap"))
        }
        2 => decode_runs(cur, rows).map(Some),
        _ => Err(DecodeError::Malformed("unknown validity tag")),
    }
}

fn words_payload(bm: &Bitmap) -> Vec<u8> {
    let mut s = Sink::new();
    for &w in bm.words() {
        s.u64(w);
    }
    s.into_vec()
}

/// Runs alternate starting from "valid" (run 0 may be 0 when the column starts null).
fn runs_payload(bm: &Bitmap) -> Vec<u8> {
    let mut s = Sink::new();
    let runs = compute_runs(bm);
    s.uvarint(runs.len() as u64);
    for r in runs {
        s.uvarint(r);
    }
    s.into_vec()
}

fn compute_runs(bm: &Bitmap) -> Vec<u64> {
    let mut runs = Vec::new();
    let mut state = true;
    let mut run = 0u64;
    for i in 0..bm.len() {
        if bm.get(i) == state {
            run += 1;
        } else {
            runs.push(run);
            run = 1;
            state = !state;
        }
    }
    runs.push(run);
    runs
}

/// Rebuilds the bitmap run by run, bailing as soon as the running sum would pass `rows` so a
/// hostile single run length can never drive an unbounded push loop.
fn decode_runs(cur: &mut Cursor, rows: usize) -> Result<Bitmap, DecodeError> {
    let nruns = cur.uvarint()?;
    let mut bm = Bitmap::new_valid(0);
    let mut state = true;
    let mut pos: u64 = 0;
    for _ in 0..nruns {
        let run = cur.uvarint()?;
        pos = pos
            .checked_add(run)
            .filter(|&p| p <= rows as u64)
            .ok_or(DecodeError::Malformed("validity runs exceed row count"))?;
        for _ in 0..run {
            bm.push(state);
        }
        state = !state;
    }
    if pos != rows as u64 {
        return Err(DecodeError::Malformed("validity runs do not sum to rows"));
    }
    Ok(bm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bitmap_from_bits(bits: &[bool]) -> Bitmap {
        let mut bm = Bitmap::new_valid(0);
        for &b in bits {
            bm.push(b);
        }
        bm
    }

    #[test]
    fn no_bitmap_round_trips_as_none() {
        let mut s = Sink::new();
        encode_validity(None, 5, &mut s);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert_eq!(decode_validity(&mut c, 5).unwrap(), None);
    }

    #[test]
    fn some_bitmap_with_no_nulls_round_trips_as_some() {
        let bm = bitmap_from_bits(&[true, true, true]);
        let mut s = Sink::new();
        encode_validity(Some(&bm), 3, &mut s);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert_eq!(decode_validity(&mut c, 3).unwrap(), Some(bm));
    }

    #[test]
    fn mixed_bitmap_round_trips() {
        let bits = [true, false, false, true, true, false, true];
        let bm = bitmap_from_bits(&bits);
        let mut s = Sink::new();
        encode_validity(Some(&bm), bits.len(), &mut s);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert_eq!(decode_validity(&mut c, bits.len()).unwrap(), Some(bm));
    }

    #[test]
    fn tag2_run_sum_mismatch_is_malformed() {
        let mut s = Sink::new();
        s.u8(2);
        s.uvarint(1); // one run
        s.uvarint(4); // claims 4 rows, but caller says 5
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert!(matches!(
            decode_validity(&mut c, 5),
            Err(DecodeError::Malformed(_))
        ));
    }

    #[test]
    fn unknown_tag_is_malformed() {
        let buf = [9u8];
        let mut c = Cursor::new(&buf);
        assert!(matches!(
            decode_validity(&mut c, 0),
            Err(DecodeError::Malformed(_))
        ));
    }
}
