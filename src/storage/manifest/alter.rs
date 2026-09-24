//! `Alter` and the migration planner (SPEC §19, D0012): what a set of `ALTER TABLE` clauses
//! would do to a table's definition, computed without touching any segment.

use std::time::Duration;

use crate::exec::Field;
use crate::types::DataType;

use super::error::Error;
use super::{FieldId, SchemaField, TableEntry, TableId, TableSpec};

/// One step of a migration (SPEC §19). Engine, KEY and VERSION have no op: they are fixed for
/// a table's life (D0012).
#[derive(Debug, Clone, PartialEq)]
pub enum Alter {
    AddColumn(Field),
    DropColumn(String),
    RenameColumn {
        from: String,
        to: String,
    },
    /// Resolved as SPEC §18 does: empty on a keyed engine means KEY.
    OrderBy(Vec<String>),
    /// Refused (`MigrationNotBuilt`): flush writes every segment to partition `_` today.
    PartitionBy(Option<(String, Duration)>),
    /// Refused (`MigrationNotBuilt`): widening needs coerce-on-read, not built.
    SetType {
        column: String,
        ty: DataType,
    },
}

/// What a migration would do (`EXPLAIN ALTER TABLE`, SPEC §19).
#[derive(Debug, Clone, PartialEq)]
pub struct AlterPlan {
    pub segments: usize,
    pub rewrites: usize,
    pub rewrite_bytes: u64,
}

// Rollups (SPEC §16.1) are not built; adelie-zit.2 fills this in (D0013).
fn rollups_using(_t: &TableEntry, _f: FieldId) -> Vec<String> {
    Vec::new()
}

