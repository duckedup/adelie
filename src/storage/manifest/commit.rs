//! `Edit` and `Commit`: OCC per table (SPEC §5, §18) — appends never conflict; a commit
//! conflicts only over a segment no longer live or a table that already exists.

use crate::exec::Field;
use crate::types::{DataType, Value, coerce};

use super::error::Error;
use super::{
    Garbage, Manifest, Predicate, SegmentEntry, TableEntry, TableName, Tombstone, is_path_component,
};

/// One change to a `Manifest`. `Commit::apply` applies a list of these in order.
#[derive(Debug, Clone, PartialEq)]
pub enum Edit {
    CreateTable {
        name: TableName,
        engine: String,
        schema: Vec<Field>,
    },
    /// `seq` is set by the caller: the new version for a flush, the max input seq for a
    /// compaction.
    AddSegments {
        table: TableName,
        segments: Vec<SegmentEntry>,
    },
    /// Moves the named ids to garbage with `removed_at_ms`.
    RemoveSegments {
        table: TableName,
        ids: Vec<u64>,
    },
    /// The pushed tombstone's `seq` is the commit's new version.
    AddTombstone {
        table: TableName,
        predicates: Vec<Predicate>,
    },
    /// GC'd files. Missing ids are fine; this edit never conflicts.
    ForgetGarbage {
        ids: Vec<u64>,
    },
    ReserveSegmentIds {
        next: u64,
    },
}

/// A batch of edits applied together against a recorded `base` version. `base` is kept for
/// audit only: the scope gate chose per-table OCC, so only the edits below decide a conflict.
#[derive(Debug, Clone, PartialEq)]
pub struct Commit {
    pub base: u64,
    pub edits: Vec<Edit>,
}

impl Commit {
    /// OCC check and apply: returns the next manifest, `version = current.version + 1`.
    pub fn apply(&self, current: &Manifest, now_ms: u64) -> Result<Manifest, Error> {
        let mut next = current.clone();
        for edit in &self.edits {
            apply_edit(&mut next, edit, now_ms)?;
        }
        next.version = current.version + 1;
        Ok(next)
    }
}

fn apply_edit(m: &mut Manifest, edit: &Edit, now_ms: u64) -> Result<(), Error> {
    match edit {
        Edit::CreateTable {
            name,
            engine,
            schema,
        } => create_table(m, name, engine, schema),
        Edit::AddSegments { table, segments } => add_segments(m, table, segments),
        Edit::RemoveSegments { table, ids } => remove_segments(m, table, ids, now_ms),
        Edit::AddTombstone { table, predicates } => add_tombstone(m, table, predicates),
        Edit::ForgetGarbage { ids } => {
            forget_garbage(m, ids);
            Ok(())
        }
        Edit::ReserveSegmentIds { next } => {
            reserve_segment_ids(m, *next);
            Ok(())
        }
    }
}

fn table_index(m: &Manifest, table: &TableName) -> Result<usize, Error> {
    m.tables
        .iter()
        .position(|t| &t.name == table)
        .ok_or_else(|| Error::Conflict {
            table: table.to_string(),
            detail: "table does not exist".to_string(),
        })
}

fn create_table(
    m: &mut Manifest,
    name: &TableName,
    engine: &str,
    schema: &[Field],
) -> Result<(), Error> {
    if !is_path_component(&name.db) || !is_path_component(&name.name) {
        return Err(Error::Usage(format!(
            "table name {name}: db and name must each be one path component"
        )));
    }
    if m.table(name).is_some() {
        return Err(Error::Conflict {
            table: name.to_string(),
            detail: "table already exists".to_string(),
        });
    }
    let entry = TableEntry {
        name: name.clone(),
        engine: engine.to_string(),
        schema: schema.to_vec(),
        segments: Vec::new(),
        tombstones: Vec::new(),
    };
    let pos = m.tables.partition_point(|t| t.name < *name);
    m.tables.insert(pos, entry);
    Ok(())
}

fn add_segments(
    m: &mut Manifest,
    table: &TableName,
    segments: &[SegmentEntry],
) -> Result<(), Error> {
    let idx = table_index(m, table)?;
    let schema_len = m.tables[idx].schema.len();
    for s in segments {
        if s.columns.len() != schema_len {
            return Err(Error::Usage(format!(
                "segment {} has {} column stats, table {table} has {schema_len} columns",
                s.id,
                s.columns.len()
            )));
        }
    }
    let entry = &mut m.tables[idx];
    entry.segments.extend(segments.iter().cloned());
    entry.segments.sort_by_key(|s| (s.seq, s.id));
    let max_id = entry.segments.iter().map(|s| s.id).max();
    if let Some(max_id) = max_id {
        m.next_segment_id = m.next_segment_id.max(max_id + 1);
    }
    Ok(())
}

