//! Deterministic backfill for a manifest written before D0012 (stable ids). Pure and
//! order-driven: tables in manifest order, columns in schema order, so every open of the same
//! bytes agrees and the next ordinary commit simply persists what this derived (SPEC §5).

use super::{FieldId, Manifest, TableEntry, TableId};

/// Gives ids to whatever this manifest lacks. Idempotent: a manifest that already has
/// everything comes back unchanged.
pub(crate) fn assign_missing(m: &mut Manifest) {
    let max_assigned = m.tables.iter().map(|t| t.id.0).max().unwrap_or(0);
    let mut next = m.next_table_id.max(max_assigned + 1).max(1);
    for t in m.tables.iter_mut() {
        if t.id.0 == 0 {
            t.id = TableId(next);
            next += 1;
        }
    }
    m.next_table_id = next;

    for t in m.tables.iter_mut() {
        assign_field_ids(t);
        assign_segment_dirs(t);
    }

    for g in m.garbage.iter_mut() {
        if g.segment.dir.is_empty() {
            g.segment.dir = format!("{}/{}", g.table.db, g.table.name);
        }
    }
}

fn assign_field_ids(t: &mut TableEntry) {
    let max_assigned = t.schema.iter().map(|f| f.id.0).max().unwrap_or(0);
    let mut next = t.next_field_id.max(max_assigned + 1).max(1);
    for f in t.schema.iter_mut() {
        if f.id.0 == 0 {
            f.id = FieldId(next);
            next += 1;
        }
    }
    t.next_field_id = next;
}

/// A legacy segment (no recorded `dir`) lives at `<db>/<name>`, where it was actually written.
/// A legacy segment's `columns` are positional against the schema (the pre-D0012 invariant), so
/// an empty `field_ids` becomes the schema's ids in that same order.
fn assign_segment_dirs(t: &mut TableEntry) {
    let schema_ids: Vec<FieldId> = t.schema.iter().map(|f| f.id).collect();
    let legacy_dir = format!("{}/{}", t.name.db, t.name.name);
    for s in t.segments.iter_mut() {
        if s.dir.is_empty() {
            s.dir = legacy_dir.clone();
        }
        if s.field_ids.is_empty() && !s.columns.is_empty() {
            s.field_ids = schema_ids.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{ColumnStats, Field};
    use crate::storage::manifest::{Garbage, SchemaField, SegmentEntry, TableName};
    use crate::types::DataType;
    use std::collections::BTreeMap;

    fn field(id: u64, name: &str) -> SchemaField {
        SchemaField {
            id: FieldId(id),
            field: Field {
                name: name.to_string(),
                ty: DataType::Int64,
            },
        }
    }

    fn seg(id: u64, dir: &str, columns: usize, field_ids: Vec<u64>) -> SegmentEntry {
        SegmentEntry {
            id,
            partition: "_".to_string(),
            seq: 1,
            rows: 1,
            bytes: 1,
            footer_crc: 0,
            columns: (0..columns)
                .map(|_| ColumnStats {
                    rows: 1,
                    null_count: 0,
                    min: None,
                    max: None,
                })
                .collect(),
            side_files: Vec::new(),
            dir: dir.to_string(),
            field_ids: field_ids.into_iter().map(FieldId).collect(),
            file_field_ids: Vec::new(),
        }
    }

    fn table(id: u64, name: &str, next_field_id: u64, schema: Vec<SchemaField>) -> TableEntry {
        TableEntry {
            id: TableId(id),
            name: TableName::new("d", name),
            engine: "append".to_string(),
            schema,
            next_field_id,
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
    fn a_fully_assigned_manifest_is_left_exactly_equal() {
        let mut m = Manifest {
            version: 1,
            next_segment_id: 1,
            next_table_id: 5,
            tables: vec![table(4, "a", 3, vec![field(1, "x"), field(2, "y")])],
            garbage: vec![],
            next_job_id: 1,
            jobs: vec![],
            retired: vec![],
        };
        m.tables[0]
            .segments
            .push(seg(1, "d/0000000000000004", 2, vec![1, 2]));
        let before = m.clone();
        assign_missing(&mut m);
        assert_eq!(m, before);
    }

    #[test]
    fn a_mixed_manifest_assigns_the_unassigned_table_past_the_existing_max() {
        let mut m = Manifest {
            version: 1,
            next_segment_id: 1,
            next_table_id: 0,
            tables: vec![
                table(4, "a", 1, vec![]),
                table(0, "b", 0, vec![field(0, "z")]),
            ],
            garbage: vec![],
            next_job_id: 1,
            jobs: vec![],
            retired: vec![],
        };
        assign_missing(&mut m);
        assert_eq!(m.tables[0].id, TableId(4));
        assert_eq!(m.tables[1].id, TableId(5), "never re-uses id 1");
        assert_eq!(m.next_table_id, 6);
        assert_eq!(m.tables[1].schema[0].id, FieldId(1));
    }

    #[test]
    fn two_calls_on_the_same_input_agree() {
        let mut m = Manifest {
            version: 1,
            next_segment_id: 1,
            next_table_id: 0,
            tables: vec![table(0, "a", 0, vec![field(0, "x")])],
            garbage: vec![Garbage {
                table: TableName::new("d", "a"),
                segment: seg(9, "", 0, vec![]),
                removed_at_ms: 1,
            }],
            next_job_id: 1,
            jobs: vec![],
            retired: vec![],
        };
        m.tables[0].segments.push(seg(1, "", 1, vec![]));
        let mut once = m.clone();
        assign_missing(&mut once);
        let mut twice = m.clone();
        assign_missing(&mut twice);
        assign_missing(&mut twice);
        assert_eq!(once, twice);
        assert_eq!(once.garbage[0].segment.dir, "d/a");
        assert_eq!(once.tables[0].segments[0].dir, "d/a");
        assert_eq!(once.tables[0].segments[0].field_ids, vec![FieldId(1)]);
    }
}