/// The target definition `ops` produce from `source`: surviving columns keep their field ids,
/// a renamed column keeps its id, an added column takes `next_field_id` (never reused). The
/// result has `id = TableId(0)` (CreateJob assigns it) and no segments or tombstones.
pub fn plan_alter(source: &TableEntry, ops: &[Alter]) -> Result<TableEntry, Error> {
    let table = source.name.to_string();
    if ops.is_empty() {
        return Err(Error::Usage(format!(
            "table {table}: ALTER TABLE needs at least one clause"
        )));
    }

    let mut schema = source.schema.clone();
    let mut next_field_id = source.next_field_id;
    // Name-based, mirroring `TableEntry::spec().order_by`: empty means implicit (defaults to
    // KEY for a keyed engine).
    let mut order_by = source.spec().order_by;

    for op in ops {
        match op {
            Alter::AddColumn(f) => {
                if schema.iter().any(|sf| sf.field.name == f.name) {
                    return Err(Error::Usage(format!(
                        "table {table}: column {} already exists",
                        f.name
                    )));
                }
                schema.push(SchemaField {
                    id: FieldId(next_field_id),
                    field: f.clone(),
                });
                next_field_id += 1;
            }
            Alter::DropColumn(c) => {
                let idx = schema
                    .iter()
                    .position(|sf| &sf.field.name == c)
                    .ok_or_else(|| Error::Usage(format!("table {table}: unknown column {c}")))?;
                let id = schema[idx].id;
                if let Some(rollup) = rollups_using(source, id).into_iter().next() {
                    return Err(Error::ColumnUsedByRollup {
                        table,
                        column: c.clone(),
                        rollup,
                    });
                }
                if source.key.contains(&id) {
                    return Err(Error::FixedColumn {
                        table,
                        column: c.clone(),
                        clause: "KEY".to_string(),
                    });
                }
                if source.version == Some(id) {
                    return Err(Error::FixedColumn {
                        table,
                        column: c.clone(),
                        clause: "VERSION".to_string(),
                    });
                }
                if order_by.iter().any(|n| n == c) {
                    return Err(Error::Usage(format!(
                        "table {table}: column {c} is in ORDER BY; change ORDER BY first"
                    )));
                }
                if source.partition_by.is_some_and(|p| p.column == id) {
                    return Err(Error::Usage(format!(
                        "table {table}: column {c} is in PARTITION BY; change PARTITION BY first"
                    )));
                }
                if source.ttl.is_some_and(|t| t.column == id) {
                    return Err(Error::Usage(format!(
                        "table {table}: column {c} is in TTL; change TTL first"
                    )));
                }
                let orig_name = source
                    .field(id)
                    .map(|f| f.field.name.clone())
                    .unwrap_or_else(|| c.clone());
                if let Some(seq) = source.tombstones.iter().find_map(|t| {
                    t.predicates
                        .iter()
                        .any(|p| p.column == orig_name)
                        .then_some(t.seq)
                }) {
                    return Err(Error::ColumnInTombstone {
                        table,
                        column: c.clone(),
                        seq,
                    });
                }
                schema.remove(idx);
            }
            Alter::RenameColumn { from, to } => {
                let idx = schema
                    .iter()
                    .position(|sf| &sf.field.name == from)
                    .ok_or_else(|| Error::Usage(format!("table {table}: unknown column {from}")))?;
                if schema.iter().any(|sf| &sf.field.name == to) {
                    return Err(Error::Usage(format!(
                        "table {table}: column {to} already exists"
                    )));
                }
                schema[idx].field.name = to.clone();
                for name in order_by.iter_mut() {
                    if name == from {
                        *name = to.clone();
                    }
                }
            }
            Alter::OrderBy(cols) => {
                for c in cols {
                    if !schema.iter().any(|sf| &sf.field.name == c) {
                        return Err(Error::Usage(format!(
                            "table {table}: ORDER BY names unknown column {c}"
                        )));
                    }
                }
                order_by = cols.clone();
            }
            Alter::PartitionBy(_) => {
                return Err(Error::MigrationNotBuilt {
                    table,
                    kind: "PARTITION BY".to_string(),
                });
            }
            Alter::SetType { column, .. } => {
                let id = schema
                    .iter()
                    .find(|sf| &sf.field.name == column)
                    .ok_or_else(|| Error::Usage(format!("table {table}: unknown column {column}")))?
                    .id;
                if let Some(rollup) = rollups_using(source, id).into_iter().next() {
                    return Err(Error::ColumnUsedByRollup {
                        table,
                        column: column.clone(),
                        rollup,
                    });
                }
                return Err(Error::MigrationNotBuilt {
                    table,
                    kind: "changing a column's type".to_string(),
                });
            }
        }
    }

    let name_of = |id: FieldId| -> String {
        schema
            .iter()
            .find(|sf| sf.id == id)
            .map(|sf| sf.field.name.clone())
            .unwrap_or_default()
    };
    let mut spec = TableSpec::new(
        source.name.clone(),
        schema.iter().map(|sf| sf.field.clone()).collect(),
    )
    .engine(&source.engine);
    if !source.key.is_empty() {
        spec = spec.key(source.key.iter().map(|&id| name_of(id)));
    }
    if let Some(v) = source.version {
        spec = spec.version(name_of(v));
    }
    if !order_by.is_empty() {
        spec = spec.order_by(order_by);
    }
    if let Some(p) = source.partition_by {
        spec = spec.partition_by(name_of(p.column), p.bucket);
    }
    if let Some(t) = source.ttl {
        spec = spec.ttl(name_of(t.column), t.after);
    }
    for (k, v) in &source.options {
        spec = spec.with(k.clone(), v.clone());
    }

    let resolved_order_by: Vec<FieldId> = spec
        .resolved_order_by()
        .iter()
        .map(|name| {
            schema
                .iter()
                .find(|sf| &sf.field.name == name)
                .expect("resolved_order_by only names columns already checked to exist")
                .id
        })
        .collect();

    let target = TableEntry {
        id: TableId(0),
        name: source.name.clone(),
        engine: source.engine.clone(),
        schema,
        next_field_id,
        key: source.key.clone(),
        version: source.version,
        order_by: resolved_order_by,
        partition_by: source.partition_by,
        ttl: source.ttl,
        options: source.options.clone(),
        segments: Vec::new(),
        tombstones: Vec::new(),
    };
    target.spec().validate()?;
    Ok(target)
}

/// Whether every segment must be rewritten: the sort or the partition layout changed. With no
/// type widening, a field id's type never changes, so otherwise every segment is reused.
pub fn rewrites_segments(source: &TableEntry, target: &TableEntry) -> bool {
    source.order_by != target.order_by || source.partition_by != target.partition_by
}

