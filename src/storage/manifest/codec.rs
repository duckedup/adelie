//! Manifest v1 codec (SPEC §5, §18, §19, D0009, D0012, D0013): `ADLMAN` header, four
//! length-prefixed body records, then the segment format's own CRC32C trailer framing. Every
//! record is length-prefixed, so a reader ignores fields it doesn't know and new fields are
//! additive.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::exec::{ColumnStats, Field};
use crate::storage::segment::wire::{Cursor, Sink};
use crate::storage::segment::{DecodeError, footer, type_id, value};
use crate::types::DataType;

use super::error::Error;
use super::lifecycle::{Job, RetireReason, Retired};
use super::{
    CmpOp, FieldId, Garbage, Manifest, PartitionBy, Predicate, SchemaField, SegmentEntry, SideFile,
    TableEntry, TableId, TableName, Tombstone, Ttl, is_path_component,
};

const MANIFEST_MAGIC: [u8; 6] = *b"ADLMAN";
const MANIFEST_VERSION: u16 = 1;
const HEADER_LEN: usize = 8;

pub(crate) fn encode(m: &Manifest) -> Vec<u8> {
    // `write_trailer` hard-codes the segment's own FORMAT_VERSION into the trailer's version
    // field; keep MANIFEST_VERSION equal to it until the two formats deliberately diverge.
    debug_assert_eq!(MANIFEST_VERSION, crate::storage::segment::FORMAT_VERSION);

    let body = encode_body(m);
    let mut out =
        Vec::with_capacity(HEADER_LEN + body.len() + crate::storage::segment::TRAILER_LEN);
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
    body.record(|r| encode_lifecycle(m, r));
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
    let (version, next_segment_id, next_table_id) = decode_header_fields(&mut cur, path)?;
    let tables = decode_tables(&mut cur, path)?;
    let garbage = decode_garbage(&mut cur, path)?;
    let (next_job_id, jobs, retired) = if cur.is_empty() {
        (1, vec![], vec![])
    } else {
        decode_lifecycle(&mut cur, path)?
    };
    Ok(Manifest {
        version,
        next_segment_id,
        next_table_id,
        tables,
        garbage,
        next_job_id,
        jobs,
        retired,
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

/// A name read off disk becomes a path under the store root: one that could escape it is
/// corrupt, never followed.
fn check_component(path: &str, s: &str) -> Result<(), Error> {
    if is_path_component(s) {
        Ok(())
    } else {
        Err(corrupt_str(path, "name is not a single path component"))
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
    out.uvarint(m.next_table_id);
}

fn decode_header_fields(cur: &mut Cursor, path: &str) -> Result<(u64, u64, u64), Error> {
    let mut body = cur.record().map_err(|e| corrupt(path, e))?;
    let version = body.uvarint().map_err(|e| corrupt(path, e))?;
    let next_segment_id = body.uvarint().map_err(|e| corrupt(path, e))?;
    let next_table_id = if body.is_empty() {
        0
    } else {
        body.uvarint().map_err(|e| corrupt(path, e))?
    };
    Ok((version, next_segment_id, next_table_id))
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
    out.record(|r| encode_definition(t, r));
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
    check_component(path, &db)?;
    check_component(path, &name)?;
    let schema = decode_schema(&mut body, path)?;
    let segments = decode_segments(&mut body, &schema, path)?;
    let tombstones = decode_tombstones(&mut body, &schema, path)?;
    let def = decode_definition(&mut body, path)?;
    check_definition_ids(&def, &schema, path)?;
    Ok(TableEntry {
        id: def.id,
        name: TableName::new(db, name),
        engine,
        schema,
        next_field_id: def.next_field_id,
        key: def.key,
        version: def.version,
        order_by: def.order_by,
        partition_by: def.partition_by,
        ttl: def.ttl,
        options: def.options,
        segments,
        tombstones,
    })
}

// ── definition: id, next_field_id, KEY, VERSION, ORDER BY, PARTITION BY, TTL, WITH (D0012) ──
// Trailing on the table-entry record as a whole: absent (an old manifest's record ends right
// after tombstones) decodes as `Definition::absent()` — id 0, everything else empty/`None`.

struct Definition {
    id: TableId,
    next_field_id: u64,
    key: Vec<FieldId>,
    version: Option<FieldId>,
    order_by: Vec<FieldId>,
    partition_by: Option<PartitionBy>,
    ttl: Option<Ttl>,
    options: BTreeMap<String, String>,
}

impl Definition {
    fn absent() -> Definition {
        Definition {
            id: TableId(0),
            next_field_id: 0,
            key: Vec::new(),
            version: None,
            order_by: Vec::new(),
            partition_by: None,
            ttl: None,
            options: BTreeMap::new(),
        }
    }
}

fn encode_definition(t: &TableEntry, out: &mut Sink) {
    out.uvarint(t.id.0);
    out.uvarint(t.next_field_id);
    encode_field_id_list(&t.key, out);
    match t.version {
        Some(id) => {
            out.u8(1);
            out.uvarint(id.0);
        }
        None => out.u8(0),
    }
    encode_field_id_list(&t.order_by, out);
    match &t.partition_by {
        Some(p) => {
            out.u8(1);
            out.uvarint(p.column.0);
            out.uvarint(p.bucket.as_nanos() as u64);
        }
        None => out.u8(0),
    }
    match &t.ttl {
        Some(ttl) => {
            out.u8(1);
            out.uvarint(ttl.column.0);
            out.uvarint(ttl.after.as_nanos() as u64);
        }
        None => out.u8(0),
    }
    out.uvarint(t.options.len() as u64);
    for (k, v) in &t.options {
        out.str(k);
        out.str(v);
    }
}

fn encode_field_id_list(ids: &[FieldId], out: &mut Sink) {
    out.uvarint(ids.len() as u64);
    for id in ids {
        out.uvarint(id.0);
    }
}

fn decode_field_id_list(cur: &mut Cursor, path: &str) -> Result<Vec<FieldId>, Error> {
    let n = cur.uvarint().map_err(|e| corrupt(path, e))?;
    let n = cur.guard_len(n, 1).map_err(|e| corrupt(path, e))?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(FieldId(cur.uvarint().map_err(|e| corrupt(path, e))?));
    }
    Ok(out)
}

/// Every field id a definition names must be one of its own table's columns: an engine indexes
/// the schema by these ids, so a dangling one off disk is corrupt, never a panic on the next
/// write (the same rule `decode_predicate` applies to a tombstone's column).
fn check_definition_ids(def: &Definition, schema: &[SchemaField], path: &str) -> Result<(), Error> {
    let named = def
        .key
        .iter()
        .chain(&def.order_by)
        .chain(&def.version)
        .chain(def.partition_by.as_ref().map(|p| &p.column))
        .chain(def.ttl.as_ref().map(|t| &t.column));
    for id in named {
        if !schema.iter().any(|f| f.id == *id) {
            return Err(corrupt_str(
                path,
                &format!("table definition names unknown field id {}", id.0),
            ));
        }
    }
    Ok(())
}

fn decode_definition(cur: &mut Cursor, path: &str) -> Result<Definition, Error> {
    if cur.is_empty() {
        return Ok(Definition::absent());
    }
    let mut body = cur.record().map_err(|e| corrupt(path, e))?;
    let id = body.uvarint().map_err(|e| corrupt(path, e))?;
    if id == 0 {
        return Err(corrupt_str(path, "table id is present but zero"));
    }
    let next_field_id = body.uvarint().map_err(|e| corrupt(path, e))?;
    let key = decode_field_id_list(&mut body, path)?;
    let version = match body.u8().map_err(|e| corrupt(path, e))? {
        0 => None,
        1 => Some(FieldId(body.uvarint().map_err(|e| corrupt(path, e))?)),
        other => return Err(corrupt_str(path, &format!("bad VERSION tag {other}"))),
    };
    let order_by = decode_field_id_list(&mut body, path)?;
    let partition_by = match body.u8().map_err(|e| corrupt(path, e))? {
        0 => None,
        1 => Some(PartitionBy {
            column: FieldId(body.uvarint().map_err(|e| corrupt(path, e))?),
            bucket: Duration::from_nanos(body.uvarint().map_err(|e| corrupt(path, e))?),
        }),
        other => return Err(corrupt_str(path, &format!("bad PARTITION BY tag {other}"))),
    };
    let ttl = match body.u8().map_err(|e| corrupt(path, e))? {
        0 => None,
        1 => Some(Ttl {
            column: FieldId(body.uvarint().map_err(|e| corrupt(path, e))?),
            after: Duration::from_nanos(body.uvarint().map_err(|e| corrupt(path, e))?),
        }),
        other => return Err(corrupt_str(path, &format!("bad TTL tag {other}"))),
    };
    let n = body.uvarint().map_err(|e| corrupt(path, e))?;
    let n = body.guard_len(n, 2).map_err(|e| corrupt(path, e))?;
    let mut options = BTreeMap::new();
    for _ in 0..n {
        let k = body.str().map_err(|e| corrupt(path, e))?.to_string();
        let v = body.str().map_err(|e| corrupt(path, e))?.to_string();
        options.insert(k, v);
    }
    Ok(Definition {
        id: TableId(id),
        next_field_id,
        key,
        version,
        order_by,
        partition_by,
        ttl,
        options,
    })
}

// ── schema (mirrors segment::footer's encode_schema/decode_schema, plus a trailing field id) ──

fn encode_schema(fields: &[SchemaField], out: &mut Sink) {
    out.uvarint(fields.len() as u64);
    for f in fields {
        out.record(|r| {
            r.str(&f.field.name);
            type_id::encode_type(&f.field.ty, r);
            r.uvarint(f.id.0);
        });
    }
}

fn decode_schema(cur: &mut Cursor, path: &str) -> Result<Vec<SchemaField>, Error> {
    let mut body = cur.record().map_err(|e| corrupt(path, e))?;
    let n = body.uvarint().map_err(|e| corrupt(path, e))?;
    let n = body.guard_len(n, 1).map_err(|e| corrupt(path, e))?;
    let mut fields = Vec::with_capacity(n);
    for _ in 0..n {
        let mut fr = body.record().map_err(|e| corrupt(path, e))?;
        let name = fr.str().map_err(|e| corrupt(path, e))?.to_string();
        let ty = type_id::decode_type(&mut fr).map_err(|e| corrupt(path, e))?;
        let id = if fr.is_empty() {
            0
        } else {
            let raw = fr.uvarint().map_err(|e| corrupt(path, e))?;
            if raw == 0 {
                return Err(corrupt_str(path, "field id is present but zero"));
            }
            raw
        };
        fields.push(SchemaField {
            id: FieldId(id),
            field: Field { name, ty },
        });
    }
    Ok(fields)
}

// ── segments ─────────────────────────────────────────────────────────────

fn encode_segments(segments: &[SegmentEntry], schema: &[SchemaField], out: &mut Sink) {
    out.uvarint(segments.len() as u64);
    for s in segments {
        out.record(|r| encode_segment(s, schema, r));
    }
}

fn decode_segments(
    cur: &mut Cursor,
    schema: &[SchemaField],
    path: &str,
) -> Result<Vec<SegmentEntry>, Error> {
    let mut body = cur.record().map_err(|e| corrupt(path, e))?;
    let n = body.uvarint().map_err(|e| corrupt(path, e))?;
    let n = body.guard_len(n, 1).map_err(|e| corrupt(path, e))?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(decode_segment(&mut body, schema, path)?);
    }
    Ok(out)
}

fn encode_segment(s: &SegmentEntry, schema: &[SchemaField], out: &mut Sink) {
    out.uvarint(s.id);
    out.str(&s.partition);
    out.uvarint(s.seq);
    out.uvarint(s.rows);
    out.uvarint(s.bytes);
    out.u32(s.footer_crc);
    out.record(|r| encode_columns(&s.columns, schema, r));
    out.record(|r| encode_side_files(&s.side_files, r));
    out.str(&s.dir);
    out.record(|r| encode_field_id_list(&s.field_ids, r));
    out.record(|r| encode_field_id_list(&s.file_field_ids, r));
}

fn decode_segment(
    cur: &mut Cursor,
    schema: &[SchemaField],
    path: &str,
) -> Result<SegmentEntry, Error> {
    let mut body = cur.record().map_err(|e| corrupt(path, e))?;
    let id = body.uvarint().map_err(|e| corrupt(path, e))?;
    let partition = body.str().map_err(|e| corrupt(path, e))?.to_string();
    check_component(path, &partition)?;
    let seq = body.uvarint().map_err(|e| corrupt(path, e))?;
    let rows = body.uvarint().map_err(|e| corrupt(path, e))?;
    let bytes = body.uvarint().map_err(|e| corrupt(path, e))?;
    let footer_crc = body.u32().map_err(|e| corrupt(path, e))?;
    let columns = decode_columns(&mut body, schema, path)?;
    let side_files = decode_side_files(&mut body, path)?;
    let dir = if body.is_empty() {
        String::new()
    } else {
        let d = body.str().map_err(|e| corrupt(path, e))?.to_string();
        for part in d.split('/') {
            check_component(path, part)?;
        }
        d
    };
    let field_ids = if body.is_empty() {
        Vec::new()
    } else {
        let mut fr = body.record().map_err(|e| corrupt(path, e))?;
        decode_field_id_list(&mut fr, path)?
    };
    let file_field_ids = if body.is_empty() {
        Vec::new()
    } else {
        let mut fr = body.record().map_err(|e| corrupt(path, e))?;
        let ids = decode_field_id_list(&mut fr, path)?;
        check_distinct_file_field_ids(path, &ids)?;
        ids
    };
    // `field_ids` pairs with `columns` entry for entry (`project_onto` indexes one by the
    // other); empty is a legacy segment, backfilled from the schema on open.
    if !field_ids.is_empty() && field_ids.len() != columns.len() {
        return Err(corrupt_str(
            path,
            &format!(
                "segment has {} field ids for {} column stats",
                field_ids.len(),
                columns.len()
            ),
        ));
    }
    Ok(SegmentEntry {
        id,
        partition,
        seq,
        rows,
        bytes,
        footer_crc,
        columns,
        side_files,
        dir,
        field_ids,
        file_field_ids,
    })
}

/// Every non-zero id in a segment's `file_field_ids` names one physical file column, so two
/// entries sharing an id would make a reader unable to tell them apart.
fn check_distinct_file_field_ids(path: &str, ids: &[FieldId]) -> Result<(), Error> {
    let mut seen = std::collections::BTreeSet::new();
    for id in ids {
        if id.0 != 0 && !seen.insert(id.0) {
            return Err(corrupt_str(
                path,
                &format!("duplicate file field id {} in file_field_ids", id.0),
            ));
        }
    }
    Ok(())
}

// ── columns (segment-wide ColumnStats, one per schema field) ───────────────

fn encode_columns(columns: &[ColumnStats], schema: &[SchemaField], out: &mut Sink) {
    debug_assert_eq!(
        columns.len(),
        schema.len(),
        "segment columns must match schema arity"
    );
    out.uvarint(columns.len() as u64);
    for (c, field) in columns.iter().zip(schema) {
        out.record(|r| {
            r.uvarint(c.rows as u64);
            r.uvarint(c.null_count as u64);
            // LIST has no value codec (segment::value), so its stats are always absent.
            if matches!(field.field.ty, DataType::List(_)) {
                value::encode_opt(None, &field.field.ty, r);
                value::encode_opt(None, &field.field.ty, r);
            } else {
                value::encode_opt(c.min.as_ref(), &field.field.ty, r);
                value::encode_opt(c.max.as_ref(), &field.field.ty, r);
            }
        });
    }
}

fn decode_columns(
    cur: &mut Cursor,
    schema: &[SchemaField],
    path: &str,
) -> Result<Vec<ColumnStats>, Error> {
    let mut body = cur.record().map_err(|e| corrupt(path, e))?;
    let n = body.uvarint().map_err(|e| corrupt(path, e))?;
    let n = body.guard_len(n, 1).map_err(|e| corrupt(path, e))?;
    if n != schema.len() {
        return Err(corrupt_str(
            path,
            &format!(
                "segment has {n} column stats, schema has {} fields",
                schema.len()
            ),
        ));
    }
    let mut out = Vec::with_capacity(n);
    for field in schema {
        let mut cr = body.record().map_err(|e| corrupt(path, e))?;
        let rows = cr.uvarint().map_err(|e| corrupt(path, e))? as usize;
        let null_count = cr.uvarint().map_err(|e| corrupt(path, e))? as usize;
        let min = value::decode_opt(&mut cr, &field.field.ty).map_err(|e| corrupt(path, e))?;
        let max = value::decode_opt(&mut cr, &field.field.ty).map_err(|e| corrupt(path, e))?;
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

fn encode_tombstones(tombstones: &[Tombstone], schema: &[SchemaField], out: &mut Sink) {
    out.uvarint(tombstones.len() as u64);
    for t in tombstones {
        out.record(|r| encode_tombstone(t, schema, r));
    }
}

fn decode_tombstones(
    cur: &mut Cursor,
    schema: &[SchemaField],
    path: &str,
) -> Result<Vec<Tombstone>, Error> {
    let mut body = cur.record().map_err(|e| corrupt(path, e))?;
    let n = body.uvarint().map_err(|e| corrupt(path, e))?;
    let n = body.guard_len(n, 1).map_err(|e| corrupt(path, e))?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(decode_tombstone(&mut body, schema, path)?);
    }
    Ok(out)
}

fn encode_tombstone(t: &Tombstone, schema: &[SchemaField], out: &mut Sink) {
    out.uvarint(t.seq);
    out.uvarint(t.predicates.len() as u64);
    for p in &t.predicates {
        out.record(|r| encode_predicate(p, schema, r));
    }
}

fn decode_tombstone(
    cur: &mut Cursor,
    schema: &[SchemaField],
    path: &str,
) -> Result<Tombstone, Error> {
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

fn encode_predicate(p: &Predicate, schema: &[SchemaField], out: &mut Sink) {
    out.str(&p.column);
    out.u8(cmp_op_id(p.op));
    // Validated against `schema` at `Commit::apply` time: every live predicate names a real
    // column, so this always finds one.
    let ty = &schema
        .iter()
        .find(|f| f.field.name == p.column)
        .expect("predicate column in schema")
        .field
        .ty;
    value::encode_value(&p.value, ty, out);
}

fn decode_predicate(
    cur: &mut Cursor,
    schema: &[SchemaField],
    path: &str,
) -> Result<Predicate, Error> {
    let mut body = cur.record().map_err(|e| corrupt(path, e))?;
    let column = body.str().map_err(|e| corrupt(path, e))?.to_string();
    let op_byte = body.u8().map_err(|e| corrupt(path, e))?;
    let op = cmp_op_from_id(op_byte)
        .ok_or_else(|| corrupt_str(path, &format!("unknown comparison op {op_byte}")))?;
    let field = schema
        .iter()
        .find(|f| f.field.name == column)
        .ok_or_else(|| {
            corrupt_str(
                path,
                &format!("tombstone predicate names unknown column {column}"),
            )
        })?;
    let value = value::decode_value(&mut body, &field.field.ty).map_err(|e| corrupt(path, e))?;
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
            // stats: id, partition, seq, rows and dir are all a garbage-collector needs.
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
        check_component(path, &db)?;
        check_component(path, &name)?;
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

// ── lifecycle: jobs and retired table entries (SPEC §19, D0012, D0013) ─────

fn encode_lifecycle(m: &Manifest, out: &mut Sink) {
    out.uvarint(m.next_job_id);
    out.uvarint(m.jobs.len() as u64);
    for j in &m.jobs {
        out.record(|r| encode_job(j, r));
    }
    out.uvarint(m.retired.len() as u64);
    for ret in &m.retired {
        out.record(|r| encode_retired(ret, r));
    }
}

fn decode_lifecycle(cur: &mut Cursor, path: &str) -> Result<(u64, Vec<Job>, Vec<Retired>), Error> {
    let mut body = cur.record().map_err(|e| corrupt(path, e))?;
    let next_job_id = body.uvarint().map_err(|e| corrupt(path, e))?;
    let n = body.uvarint().map_err(|e| corrupt(path, e))?;
    let n = body.guard_len(n, 1).map_err(|e| corrupt(path, e))?;
    let mut jobs = Vec::with_capacity(n);
    for _ in 0..n {
        let mut jr = body.record().map_err(|e| corrupt(path, e))?;
        jobs.push(decode_job(&mut jr, path, next_job_id)?);
    }
    let n = body.uvarint().map_err(|e| corrupt(path, e))?;
    let n = body.guard_len(n, 1).map_err(|e| corrupt(path, e))?;
    let mut retired = Vec::with_capacity(n);
    for _ in 0..n {
        let mut rr = body.record().map_err(|e| corrupt(path, e))?;
        retired.push(decode_retired(&mut rr, path)?);
    }
    Ok((next_job_id, jobs, retired))
}

fn encode_job(j: &Job, out: &mut Sink) {
    out.uvarint(j.id);
    out.uvarint(j.source.0);
    out.uvarint(j.snapshot);
    out.record(|r| encode_u64_list(&j.handled, r));
    out.uvarint(j.reused);
    out.uvarint(j.rewritten);
    out.record(|r| encode_table_entry(&j.target, r));
}

fn decode_job(cur: &mut Cursor, path: &str, next_job_id: u64) -> Result<Job, Error> {
    let id = cur.uvarint().map_err(|e| corrupt(path, e))?;
    if id == 0 || id >= next_job_id {
        return Err(corrupt_str(path, &format!("job id {id} is invalid")));
    }
    let source = TableId(cur.uvarint().map_err(|e| corrupt(path, e))?);
    let snapshot = cur.uvarint().map_err(|e| corrupt(path, e))?;
    let mut hr = cur.record().map_err(|e| corrupt(path, e))?;
    let handled = decode_u64_list(&mut hr, path)?;
    let reused = cur.uvarint().map_err(|e| corrupt(path, e))?;
    let rewritten = cur.uvarint().map_err(|e| corrupt(path, e))?;
    let target = decode_table_entry(cur, path)?;
    Ok(Job {
        id,
        source,
        snapshot,
        target,
        handled,
        reused,
        rewritten,
    })
}

fn encode_retired(r: &Retired, out: &mut Sink) {
    out.u8(retire_reason_id(r.reason));
    out.uvarint(r.retired_at_ms);
    out.uvarint(r.version);
    out.record(|rr| encode_u64_list(&r.successor_segments, rr));
    out.record(|rr| encode_table_entry(&r.entry, rr));
}

fn decode_retired(cur: &mut Cursor, path: &str) -> Result<Retired, Error> {
    let reason_byte = cur.u8().map_err(|e| corrupt(path, e))?;
    let reason = retire_reason_from_id(reason_byte)
        .ok_or_else(|| corrupt_str(path, &format!("unknown retire reason {reason_byte}")))?;
    let retired_at_ms = cur.uvarint().map_err(|e| corrupt(path, e))?;
    let version = cur.uvarint().map_err(|e| corrupt(path, e))?;
    let mut sr = cur.record().map_err(|e| corrupt(path, e))?;
    let successor_segments = decode_u64_list(&mut sr, path)?;
    let entry = decode_table_entry(cur, path)?;
    Ok(Retired {
        entry,
        reason,
        retired_at_ms,
        version,
        successor_segments,
    })
}

fn retire_reason_id(r: RetireReason) -> u8 {
    match r {
        RetireReason::Swapped => 0,
        RetireReason::Reverted => 1,
        RetireReason::Dropped => 2,
    }
}

fn retire_reason_from_id(id: u8) -> Option<RetireReason> {
    match id {
        0 => Some(RetireReason::Swapped),
        1 => Some(RetireReason::Reverted),
        2 => Some(RetireReason::Dropped),
        _ => None,
    }
}

fn encode_u64_list(ids: &[u64], out: &mut Sink) {
    out.uvarint(ids.len() as u64);
    for &id in ids {
        out.uvarint(id);
    }
}

fn decode_u64_list(cur: &mut Cursor, path: &str) -> Result<Vec<u64>, Error> {
    let n = cur.uvarint().map_err(|e| corrupt(path, e))?;
    let n = cur.guard_len(n, 1).map_err(|e| corrupt(path, e))?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(cur.uvarint().map_err(|e| corrupt(path, e))?);
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
                tr.record(|sr| encode_definition(t, sr));
            });
        }
    });
    body.record(|r| encode_garbage(m, r));
    let body = body.into_vec();

    let mut out =
        Vec::with_capacity(HEADER_LEN + body.len() + crate::storage::segment::TRAILER_LEN);
    out.extend_from_slice(&MANIFEST_MAGIC);
    out.extend_from_slice(&MANIFEST_VERSION.to_le_bytes());
    footer::write_trailer(&mut out, MANIFEST_MAGIC, &body);
    out
}

