//! Manifest v1 codec (SPEC §5, §18, D0009): `ADLMAN` header, three length-prefixed body
//! records, then the segment format's own CRC32C trailer framing. Every record is
//! length-prefixed, so a reader ignores fields it doesn't know and new fields are additive.

use crate::exec::{ColumnStats, Field};
use crate::segment::wire::{Cursor, Sink};
use crate::segment::{DecodeError, footer, type_id, value};
use crate::types::DataType;

use super::error::Error;
use super::{CmpOp, Garbage, Manifest, Predicate, SegmentEntry, SideFile, TableEntry, TableName, Tombstone};

const MANIFEST_MAGIC: [u8; 6] = *b"ADLMAN";
const MANIFEST_VERSION: u16 = 1;
const HEADER_LEN: usize = 8;

pub(crate) fn encode(m: &Manifest) -> Vec<u8> {
    // `write_trailer` hard-codes the segment's own FORMAT_VERSION into the trailer's version
    // field; keep MANIFEST_VERSION equal to it until the two formats deliberately diverge.
    debug_assert_eq!(MANIFEST_VERSION, crate::segment::FORMAT_VERSION);

    let body = encode_body(m);
    let mut out = Vec::with_capacity(HEADER_LEN + body.len() + crate::segment::TRAILER_LEN);
    out.extend_from_slice(&MANIFEST_MAGIC);
    out.extend_from_slice(&MANIFEST_VERSION.to_le_bytes());
    footer::write_trailer(&mut out, MANIFEST_MAGIC, &body);
    out
}

fn encode_body(m: &Manifest) -> Vec<u8> {
    let mut body = Sink::new();
    body.record(|r| encode_header_fields(m, r));
    body.record(|r| encode_tables(m, r));
    body.record(|r| encode_garbage(m, r));
    body.into_vec()
}

pub(crate) fn decode(path: &str, bytes: &[u8]) -> Result<Manifest, Error> {
    if bytes.len() < HEADER_LEN {
        return Err(corrupt_str(path, "truncated header"));
    }
    let magic: [u8; 6] = bytes[0..6].try_into().unwrap();
    let header_version = u16::from_le_bytes([bytes[6], bytes[7]]);
    if magic != MANIFEST_MAGIC {
        return Err(corrupt_str(path, "bad magic"));
    }
    if header_version > MANIFEST_VERSION {
        return Err(Error::UnsupportedVersion {
            path: path.to_string(),
            version: header_version,
        });
    }
    let range = footer::read_trailer(bytes, MANIFEST_MAGIC, HEADER_LEN)
        .map_err(|e| trailer_err(path, e))?;
    decode_body(path, &bytes[range])
}

fn decode_body(path: &str, body: &[u8]) -> Result<Manifest, Error> {
    let mut cur = Cursor::new(body);
    let (version, next_segment_id) = decode_header_fields(&mut cur, path)?;
    let tables = decode_tables(&mut cur, path)?;
    let garbage = decode_garbage(&mut cur, path)?;
    Ok(Manifest {
        version,
        next_segment_id,
        tables,
        garbage,
    })
}

fn trailer_err(path: &str, e: footer::TrailerError) -> Error {
    match e {
        footer::TrailerError::BadMagic => corrupt_str(path, "bad magic"),
        footer::TrailerError::Truncated => corrupt_str(path, "truncated"),
        footer::TrailerError::CorruptFooter => corrupt_str(path, "corrupt footer"),
        footer::TrailerError::UnsupportedVersion(version) => Error::UnsupportedVersion {
            path: path.to_string(),
            version,
        },
    }
}

fn corrupt_str(path: &str, detail: &str) -> Error {
    Error::Corrupt {
        path: path.to_string(),
        detail: detail.to_string(),
    }
}

fn corrupt(path: &str, e: DecodeError) -> Error {
    let detail = match e {
        DecodeError::Truncated => "truncated".to_string(),
        DecodeError::Malformed(m) => m.to_string(),
        DecodeError::UnknownTypeId(id) => format!("unknown type id {id}"),
        DecodeError::UnknownEncoding(id) => format!("unknown encoding id {id}"),
    };
    Error::Corrupt {
        path: path.to_string(),
        detail,
    }
}

// ── header_fields ────────────────────────────────────────────────────────

fn encode_header_fields(m: &Manifest, out: &mut Sink) {
    out.uvarint(m.version);
    out.uvarint(m.next_segment_id);
}

