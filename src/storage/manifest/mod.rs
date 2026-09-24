//! Manifest v1 (SPEC §5, §18, D0009, D0012).

mod codec;
mod commit;
mod error;
mod ids;
mod publish;
mod snapshot;
mod spec;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::exec::{ColumnStats, Field};
use crate::types::Value;

pub use commit::{Commit, Edit};
pub use error::Error;
pub use publish::{MANIFEST_FILE, Publisher};
pub use snapshot::Snapshot;
pub use spec::TableSpec;

/// A table's fully qualified name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TableName {
    pub db: String,
    pub name: String,
}

impl TableName {
    pub fn new(db: impl Into<String>, name: impl Into<String>) -> Self {
        TableName {
            db: db.into(),
            name: name.into(),
        }
    }
}

/// A db, table or partition name becomes one directory under the store root, so it must be a
/// single, ordinary path component: never empty, `.`, `..`, or holding a separator or NUL.
pub(crate) fn is_path_component(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\', '\0'])
}

impl fmt::Display for TableName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.db, self.name)
    }
}

/// A table's stable id (D0012): store-wide, assigned once, never reused. 0 = not yet assigned
/// (only ever seen between decode and `ids::assign_missing`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TableId(pub u64);

/// A column's stable id (D0012): per table, assigned once, never reused. 0 = not yet assigned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FieldId(pub u64);

/// A manifest schema column: its stable id plus the id-less `Field` batches and segments use.
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaField {
    pub id: FieldId,
    pub field: Field,
}

/// `PARTITION BY column, bucket` (SPEC §18): the bucket width as a duration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PartitionBy {
    pub column: FieldId,
    pub bucket: Duration,
}

/// `TTL column, after` (SPEC §18).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ttl {
    pub column: FieldId,
    pub after: Duration,
}

/// A file attached to a segment outside its own bytes. v1 defines no kinds: any side file
/// present in an encoded manifest is rejected on decode (`Error::UnknownSideFile`).
#[derive(Debug, Clone, PartialEq)]
pub struct SideFile {
    pub kind: u64,
    pub path: String,
    pub crc: u32,
}

/// One live segment of a table.
#[derive(Debug, Clone, PartialEq)]
pub struct SegmentEntry {
    pub id: u64,
    pub partition: String,
    /// The commit sequence that added this segment (§18 hook 3).
    pub seq: u64,
    pub rows: u64,
    pub bytes: u64,
    pub footer_crc: u32,
    /// One entry per schema field, segment-wide.
    pub columns: Vec<ColumnStats>,
    pub side_files: Vec<SideFile>,
    /// The table directory this segment lives in, relative to the store root and `/`-separated:
    /// `<db>/<table-id:016x>` for segments written since D0012, `<db>/<name>` for older ones.
    /// A segment is found by this, never by a path derived from its table (SPEC §5).
    pub dir: String,
    /// The field id of each entry of `columns`, in the same order.
    pub field_ids: Vec<FieldId>,
}

/// A tombstone predicate's comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// `column <op> value`. `value` is never `Value::Null`.
#[derive(Debug, Clone, PartialEq)]
pub struct Predicate {
    pub column: String,
    pub op: CmpOp,
    pub value: Value,
}

/// An AND of predicates. Applies to segments with `seq < self.seq` (`TableEntry::tombstones_for`).
#[derive(Debug, Clone, PartialEq)]
pub struct Tombstone {
    pub seq: u64,
    pub predicates: Vec<Predicate>,
}

/// A segment removed from a table, kept until it is safe to delete (E4 scope: GC'd by the
/// store once no live `Snapshot` names it and `gc_grace` has elapsed).
#[derive(Debug, Clone, PartialEq)]
pub struct Garbage {
    pub table: TableName,
    pub segment: SegmentEntry,
    pub removed_at_ms: u64,
}

/// One table's schema, engine, D0012 definition and live state.
#[derive(Debug, Clone, PartialEq)]
pub struct TableEntry {
    pub id: TableId,
    pub name: TableName,
    pub engine: String,
    pub schema: Vec<SchemaField>,
    /// The next field id to hand out; always > every id in `schema` (never reused, D0012).
    pub next_field_id: u64,
    pub key: Vec<FieldId>,
    pub version: Option<FieldId>,
    /// Stored resolved: for a keyed engine with no ORDER BY, this is `key` (SPEC §18 defaults).
    pub order_by: Vec<FieldId>,
    pub partition_by: Option<PartitionBy>,
    pub ttl: Option<Ttl>,
    pub options: BTreeMap<String, String>,
    /// Ascending by `(seq, id)`.
    pub segments: Vec<SegmentEntry>,
    /// Ascending by `seq`.
    pub tombstones: Vec<Tombstone>,
}