/// Test-only: like `encode_segments`, but appends an unknown trailing `uvarint 99` to the
/// first segment's record — the additive-fields case a reader must skip past.
#[cfg(test)]
fn encode_segments_with_extra(
    segments: &[SegmentEntry],
    schema: &[SchemaField],
    out: &mut Sink,
    add_extra: bool,
) {
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

/// Test-only: exactly the D0009 layout (no `next_table_id`, no per-field id, no `definition`
/// record, no segment `dir`/`field_ids`) — what `codec::decode` must read as absence and
/// `Manifest::decode`'s backfill must derive deterministically.
#[cfg(test)]
fn encode_pre_d0012(m: &Manifest) -> Vec<u8> {
    let mut body = Sink::new();
    body.record(|r| {
        r.uvarint(m.version);
        r.uvarint(m.next_segment_id);
    });
    body.record(|r| {
        r.uvarint(m.tables.len() as u64);
        for t in &m.tables {
            r.record(|tr| {
                tr.str(&t.name.db);
                tr.str(&t.name.name);
                tr.str(&t.engine);
                tr.record(|sr| {
                    sr.uvarint(t.schema.len() as u64);
                    for f in &t.schema {
                        sr.record(|fr| {
                            fr.str(&f.field.name);
                            type_id::encode_type(&f.field.ty, fr);
                        });
                    }
                });
                tr.record(|sr| encode_segments_pre_d0012(&t.segments, &t.schema, sr));
                tr.record(|sr| encode_tombstones(&t.tombstones, &t.schema, sr));
            });
        }
    });
    body.record(|r| encode_garbage_pre_d0012(m, r));
    let body = body.into_vec();

    let mut out =
        Vec::with_capacity(HEADER_LEN + body.len() + crate::storage::segment::TRAILER_LEN);
    out.extend_from_slice(&MANIFEST_MAGIC);
    out.extend_from_slice(&MANIFEST_VERSION.to_le_bytes());
    footer::write_trailer(&mut out, MANIFEST_MAGIC, &body);
    out
}

#[cfg(test)]
fn encode_segments_pre_d0012(segments: &[SegmentEntry], schema: &[SchemaField], out: &mut Sink) {
    out.uvarint(segments.len() as u64);
    for s in segments {
        out.record(|r| encode_segment_pre_d0012(s, schema, r));
    }
}

#[cfg(test)]
fn encode_segment_pre_d0012(s: &SegmentEntry, schema: &[SchemaField], out: &mut Sink) {
    out.uvarint(s.id);
    out.str(&s.partition);
    out.uvarint(s.seq);
    out.uvarint(s.rows);
    out.uvarint(s.bytes);
    out.u32(s.footer_crc);
    out.record(|r| encode_columns(&s.columns, schema, r));
    out.record(|r| encode_side_files(&s.side_files, r));
}

#[cfg(test)]
fn encode_garbage_pre_d0012(m: &Manifest, out: &mut Sink) {
    out.uvarint(m.garbage.len() as u64);
    for g in &m.garbage {
        out.record(|r| {
            r.str(&g.table.db);
            r.str(&g.table.name);
            r.uvarint(g.removed_at_ms);
            r.record(|sr| encode_segment_pre_d0012(&g.segment, &[], sr));
        });
    }
}

/// Test-only: today's format (D0012, before this change) — the `dir`/`field_ids` segment
/// record with no trailing `file_field_ids`, and no 4th `lifecycle` record — proving a manifest
/// written before D0013 still decodes.
#[cfg(test)]
fn encode_pre_lifecycle(m: &Manifest) -> Vec<u8> {
    let mut body = Sink::new();
    body.record(|r| encode_header_fields(m, r));
    body.record(|r| {
        r.uvarint(m.tables.len() as u64);
        for t in &m.tables {
            r.record(|tr| {
                tr.str(&t.name.db);
                tr.str(&t.name.name);
                tr.str(&t.engine);
                tr.record(|sr| encode_schema(&t.schema, sr));
                tr.record(|sr| encode_segments_pre_lifecycle(&t.segments, &t.schema, sr));
                tr.record(|sr| encode_tombstones(&t.tombstones, &t.schema, sr));
                tr.record(|sr| encode_definition(t, sr));
            });
        }
    });
    body.record(|r| encode_garbage_pre_lifecycle(m, r));
    let body = body.into_vec();

    let mut out =
        Vec::with_capacity(HEADER_LEN + body.len() + crate::storage::segment::TRAILER_LEN);
    out.extend_from_slice(&MANIFEST_MAGIC);
    out.extend_from_slice(&MANIFEST_VERSION.to_le_bytes());
    footer::write_trailer(&mut out, MANIFEST_MAGIC, &body);
    out
}

#[cfg(test)]
fn encode_segments_pre_lifecycle(
    segments: &[SegmentEntry],
    schema: &[SchemaField],
    out: &mut Sink,
) {
    out.uvarint(segments.len() as u64);
    for s in segments {
        out.record(|r| encode_segment_pre_lifecycle(s, schema, r));
    }
}

#[cfg(test)]
fn encode_segment_pre_lifecycle(s: &SegmentEntry, schema: &[SchemaField], out: &mut Sink) {
    out.uvarint(s.id);
    out.str(&s.partition);
    out.uvarint(s.seq);
    out.uvarint(s.rows);
    out.uvarint(s.bytes);
    out.u32(s.footer_crc);
    out.record(|r| encode_columns(&s.columns, schema, r));
    out.record(|r| encode_side_files(&s.side_files, r));
    out.str(&s.dir);
    out.record(|r| encode_field_id_list(&s.field_ids, r));
}

#[cfg(test)]
fn encode_garbage_pre_lifecycle(m: &Manifest, out: &mut Sink) {
    out.uvarint(m.garbage.len() as u64);
    for g in &m.garbage {
        out.record(|r| {
            r.str(&g.table.db);
            r.str(&g.table.name);
            r.uvarint(g.removed_at_ms);
            r.record(|sr| encode_segment_pre_lifecycle(&g.segment, &[], sr));
        });
    }
}

/// Test-only: a `file_field_ids` list with a duplicate non-zero id — bytes a fixed decoder
/// must reject that `encode_segment` itself can never produce.
#[cfg(test)]
fn encode_segment_with_bad_file_field_ids(
    s: &SegmentEntry,
    schema: &[SchemaField],
    out: &mut Sink,
) {
    out.uvarint(s.id);
    out.str(&s.partition);
    out.uvarint(s.seq);
    out.uvarint(s.rows);
    out.uvarint(s.bytes);
    out.u32(s.footer_crc);
    out.record(|r| encode_columns(&s.columns, schema, r));
    out.record(|r| encode_side_files(&s.side_files, r));
    out.str(&s.dir);
    out.record(|r| encode_field_id_list(&s.field_ids, r));
    out.record(|r| {
        r.uvarint(2);
        r.uvarint(7);
        r.uvarint(7);
    });
}

/// Test-only: the first table's first segment gets a `file_field_ids` list with a duplicate
/// non-zero id — bytes `encode_segment` can never itself produce.
#[cfg(test)]
fn encode_with_bad_file_field_ids(m: &Manifest) -> Vec<u8> {
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
                tr.record(|sr| {
                    sr.uvarint(t.segments.len() as u64);
                    for (j, s) in t.segments.iter().enumerate() {
                        sr.record(|r2| {
                            if i == 0 && j == 0 {
                                encode_segment_with_bad_file_field_ids(s, &t.schema, r2);
                            } else {
                                encode_segment(s, &t.schema, r2);
                            }
                        });
                    }
                });
                tr.record(|sr| encode_tombstones(&t.tombstones, &t.schema, sr));
                tr.record(|sr| encode_definition(t, sr));
            });
        }
    });
    body.record(|r| encode_garbage(m, r));
    body.record(|r| encode_lifecycle(m, r));
    let body = body.into_vec();

    let mut out =
        Vec::with_capacity(HEADER_LEN + body.len() + crate::storage::segment::TRAILER_LEN);
    out.extend_from_slice(&MANIFEST_MAGIC);
    out.extend_from_slice(&MANIFEST_VERSION.to_le_bytes());
    footer::write_trailer(&mut out, MANIFEST_MAGIC, &body);
    out
}