fn decode_header_fields(cur: &mut Cursor, path: &str) -> Result<(u64, u64), Error> {
    let mut body = cur.record().map_err(|e| corrupt(path, e))?;
    let version = body.uvarint().map_err(|e| corrupt(path, e))?;
    let next_segment_id = body.uvarint().map_err(|e| corrupt(path, e))?;
    Ok((version, next_segment_id))
}

// ── tables ───────────────────────────────────────────────────────────────

fn encode_tables(m: &Manifest, out: &mut Sink) {
    out.uvarint(m.tables.len() as u64);
    for t in &m.tables {
        out.record(|r| encode_table_entry(t, r));
    }
}

fn encode_table_entry(t: &TableEntry, out: &mut Sink) {
    out.str(&t.name.db);
    out.str(&t.name.name);
    out.str(&t.engine);
    out.record(|r| encode_schema(&t.schema, r));
    out.record(|r| encode_segments(&t.segments, &t.schema, r));
    out.record(|r| encode_tombstones(&t.tombstones, &t.schema, r));
}

fn decode_tables(cur: &mut Cursor, path: &str) -> Result<Vec<TableEntry>, Error> {
    let mut body = cur.record().map_err(|e| corrupt(path, e))?;
    let n = body.uvarint().map_err(|e| corrupt(path, e))?;
    let n = body.guard_len(n, 1).map_err(|e| corrupt(path, e))?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(decode_table_entry(&mut body, path)?);
    }
    Ok(out)
}

fn decode_table_entry(cur: &mut Cursor, path: &str) -> Result<TableEntry, Error> {
    let mut body = cur.record().map_err(|e| corrupt(path, e))?;
    let db = body.str().map_err(|e| corrupt(path, e))?.to_string();
    let name = body.str().map_err(|e| corrupt(path, e))?.to_string();
    let engine = body.str().map_err(|e| corrupt(path, e))?.to_string();
    let schema = decode_schema(&mut body, path)?;
    let segments = decode_segments(&mut body, &schema, path)?;
    let tombstones = decode_tombstones(&mut body, &schema, path)?;
    Ok(TableEntry {
        name: TableName::new(db, name),
        engine,
        schema,
        segments,
        tombstones,
    })
}

// ── schema (mirrors segment::footer's encode_schema/decode_schema) ─────────

fn encode_schema(fields: &[Field], out: &mut Sink) {
    out.uvarint(fields.len() as u64);
    for f in fields {
        out.record(|r| {
            r.str(&f.name);
            type_id::encode_type(&f.ty, r);
        });
    }
}

fn decode_schema(cur: &mut Cursor, path: &str) -> Result<Vec<Field>, Error> {
    let mut body = cur.record().map_err(|e| corrupt(path, e))?;
    let n = body.uvarint().map_err(|e| corrupt(path, e))?;
    let n = body.guard_len(n, 1).map_err(|e| corrupt(path, e))?;
    let mut fields = Vec::with_capacity(n);
    for _ in 0..n {
        let mut fr = body.record().map_err(|e| corrupt(path, e))?;
        let name = fr.str().map_err(|e| corrupt(path, e))?.to_string();
        let ty = type_id::decode_type(&mut fr).map_err(|e| corrupt(path, e))?;
        fields.push(Field { name, ty });
    }
    Ok(fields)
}

// ── segments ─────────────────────────────────────────────────────────────

fn encode_segments(segments: &[SegmentEntry], schema: &[Field], out: &mut Sink) {
    out.uvarint(segments.len() as u64);
    for s in segments {
        out.record(|r| encode_segment(s, schema, r));
    }
}

fn decode_segments(cur: &mut Cursor, schema: &[Field], path: &str) -> Result<Vec<SegmentEntry>, Error> {
    let mut body = cur.record().map_err(|e| corrupt(path, e))?;
    let n = body.uvarint().map_err(|e| corrupt(path, e))?;
    let n = body.guard_len(n, 1).map_err(|e| corrupt(path, e))?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(decode_segment(&mut body, schema, path)?);
    }
    Ok(out)
}

fn encode_segment(s: &SegmentEntry, schema: &[Field], out: &mut Sink) {
    out.uvarint(s.id);
    out.str(&s.partition);
    out.uvarint(s.seq);
    out.uvarint(s.rows);
    out.uvarint(s.bytes);
    out.u32(s.footer_crc);
    out.record(|r| encode_columns(&s.columns, schema, r));
    out.record(|r| encode_side_files(&s.side_files, r));
}

