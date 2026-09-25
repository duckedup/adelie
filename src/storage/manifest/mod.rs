//! Manifest v1 (SPEC §5, §18, D0009, D0012).

mod alter;
mod codec;
mod commit;
mod error;
mod ids;
mod ledger;
mod lifecycle;
mod publish;
mod snapshot;
mod spec;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::exec::{ColumnStats, Field};
use crate::types::Value;

pub use alter::{Alter, AlterPlan, explain, plan_alter, rewrites_segments};
pub use commit::{Commit, Edit};
pub use error::Error;
pub use ledger::MigrationRecord;
pub use lifecycle::{Job, RetireReason, Retired};
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
    /// The field id of each column of the segment *file*, in file order; `FieldId(0)` marks a
    /// file column this entry does not read. Empty means the file's columns are exactly
    /// `field_ids` (every flush and compaction output). A segment reused by a migration is
    /// referenced by two tables, each with its own `columns`/`field_ids` view of the one file.
    pub file_field_ids: Vec<FieldId>,
}

impl SegmentEntry {
    /// The file's column ids: `file_field_ids`, or `field_ids` when that is empty.
    pub fn file_ids(&self) -> &[FieldId] {
        if self.file_field_ids.is_empty() {
            &self.field_ids
        } else {
            &self.file_field_ids
        }
    }

    /// This segment as a table with `schema` sees it: `columns`/`field_ids` realigned to
    /// `schema` by field id (a field the entry has no stats for gets all-null stats:
    /// rows = null_count = self.rows, min = max = None), and `file_field_ids` = `file_ids()`
    /// with every id the entry has no stats for replaced by `FieldId(0)` — the reader then
    /// yields NULL for it, so data never disagrees with stats. Stored empty when the result
    /// equals the new `field_ids`.
    pub fn project_onto(&self, schema: &[SchemaField]) -> SegmentEntry {
        let field_ids: Vec<FieldId> = schema.iter().map(|f| f.id).collect();
        let columns: Vec<ColumnStats> = schema
            .iter()
            .map(|f| match self.field_ids.iter().position(|&id| id == f.id) {
                Some(j) => self.columns[j].clone(),
                None => ColumnStats {
                    rows: self.rows as usize,
                    null_count: self.rows as usize,
                    min: None,
                    max: None,
                },
            })
            .collect();
        // A file column keeps its id only when self itself has real stats for it (was one of
        // self's own `field_ids`); everything else — a column `self` never read, or the
        // sentinel `FieldId(0)` — becomes/stays `FieldId(0)`, whether or not `schema` wants it.
        let file_field_ids: Vec<FieldId> = self
            .file_ids()
            .iter()
            .map(|&id| {
                if id.0 != 0 && self.field_ids.contains(&id) {
                    id
                } else {
                    FieldId(0)
                }
            })
            .collect();
        let file_field_ids = if file_field_ids == field_ids {
            Vec::new()
        } else {
            file_field_ids
        };
        SegmentEntry {
            id: self.id,
            partition: self.partition.clone(),
            seq: self.seq,
            rows: self.rows,
            bytes: self.bytes,
            footer_crc: self.footer_crc,
            columns,
            side_files: self.side_files.clone(),
            dir: self.dir.clone(),
            field_ids,
            file_field_ids,
        }
    }
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
    /// The version whose commit released the segment; 0 on manifests written before it was
    /// recorded, which retention then does not protect (SPEC §19 AT VERSION, D0014).
    pub removed_at_version: u64,
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
    /// The next job id to hand out; starts at 1 (0 is never a job).
    pub next_job_id: u64,
    /// Running migrations, ascending by id (SPEC §5 "running jobs", §19).
    pub jobs: Vec<Job>,
    /// Entries kept for the grace period, in the order they were retired.
    pub retired: Vec<Retired>,
    /// Applied migration files, in apply order.
    pub migrations: Vec<MigrationRecord>,
}

impl Manifest {
    pub fn empty() -> Manifest {
        Manifest {
            version: 0,
            next_segment_id: 0,
            next_table_id: 1,
            tables: Vec::new(),
            garbage: Vec::new(),
            next_job_id: 1,
            jobs: Vec::new(),
            retired: Vec::new(),
            migrations: Vec::new(),
        }
    }

    pub fn table(&self, name: &TableName) -> Option<&TableEntry> {
        self.tables.iter().find(|t| &t.name == name)
    }

    /// Whether any live table, retired entry or job target lists segment `id`: the reference
    /// count D0012 asks for ("GC counts references across tables").
    pub fn references_segment(&self, id: u64) -> bool {
        let has = |segs: &[SegmentEntry]| segs.iter().any(|s| s.id == id);
        self.tables.iter().any(|t| has(&t.segments))
            || self.retired.iter().any(|r| has(&r.entry.segments))
            || self.jobs.iter().any(|j| has(&j.target.segments))
    }

    pub fn job(&self, id: u64) -> Option<&Job> {
        self.jobs.iter().find(|j| j.id == id)
    }