impl TableEntry {
    /// Tombstones with `t.seq > seg.seq`: the scan-resolution contract E5 consumes.
    pub fn tombstones_for(&self, seg: &SegmentEntry) -> Vec<&Tombstone> {
        self.tombstones.iter().filter(|t| t.seq > seg.seq).collect()
    }

    /// The id-less schema every engine, segment writer and `write_many` comparison uses.
    pub fn fields(&self) -> Vec<Field> {
        self.schema.iter().map(|f| f.field.clone()).collect()
    }

    /// `<db>/<table-id:016x>`: where this table's new segments are written (SPEC §5, D0012).
    pub fn dir(&self) -> String {
        format!("{}/{:016x}", self.name.db, self.id.0)
    }

    pub fn field(&self, id: FieldId) -> Option<&SchemaField> {
        self.schema.iter().find(|f| f.id == id)
    }

    /// The inverse of create, with every id resolved back to its column name: what `SHOW CREATE
    /// TABLE` prints, and what a reopen compares against the spec it was created with. An ORDER BY
    /// equal to KEY is SPEC §18's default for a keyed engine, so it is left implicit, as a spec
    /// that omitted it was written.
    pub fn spec(&self) -> TableSpec {
        let name_of = |id: FieldId| -> String {
            self.field(id)
                .map(|f| f.field.name.clone())
                .unwrap_or_default()
        };
        let mut spec = TableSpec::new(self.name.clone(), self.fields()).engine(&self.engine);
        if !self.key.is_empty() {
            spec = spec.key(self.key.iter().map(|&id| name_of(id)));
        }
        if let Some(v) = self.version {
            spec = spec.version(name_of(v));
        }
        if !self.order_by.is_empty() && self.order_by != self.key {
            spec = spec.order_by(self.order_by.iter().map(|&id| name_of(id)));
        }
        if let Some(p) = &self.partition_by {
            spec = spec.partition_by(name_of(p.column), p.bucket);
        }
        if let Some(t) = &self.ttl {
            spec = spec.ttl(name_of(t.column), t.after);
        }
        for (k, v) in &self.options {
            spec = spec.with(k.clone(), v.clone());
        }
        spec
    }
}

/// The one mutable object (SPEC §5): the live segment set per table, the table schemas, and a
/// monotonic version. Published by write-to-temp, fsync, rename, fsync-directory.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    /// 0 = an empty store that was never published.
    pub version: u64,
    pub next_segment_id: u64,
    /// The next table id to hand out (D0012); real ids start at 1, so a fresh store's first
    /// table is 1, never 0 (0 means "unassigned", only ever seen mid-backfill).
    pub next_table_id: u64,
    /// Ascending by name.
    pub tables: Vec<TableEntry>,
    pub garbage: Vec<Garbage>,
}

impl Manifest {
    pub fn empty() -> Manifest {
        Manifest {
            version: 0,
            next_segment_id: 0,
            next_table_id: 1,
            tables: Vec::new(),
            garbage: Vec::new(),
        }
    }

    pub fn table(&self, name: &TableName) -> Option<&TableEntry> {
        self.tables.iter().find(|t| &t.name == name)
    }

    pub fn encode(&self) -> Vec<u8> {
        codec::encode(self)
    }

    /// Decodes, then backfills whatever a manifest written before D0012 lacks
    /// (`ids::assign_missing`): deterministic, so every open of the same bytes agrees.
    pub fn decode(path: &str, bytes: &[u8]) -> Result<Manifest, Error> {
        let mut m = codec::decode(path, bytes)?;
        ids::assign_missing(&mut m);
        Ok(m)
    }

    /// `root` joined with each `/`-component of `dir` — the one place a table directory is
    /// turned into a path, shared by `segment_path` and the flush/compact writers.
    pub fn table_dir(root: &Path, dir: &str) -> PathBuf {
        let mut p = root.to_path_buf();
        for part in dir.split('/') {
            p.push(part);
        }
        p
    }