fn decode_segment(cur: &mut Cursor, schema: &[Field], path: &str) -> Result<SegmentEntry, Error> {
    let mut body = cur.record().map_err(|e| corrupt(path, e))?;
    let id = body.uvarint().map_err(|e| corrupt(path, e))?;
    let partition = body.str().map_err(|e| corrupt(path, e))?.to_string();
    let seq = body.uvarint().map_err(|e| corrupt(path, e))?;
    let rows = body.uvarint().map_err(|e| corrupt(path, e))?;
    let bytes = body.uvarint().map_err(|e| corrupt(path, e))?;
    let footer_crc = body.u32().map_err(|e| corrupt(path, e))?;
    let columns = decode_columns(&mut body, schema, path)?;
    let side_files = decode_side_files(&mut body, path)?;
    Ok(SegmentEntry {
        id,
        partition,
        seq,
        rows,
        bytes,
        footer_crc,
        columns,
        side_files,
    })
}

// ── columns (segment-wide ColumnStats, one per schema field) ───────────────

fn encode_columns(columns: &[ColumnStats], schema: &[Field], out: &mut Sink) {
    debug_assert_eq!(columns.len(), schema.len(), "segment columns must match schema arity");
    out.uvarint(columns.len() as u64);
    for (c, field) in columns.iter().zip(schema) {
        out.record(|r| {
            r.uvarint(c.rows as u64);
            r.uvarint(c.null_count as u64);
            // LIST has no value codec (segment::value), so its stats are always absent.
            if matches!(field.ty, DataType::List(_)) {
                value::encode_opt(None, &field.ty, r);
                value::encode_opt(None, &field.ty, r);
            } else {
                value::encode_opt(c.min.as_ref(), &field.ty, r);
                value::encode_opt(c.max.as_ref(), &field.ty, r);
            }
        });
    }
}

fn decode_columns(cur: &mut Cursor, schema: &[Field], path: &str) -> Result<Vec<ColumnStats>, Error> {
    let mut body = cur.record().map_err(|e| corrupt(path, e))?;
    let n = body.uvarint().map_err(|e| corrupt(path, e))?;
    let n = body.guard_len(n, 1).map_err(|e| corrupt(path, e))?;
    if n != schema.len() {
        return Err(corrupt_str(
            path,
            &format!("segment has {n} column stats, schema has {} fields", schema.len()),
        ));
    }
    let mut out = Vec::with_capacity(n);
    for field in schema {
        let mut cr = body.record().map_err(|e| corrupt(path, e))?;
        let rows = cr.uvarint().map_err(|e| corrupt(path, e))? as usize;
        let null_count = cr.uvarint().map_err(|e| corrupt(path, e))? as usize;
        let min = value::decode_opt(&mut cr, &field.ty).map_err(|e| corrupt(path, e))?;
        let max = value::decode_opt(&mut cr, &field.ty).map_err(|e| corrupt(path, e))?;
        out.push(ColumnStats {
            rows,
            null_count,
            min,
            max,
        });
    }
    Ok(out)
}

// ── side_files: v1 defines no kinds, so any present entry is an error naming it ────

fn encode_side_files(side_files: &[SideFile], out: &mut Sink) {
    out.uvarint(side_files.len() as u64);
    for sf in side_files {
        out.record(|r| {
            r.uvarint(sf.kind);
            r.str(&sf.path);
            r.u32(sf.crc);
        });
    }
}

fn decode_side_files(cur: &mut Cursor, path: &str) -> Result<Vec<SideFile>, Error> {
    let mut body = cur.record().map_err(|e| corrupt(path, e))?;
    let n = body.uvarint().map_err(|e| corrupt(path, e))?;
    let n = body.guard_len(n, 1).map_err(|e| corrupt(path, e))?;
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut first = body.record().map_err(|e| corrupt(path, e))?;
    let kind = first.uvarint().map_err(|e| corrupt(path, e))?;
    Err(Error::UnknownSideFile {
        path: path.to_string(),
        kind,
    })
}

// ── tombstones ───────────────────────────────────────────────────────────