/// Test-only: a lifecycle record whose one job has an out-of-range id.
#[cfg(test)]
fn encode_with_bad_job_id(m: &Manifest) -> Vec<u8> {
    let mut body = Sink::new();
    body.record(|r| encode_header_fields(m, r));
    body.record(|r| encode_tables(m, r));
    body.record(|r| encode_garbage(m, r));
    body.record(|r| {
        r.uvarint(1); // next_job_id
        r.uvarint(1); // one job
        r.record(|jr| {
            jr.uvarint(0); // invalid: id 0
            jr.uvarint(1);
            jr.uvarint(1);
            jr.record(|hr| encode_u64_list(&[], hr));
            jr.uvarint(0);
            jr.uvarint(0);
            jr.record(|tr| encode_table_entry(&m.tables[0], tr));
        });
        r.uvarint(0); // no retired
    });
    let body = body.into_vec();

    let mut out =
        Vec::with_capacity(HEADER_LEN + body.len() + crate::storage::segment::TRAILER_LEN);
    out.extend_from_slice(&MANIFEST_MAGIC);
    out.extend_from_slice(&MANIFEST_VERSION.to_le_bytes());
    footer::write_trailer(&mut out, MANIFEST_MAGIC, &body);
    out
}

/// Test-only: a lifecycle record whose one retired entry has an unknown reason byte.
#[cfg(test)]
fn encode_with_bad_retire_reason(m: &Manifest) -> Vec<u8> {
    let mut body = Sink::new();
    body.record(|r| encode_header_fields(m, r));
    body.record(|r| encode_tables(m, r));
    body.record(|r| encode_garbage(m, r));
    body.record(|r| {
        r.uvarint(1); // next_job_id
        r.uvarint(0); // no jobs
        r.uvarint(1); // one retired entry
        r.record(|rr| {
            rr.u8(99); // invalid reason
            rr.uvarint(1);
            rr.uvarint(1);
            rr.record(|sr| encode_u64_list(&[], sr));
            rr.record(|tr| encode_table_entry(&m.tables[0], tr));
        });
    });
    let body = body.into_vec();

    let mut out =
        Vec::with_capacity(HEADER_LEN + body.len() + crate::storage::segment::TRAILER_LEN);
    out.extend_from_slice(&MANIFEST_MAGIC);
    out.extend_from_slice(&MANIFEST_VERSION.to_le_bytes());
    footer::write_trailer(&mut out, MANIFEST_MAGIC, &body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Decimal, Ip, Value};
    use std::net::IpAddr;

    fn field(id: u64, name: &str, ty: DataType) -> SchemaField {
        SchemaField {
            id: FieldId(id),
            field: Field {
                name: name.to_string(),
                ty,
            },
        }
    }

    fn schema() -> Vec<SchemaField> {
        vec![
            field(3, "i", DataType::Int64),
            field(5, "s", DataType::String),
            field(8, "d", DataType::decimal(10, 2).unwrap()),
            field(9, "l", DataType::list(DataType::Int64).unwrap()),
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
            dir: "d/0000000000000007".to_string(),
            field_ids: vec![FieldId(3), FieldId(5), FieldId(8), FieldId(9)],
            // The file's own column order differs from the entry's: exercises the trailing
            // field in the ordinary round trip, not just the dedicated lifecycle test.
            file_field_ids: vec![FieldId(9), FieldId(3), FieldId(8), FieldId(5)],
        }
    }

    fn sample_manifest() -> Manifest {
        let mut options = BTreeMap::new();
        options.insert("retention".to_string(), "30d".to_string());
        options.insert("compression".to_string(), "zstd".to_string());
        let table_a = TableEntry {
            id: TableId(7),
            name: TableName::new("d", "a"),
            engine: "append".to_string(),
            schema: schema(),
            next_field_id: 12,
            key: vec![FieldId(3)],
            // Every clause names a different column, so a decoder that cross-wires two of them
            // (or drops one) cannot round-trip.
            version: Some(FieldId(9)),
            order_by: vec![FieldId(3), FieldId(8)],
            partition_by: Some(PartitionBy {
                column: FieldId(8),
                bucket: Duration::from_secs(3600),
            }),
            ttl: Some(Ttl {
                column: FieldId(5),
                after: Duration::from_secs(86_400 * 30),
            }),
            options,
            // One segment in the id layout and one still in the legacy one: `dir` is per segment.
            segments: vec![segment(1, 1), {
                let mut legacy = segment(2, 2);
                legacy.dir = "d/a".to_string();
                legacy
            }],
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
            id: TableId(2),
            name: TableName::new("d", "b"),
            engine: "append".to_string(),
            schema: vec![field(1, "ip", DataType::Ip)],
            next_field_id: 2,
            key: vec![],
            version: None,
            order_by: vec![],
            partition_by: None,
            ttl: None,
            options: BTreeMap::new(),
            segments: vec![],
            tombstones: vec![],
        };
        Manifest {
            version: 3,
            next_segment_id: 3,
            next_table_id: 10,
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
                    dir: "d/legacy-garbage".to_string(),
                    field_ids: Vec::new(),
                    file_field_ids: Vec::new(),
                },
                removed_at_ms: 42,
            }],
            next_job_id: 1,
            jobs: vec![],
            retired: vec![],
        }
    }

    #[test]
    fn codec_round_trips_a_manifest_with_stats_tombstones_ids_and_garbage() {
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
            dir: "d/0000000000000002".to_string(),
            field_ids: vec![FieldId(1)],
            file_field_ids: Vec::new(),
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
    fn a_definition_naming_a_field_id_outside_its_schema_is_corrupt() {
        // Each clause in turn: an engine indexes the schema by these ids, so a dangling one
        // must fail decode, not panic the next flush.
        let dangling = FieldId(99);
        let cases: [fn(&mut TableEntry, FieldId); 5] = [
            |t, id| t.key = vec![id],
            |t, id| t.version = Some(id),
            |t, id| t.order_by = vec![FieldId(3), id],
            |t, id| t.partition_by.as_mut().unwrap().column = id,
            |t, id| t.ttl.as_mut().unwrap().column = id,
        ];
        for (i, set) in cases.iter().enumerate() {
            let mut m = sample_manifest();
            set(&mut m.tables[0], dangling);
            match decode("m", &m.encode()) {
                Err(Error::Corrupt { detail, .. }) => {
                    assert!(detail.contains("unknown field id 99"), "case {i}: {detail}")
                }
                other => panic!("case {i}: expected Corrupt, got {other:?}"),
            }
        }
    }

    #[test]
    fn old_bytes_decode_ids_as_absent_and_manifest_decode_backfills_them() {
        let raw = pre_d0012_manifest();
        let bytes = encode_pre_d0012(&raw);

        let plain = decode("m", &bytes).unwrap();
        assert_eq!(plain.next_table_id, 0);
        for t in &plain.tables {
            assert_eq!(t.id, TableId(0));
            assert_eq!(t.next_field_id, 0);
            assert!(t.schema.iter().all(|f| f.id == FieldId(0)));
            for s in &t.segments {
                assert_eq!(s.dir, "");
                assert!(s.field_ids.is_empty());
            }
        }

        let backfilled = Manifest::decode("m", &bytes).unwrap();
        let ids: Vec<u64> = backfilled.tables.iter().map(|t| t.id.0).collect();
        assert_eq!(ids, vec![1, 2], "assigned in name order, never re-using 0");
        for t in &backfilled.tables {
            let field_ids: Vec<u64> = t.schema.iter().map(|f| f.id.0).collect();
            assert_eq!(
                field_ids,
                (1..=t.schema.len() as u64).collect::<Vec<_>>(),
                "assigned in schema order"
            );
            let want_dir = format!("{}/{}", t.name.db, t.name.name);
            for s in &t.segments {
                assert_eq!(s.dir, want_dir);
                let want: Vec<FieldId> = t.schema.iter().map(|f| f.id).collect();
                assert_eq!(s.field_ids, want);
            }
        }

        // Deterministic: decoding the same bytes twice gives the same ids.
        let again = Manifest::decode("m", &bytes).unwrap();
        assert_eq!(backfilled, again);
    }

    fn pre_d0012_manifest() -> Manifest {
        fn unassigned_field(name: &str, ty: DataType) -> SchemaField {
            SchemaField {
                id: FieldId(0),
                field: Field {
                    name: name.to_string(),
                    ty,
                },
            }
        }
        fn unassigned_segment(id: u64, seq: u64) -> SegmentEntry {
            SegmentEntry {
                id,
                partition: "_".to_string(),
                seq,
                rows: 1,
                bytes: 1,
                footer_crc: 0,
                columns: vec![ColumnStats {
                    rows: 1,
                    null_count: 0,
                    min: None,
                    max: None,
                }],
                side_files: Vec::new(),
                dir: String::new(),
                field_ids: Vec::new(),
                file_field_ids: Vec::new(),
            }
        }
        let table_a = TableEntry {
            id: TableId(0),
            name: TableName::new("d", "a"),
            engine: "append".to_string(),
            schema: vec![unassigned_field("x", DataType::Int64)],
            next_field_id: 0,
            key: vec![],
            version: None,
            order_by: vec![],
            partition_by: None,
            ttl: None,
            options: BTreeMap::new(),
            segments: vec![unassigned_segment(1, 1)],
            tombstones: vec![],
        };
        let table_b = TableEntry {
            id: TableId(0),
            name: TableName::new("d", "b"),
            engine: "append".to_string(),
            schema: vec![
                unassigned_field("y", DataType::Int64),
                unassigned_field("z", DataType::Int64),
            ],
            next_field_id: 0,
            key: vec![],
            version: None,
            order_by: vec![],
            partition_by: None,
            ttl: None,
            options: BTreeMap::new(),
            segments: vec![],
            tombstones: vec![],
        };
        Manifest {
            version: 1,
            next_segment_id: 2,
            next_table_id: 0,
            tables: vec![table_a, table_b],
            garbage: vec![],
            next_job_id: 1,
            jobs: vec![],
            retired: vec![],
        }
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
    fn a_name_off_disk_that_could_escape_the_root_is_corrupt() {
        let mut escaped_table = sample_manifest();
        escaped_table.tables[0].name.db = "..".to_string();
        let mut escaped_partition = sample_manifest();
        escaped_partition.tables[0].segments[0].partition = "../x".to_string();
        let mut escaped_dir = sample_manifest();
        escaped_dir.tables[0].segments[0].dir = "d/../x".to_string();
        for m in [escaped_table, escaped_partition, escaped_dir] {
            let r = Manifest::decode("manifest", &m.encode());
            assert!(matches!(r, Err(Error::Corrupt { .. })), "{r:?}");
        }
    }

    #[test]
    fn empty_manifest_round_trips() {
        let m = Manifest::empty();
        let bytes = m.encode();
        assert_eq!(Manifest::decode("m", &bytes).unwrap(), m);
    }

    // ── lifecycle: jobs, retired entries (SPEC §19, D0012, D0013) ─────────

    fn job_target(id: u64, name: &str) -> TableEntry {
        TableEntry {
            id: TableId(id),
            name: TableName::new("d", name),
            engine: "append".to_string(),
            schema: schema(),
            next_field_id: 12,
            key: vec![],
            version: None,
            order_by: vec![],
            partition_by: None,
            ttl: None,
            options: BTreeMap::new(),
            segments: vec![],
            tombstones: vec![],
        }
    }

    fn without_file_field_ids(mut m: Manifest) -> Manifest {
        for t in m.tables.iter_mut() {
            for s in t.segments.iter_mut() {
                s.file_field_ids = Vec::new();
            }
        }
        m
    }

    fn lifecycle_manifest() -> Manifest {
        let mut m = sample_manifest();
        // The reused segment's file layout differs from its own view: reordered, plus a
        // physical column (FieldId(0)) this job's target doesn't read.
        let mut reused = segment(50, 1);
        reused.file_field_ids = vec![FieldId(9), FieldId(0), FieldId(3), FieldId(8), FieldId(5)];
        let mut target_a = job_target(20, "a2");
        target_a.segments = vec![reused];

        let job1 = Job {
            id: 1,
            source: TableId(7),
            snapshot: 3,
            target: target_a,
            handled: vec![1, 2],
            reused: 1,
            rewritten: 0,
        };
        let job2 = Job {
            id: 2,
            source: TableId(2),
            snapshot: 3,
            target: job_target(21, "b2"),
            handled: vec![],
            reused: 0,
            rewritten: 1,
        };
        m.next_job_id = 3;
        m.jobs = vec![job1, job2];
        m.retired = vec![
            Retired {
                entry: job_target(30, "r1"),
                reason: RetireReason::Swapped,
                retired_at_ms: 1_000,
                version: 4,
                successor_segments: vec![10, 11],
            },
            Retired {
                entry: job_target(31, "r2"),
                reason: RetireReason::Reverted,
                retired_at_ms: 2_000,
                version: 5,
                successor_segments: vec![],
            },
            Retired {
                entry: job_target(32, "r3"),
                reason: RetireReason::Dropped,
                retired_at_ms: 3_000,
                version: 6,
                successor_segments: vec![],
            },
        ];
        m
    }

    #[test]
    fn lifecycle_round_trips_jobs_and_retired_entries() {
        let m = lifecycle_manifest();
        let bytes = m.encode();
        assert_eq!(Manifest::decode("m", &bytes).unwrap(), m);
    }

    #[test]
    fn pre_lifecycle_bytes_decode_with_empty_jobs_and_retired() {
        // The old segment record can't carry `file_field_ids`, so a manifest that has any must
        // be normalized before comparing against what the pre-lifecycle bytes decode to.
        let want = without_file_field_ids(sample_manifest());
        let bytes = encode_pre_lifecycle(&want);

        let plain = decode("m", &bytes).unwrap();
        assert_eq!(plain.next_job_id, 1);
        assert!(plain.jobs.is_empty());
        assert!(plain.retired.is_empty());
        for t in &plain.tables {
            for s in &t.segments {
                assert!(s.file_field_ids.is_empty());
            }
        }
        assert_eq!(plain, want);

        // Stable: re-encoding the backfilled result and decoding again agrees.
        let backfilled = Manifest::decode("m", &bytes).unwrap();
        assert_eq!(backfilled, want);
        let again = Manifest::decode("m", &backfilled.encode()).unwrap();
        assert_eq!(again, backfilled);
    }

    #[test]
    fn unknown_retire_reason_byte_is_corrupt() {
        let m = sample_manifest();
        let bytes = encode_with_bad_retire_reason(&m);
        match decode("m", &bytes) {
            Err(Error::Corrupt { detail, .. }) => {
                assert!(detail.contains("retire reason 99"), "{detail}")
            }
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn job_id_zero_is_corrupt() {
        let m = sample_manifest();
        let bytes = encode_with_bad_job_id(&m);
        match decode("m", &bytes) {
            Err(Error::Corrupt { detail, .. }) => assert!(detail.contains("job id 0"), "{detail}"),
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    /// `project_onto` indexes `columns` by a position found in `field_ids`: a manifest whose
    /// two lists disagree must be rejected on decode, never panic later.
    #[test]
    fn field_ids_longer_than_column_stats_is_corrupt() {
        let mut m = sample_manifest();
        let seg = m
            .tables
            .iter_mut()
            .flat_map(|t| t.segments.iter_mut())
            .find(|s| !s.field_ids.is_empty())
            .expect("sample has a segment with field ids");
        seg.field_ids.push(FieldId(999));
        match decode("m", &encode(&m)) {
            Err(Error::Corrupt { detail, .. }) => {
                assert!(detail.contains("field ids for"), "{detail}")
            }
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_non_zero_file_field_id_is_corrupt() {
        let m = sample_manifest();
        let bytes = encode_with_bad_file_field_ids(&m);
        match decode("m", &bytes) {
            Err(Error::Corrupt { detail, .. }) => {
                assert!(detail.contains("duplicate file field id 7"), "{detail}")
            }
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }
}
