//! `Reader` (SPEC §5, D0008): opens a `.seg` file, validates its footer against the
//! body, and decodes columns, row groups and skip indexes on demand.

use crate::exec::{Batch, Column, Field};
use crate::types::Value;

use super::crc::crc32c;
use super::directory::RawIndexEntry;
use super::encode::{Encoding, decode_column};
use super::error::{Error, Located};
use super::footer::{Footer, TrailerError, decode_footer, read_trailer};
use super::index::{self, IndexKind, SkipIndex};
use super::{FORMAT_VERSION, HEADER_LEN, MAX_DECODE_ROWS, SEGMENT_MAGIC};

/// One row group's stats for one column (SPEC §5 `chunk_meta`). `len` is the chunk's encoded
/// byte length, not its row count (every chunk in a row group shares the row group's rows).
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkMeta {
    pub encoding: Encoding,
    pub len: u64,
    pub null_count: u64,
    pub min: Option<Value>,
    pub max: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RowGroupMeta {
    pub rows: u64,
    pub chunks: Vec<ChunkMeta>,
}

/// A validated index directory entry. `offset`/`len`/`crc` stay private: `load_index` is the
/// only way to reach the blob they name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub kind: IndexKind,
    pub row_group: Option<usize>,
    pub columns: Vec<usize>,
    offset: u64,
    len: u64,
    crc: u32,
}

/// An open, validated `.seg` file (SPEC §5). Every offset stored here has already been
/// checked to lie in `[HEADER_LEN, footer_start)`, so reads never need to re-check bounds.
pub struct Reader<B: AsRef<[u8]>> {
    name: String,
    bytes: B,
    fields: Vec<Field>,
    row_groups: Vec<RowGroupMeta>,
    /// `[row_group][column] = (offset, len, crc)`, parallel to `row_groups[rg].chunks`.
    chunk_ranges: Vec<Vec<(u64, u64, u32)>>,
    indexes: Vec<IndexEntry>,
    footer_crc: u32,
}