fn encode_tombstones(tombstones: &[Tombstone], schema: &[Field], out: &mut Sink) {
    out.uvarint(tombstones.len() as u64);
    for t in tombstones {
        out.record(|r| encode_tombstone(t, schema, r));
    }
}

fn decode_tombstones(cur: &mut Cursor, schema: &[Field], path: &str) -> Result<Vec<Tombstone>, Error> {
    let mut body = cur.record().map_err(|e| corrupt(path, e))?;
    let n = body.uvarint().map_err(|e| corrupt(path, e))?;
    let n = body.guard_len(n, 1).map_err(|e| corrupt(path, e))?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(decode_tombstone(&mut body, schema, path)?);
    }
    Ok(out)
}

fn encode_tombstone(t: &Tombstone, schema: &[Field], out: &mut Sink) {
    out.uvarint(t.seq);
    out.uvarint(t.predicates.len() as u64);
    for p in &t.predicates {
        out.record(|r| encode_predicate(p, schema, r));
    }
}

fn decode_tombstone(cur: &mut Cursor, schema: &[Field], path: &str) -> Result<Tombstone, Error> {
    let mut body = cur.record().map_err(|e| corrupt(path, e))?;
    let seq = body.uvarint().map_err(|e| corrupt(path, e))?;
    let n = body.uvarint().map_err(|e| corrupt(path, e))?;
    let n = body.guard_len(n, 1).map_err(|e| corrupt(path, e))?;
    let mut predicates = Vec::with_capacity(n);
    for _ in 0..n {
        predicates.push(decode_predicate(&mut body, schema, path)?);
    }
    Ok(Tombstone { seq, predicates })
}

fn encode_predicate(p: &Predicate, schema: &[Field], out: &mut Sink) {
    out.str(&p.column);
    out.u8(cmp_op_id(p.op));
    // Validated against `schema` at `Commit::apply` time: every live predicate names a real
    // column, so this always finds one.
    let ty = &schema.iter().find(|f| f.name == p.column).expect("predicate column in schema").ty;
    value::encode_value(&p.value, ty, out);
}

fn decode_predicate(cur: &mut Cursor, schema: &[Field], path: &str) -> Result<Predicate, Error> {
    let mut body = cur.record().map_err(|e| corrupt(path, e))?;
    let column = body.str().map_err(|e| corrupt(path, e))?.to_string();
    let op_byte = body.u8().map_err(|e| corrupt(path, e))?;
    let op = cmp_op_from_id(op_byte)
        .ok_or_else(|| corrupt_str(path, &format!("unknown comparison op {op_byte}")))?;
    let field = schema
        .iter()
        .find(|f| f.name == column)
        .ok_or_else(|| corrupt_str(path, &format!("tombstone predicate names unknown column {column}")))?;
    let value = value::decode_value(&mut body, &field.ty).map_err(|e| corrupt(path, e))?;
    Ok(Predicate { column, op, value })
}

fn cmp_op_id(op: CmpOp) -> u8 {
    match op {
        CmpOp::Eq => 0,
        CmpOp::Ne => 1,
        CmpOp::Lt => 2,
        CmpOp::Le => 3,
        CmpOp::Gt => 4,
        CmpOp::Ge => 5,
    }
}

fn cmp_op_from_id(id: u8) -> Option<CmpOp> {
    match id {
        0 => Some(CmpOp::Eq),
        1 => Some(CmpOp::Ne),
        2 => Some(CmpOp::Lt),
        3 => Some(CmpOp::Le),
        4 => Some(CmpOp::Gt),
        5 => Some(CmpOp::Ge),
        _ => None,
    }
}

// ── garbage ──────────────────────────────────────────────────────────────

fn encode_garbage(m: &Manifest, out: &mut Sink) {
    out.uvarint(m.garbage.len() as u64);
    for g in &m.garbage {
        out.record(|r| {
            r.str(&g.table.db);
            r.str(&g.table.name);
            r.uvarint(g.removed_at_ms);
            // No schema is available for a garbage entry's own table, so it carries no column
            // stats: id, partition, seq and rows are all a garbage-collector needs.
            r.record(|sr| encode_segment(&g.segment, &[], sr));
        });
    }
}

