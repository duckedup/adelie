//! The segment footer (SPEC §5, D0008): schema, per-row-group chunk metadata and the index
//! directory, plus the trailer framing shared by `.seg` and `.idx`.

use crate::exec::{ColumnStats, Field};
use crate::types::{DataType, Value};

use super::crc::crc32c;
use super::directory::{RawIndexEntry, decode_directory, encode_directory};
use super::error::{DecodeError, Located};
use super::type_id;
use super::value::{decode_opt, encode_opt};
use super::wire::{Cursor, Sink};
use super::{FORMAT_VERSION, STATS_MAX_BYTES, TRAILER_LEN};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Footer {
    pub fields: Vec<Field>,
    pub row_groups: Vec<RawRowGroup>,
    pub indexes: Vec<RawIndexEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RawRowGroup {
    pub rows: u64,
    pub chunks: Vec<RawChunk>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RawChunk {
    pub offset: u64,
    pub len: u64,
    pub crc: u32,
    pub encoding: u64,
    pub null_count: u64,
    pub min: Option<Value>,
    pub max: Option<Value>,
}

pub(crate) fn encode_footer(f: &Footer) -> Vec<u8> {
    let mut out = Sink::new();
    out.record(|r| encode_schema(&f.fields, r));
    out.record(|r| encode_row_groups(f, r));
    out.record(|r| encode_directory(&f.indexes, r));
    out.into_vec()
}

fn encode_schema(fields: &[Field], out: &mut Sink) {
    out.uvarint(fields.len() as u64);
    for field in fields {
        out.record(|r| {
            r.str(&field.name);
            type_id::encode_type(&field.ty, r);
        });
    }
}

fn encode_row_groups(f: &Footer, out: &mut Sink) {
    out.uvarint(f.row_groups.len() as u64);
    for rg in &f.row_groups {
        out.record(|r| {
            r.uvarint(rg.rows);
            for (chunk, field) in rg.chunks.iter().zip(&f.fields) {
                r.record(|cr| encode_chunk_meta(chunk, &field.ty, cr));
            }
        });
    }
}

fn encode_chunk_meta(chunk: &RawChunk, ty: &DataType, out: &mut Sink) {
    out.uvarint(chunk.offset);
    out.uvarint(chunk.len);
    out.u32(chunk.crc);
    out.uvarint(chunk.encoding);
    out.uvarint(chunk.null_count);
    encode_opt(chunk.min.as_ref(), ty, out);
    encode_opt(chunk.max.as_ref(), ty, out);
}

/// `Located.column` names the column when the failure is in its descriptor or a chunk of it.
pub(crate) fn decode_footer(bytes: &[u8]) -> Result<Footer, Located> {
    let mut cur = Cursor::new(bytes);
    let fields = decode_schema(&mut cur)?;
    let row_groups = decode_row_groups(&mut cur, &fields)?;
    let mut idx_body = cur.record().map_err(Located::from)?;
    let indexes = decode_directory(&mut idx_body).map_err(Located::from)?;
    Ok(Footer { fields, row_groups, indexes })
}

fn decode_schema(cur: &mut Cursor) -> Result<Vec<Field>, Located> {
    let mut body = cur.record().map_err(Located::from)?;
    let ncols = body.uvarint().map_err(Located::from)?;
    let ncols = body.guard_len(ncols, 1).map_err(Located::from)?;
    let mut fields = Vec::with_capacity(ncols);
    for _ in 0..ncols {
        let mut cd = body.record().map_err(Located::from)?;
        let name = cd.str().map_err(Located::from)?.to_string();
        let ty = type_id::decode_type(&mut cd)
            .map_err(|err| Located { err, column: Some(name.clone()) })?;
        fields.push(Field { name, ty });
    }
    Ok(fields)
}

fn decode_row_groups(cur: &mut Cursor, fields: &[Field]) -> Result<Vec<RawRowGroup>, Located> {
    let mut body = cur.record().map_err(Located::from)?;
    let nrg = body.uvarint().map_err(Located::from)?;
    let nrg = body.guard_len(nrg, 1).map_err(Located::from)?;
    let mut row_groups = Vec::with_capacity(nrg);
    for _ in 0..nrg {
        let mut rg_body = body.record().map_err(Located::from)?;
        let rows = rg_body.uvarint().map_err(Located::from)?;
        let mut chunks = Vec::with_capacity(fields.len());
        for field in fields {
            chunks.push(decode_chunk_meta(&mut rg_body, &field.ty, rows, &field.name)?);
        }
        row_groups.push(RawRowGroup { rows, chunks });
    }
    Ok(row_groups)
}

fn decode_chunk_meta(
    cur: &mut Cursor,
    ty: &DataType,
    rows: u64,
    column: &str,
) -> Result<RawChunk, Located> {
    let loc = |err: DecodeError| Located { err, column: Some(column.to_string()) };
    let mut body = cur.record().map_err(loc)?;
    let offset = body.uvarint().map_err(loc)?;
    let len = body.uvarint().map_err(loc)?;
    let crc = body.u32().map_err(loc)?;
    let encoding = body.uvarint().map_err(loc)?;
    let null_count = body.uvarint().map_err(loc)?;
    if null_count > rows {
        return Err(loc(DecodeError::Malformed("null_count exceeds row group rows")));
    }
    let min = decode_opt(&mut body, ty).map_err(loc)?;
    let max = decode_opt(&mut body, ty).map_err(loc)?;
    Ok(RawChunk { offset, len, crc, encoding, null_count, min, max })
}

/// SPEC §5 bounds: STRING/BYTES min truncated to STATS_MAX_BYTES (char boundary), max dropped
/// when its own encoding would exceed it. Everything else passes through unchanged.
pub(crate) fn bound_stats(stats: &ColumnStats) -> (Option<Value>, Option<Value>) {
    let min = stats.min.as_ref().map(bound_min);
    let max = stats.max.clone().filter(|v| !is_oversized(v));
    (min, max)
}

fn is_oversized(v: &Value) -> bool {
    match v {
        Value::String(s) => s.len() > STATS_MAX_BYTES,
        Value::Bytes(b) => b.len() > STATS_MAX_BYTES,
        _ => false,
    }
}

fn bound_min(v: &Value) -> Value {
    match v {
        Value::String(s) if s.len() > STATS_MAX_BYTES => {
            Value::String(truncate_str(s, STATS_MAX_BYTES).to_string())
        }
        Value::Bytes(b) if b.len() > STATS_MAX_BYTES => Value::Bytes(b[..STATS_MAX_BYTES].to_vec()),
        other => other.clone(),
    }
}

/// The longest prefix of `s` that is at most `max_bytes` long and ends on a char boundary.
fn truncate_str(s: &str, max_bytes: usize) -> &str {
    let mut cut = max_bytes.min(s.len());
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}

/// Trailer framing shared by `.seg` and `.idx`: appends the footer bytes, then
/// `footer_len · footer_crc32c · magic · version` (`TRAILER_LEN` bytes) to `out`.
pub(crate) fn write_trailer(out: &mut Vec<u8>, magic: [u8; 6], footer: &[u8]) {
    out.extend_from_slice(footer);
    out.extend_from_slice(&(footer.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32c(footer).to_le_bytes());
    out.extend_from_slice(&magic);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrailerError {
    BadMagic,
    UnsupportedVersion(u16),
    Truncated,
    CorruptFooter,
}

/// Checks magic, version, footer bounds (`>= header_len`) and the footer CRC, in that order,
/// and returns the footer's byte range on success. Never panics on a short or hostile input.
pub(crate) fn read_trailer(
    bytes: &[u8],
    magic: [u8; 6],
    header_len: usize,
) -> Result<std::ops::Range<usize>, TrailerError> {
    if bytes.len() < header_len + TRAILER_LEN {
        return Err(TrailerError::Truncated);
    }
    let trailer_start = bytes.len() - TRAILER_LEN;
    let trailer = &bytes[trailer_start..];
    let footer_len = u32::from_le_bytes(trailer[0..4].try_into().unwrap()) as usize;
    let footer_crc = u32::from_le_bytes(trailer[4..8].try_into().unwrap());
    let found_magic: [u8; 6] = trailer[8..14].try_into().unwrap();
    let version = u16::from_le_bytes(trailer[14..16].try_into().unwrap());
    if found_magic != magic {
        return Err(TrailerError::BadMagic);
    }
    if version > FORMAT_VERSION {
        return Err(TrailerError::UnsupportedVersion(version));
    }
    let footer_start = trailer_start
        .checked_sub(footer_len)
        .filter(|&s| s >= header_len)
        .ok_or(TrailerError::Truncated)?;
    if crc32c(&bytes[footer_start..trailer_start]) != footer_crc {
        return Err(TrailerError::CorruptFooter);
    }
    Ok(footer_start..trailer_start)
}

/// Encodes `f` the way `encode_footer` would, except column `column`'s type_desc is written
/// with a raw, possibly-unknown type id. Used by U9's criteria tests for `UnknownTypeId`.
#[cfg(test)]
pub(crate) fn encode_footer_with_raw_type(
    f: &Footer,
    column: usize,
    type_id: u64,
    params: &[u8],
) -> Vec<u8> {
    let mut out = Sink::new();
    out.record(|r| {
        r.uvarint(f.fields.len() as u64);
        for (i, field) in f.fields.iter().enumerate() {
            r.record(|fr| {
                fr.str(&field.name);
                if i == column {
                    self::type_id::encode_raw_type(type_id, params, fr);
                } else {
                    self::type_id::encode_type(&field.ty, fr);
                }
            });
        }
    });
    out.record(|r| {
        r.uvarint(f.row_groups.len() as u64);
        for rg in &f.row_groups {
            r.record(|rr| {
                rr.uvarint(rg.rows);
                for (i, chunk) in rg.chunks.iter().enumerate() {
                    rr.record(|cr| {
                        if i == column {
                            // The raw type may be unknown, so its stats cannot be encoded.
                            let mut blank = chunk.clone();
                            blank.min = None;
                            blank.max = None;
                            encode_chunk_meta(&blank, &f.fields[i].ty, cr);
                        } else {
                            encode_chunk_meta(chunk, &f.fields[i].ty, cr);
                        }
                    });
                }
            });
        }
    });
    out.record(|r| encode_directory(&f.indexes, r));
    out.into_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Ip, total_cmp};
    use std::cmp::Ordering;
    use std::net::IpAddr;

    use super::super::SEGMENT_MAGIC;

    fn fields() -> Vec<Field> {
        vec![
            Field { name: "i".to_string(), ty: DataType::Int64 },
            Field { name: "s".to_string(), ty: DataType::String },
            Field { name: "l".to_string(), ty: DataType::list(DataType::Int64).unwrap() },
        ]
    }

    fn sample_footer() -> Footer {
        let f = fields();
        let rg = |rows: u64, base: i64| RawRowGroup {
            rows,
            chunks: vec![
                RawChunk {
                    offset: 0,
                    len: 8,
                    crc: 1,
                    encoding: 0,
                    null_count: 0,
                    min: Some(Value::Int64(base)),
                    max: Some(Value::Int64(base + 9)),
                },
                RawChunk {
                    offset: 8,
                    len: 20,
                    crc: 2,
                    encoding: 1,
                    null_count: 1,
                    min: Some(Value::String("a".to_string())),
                    max: Some(Value::String("z".to_string())),
                },
                RawChunk {
                    offset: 28,
                    len: 4,
                    crc: 3,
                    encoding: 0,
                    null_count: 0,
                    min: None,
                    max: None,
                },
            ],
        };
        Footer {
            fields: f,
            row_groups: vec![rg(10, 0), rg(5, 100)],
            indexes: vec![
                RawIndexEntry {
                    kind: 1,
                    row_group: Some(0),
                    columns: vec![1],
                    offset: 1000,
                    len: 32,
                    crc: 0xabc,
                    params: vec![],
                },
                RawIndexEntry {
                    kind: 0x7FFF,
                    row_group: None,
                    columns: vec![0, 1],
                    offset: 2000,
                    len: 16,
                    crc: 0xdef,
                    params: vec![9],
                },
            ],
        }
    }

    #[test]
    fn footer_round_trips() {
        let f = sample_footer();
        let bytes = encode_footer(&f);
        let decoded = decode_footer(&bytes).unwrap();
        assert_eq!(decoded, f);
    }

    #[test]
    fn additive_trailing_bytes_are_ignored() {
        let f = sample_footer();
        let mut s = Sink::new();
        s.record(|r| encode_schema(&f.fields, r));
        s.record(|r| {
            r.uvarint(f.row_groups.len() as u64);
            for (rg_idx, rg) in f.row_groups.iter().enumerate() {
                r.record(|rr| {
                    rr.uvarint(rg.rows);
                    for (i, chunk) in rg.chunks.iter().enumerate() {
                        rr.record(|cr| {
                            encode_chunk_meta(chunk, &f.fields[i].ty, cr);
                            if rg_idx == 0 && i == 0 {
                                cr.u32(0xdead_beef); // extra trailing field in one chunk_meta
                            }
                        });
                    }
                });
            }
        });
        s.record(|r| encode_directory(&f.indexes, r));
        s.record(|r| r.u8(7)); // an unknown 4th top-level record
        let bytes = s.into_vec();
        let decoded = decode_footer(&bytes).unwrap();
        assert_eq!(decoded, f);
    }

    #[test]
    fn unknown_type_id_names_its_column() {
        let f = sample_footer();
        let bytes = encode_footer_with_raw_type(&f, 1, 999, &[]);
        assert_eq!(
            decode_footer(&bytes).unwrap_err(),
            Located { err: DecodeError::UnknownTypeId(999), column: Some("s".to_string()) }
        );
        // Control: the real STRING id (6) still decodes fine.
        let bytes_ok = encode_footer_with_raw_type(&f, 1, 6, &[]);
        assert!(decode_footer(&bytes_ok).is_ok());
    }

    #[test]
    fn bound_stats_truncates_long_min_and_drops_long_max() {
        let long = "€".repeat(100); // 3-byte chars: a raw 128-byte cut would split one in half
        let stats = ColumnStats {
            rows: 1,
            null_count: 0,
            min: Some(Value::String(long.clone())),
            max: Some(Value::String(long)),
        };
        let (min, max) = bound_stats(&stats);
        let min = match min {
            Some(Value::String(s)) => s,
            other => panic!("expected a bounded string min, got {other:?}"),
        };
        assert!(min.len() <= STATS_MAX_BYTES);
        assert!(min.is_char_boundary(min.len()));
        assert_eq!(
            total_cmp(&Value::String(min), &stats.min.clone().unwrap()),
            Some(Ordering::Less)
        );
        assert_eq!(max, None);
    }

    #[test]
    fn bound_stats_passes_short_values_through() {
        let stats = ColumnStats {
            rows: 1,
            null_count: 0,
            min: Some(Value::Int64(1)),
            max: Some(Value::Int64(2)),
        };
        assert_eq!(bound_stats(&stats), (stats.min.clone(), stats.max.clone()));
    }

    fn trailer_bytes(magic: [u8; 6], version: u16, footer: &[u8], footer_len_override: Option<u32>) -> Vec<u8> {
        let mut out = vec![0u8; 8];
        out.extend_from_slice(footer);
        let footer_len = footer_len_override.unwrap_or(footer.len() as u32);
        out.extend_from_slice(&footer_len.to_le_bytes());
        out.extend_from_slice(&crc32c(footer).to_le_bytes());
        out.extend_from_slice(&magic);
        out.extend_from_slice(&version.to_le_bytes());
        out
    }

    #[test]
    fn read_trailer_accepts_a_good_frame() {
        let footer = b"hello".to_vec();
        let bytes = trailer_bytes(SEGMENT_MAGIC, 1, &footer, None);
        let range = read_trailer(&bytes, SEGMENT_MAGIC, 8).unwrap();
        assert_eq!(&bytes[range], &footer[..]);
    }

    #[test]
    fn read_trailer_rejects_a_flipped_footer_byte() {
        let footer = b"hello".to_vec();
        let mut bytes = trailer_bytes(SEGMENT_MAGIC, 1, &footer, None);
        bytes[8] ^= 0xff;
        assert_eq!(
            read_trailer(&bytes, SEGMENT_MAGIC, 8),
            Err(TrailerError::CorruptFooter)
        );
    }

    #[test]
    fn read_trailer_rejects_wrong_magic() {
        let footer = b"hello".to_vec();
        let bytes = trailer_bytes(*b"XXXXXX", 1, &footer, None);
        assert_eq!(
            read_trailer(&bytes, SEGMENT_MAGIC, 8),
            Err(TrailerError::BadMagic)
        );
    }

    #[test]
    fn read_trailer_rejects_unsupported_version() {
        let footer = b"hello".to_vec();
        let bytes = trailer_bytes(SEGMENT_MAGIC, 2, &footer, None);
        assert_eq!(
            read_trailer(&bytes, SEGMENT_MAGIC, 8),
            Err(TrailerError::UnsupportedVersion(2))
        );
    }

    #[test]
    fn read_trailer_rejects_a_10_byte_input() {
        let bytes = vec![0u8; 10];
        assert_eq!(
            read_trailer(&bytes, SEGMENT_MAGIC, 8),
            Err(TrailerError::Truncated)
        );
    }

    #[test]
    fn read_trailer_rejects_a_footer_len_before_the_header() {
        let footer = b"hello".to_vec();
        let bytes = trailer_bytes(SEGMENT_MAGIC, 1, &footer, Some(100));
        assert_eq!(
            read_trailer(&bytes, SEGMENT_MAGIC, 8),
            Err(TrailerError::Truncated)
        );
    }

    #[test]
    fn write_trailer_round_trips_with_read_trailer() {
        let f = sample_footer();
        let footer_bytes = encode_footer(&f);
        let mut out = vec![0u8; 8];
        write_trailer(&mut out, SEGMENT_MAGIC, &footer_bytes);
        let range = read_trailer(&out, SEGMENT_MAGIC, 8).unwrap();
        assert_eq!(decode_footer(&out[range]).unwrap(), f);
    }

    fn ip_field(name: &str) -> Field {
        Field { name: name.to_string(), ty: DataType::Ip }
    }

    #[test]
    fn ip_stats_round_trip_through_the_footer() {
        let fields = vec![ip_field("addr")];
        let addr = Value::Ip(Ip::from("127.0.0.1".parse::<IpAddr>().unwrap()));
        let f = Footer {
            fields,
            row_groups: vec![RawRowGroup {
                rows: 1,
                chunks: vec![RawChunk {
                    offset: 0,
                    len: 16,
                    crc: 0,
                    encoding: 0,
                    null_count: 0,
                    min: Some(addr.clone()),
                    max: Some(addr),
                }],
            }],
            indexes: vec![],
        };
        let bytes = encode_footer(&f);
        assert_eq!(decode_footer(&bytes).unwrap(), f);
    }
}