impl<B: AsRef<[u8]>> Reader<B> {
    /// Validates the trailer, the header, the footer, every chunk's encoding/range, and
    /// every index entry's range/ordinals, in that order (SPEC §5). See the module tests.
    pub fn open(name: impl Into<String>, bytes: B) -> Result<Self, Error> {
        let name = name.into();
        let buf = bytes.as_ref();
        let footer_range =
            read_trailer(buf, SEGMENT_MAGIC, HEADER_LEN).map_err(|e| trailer_err(e, &name))?;
        check_header(buf, SEGMENT_MAGIC, &name)?;

        let footer_start = footer_range.start as u64;
        let footer: Footer = decode_footer(&buf[footer_range.clone()]).map_err(|e| {
            let Located { err, column } = e;
            err.at(&name, column.as_deref())
        })?;

        let mut row_groups = Vec::with_capacity(footer.row_groups.len());
        let mut chunk_ranges = Vec::with_capacity(footer.row_groups.len());
        for rg in &footer.row_groups {
            if rg.rows > MAX_DECODE_ROWS as u64 {
                return Err(malformed(
                    &name,
                    None,
                    "row group rows exceeds MAX_DECODE_ROWS",
                ));
            }
            let mut chunks = Vec::with_capacity(rg.chunks.len());
            let mut ranges = Vec::with_capacity(rg.chunks.len());
            for (chunk, field) in rg.chunks.iter().zip(&footer.fields) {
                let encoding =
                    Encoding::from_id(chunk.encoding).ok_or_else(|| Error::UnknownEncoding {
                        segment: name.clone(),
                        column: field.name.clone(),
                        id: chunk.encoding,
                    })?;
                if !encoding.applies_to(&field.ty) {
                    return Err(malformed(
                        &name,
                        Some(field.name.as_str()),
                        "encoding does not apply to column type",
                    ));
                }
                chunk
                    .offset
                    .checked_add(chunk.len)
                    .filter(|&e| chunk.offset >= HEADER_LEN as u64 && e <= footer_start)
                    .ok_or_else(|| {
                        malformed(
                            &name,
                            Some(field.name.as_str()),
                            "chunk range out of bounds",
                        )
                    })?;
                chunks.push(ChunkMeta {
                    encoding,
                    len: chunk.len,
                    null_count: chunk.null_count,
                    min: chunk.min.clone(),
                    max: chunk.max.clone(),
                });
                ranges.push((chunk.offset, chunk.len, chunk.crc));
            }
            row_groups.push(RowGroupMeta {
                rows: rg.rows,
                chunks,
            });
            chunk_ranges.push(ranges);
        }

        let body = HEADER_LEN as u64..footer_start;
        let indexes = resolve_entries(
            footer.indexes,
            &footer.fields,
            row_groups.len(),
            body,
            &name,
        )?;
        let footer_crc = crc32c(&buf[footer_range]);

        Ok(Reader {
            name,
            bytes,
            fields: footer.fields,
            row_groups,
            chunk_ranges,
            indexes,
            footer_crc,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    pub fn rows(&self) -> u64 {
        self.row_groups.iter().map(|rg| rg.rows).sum()
    }

    pub fn footer_crc(&self) -> u32 {
        self.footer_crc
    }

    pub fn row_groups(&self) -> &[RowGroupMeta] {
        &self.row_groups
    }

    /// `Usage` for an out-of-range `row_group`/`column`. `CorruptChunk` if the stored CRC no
    /// longer matches; the CRC is per chunk, so a flipped byte never affects a sibling chunk.
    pub fn read_column(&self, row_group: usize, column: usize) -> Result<Column, Error> {
        let rg_meta = self
            .row_groups
            .get(row_group)
            .ok_or_else(|| Error::Usage(format!("row group {row_group} out of range")))?;
        let &(offset, len, crc) = self
            .chunk_ranges
            .get(row_group)
            .and_then(|ranges| ranges.get(column))
            .ok_or_else(|| Error::Usage(format!("column {column} out of range")))?;

        let buf = self.bytes.as_ref();
        let bytes = &buf[offset as usize..(offset + len) as usize];
        if crc32c(bytes) != crc {
            return Err(Error::CorruptChunk {
                segment: self.name.clone(),
                column: self.fields[column].name.clone(),
                row_group,
            });
        }
        let field = &self.fields[column];
        let rows = rg_meta.rows as usize;
        let col = decode_column(&field.ty, rows, rg_meta.chunks[column].encoding.id(), bytes)
            .map_err(|e| e.at(&self.name, Some(field.name.as_str())))?;
        if col.len() != rows {
            return Err(malformed(
                &self.name,
                Some(field.name.as_str()),
                "decoded length does not match row group rows",
            ));
        }
        Ok(col)
    }

    /// `Usage` for a bad projection (out-of-range or duplicate column), surfaced through
    /// `Batch::new`.
    pub fn read_row_group(&self, row_group: usize, projection: &[usize]) -> Result<Batch, Error> {
        let mut fields = Vec::with_capacity(projection.len());
        let mut columns = Vec::with_capacity(projection.len());
        for &column in projection {
            columns.push(self.read_column(row_group, column)?);
            fields.push(self.fields[column].clone());
        }
        Batch::new(fields, columns).map_err(|e| Error::Usage(e.to_string()))
    }

    pub fn indexes(&self) -> &[IndexEntry] {
        &self.indexes
    }

    /// `CorruptIndex` if the stored CRC no longer matches; otherwise `index::load`.
    pub fn load_index(&self, entry: &IndexEntry) -> Result<SkipIndex, Error> {
        load_entry(self.bytes.as_ref(), entry, &self.fields, &self.name)
    }
}

/// Shared with `idx.rs`: resolves a raw index directory into validated entries. Unknown
/// kinds are dropped before anything else about them (range, ordinals, type) is examined.
pub(crate) fn resolve_entries(
    raw: Vec<RawIndexEntry>,
    fields: &[Field],
    row_groups: usize,
    body: std::ops::Range<u64>,
    segment: &str,
) -> Result<Vec<IndexEntry>, Error> {
    let mut out = Vec::with_capacity(raw.len());
    for e in raw {
        let Some(kind) = IndexKind::from_id(e.kind) else {
            continue;
        };
        if e.row_group.is_some_and(|rg| rg >= row_groups) {
            return Err(malformed(
                segment,
                None,
                "index entry row group out of range",
            ));
        }
        if e.columns.is_empty() {
            return Err(malformed(segment, None, "index entry has no columns"));
        }
        for &c in &e.columns {
            let Some(field) = fields.get(c) else {
                return Err(malformed(segment, None, "index entry column out of range"));
            };
            if !kind.applies_to(&field.ty) {
                return Err(malformed(
                    segment,
                    None,
                    "index kind does not apply to column type",
                ));
            }
        }
        e.offset
            .checked_add(e.len)
            .filter(|&end| e.offset >= body.start && end <= body.end)
            .ok_or_else(|| malformed(segment, None, "index entry range out of bounds"))?;
        out.push(IndexEntry {
            kind,
            row_group: e.row_group,
            columns: e.columns,
            offset: e.offset,
            len: e.len,
            crc: e.crc,
        });
    }
    Ok(out)
}

/// Shared with `idx.rs`: loads and CRC-checks one index entry's blob, naming its first
/// column on either failure.
pub(crate) fn load_entry(
    bytes: &[u8],
    entry: &IndexEntry,
    fields: &[Field],
    segment: &str,
) -> Result<SkipIndex, Error> {
    let field = &fields[entry.columns[0]];
    let blob = &bytes[entry.offset as usize..(entry.offset + entry.len) as usize];
    if crc32c(blob) != entry.crc {
        return Err(Error::CorruptIndex {
            segment: segment.to_string(),
            column: field.name.clone(),
            kind: entry.kind,
        });
    }
    index::load(entry.kind, &field.ty, blob).map_err(|e| e.at(segment, Some(field.name.as_str())))
}

/// Shared with `idx.rs`: both file kinds start with `magic (6) + FORMAT_VERSION (2)`.
pub(crate) fn check_header(buf: &[u8], magic: [u8; 6], name: &str) -> Result<(), Error> {
    let found: [u8; 6] = buf[..6].try_into().unwrap();
    if found != magic {
        return Err(Error::BadMagic {
            segment: name.to_string(),
        });
    }
    let version = u16::from_le_bytes(buf[6..8].try_into().unwrap());
    if version > FORMAT_VERSION {
        return Err(Error::UnsupportedVersion {
            segment: name.to_string(),
            version,
        });
    }
    Ok(())
}

/// Shared with `idx.rs`: lifts a trailer-framing failure to the public error.
pub(crate) fn trailer_err(e: TrailerError, segment: &str) -> Error {
    match e {
        TrailerError::BadMagic => Error::BadMagic {
            segment: segment.to_string(),
        },
        TrailerError::UnsupportedVersion(version) => Error::UnsupportedVersion {
            segment: segment.to_string(),
            version,
        },
        TrailerError::Truncated => Error::Truncated {
            segment: segment.to_string(),
        },
        TrailerError::CorruptFooter => Error::CorruptFooter {
            segment: segment.to_string(),
        },
    }
}

fn malformed(segment: &str, column: Option<&str>, detail: &str) -> Error {
    Error::Malformed {
        segment: segment.to_string(),
        column: column.map(str::to_string),
        detail: detail.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DataType;

    use super::super::IDX_MAGIC;
    use super::super::directory::{decode_directory, encode_directory};
    use super::super::footer::{encode_footer, encode_footer_with_raw_type, write_trailer};
    use super::super::idx::{IdxReader, IdxWriter};
    use super::super::wire::{Cursor, Sink};
    use super::super::writer::{Writer, WriterOptions};

    fn sample_fields() -> Vec<Field> {
        vec![
            Field {
                name: "a".to_string(),
                ty: DataType::Int64,
            },
            Field {
                name: "s".to_string(),
                ty: DataType::String,
            },
        ]
    }

    fn sample_batch(a: &[i64], s: &[&str]) -> Batch {
        let avals: Vec<Value> = a.iter().copied().map(Value::Int64).collect();
        let svals: Vec<Value> = s.iter().map(|x| Value::String((*x).to_string())).collect();
        Batch::new(
            sample_fields(),
            vec![
                Column::from_values(&DataType::Int64, &avals).unwrap(),
                Column::from_values(&DataType::String, &svals).unwrap(),
            ],
        )
        .unwrap()
    }

    /// 11 rows split 4/4/3 so `row_group_rows = 4` gives exactly 3 row groups, the last short.
    fn sample_batches(a_base: i64, s_prefix: &str) -> Vec<Batch> {
        let a: Vec<i64> = (a_base..a_base + 11).collect();
        let s: Vec<String> = (0..11).map(|i| format!("{s_prefix}{i}")).collect();
        [(0, 4), (4, 8), (8, 11)]
            .into_iter()
            .map(|(start, end)| {
                let s_refs: Vec<&str> = s[start..end].iter().map(String::as_str).collect();
                sample_batch(&a[start..end], &s_refs)
            })
            .collect()
    }

    fn write_segment(batches: &[Batch]) -> Vec<u8> {
        let opts = WriterOptions {
            row_group_rows: 4,
            pins: Vec::new(),
            indexes: vec![("a".to_string(), IndexKind::Bloom)],
        };
        let mut w = Writer::new(Vec::new(), sample_fields(), opts).unwrap();
        for b in batches {
            w.push(b).unwrap();
        }
        w.finish().unwrap().0
    }

    fn sample_segment() -> (Vec<u8>, Vec<Batch>) {
        let batches = sample_batches(0, "s");
        (write_segment(&batches), batches)
    }

    /// `bytes[..footer_start]` followed by a freshly-framed `footer` (recomputes the CRC).
    fn reframe(bytes: &[u8], footer: Vec<u8>) -> Vec<u8> {
        let footer_start = read_trailer(bytes, SEGMENT_MAGIC, HEADER_LEN)
            .unwrap()
            .start;
        let mut out = bytes[..footer_start].to_vec();
        write_trailer(&mut out, SEGMENT_MAGIC, &footer);
        out
    }

    fn garbage_entry(kind: u64) -> RawIndexEntry {
        RawIndexEntry {
            kind,
            row_group: Some(99),
            columns: vec![42],
            offset: 0,
            len: u64::MAX,
            crc: 0,
            params: vec![1, 2, 3],
        }
    }

    #[test]
    fn unknown_type_id_is_rejected_by_name() {
        let (bytes, _batches) = sample_segment();
        let range = read_trailer(&bytes, SEGMENT_MAGIC, HEADER_LEN).unwrap();
        let footer = decode_footer(&bytes[range]).unwrap();

        let bad = encode_footer_with_raw_type(&footer, 1, 999, &[]);
        let patched = reframe(&bytes, bad);
        let err = Reader::open("seg1", patched).unwrap_err();
        assert!(matches!(&err, Error::UnknownTypeId { column, id: 999, .. } if column == "s"));
        let msg = err.to_string();
        assert!(msg.contains("seg1"));
        assert!(msg.contains('s'));
        assert!(msg.contains("999"));

        // Control: the real STRING id (6) still opens, and column s round-trips.
        let good = encode_footer_with_raw_type(&footer, 1, 6, &[]);
        let patched_good = reframe(&bytes, good);
        let control = Reader::open("seg1", bytes.clone()).unwrap();
        let reader = Reader::open("seg1", patched_good).unwrap();
        assert_eq!(
            reader.read_column(0, 1).unwrap(),
            control.read_column(0, 1).unwrap()
        );
    }

    #[test]
    fn unknown_index_kind_is_skipped_before_its_garbage_is_examined() {
        let (bytes, batches) = sample_segment();
        let range = read_trailer(&bytes, SEGMENT_MAGIC, HEADER_LEN).unwrap();
        let mut footer = decode_footer(&bytes[range]).unwrap();
        footer.indexes.push(garbage_entry(0x7FFF));
        let patched = reframe(&bytes, encode_footer(&footer));

        let reader = Reader::open("seg1", patched).unwrap();
        assert_eq!(reader.indexes().len(), 3);
        for entry in reader.indexes() {
            assert_eq!(entry.kind, IndexKind::Bloom);
            let rg = entry.row_group.unwrap();
            let idx = reader.load_index(entry).unwrap();
            let value = batches[rg].column(0).get(0);
            assert!(idx.might_contain(&value));
        }
        for (rg, batch) in batches.iter().enumerate() {
            assert_eq!(reader.read_row_group(rg, &[0, 1]).unwrap(), *batch);
        }

        // Control: the same garbage entry with a known kind (Bloom) makes open Malformed.
        let range2 = read_trailer(&bytes, SEGMENT_MAGIC, HEADER_LEN).unwrap();
        let mut footer2 = decode_footer(&bytes[range2]).unwrap();
        footer2.indexes.push(garbage_entry(1));
        let patched2 = reframe(&bytes, encode_footer(&footer2));
        let err = Reader::open("seg1", patched2).unwrap_err();
        assert!(matches!(err, Error::Malformed { .. }));
    }

    #[test]
    fn idx_unknown_index_kind_is_skipped() {
        let (bytes, _batches) = sample_segment();
        let seg = Reader::open("seg1", bytes).unwrap();
        let idx_bytes = IdxWriter::build(&seg, &[(1, IndexKind::Ngram)]).unwrap();

        let range = read_trailer(&idx_bytes, IDX_MAGIC, 12).unwrap();
        let footer_start = range.start;
        let mut cur = Cursor::new(&idx_bytes[range]);
        let mut body = cur.record().unwrap();
        let mut raw = decode_directory(&mut body).unwrap();
        raw.push(garbage_entry(0x7FFF));
        let mut sink = Sink::new();
        sink.record(|r| encode_directory(&raw, r));
        let mut patched = idx_bytes[..footer_start].to_vec();
        write_trailer(&mut patched, IDX_MAGIC, &sink.into_vec());

        let idx_reader = IdxReader::open("seg1.idx", patched, &seg).unwrap();
        assert_eq!(idx_reader.indexes().len(), 3);
        for entry in idx_reader.indexes() {
            assert_eq!(entry.kind, IndexKind::Ngram);
        }
    }

    #[test]
    fn corrupt_chunk_names_its_column_and_row_group() {
        let (bytes, _batches) = sample_segment();
        let range = read_trailer(&bytes, SEGMENT_MAGIC, HEADER_LEN).unwrap();
        let footer_start = range.start;
        let footer = decode_footer(&bytes[range]).unwrap();
        let chunk = &footer.row_groups[1].chunks[1];

        let mut patched = bytes.clone();
        patched[chunk.offset as usize] ^= 0xFF;
        let reader = Reader::open("seg1", patched).unwrap();
        let err = reader.read_column(1, 1).unwrap_err();
        assert!(matches!(&err, Error::CorruptChunk { column, row_group: 1, .. } if column == "s"));
        assert!(reader.read_column(1, 0).is_ok());
        assert!(reader.read_column(0, 1).is_ok());

        let mut footer_flip = bytes.clone();
        footer_flip[footer_start] ^= 0xFF;
        let err = Reader::open("seg1", footer_flip).unwrap_err();
        assert!(matches!(err, Error::CorruptFooter { .. }));
    }

    #[test]
    fn unknown_encoding_id_and_inapplicable_encoding_are_rejected() {
        let (bytes, _batches) = sample_segment();
        let range = read_trailer(&bytes, SEGMENT_MAGIC, HEADER_LEN).unwrap();
        let footer = decode_footer(&bytes[range]).unwrap();

        let mut unknown = footer.clone();
        unknown.row_groups[0].chunks[0].encoding = 99;
        let patched = reframe(&bytes, encode_footer(&unknown));
        let err = Reader::open("seg1", patched).unwrap_err();
        assert!(matches!(&err, Error::UnknownEncoding { column, id: 99, .. } if column == "a"));

        let mut inapplicable = footer;
        inapplicable.row_groups[0].chunks[0].encoding = Encoding::Xor.id();
        let patched2 = reframe(&bytes, encode_footer(&inapplicable));
        let err2 = Reader::open("seg1", patched2).unwrap_err();
        assert!(matches!(err2, Error::Malformed { .. }));
    }

    #[test]
    fn idx_reader_open_against_a_different_segment_is_idx_mismatch() {
        let (bytes_a, _) = sample_segment();
        let seg_a = Reader::open("a", bytes_a).unwrap();
        let idx_bytes = IdxWriter::build(&seg_a, &[(0, IndexKind::Bloom)]).unwrap();

        let bytes_b = write_segment(&sample_batches(1000, "t"));
        let seg_b = Reader::open("b", bytes_b).unwrap();

        let err = IdxReader::open("a.idx", idx_bytes, &seg_b).unwrap_err();
        assert!(matches!(err, Error::IdxMismatch { .. }));
    }
}