fn decode_garbage(cur: &mut Cursor, path: &str) -> Result<Vec<Garbage>, Error> {
    let mut body = cur.record().map_err(|e| corrupt(path, e))?;
    let n = body.uvarint().map_err(|e| corrupt(path, e))?;
    let n = body.guard_len(n, 1).map_err(|e| corrupt(path, e))?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let mut gr = body.record().map_err(|e| corrupt(path, e))?;
        let db = gr.str().map_err(|e| corrupt(path, e))?.to_string();
        let name = gr.str().map_err(|e| corrupt(path, e))?.to_string();
        let removed_at_ms = gr.uvarint().map_err(|e| corrupt(path, e))?;
        let segment = decode_segment(&mut gr, &[], path)?;
        out.push(Garbage {
            table: TableName::new(db, name),
            segment,
            removed_at_ms,
        });
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) fn encode_with_extra_segment_field(m: &Manifest) -> Vec<u8> {
    let mut body = Sink::new();
    body.record(|r| encode_header_fields(m, r));
    body.record(|r| {
        r.uvarint(m.tables.len() as u64);
        for (i, t) in m.tables.iter().enumerate() {
            r.record(|tr| {
                tr.str(&t.name.db);
                tr.str(&t.name.name);
                tr.str(&t.engine);
                tr.record(|sr| encode_schema(&t.schema, sr));
                tr.record(|sr| encode_segments_with_extra(&t.segments, &t.schema, sr, i == 0));
                tr.record(|sr| encode_tombstones(&t.tombstones, &t.schema, sr));
            });
        }
    });
    body.record(|r| encode_garbage(m, r));
    let body = body.into_vec();

    let mut out = Vec::with_capacity(HEADER_LEN + body.len() + crate::segment::TRAILER_LEN);
    out.extend_from_slice(&MANIFEST_MAGIC);
    out.extend_from_slice(&MANIFEST_VERSION.to_le_bytes());
    footer::write_trailer(&mut out, MANIFEST_MAGIC, &body);
    out
}