fn remove_segments(
    m: &mut Manifest,
    table: &TableName,
    ids: &[u64],
    now_ms: u64,
) -> Result<(), Error> {
    let idx = table_index(m, table)?;
    for &id in ids {
        if !m.tables[idx].segments.iter().any(|s| s.id == id) {
            return Err(Error::Conflict {
                table: table.to_string(),
                detail: format!("segment {id} no longer live"),
            });
        }
    }
    let mut removed = Vec::new();
    m.tables[idx].segments.retain(|s| {
        if ids.contains(&s.id) {
            let mut garbage = s.clone();
            garbage.columns.clear();
            removed.push(garbage);
            false
        } else {
            true
        }
    });
    for segment in removed {
        m.garbage.push(Garbage {
            table: table.clone(),
            segment,
            removed_at_ms: now_ms,
        });
    }
    Ok(())
}

fn add_tombstone(
    m: &mut Manifest,
    table: &TableName,
    predicates: &[Predicate],
) -> Result<(), Error> {
    let idx = table_index(m, table)?;
    let schema = &m.tables[idx].schema;
    let mut stored = Vec::with_capacity(predicates.len());
    for p in predicates {
        if p.value.is_null() {
            return Err(Error::Usage(format!(
                "tombstone predicate on {} has a null value",
                p.column
            )));
        }
        let field = schema
            .iter()
            .find(|f| f.name == p.column)
            .ok_or_else(|| Error::Usage(format!("unknown column {}", p.column)))?;
        // Stored at the column's exact type, as `Column::push` does: the codec writes a DECIMAL's
        // unscaled digits and reads them back at the column's scale.
        let value = coerce(&p.value, &field.ty)
            .filter(|v| value_matches_type(v, &field.ty))
            .ok_or_else(|| {
                Error::Usage(format!(
                    "tombstone predicate value for {} does not fit column type {}",
                    p.column, field.ty
                ))
            })?;
        stored.push(Predicate { value, ..p.clone() });
    }
    let seq = m.version + 1;
    m.tables[idx].tombstones.push(Tombstone {
        seq,
        predicates: stored,
    });
    Ok(())
}

/// The same value/type pairing `segment::value::encode_value` accepts, checked ahead of time so
/// a bad tombstone is `Usage` here rather than a panic at the next `Manifest::encode`.
fn value_matches_type(v: &Value, ty: &DataType) -> bool {
    matches!(
        (v, ty),
        (Value::Bool(_), DataType::Bool)
            | (Value::Int64(_), DataType::Int64)
            | (Value::UInt64(_), DataType::UInt64)
            | (Value::Float64(_), DataType::Float64)
            | (Value::Decimal(_), DataType::Decimal(_))
            | (Value::String(_), DataType::String)
            | (Value::Bytes(_), DataType::Bytes)
            | (Value::Timestamp(_), DataType::Timestamp)
            | (Value::Date(_), DataType::Date)
            | (Value::Uuid(_), DataType::Uuid)
            | (Value::Ip(_), DataType::Ip)
    )
}

fn forget_garbage(m: &mut Manifest, ids: &[u64]) {
    m.garbage.retain(|g| !ids.contains(&g.segment.id));
}