    /// `<table_dir(root, seg.dir)>/partition/{id:016x}.seg`. The table argument is dropped
    /// deliberately: a segment is found by its own recorded directory, never derived from the
    /// table that currently owns it (SPEC §5, D0012).
    pub fn segment_path(root: &Path, seg: &SegmentEntry) -> PathBuf {
        Self::table_dir(root, &seg.dir)
            .join(&seg.partition)
            .join(format!("{:016x}.seg", seg.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(id: u64, seq: u64) -> SegmentEntry {
        SegmentEntry {
            id,
            partition: "_".to_string(),
            seq,
            rows: 1,
            bytes: 1,
            footer_crc: 0,
            columns: Vec::new(),
            side_files: Vec::new(),
            dir: "d/t".to_string(),
            field_ids: Vec::new(),
        }
    }

    fn table_entry() -> TableEntry {
        TableEntry {
            id: TableId(1),
            name: TableName::new("d", "t"),
            engine: "append".to_string(),
            schema: vec![],
            next_field_id: 1,
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

    #[test]
    fn empty_manifest_has_no_tables_and_starts_ids_at_one() {
        let m = Manifest::empty();
        assert_eq!(m.version, 0);
        assert_eq!(m.next_table_id, 1);
        assert!(m.tables.is_empty());
        assert_eq!(m.table(&TableName::new("d", "t")), None);
    }

    #[test]
    fn tombstones_for_only_returns_tombstones_newer_than_the_segment() {
        let mut table = table_entry();
        table.tombstones = vec![
            Tombstone {
                seq: 3,
                predicates: vec![],
            },
            Tombstone {
                seq: 5,
                predicates: vec![],
            },
        ];
        assert_eq!(table.tombstones_for(&seg(1, 2)).len(), 2);
        assert_eq!(table.tombstones_for(&seg(1, 3)).len(), 1);
        assert_eq!(table.tombstones_for(&seg(1, 5)).len(), 0);
    }

    #[test]
    fn dir_is_db_slash_table_id_in_16_digit_hex() {
        let mut t = table_entry();
        t.id = TableId(0x2a);
        assert_eq!(t.dir(), "d/000000000000002a");
    }

    #[test]
    fn fields_drops_the_ids_and_keeps_declaration_order() {
        let mut t = table_entry();
        t.schema = vec![
            SchemaField {
                id: FieldId(3),
                field: Field {
                    name: "a".to_string(),
                    ty: crate::types::DataType::Int64,
                },
            },
            SchemaField {
                id: FieldId(5),
                field: Field {
                    name: "b".to_string(),
                    ty: crate::types::DataType::String,
                },
            },
        ];
        assert_eq!(
            t.fields(),
            vec![
                Field {
                    name: "a".to_string(),
                    ty: crate::types::DataType::Int64
                },
                Field {
                    name: "b".to_string(),
                    ty: crate::types::DataType::String
                },
            ]
        );
    }

    #[test]
    fn spec_is_the_inverse_of_create() {
        let mut t = table_entry();
        t.engine = "latest".to_string();
        t.schema = vec![
            SchemaField {
                id: FieldId(3),
                field: Field {
                    name: "id".to_string(),
                    ty: crate::types::DataType::Int64,
                },
            },
            SchemaField {
                id: FieldId(5),
                field: Field {
                    name: "ts".to_string(),
                    ty: crate::types::DataType::Timestamp,
                },
            },
        ];
        t.key = vec![FieldId(3)];
        t.version = Some(FieldId(5));
        t.order_by = vec![FieldId(3), FieldId(5)];
        t.options.insert("x".to_string(), "y".to_string());

        let spec = t.spec();
        assert_eq!(spec.engine, "latest");
        assert_eq!(spec.key, vec!["id".to_string()]);
        assert_eq!(spec.version, Some("ts".to_string()));
        assert_eq!(spec.order_by, vec!["id".to_string(), "ts".to_string()]);
        assert_eq!(spec.options.get("x"), Some(&"y".to_string()));
    }

    #[test]
    fn table_dir_joins_every_slash_separated_component() {
        assert_eq!(
            Manifest::table_dir(Path::new("/root"), "d/000000000000002a"),
            Path::new("/root/d/000000000000002a")
        );
    }

    #[test]
    fn segment_path_reads_the_segments_own_dir_not_the_table() {
        let mut s = seg(0x2a, 1);
        s.partition = "p".to_string();
        s.dir = "d/000000000000002a".to_string();
        let path = Manifest::segment_path(Path::new("/root"), &s);
        assert_eq!(
            path,
            Path::new("/root/d/000000000000002a/p/000000000000002a.seg")
        );
    }

    #[test]
    fn segment_path_keeps_a_legacy_name_directory() {
        let mut s = seg(1, 1);
        s.dir = "d/t".to_string();
        let path = Manifest::segment_path(Path::new("/root"), &s);
        assert_eq!(path, Path::new("/root/d/t/_/0000000000000001.seg"));
    }

    #[test]
    fn table_name_displays_as_db_dot_name() {
        assert_eq!(TableName::new("d", "t").to_string(), "d.t");
    }

    #[test]
    fn table_name_orders_by_db_then_name() {
        assert!(TableName::new("a", "z") < TableName::new("b", "a"));
        assert!(TableName::new("a", "a") < TableName::new("a", "b"));
    }

    #[test]
    fn cmp_op_is_copy() {
        let op = CmpOp::Eq;
        let copied = op;
        assert_eq!(op, copied);
    }
}