/// Test-only: like `encode_segments`, but appends an unknown trailing `uvarint 99` to the
/// first segment's record — the additive-fields case a reader must skip past.
#[cfg(test)]
fn encode_segments_with_extra(segments: &[SegmentEntry], schema: &[Field], out: &mut Sink, add_extra: bool) {
    out.uvarint(segments.len() as u64);
    for (j, s) in segments.iter().enumerate() {
        out.record(|r| {
            encode_segment(s, schema, r);
            if add_extra && j == 0 {
                r.uvarint(99);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Decimal, Ip, Value};
    use std::net::IpAddr;

    fn schema() -> Vec<Field> {
        vec![
            Field {
                name: "i".to_string(),
                ty: DataType::Int64,
            },
            Field {
                name: "s".to_string(),
                ty: DataType::String,
            },
            Field {
                name: "d".to_string(),
                ty: DataType::decimal(10, 2).unwrap(),
            },
            Field {
                name: "l".to_string(),
                ty: DataType::list(DataType::Int64).unwrap(),
            },
        ]
    }

    fn segment(id: u64, seq: u64) -> SegmentEntry {
        SegmentEntry {
            id,
            partition: "_".to_string(),
            seq,
            rows: 10,
            bytes: 100,
            footer_crc: 0xdead_beef,
            columns: vec![
                ColumnStats {
                    rows: 10,
                    null_count: 0,
                    min: Some(Value::Int64(-5)),
                    max: Some(Value::Int64(5)),
                },
                ColumnStats {
                    rows: 10,
                    null_count: 1,
                    min: Some(Value::String("a".to_string())),
                    max: Some(Value::String("z".to_string())),
                },
                ColumnStats {
                    rows: 10,
                    null_count: 0,
                    min: Some(Value::Decimal(Decimal::new(100, 2).unwrap())),
                    max: Some(Value::Decimal(Decimal::new(900, 2).unwrap())),
                },
                ColumnStats {
                    rows: 10,
                    null_count: 0,
                    min: None,
                    max: None,
                },
            ],
            side_files: Vec::new(),
        }
    }

    fn sample_manifest() -> Manifest {
        let table_a = TableEntry {
            name: TableName::new("d", "a"),
            engine: "append".to_string(),
            schema: schema(),
            segments: vec![segment(1, 1), segment(2, 2)],
            tombstones: vec![Tombstone {
                seq: 3,
                predicates: vec![
                    Predicate {
                        column: "i".to_string(),
                        op: CmpOp::Ge,
                        value: Value::Int64(0),
                    },
                    Predicate {
                        column: "s".to_string(),
                        op: CmpOp::Ne,
                        value: Value::String("x".to_string()),
                    },
                ],
            }],
        };
        let table_b = TableEntry {
            name: TableName::new("d", "b"),
            engine: "append".to_string(),
            schema: vec![Field {
                name: "ip".to_string(),
                ty: DataType::Ip,
            }],
            segments: vec![],
            tombstones: vec![],
        };
        Manifest {
            version: 3,
            next_segment_id: 3,
            tables: vec![table_a, table_b],
            garbage: vec![Garbage {
                table: TableName::new("d", "a"),
                segment: SegmentEntry {
                    id: 9,
                    partition: "_".to_string(),
                    seq: 1,
                    rows: 1,
                    bytes: 1,
                    footer_crc: 1,
                    columns: Vec::new(),
                    side_files: Vec::new(),
                },
                removed_at_ms: 42,
            }],
        }
    }

    #[test]
    fn codec_round_trips_a_manifest_with_stats_tombstones_and_garbage() {
        let m = sample_manifest();
        let bytes = m.encode();
        let decoded = Manifest::decode("m", &bytes).unwrap();
        assert_eq!(decoded, m);
    }

    #[test]
    fn ip_stats_round_trip() {
        let mut m = sample_manifest();
        let ip = Value::Ip(Ip::from("127.0.0.1".parse::<IpAddr>().unwrap()));
        m.tables[1].segments.push(SegmentEntry {
            id: 5,
            partition: "_".to_string(),
            seq: 1,
            rows: 1,
            bytes: 1,
            footer_crc: 0,
            columns: vec![ColumnStats {
                rows: 1,
                null_count: 0,
                min: Some(ip.clone()),
                max: Some(ip),
            }],
            side_files: Vec::new(),
        });
        let bytes = m.encode();
        assert_eq!(Manifest::decode("m", &bytes).unwrap(), m);
    }

    #[test]
    fn a_side_file_is_rejected_on_decode_naming_its_kind() {
        let mut m = sample_manifest();
        m.tables[0].segments[0].side_files.push(SideFile {
            kind: 7,
            path: "x.dv".to_string(),
            crc: 0,
        });
        let bytes = m.encode();
        match Manifest::decode("m", &bytes) {
            Err(Error::UnknownSideFile { path, kind }) => {
                assert_eq!(path, "m");
                assert_eq!(kind, 7);
            }
            other => panic!("expected UnknownSideFile, got {other:?}"),
        }
    }

    #[test]
    fn unknown_trailing_fields_are_skipped() {
        let m = sample_manifest();
        let bytes = encode_with_extra_segment_field(&m);
        let decoded = Manifest::decode("m", &bytes).unwrap();
        assert_eq!(decoded, m);
    }

    #[test]
    fn every_flipped_byte_is_rejected_never_silently_accepted() {
        let m = sample_manifest();
        let bytes = m.encode();
        for i in 0..bytes.len() {
            let mut flipped = bytes.clone();
            flipped[i] ^= 0xff;
            match Manifest::decode("m", &flipped) {
                Err(Error::Corrupt { .. }) | Err(Error::UnsupportedVersion { .. }) => {}
                other => panic!("byte {i}: expected Corrupt or UnsupportedVersion, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_truncated_manifest_is_corrupt_not_a_panic() {
        let m = sample_manifest();
        let bytes = m.encode();
        for cut in [0, 1, 4, 8, bytes.len() / 2] {
            assert!(matches!(
                Manifest::decode("m", &bytes[..cut]),
                Err(Error::Corrupt { .. })
            ));
        }
    }

    #[test]
    fn bad_magic_is_corrupt() {
        let mut bytes = Manifest::empty().encode();
        bytes[0] = b'X';
        assert!(matches!(
            Manifest::decode("m", &bytes),
            Err(Error::Corrupt { .. })
        ));
    }

    #[test]
    fn unsupported_header_version_is_rejected() {
        let mut bytes = Manifest::empty().encode();
        bytes[6..8].copy_from_slice(&99u16.to_le_bytes());
        assert!(matches!(
            Manifest::decode("m", &bytes),
            Err(Error::UnsupportedVersion { version: 99, .. })
        ));
    }

    #[test]
    fn empty_manifest_round_trips() {
        let m = Manifest::empty();
        let bytes = m.encode();
        assert_eq!(Manifest::decode("m", &bytes).unwrap(), m);
    }
}