fn reserve_segment_ids(m: &mut Manifest, next: u64) {
    m.next_segment_id = m.next_segment_id.max(next);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::ColumnStats;
    use crate::storage::manifest::CmpOp;
    use crate::types::Decimal;

    fn schema() -> Vec<Field> {
        vec![Field {
            name: "a".to_string(),
            ty: DataType::Int64,
        }]
    }

    fn base_with_table() -> Manifest {
        let mut m = Manifest::empty();
        m = Commit {
            base: 0,
            edits: vec![Edit::CreateTable {
                name: TableName::new("d", "t"),
                engine: "append".to_string(),
                schema: schema(),
            }],
        }
        .apply(&m, 0)
        .unwrap();
        m
    }

    fn seg(id: u64, seq: u64) -> SegmentEntry {
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
                min: Some(Value::Int64(0)),
                max: Some(Value::Int64(0)),
            }],
            side_files: Vec::new(),
        }
    }

    #[test]
    fn a_table_name_that_could_escape_the_store_root_is_usage() {
        for (db, name) in [
            ("..", "t"),
            ("d", ".."),
            ("d", "a/b"),
            ("", "t"),
            ("d", "."),
            ("/abs", "t"),
        ] {
            let commit = Commit {
                base: 0,
                edits: vec![Edit::CreateTable {
                    name: TableName::new(db, name),
                    engine: "append".to_string(),
                    schema: schema(),
                }],
            };
            assert!(
                matches!(commit.apply(&Manifest::empty(), 0), Err(Error::Usage(_))),
                "{db:?}.{name:?}"
            );
        }
    }

    #[test]
    fn duplicate_create_table_conflicts() {
        let m = base_with_table();
        let commit = Commit {
            base: m.version,
            edits: vec![Edit::CreateTable {
                name: TableName::new("d", "t"),
                engine: "append".to_string(),
                schema: schema(),
            }],
        };
        assert!(matches!(commit.apply(&m, 0), Err(Error::Conflict { .. })));
    }

    #[test]
    fn append_from_a_stale_base_succeeds() {
        let m = base_with_table();
        let commit = Commit {
            base: 0, // stale: m.version is already 1
            edits: vec![Edit::AddSegments {
                table: TableName::new("d", "t"),
                segments: vec![seg(1, 1)],
            }],
        };
        let next = commit.apply(&m, 0).unwrap();
        assert_eq!(next.version, m.version + 1);
        assert_eq!(
            next.table(&TableName::new("d", "t"))
                .unwrap()
                .segments
                .len(),
            1
        );
    }

    #[test]
    fn add_segments_rejects_a_column_count_mismatch() {
        let m = base_with_table();
        let mut bad = seg(1, 1);
        bad.columns.clear();
        let commit = Commit {
            base: m.version,
            edits: vec![Edit::AddSegments {
                table: TableName::new("d", "t"),
                segments: vec![bad],
            }],
        };
        assert!(matches!(commit.apply(&m, 0), Err(Error::Usage(_))));
    }

    #[test]
    fn add_segments_bumps_next_segment_id_and_sorts_by_seq_then_id() {
        let m = base_with_table();
        let commit = Commit {
            base: m.version,
            edits: vec![Edit::AddSegments {
                table: TableName::new("d", "t"),
                segments: vec![seg(5, 2), seg(1, 1)],
            }],
        };
        let next = commit.apply(&m, 0).unwrap();
        let table = next.table(&TableName::new("d", "t")).unwrap();
        let ids: Vec<u64> = table.segments.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![1, 5]);
        assert_eq!(next.next_segment_id, 6);
    }

    #[test]
    fn removing_the_same_segment_twice_conflicts_on_the_second_commit() {
        let m = base_with_table();
        let m = Commit {
            base: m.version,
            edits: vec![Edit::AddSegments {
                table: TableName::new("d", "t"),
                segments: vec![seg(7, 1)],
            }],
        }
        .apply(&m, 0)
        .unwrap();

        let remove = Commit {
            base: m.version,
            edits: vec![Edit::RemoveSegments {
                table: TableName::new("d", "t"),
                ids: vec![7],
            }],
        };
        let after_first = remove.apply(&m, 100).unwrap();
        assert!(matches!(
            remove.apply(&after_first, 200),
            Err(Error::Conflict { .. })
        ));
    }

    #[test]
    fn remove_segments_moves_the_segment_to_garbage_with_removed_at_ms() {
        let m = base_with_table();
        let m = Commit {
            base: m.version,
            edits: vec![Edit::AddSegments {
                table: TableName::new("d", "t"),
                segments: vec![seg(7, 1)],
            }],
        }
        .apply(&m, 0)
        .unwrap();

        let next = Commit {
            base: m.version,
            edits: vec![Edit::RemoveSegments {
                table: TableName::new("d", "t"),
                ids: vec![7],
            }],
        }
        .apply(&m, 4242)
        .unwrap();

        assert!(
            next.table(&TableName::new("d", "t"))
                .unwrap()
                .segments
                .is_empty()
        );
        assert_eq!(next.garbage.len(), 1);
        assert_eq!(next.garbage[0].removed_at_ms, 4242);
        assert!(next.garbage[0].segment.columns.is_empty());
    }

    #[test]
    fn a_concurrent_flush_does_not_conflict_with_a_compaction_remove() {
        // Two compactions from the same base that remove the same segment conflict (above);
        // a flush (AddSegments) committed between a compaction's read and its publish must not.
        let m = base_with_table();
        let m = Commit {
            base: m.version,
            edits: vec![Edit::AddSegments {
                table: TableName::new("d", "t"),
                segments: vec![seg(7, 1)],
            }],
        }
        .apply(&m, 0)
        .unwrap();

        let flush = Commit {
            base: m.version,
            edits: vec![Edit::AddSegments {
                table: TableName::new("d", "t"),
                segments: vec![seg(8, 2)],
            }],
        };
        let compact = Commit {
            base: m.version,
            edits: vec![Edit::RemoveSegments {
                table: TableName::new("d", "t"),
                ids: vec![7],
            }],
        };
        let after_flush = flush.apply(&m, 0).unwrap();
        let after_compact = compact.apply(&after_flush, 0).unwrap();
        let ids: Vec<u64> = after_compact
            .table(&TableName::new("d", "t"))
            .unwrap()
            .segments
            .iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, vec![8]);
    }

    #[test]
    fn tombstone_seq_is_the_new_version() {
        let m = base_with_table();
        let next = Commit {
            base: m.version,
            edits: vec![Edit::AddTombstone {
                table: TableName::new("d", "t"),
                predicates: vec![Predicate {
                    column: "a".to_string(),
                    op: CmpOp::Eq,
                    value: Value::Int64(1),
                }],
            }],
        }
        .apply(&m, 0)
        .unwrap();
        assert_eq!(
            next.table(&TableName::new("d", "t")).unwrap().tombstones[0].seq,
            next.version
        );
    }

    fn decimal_table(precision: u8, scale: u8) -> Manifest {
        Commit {
            base: 0,
            edits: vec![Edit::CreateTable {
                name: TableName::new("d", "p"),
                engine: "append".to_string(),
                schema: vec![Field {
                    name: "price".to_string(),
                    ty: DataType::decimal(precision, scale).unwrap(),
                }],
            }],
        }
        .apply(&Manifest::empty(), 0)
        .unwrap()
    }

    fn delete_price(m: &Manifest, value: Value) -> Result<Manifest, Error> {
        Commit {
            base: m.version,
            edits: vec![Edit::AddTombstone {
                table: TableName::new("d", "p"),
                predicates: vec![Predicate {
                    column: "price".to_string(),
                    op: CmpOp::Eq,
                    value,
                }],
            }],
        }
        .apply(m, 0)
    }

    #[test]
    fn a_decimal_tombstone_is_rescaled_to_the_column_and_survives_a_reload() {
        // 10.5 at scale 1 into DECIMAL(10, 2): stored as 10.50, not read back as 1.05.
        let m = delete_price(
            &decimal_table(10, 2),
            Value::Decimal(Decimal::new(105, 1).unwrap()),
        )
        .unwrap();
        let reloaded = Manifest::decode("manifest", &m.encode()).unwrap();
        let t = &reloaded
            .table(&TableName::new("d", "p"))
            .unwrap()
            .tombstones[0];
        assert_eq!(
            t.predicates[0].value,
            Value::Decimal(Decimal::new(1050, 2).unwrap())
        );
    }

    #[test]
    fn a_decimal_tombstone_beyond_the_column_precision_is_usage() {
        // 1234567.89 cannot fit DECIMAL(3, 2); committing it would leave an undecodable manifest.
        let r = delete_price(
            &decimal_table(3, 2),
            Value::Decimal(Decimal::new(123_456_789, 2).unwrap()),
        );
        assert!(matches!(r, Err(Error::Usage(_))), "{r:?}");
    }

    #[test]
    fn tombstone_with_a_null_value_is_usage() {
        let m = base_with_table();
        let commit = Commit {
            base: m.version,
            edits: vec![Edit::AddTombstone {
                table: TableName::new("d", "t"),
                predicates: vec![Predicate {
                    column: "a".to_string(),
                    op: CmpOp::Eq,
                    value: Value::Null,
                }],
            }],
        };
        assert!(matches!(commit.apply(&m, 0), Err(Error::Usage(_))));
    }

    #[test]
    fn tombstone_on_an_unknown_column_is_usage() {
        let m = base_with_table();
        let commit = Commit {
            base: m.version,
            edits: vec![Edit::AddTombstone {
                table: TableName::new("d", "t"),
                predicates: vec![Predicate {
                    column: "missing".to_string(),
                    op: CmpOp::Eq,
                    value: Value::Int64(1),
                }],
            }],
        };
        assert!(matches!(commit.apply(&m, 0), Err(Error::Usage(_))));
    }

    #[test]
    fn add_segments_on_an_unknown_table_conflicts() {
        let m = Manifest::empty();
        let commit = Commit {
            base: 0,
            edits: vec![Edit::AddSegments {
                table: TableName::new("d", "t"),
                segments: vec![],
            }],
        };
        assert!(matches!(commit.apply(&m, 0), Err(Error::Conflict { .. })));
    }

    #[test]
    fn reserve_segment_ids_only_moves_forward() {
        let m = base_with_table();
        let next = Commit {
            base: m.version,
            edits: vec![Edit::ReserveSegmentIds { next: 10 }],
        }
        .apply(&m, 0)
        .unwrap();
        assert_eq!(next.next_segment_id, 10);
        let after = Commit {
            base: next.version,
            edits: vec![Edit::ReserveSegmentIds { next: 3 }],
        }
        .apply(&next, 0)
        .unwrap();
        assert_eq!(after.next_segment_id, 10);
    }

    #[test]
    fn forget_garbage_ignores_missing_ids() {
        let m = base_with_table();
        let next = Commit {
            base: m.version,
            edits: vec![Edit::ForgetGarbage { ids: vec![999] }],
        }
        .apply(&m, 0)
        .unwrap();
        assert!(next.garbage.is_empty());
    }
}
