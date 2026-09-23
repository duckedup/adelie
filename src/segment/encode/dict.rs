//! DICT (encoding id 1, SPEC §5): STRING/BYTES. Distinct valid-row slices in first-occurrence
//! order, then FoR-packed per-row codes; a null row's code is 0 and decodes to an empty range.

use std::collections::HashMap;

use crate::exec::Bitmap;
use crate::segment::error::DecodeError;
use crate::segment::wire::{Cursor, Sink};

fn is_valid(validity: Option<&Bitmap>, i: usize) -> bool {
    validity.is_none_or(|v| v.get(i))
}

pub(super) fn encode(offsets: &[u32], data: &[u8], validity: Option<&Bitmap>, out: &mut Sink) {
    let rows = offsets.len().saturating_sub(1);
    // Insertion-order entries plus a lookup map; the map's own iteration order never
    // reaches the output (only `entries`, built in first-occurrence order, does).
    let mut lookup: HashMap<&[u8], u32> = HashMap::new();
    let mut entries: Vec<&[u8]> = Vec::new();
    let mut codes: Vec<u64> = Vec::with_capacity(rows);
    for i in 0..rows {
        if !is_valid(validity, i) {
            codes.push(0);
            continue;
        }
        let slice = &data[offsets[i] as usize..offsets[i + 1] as usize];
        let code = *lookup.entry(slice).or_insert_with(|| {
            entries.push(slice);
            (entries.len() - 1) as u32
        });
        codes.push(code as u64);
    }
    out.uvarint(entries.len() as u64);
    for &e in &entries {
        out.bytes(e);
    }
    super::write_for(&codes, out);
}

pub(super) fn decode(
    cur: &mut Cursor,
    rows: usize,
    validity: Option<&Bitmap>,
) -> Result<(Vec<u32>, Vec<u8>), DecodeError> {
    let ndict = cur.uvarint()?;
    if ndict > rows as u64 {
        return Err(DecodeError::Malformed("dict: ndict exceeds rows"));
    }
    let ndict = cur.guard_len(ndict, 1)?;
    let valid_count = validity.map_or(rows, |v| v.count_valid());
    if valid_count == 0 && ndict > 0 {
        return Err(DecodeError::Malformed("dict: entries with no valid rows"));
    }
    let mut entries: Vec<&[u8]> = Vec::with_capacity(ndict);
    for _ in 0..ndict {
        entries.push(cur.bytes()?);
    }
    let codes = super::read_for(cur, rows)?;

    let mut offsets = Vec::with_capacity(rows + 1);
    let mut data = Vec::new();
    offsets.push(0u32);
    for i in 0..rows {
        if is_valid(validity, i) {
            let code = codes[i];
            if code >= ndict as u64 {
                return Err(DecodeError::Malformed("dict: code out of range"));
            }
            data.extend_from_slice(entries[code as usize]);
        }
        let off =
            u32::try_from(data.len()).map_err(|_| DecodeError::Malformed("dict: too much data"))?;
        offsets.push(off);
    }
    Ok((offsets, data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Column, OwnedValues};
    use crate::types::DataType;
    use adelie_harness::rng::SplitMix64;

    /// Builds STRING offsets/data from `rows` (`None` = null), and the matching validity.
    fn build(rows: &[Option<&str>]) -> (Vec<u32>, Vec<u8>, Option<Bitmap>) {
        let mut offsets = vec![0u32];
        let mut data = Vec::new();
        let mut bm = Bitmap::new_valid(rows.len());
        let mut any_null = false;
        for (i, r) in rows.iter().enumerate() {
            match r {
                Some(s) => data.extend_from_slice(s.as_bytes()),
                None => {
                    bm.set(i, false);
                    any_null = true;
                }
            }
            offsets.push(data.len() as u32);
        }
        (offsets, data, any_null.then_some(bm))
    }

    fn roundtrip(rows: &[Option<&str>]) {
        let (offsets, data, validity) = build(rows);
        let mut s = Sink::new();
        encode(&offsets, &data, validity.as_ref(), &mut s);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        let (got_offsets, got_data) = decode(&mut c, rows.len(), validity.as_ref()).unwrap();
        assert_eq!(got_offsets, offsets);
        assert_eq!(got_data, data);
        let col = Column::from_parts(
            DataType::String,
            OwnedValues::String {
                offsets: got_offsets,
                data: got_data,
            },
            validity,
        );
        assert!(col.is_ok());
    }

    #[test]
    fn round_trip_at_boundaries() {
        roundtrip(&[]);
        roundtrip(&[Some("a")]);
        roundtrip(&[Some("a"), Some("a"), Some("a")]);
        roundtrip(&[Some("x"), None, Some("y"), None, Some("x")]);
        roundtrip(&[None, None, None]);
    }

    #[test]
    fn three_distinct_strings_compress_well() {
        let rows: Vec<Option<&str>> = (0..4096)
            .map(|i| Some(["alpha", "beta", "gamma"][i % 3]))
            .collect();
        let (offsets, data, validity) = build(&rows);
        let mut s = Sink::new();
        encode(&offsets, &data, validity.as_ref(), &mut s);
        assert!(s.len() < 1100, "encoded len {}", s.len());
    }

    #[test]
    fn decode_rejects_ndict_with_no_valid_rows() {
        let mut s = Sink::new();
        s.uvarint(1);
        s.bytes(b"x");
        super::super::write_for(&[0], &mut s);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        let bm = Bitmap::new_null(1);
        assert!(decode(&mut c, 1, Some(&bm)).is_err());
    }

    #[test]
    fn decode_rejects_code_out_of_range() {
        let mut s = Sink::new();
        s.uvarint(1);
        s.bytes(b"x");
        super::super::write_for(&[5], &mut s);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert!(decode(&mut c, 1, None).is_err());
    }

    #[test]
    fn hostile_bytes_never_panic() {
        let mut rng = SplitMix64::new(31337);
        let iters = if cfg!(miri) { 30 } else { 1000 };
        for _ in 0..iters {
            let len = rng.range(0, 32) as usize;
            let buf: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
            let mut c = Cursor::new(&buf);
            let _ = decode(&mut c, 16, None);
        }
    }

    #[test]
    fn max_decode_rows_with_tiny_payload_is_err() {
        let buf = [0xffu8, 0xff, 0xff];
        let mut c = Cursor::new(&buf);
        assert!(decode(&mut c, crate::segment::MAX_DECODE_ROWS, None).is_err());
    }
}
