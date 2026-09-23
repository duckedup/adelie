//! The generic index directory (SPEC §18 hook 2), shared by the segment footer and `.idx`.
//! Decode keeps every entry, including unknown kinds, exactly as read; filtering by kind
//! and validating ranges/ordinals is the reader's job, not this codec's.

use super::error::DecodeError;
use super::wire::{Cursor, Sink};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawIndexEntry {
    pub kind: u64,
    pub row_group: Option<usize>,
    pub columns: Vec<usize>,
    pub offset: u64,
    pub len: u64,
    pub crc: u32,
    pub params: Vec<u8>,
}

/// `index_directory := uvarint n, n × record(index_entry)`.
pub(crate) fn encode_directory(entries: &[RawIndexEntry], out: &mut Sink) {
    out.uvarint(entries.len() as u64);
    for e in entries {
        out.record(|r| {
            r.uvarint(e.kind);
            r.uvarint(e.row_group.map(|rg| rg as u64 + 1).unwrap_or(0));
            r.uvarint(e.columns.len() as u64);
            for &c in &e.columns {
                r.uvarint(c as u64);
            }
            r.uvarint(e.offset);
            r.uvarint(e.len);
            r.u32(e.crc);
            r.bytes(&e.params);
        });
    }
}

pub(crate) fn decode_directory(cur: &mut Cursor) -> Result<Vec<RawIndexEntry>, DecodeError> {
    let n = cur.uvarint()?;
    let n = cur.guard_len(n, 1)?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let mut r = cur.record()?;
        let kind = r.uvarint()?;
        let rg_plus_1 = r.uvarint()?;
        let row_group = if rg_plus_1 == 0 { None } else { Some((rg_plus_1 - 1) as usize) };
        let ncols = r.uvarint()?;
        let ncols = r.guard_len(ncols, 1)?;
        let mut columns = Vec::with_capacity(ncols);
        for _ in 0..ncols {
            columns.push(r.uvarint()? as usize);
        }
        let offset = r.uvarint()?;
        let len = r.uvarint()?;
        let crc = r.u32()?;
        let params = r.bytes()?.to_vec();
        out.push(RawIndexEntry { kind, row_group, columns, offset, len, crc, params });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: u64, row_group: Option<usize>) -> RawIndexEntry {
        RawIndexEntry {
            kind,
            row_group,
            columns: vec![0, 2],
            offset: 100,
            len: 40,
            crc: 0xdead_beef,
            params: vec![1, 2, 3],
        }
    }

    #[test]
    fn round_trips_whole_segment_and_per_row_group_entries() {
        let entries = vec![entry(1, None), entry(0x7FFF, Some(3))];
        let mut s = Sink::new();
        encode_directory(&entries, &mut s);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert_eq!(decode_directory(&mut c).unwrap(), entries);
    }

    #[test]
    fn unknown_kind_is_kept_as_read() {
        let entries = vec![entry(999, Some(0))];
        let mut s = Sink::new();
        encode_directory(&entries, &mut s);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert_eq!(decode_directory(&mut c).unwrap(), entries);
    }

    #[test]
    fn empty_directory_round_trips() {
        let mut s = Sink::new();
        encode_directory(&[], &mut s);
        let buf = s.into_vec();
        let mut c = Cursor::new(&buf);
        assert_eq!(decode_directory(&mut c).unwrap(), Vec::new());
    }
}