    /// The running job migrating live table `name`, if any.
    pub fn job_for(&self, name: &TableName) -> Option<&Job> {
        let id = self.table(name)?.id;
        self.jobs.iter().find(|j| j.source == id)
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
            file_field_ids: Vec::new(),
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

    // ── project_onto ────────────────────────────────────────────────────

    fn schema_field(id: u64, name: &str) -> SchemaField {
        SchemaField {
            id: FieldId(id),
            field: Field {
                name: name.to_string(),
                ty: crate::types::DataType::Int64,
            },
        }
    }

    fn stats(v: i64) -> ColumnStats {
        ColumnStats {
            rows: 10,
            null_count: 0,
            min: Some(Value::Int64(v)),
            max: Some(Value::Int64(v)),
        }
    }

    /// Never reused: `file_field_ids` empty, so `file_ids()` is exactly `field_ids`.
    fn plain_segment() -> SegmentEntry {
        let mut s = seg(1, 1);
        s.rows = 10;
        s.field_ids = vec![FieldId(1), FieldId(2)];
        s.columns = vec![stats(1), stats(2)];
        s
    }

    /// Reused: the file has a third physical column (id 3) this entry doesn't read.
    fn reused_segment() -> SegmentEntry {
        let mut s = plain_segment();
        s.file_field_ids = vec![FieldId(1), FieldId(2), FieldId(3)];
        s
    }

    #[test]
    fn project_onto_add_gives_the_new_field_all_null_stats() {
        let s = reused_segment();
        let schema = vec![
            schema_field(1, "a"),
            schema_field(2, "b"),
            schema_field(4, "c"),
        ];
        let p = s.project_onto(&schema);
        assert_eq!(p.field_ids, vec![FieldId(1), FieldId(2), FieldId(4)]);
        assert_eq!(p.columns[0], s.columns[0]);
        assert_eq!(p.columns[1], s.columns[1]);
        assert_eq!(p.columns[2].rows, p.columns[2].null_count);
        assert_eq!(p.columns[2].rows, s.rows as usize);
        assert_eq!(p.columns[2].min, None);
        assert_eq!(p.columns[2].max, None);
        // id 3 was never in `field_ids`, so it never gets a slot at all; only the physical
        // columns the entry actually read (1, 2) survive, id 3 zeroed.
        assert_eq!(p.file_field_ids, vec![FieldId(1), FieldId(2), FieldId(0)]);
    }

    #[test]
    fn project_onto_drop_removes_stats_but_keeps_the_file_id() {
        let s = reused_segment();
        let schema = vec![schema_field(1, "a")];
        let p = s.project_onto(&schema);
        assert_eq!(p.field_ids, vec![FieldId(1)]);
        assert_eq!(p.columns, vec![s.columns[0].clone()]);
        // id 2's stats are gone (not in the new schema), but the file still carries it, so its
        // slot in file_field_ids keeps the real id, not FieldId(0).
        assert_eq!(p.file_field_ids, vec![FieldId(1), FieldId(2), FieldId(0)]);
    }

    #[test]
    fn project_onto_rename_keeps_stats_and_stores_file_field_ids_empty() {
        let s = plain_segment();
        let schema = vec![schema_field(1, "renamed"), schema_field(2, "b")];
        let p = s.project_onto(&schema);
        assert_eq!(p.field_ids, vec![FieldId(1), FieldId(2)]);
        assert_eq!(p.columns, s.columns);
        assert!(p.file_field_ids.is_empty(), "{:?}", p.file_field_ids);
    }

    #[test]
    fn project_onto_a_column_the_entry_never_read_stays_null_even_if_the_target_wants_it() {
        let s = reused_segment();
        // The target now wants id 3 too, but `s` never had stats for it — only the file
        // happened to label that physical column with it — so it must not be resurrected.
        let schema = vec![
            schema_field(1, "a"),
            schema_field(2, "b"),
            schema_field(3, "c"),
        ];
        let p = s.project_onto(&schema);
        let c3 = &p.columns[2];
        assert_eq!(c3.rows, c3.null_count);
        assert_eq!(c3.min, None);
        assert_eq!(c3.max, None);
        assert_eq!(p.file_field_ids, vec![FieldId(1), FieldId(2), FieldId(0)]);
    }

    // ── references_segment / job / job_for ──────────────────────────────

    fn job_with_target(target: TableEntry) -> Job {
        Job {
            id: 1,
            source: TableId(1),
            snapshot: 0,
            target,
            handled: vec![],
            reused: 0,
            rewritten: 0,
        }
    }

    fn retired_with_entry(entry: TableEntry) -> Retired {
        Retired {
            entry,
            reason: RetireReason::Swapped,
            retired_at_ms: 0,
            version: 1,
            successor_segments: vec![],
        }
    }

    #[test]
    fn references_segment_finds_a_live_table_a_retired_entry_or_a_job_target() {
        let mut m = Manifest::empty();
        assert!(!m.references_segment(9));

        let mut live = table_entry();
        live.segments = vec![seg(9, 1)];
        m.tables.push(live);
        assert!(m.references_segment(9));
        m.tables.clear();
        assert!(!m.references_segment(9));

        let mut retired_entry = table_entry();
        retired_entry.segments = vec![seg(9, 1)];
        m.retired.push(retired_with_entry(retired_entry));
        assert!(m.references_segment(9));
        m.retired.clear();
        assert!(!m.references_segment(9));

        let mut target = table_entry();
        target.segments = vec![seg(9, 1)];
        m.jobs.push(job_with_target(target));
        assert!(m.references_segment(9));
        m.jobs.clear();
        assert!(!m.references_segment(9));
    }

    #[test]
    fn job_and_job_for_look_up_by_id_and_by_live_table_name() {
        let mut m = Manifest::empty();
        let mut source = table_entry();
        source.id = TableId(5);
        source.name = TableName::new("d", "t");
        m.tables.push(source);

        let mut target = table_entry();
        target.id = TableId(6);
        let job = job_with_target(target);
        m.jobs.push(Job {
            source: TableId(5),
            ..job
        });

        assert_eq!(m.job(1).map(|j| j.source), Some(TableId(5)));
        assert_eq!(m.job(2), None);
        assert_eq!(m.job_for(&TableName::new("d", "t")).map(|j| j.id), Some(1));
        assert_eq!(m.job_for(&TableName::new("d", "other")), None);
    }
}