pub fn explain(source: &TableEntry, target: &TableEntry) -> AlterPlan {
    let segments = source.segments.len();
    if rewrites_segments(source, target) {
        AlterPlan {
            segments,
            rewrites: segments,
            rewrite_bytes: source.segments.iter().map(|s| s.bytes).sum(),
        }
    } else {
        AlterPlan {
            segments,
            rewrites: 0,
            rewrite_bytes: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::manifest::{CmpOp, Commit, Edit, Manifest, Predicate, TableName};
    use crate::types::Value;

    fn append_table() -> TableEntry {
        let m = Commit {
            base: 0,
            edits: vec![Edit::CreateTable {
                spec: TableSpec::new(
                    TableName::new("d", "t"),
                    vec![
                        Field {
                            name: "a".to_string(),
                            ty: DataType::Int64,
                        },
                        Field {
                            name: "b".to_string(),
                            ty: DataType::Int64,
                        },
                    ],
                ),
            }],
        }
        .apply(&Manifest::empty(), 0)
        .unwrap();
        m.table(&TableName::new("d", "t")).unwrap().clone()
    }

    fn append_table_with_order_by() -> TableEntry {
        let m = Commit {
            base: 0,
            edits: vec![Edit::CreateTable {
                spec: TableSpec::new(
                    TableName::new("d", "t"),
                    vec![
                        Field {
                            name: "a".to_string(),
                            ty: DataType::Int64,
                        },
                        Field {
                            name: "b".to_string(),
                            ty: DataType::Int64,
                        },
                    ],
                )
                .order_by(["a"]),
            }],
        }
        .apply(&Manifest::empty(), 0)
        .unwrap();
        m.table(&TableName::new("d", "t")).unwrap().clone()
    }

    fn latest_table() -> TableEntry {
        let m = Commit {
            base: 0,
            edits: vec![Edit::CreateTable {
                spec: TableSpec::new(
                    TableName::new("d", "t"),
                    vec![
                        Field {
                            name: "id".to_string(),
                            ty: DataType::Int64,
                        },
                        Field {
                            name: "ts".to_string(),
                            ty: DataType::Timestamp,
                        },
                    ],
                )
                .engine("latest")
                .key(["id"])
                .version("ts"),
            }],
        }
        .apply(&Manifest::empty(), 0)
        .unwrap();
        m.table(&TableName::new("d", "t")).unwrap().clone()
    }

    fn table_with_tombstone() -> TableEntry {
        let m = Commit {
            base: 0,
            edits: vec![Edit::CreateTable {
                spec: TableSpec::new(
                    TableName::new("d", "t"),
                    vec![Field {
                        name: "a".to_string(),
                        ty: DataType::Int64,
                    }],
                ),
            }],
        }
        .apply(&Manifest::empty(), 0)
        .unwrap();
        let m = Commit {
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
        m.table(&TableName::new("d", "t")).unwrap().clone()
    }

    #[test]
    fn empty_ops_is_usage() {
        let source = append_table();
        assert!(matches!(plan_alter(&source, &[]), Err(Error::Usage(_))));
    }

    #[test]
    fn add_column_gets_the_next_field_id_and_bumps_it() {
        let source = append_table();
        let target = plan_alter(
            &source,
            &[Alter::AddColumn(Field {
                name: "c".to_string(),
                ty: DataType::Int64,
            })],
        )
        .unwrap();
        assert_eq!(target.next_field_id, source.next_field_id + 1);
        let added = target.schema.last().unwrap();
        assert_eq!(added.field.name, "c");
        assert_eq!(added.id, FieldId(source.next_field_id));
    }

    #[test]
    fn add_column_that_already_exists_is_usage() {
        let source = append_table();
        let r = plan_alter(
            &source,
            &[Alter::AddColumn(Field {
                name: "a".to_string(),
                ty: DataType::Int64,
            })],
        );
        assert!(matches!(r, Err(Error::Usage(_))));
    }

    #[test]
    fn drop_column_removes_it_and_keeps_the_survivors_id() {
        let source = append_table();
        let a_id = source
            .schema
            .iter()
            .find(|sf| sf.field.name == "a")
            .unwrap()
            .id;
        let target = plan_alter(&source, &[Alter::DropColumn("b".to_string())]).unwrap();
        assert_eq!(target.schema.len(), 1);
        assert_eq!(target.schema[0].field.name, "a");
        assert_eq!(target.schema[0].id, a_id);
    }

    #[test]
    fn drop_column_that_does_not_exist_is_usage() {
        let source = append_table();
        assert!(matches!(
            plan_alter(&source, &[Alter::DropColumn("z".to_string())]),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn dropping_the_key_column_is_fixed_column() {
        let source = latest_table();
        let r = plan_alter(&source, &[Alter::DropColumn("id".to_string())]);
        assert!(matches!(r, Err(Error::FixedColumn { ref clause, .. }) if clause == "KEY"));
    }

    #[test]
    fn dropping_the_version_column_is_fixed_column() {
        let source = latest_table();
        let r = plan_alter(&source, &[Alter::DropColumn("ts".to_string())]);
        assert!(matches!(r, Err(Error::FixedColumn { ref clause, .. }) if clause == "VERSION"));
    }

    #[test]
    fn dropping_an_order_by_column_names_the_clause() {
        let source = append_table_with_order_by();
        let r = plan_alter(&source, &[Alter::DropColumn("a".to_string())]);
        assert!(matches!(r, Err(Error::Usage(ref msg)) if msg.contains("ORDER BY")));
    }

    #[test]
    fn dropping_a_column_named_by_a_tombstone_is_column_in_tombstone() {
        let source = table_with_tombstone();
        let r = plan_alter(&source, &[Alter::DropColumn("a".to_string())]);
        assert!(matches!(r, Err(Error::ColumnInTombstone { .. })));
    }

    #[test]
    fn rename_column_keeps_its_id() {
        let source = append_table();
        let a_id = source
            .schema
            .iter()
            .find(|sf| sf.field.name == "a")
            .unwrap()
            .id;
        let target = plan_alter(
            &source,
            &[Alter::RenameColumn {
                from: "a".to_string(),
                to: "aa".to_string(),
            }],
        )
        .unwrap();
        let renamed = target
            .schema
            .iter()
            .find(|sf| sf.field.name == "aa")
            .unwrap();
        assert_eq!(renamed.id, a_id);
    }

    #[test]
    fn drop_then_add_the_same_name_gets_a_new_id() {
        let source = append_table();
        let old_a = source
            .schema
            .iter()
            .find(|sf| sf.field.name == "a")
            .unwrap()
            .id;
        let target = plan_alter(
            &source,
            &[
                Alter::DropColumn("a".to_string()),
                Alter::AddColumn(Field {
                    name: "a".to_string(),
                    ty: DataType::Int64,
                }),
            ],
        )
        .unwrap();
        let new_a = target
            .schema
            .iter()
            .find(|sf| sf.field.name == "a")
            .unwrap();
        assert_ne!(new_a.id, old_a);
    }

    #[test]
    fn order_by_is_resolved_to_ids() {
        let source = append_table();
        let target = plan_alter(
            &source,
            &[Alter::OrderBy(vec!["b".to_string(), "a".to_string()])],
        )
        .unwrap();
        let b_id = target
            .schema
            .iter()
            .find(|sf| sf.field.name == "b")
            .unwrap()
            .id;
        let a_id = target
            .schema
            .iter()
            .find(|sf| sf.field.name == "a")
            .unwrap()
            .id;
        assert_eq!(target.order_by, vec![b_id, a_id]);
    }

    #[test]
    fn order_by_naming_an_unknown_column_is_usage() {
        let source = append_table();
        assert!(matches!(
            plan_alter(&source, &[Alter::OrderBy(vec!["z".to_string()])]),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn order_by_that_breaks_the_key_prefix_rule_fails_validate() {
        let source = latest_table();
        let r = plan_alter(
            &source,
            &[Alter::OrderBy(vec!["ts".to_string(), "id".to_string()])],
        );
        assert!(matches!(r, Err(Error::KeyNotSortPrefix { .. })));
    }

    #[test]
    fn partition_by_is_not_built() {
        let source = append_table();
        let r = plan_alter(
            &source,
            &[Alter::PartitionBy(Some((
                "a".to_string(),
                Duration::from_secs(1),
            )))],
        );
        assert!(
            matches!(r, Err(Error::MigrationNotBuilt { ref kind, .. }) if kind == "PARTITION BY")
        );
    }

    #[test]
    fn set_type_is_not_built() {
        let source = append_table();
        let r = plan_alter(
            &source,
            &[Alter::SetType {
                column: "a".to_string(),
                ty: DataType::Int64,
            }],
        );
        assert!(
            matches!(r, Err(Error::MigrationNotBuilt { ref kind, .. }) if kind == "changing a column's type")
        );
    }

    #[test]
    fn rewrites_segments_is_false_for_add_drop_rename() {
        let source = append_table();
        let add = plan_alter(
            &source,
            &[Alter::AddColumn(Field {
                name: "c".to_string(),
                ty: DataType::Int64,
            })],
        )
        .unwrap();
        let drop = plan_alter(&source, &[Alter::DropColumn("b".to_string())]).unwrap();
        let rename = plan_alter(
            &source,
            &[Alter::RenameColumn {
                from: "a".to_string(),
                to: "aa".to_string(),
            }],
        )
        .unwrap();
        assert!(!rewrites_segments(&source, &add));
        assert!(!rewrites_segments(&source, &drop));
        assert!(!rewrites_segments(&source, &rename));
    }

    #[test]
    fn rewrites_segments_is_true_for_order_by() {
        let source = append_table();
        let target = plan_alter(
            &source,
            &[Alter::OrderBy(vec!["b".to_string(), "a".to_string()])],
        )
        .unwrap();
        assert!(rewrites_segments(&source, &target));
    }
}
