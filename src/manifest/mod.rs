//! Manifest v1 (SPEC §5, §18, D0009).

mod codec;
mod commit;
mod error;
mod publish;
mod snapshot;

use std::fmt;
use std::path::{Path, PathBuf};

use crate::exec::{ColumnStats, Field};
use crate::types::Value;

pub use commit::{Commit, Edit};
pub use error::Error;
pub use publish::{MANIFEST_FILE, Publisher};
pub use snapshot::Snapshot;

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

/// One table's schema, engine and live state.
#[derive(Debug, Clone, PartialEq)]
pub struct TableEntry {
    pub name: TableName,
    pub engine: String,
    pub schema: Vec<Field>,
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
}

/// The one mutable object (SPEC §5): the live segment set per table, the table schemas, and a
/// monotonic version. Published by write-to-temp, fsync, rename, fsync-directory.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    /// 0 = an empty store that was never published.
    pub version: u64,
    pub next_segment_id: u64,
    /// Ascending by name.
    pub tables: Vec<TableEntry>,
    pub garbage: Vec<Garbage>,
}

impl Manifest {
    pub fn empty() -> Manifest {
        Manifest {
            version: 0,
            next_segment_id: 0,
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

    pub fn decode(path: &str, bytes: &[u8]) -> Result<Manifest, Error> {
        codec::decode(path, bytes)
    }

    /// `root/db/name/partition/{id:016x}.seg`
    pub fn segment_path(root: &Path, table: &TableName, seg: &SegmentEntry) -> PathBuf {
        root.join(&table.db)
            .join(&table.name)
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
        }
    }

    #[test]
    fn empty_manifest_has_no_tables_and_version_zero() {
        let m = Manifest::empty();
        assert_eq!(m.version, 0);
        assert!(m.tables.is_empty());
        assert_eq!(m.table(&TableName::new("d", "t")), None);
    }

    #[test]
    fn tombstones_for_only_returns_tombstones_newer_than_the_segment() {
        let table = TableEntry {
            name: TableName::new("d", "t"),
            engine: "append".to_string(),
            schema: vec![],
            segments: vec![],
            tombstones: vec![
                Tombstone {
                    seq: 3,
                    predicates: vec![],
                },
                Tombstone {
                    seq: 5,
                    predicates: vec![],
                },
            ],
        };
        assert_eq!(table.tombstones_for(&seg(1, 2)).len(), 2);
        assert_eq!(table.tombstones_for(&seg(1, 3)).len(), 1);
        assert_eq!(table.tombstones_for(&seg(1, 5)).len(), 0);
    }

    #[test]
    fn segment_path_is_root_db_name_partition_hex_id() {
        let table = TableName::new("d", "t");
        let mut s = seg(0x2a, 1);
        s.partition = "p".to_string();
        let path = Manifest::segment_path(Path::new("/root"), &table, &s);
        assert_eq!(path, Path::new("/root/d/t/p/000000000000002a.seg"));
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
