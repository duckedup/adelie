//! Catalog read: per-table row counts, sizes, column presence and time range, from the
//! manifest's stats and the writer's buffered batches. Reads no segment data.

use std::cmp::Ordering;
use std::sync::Arc;

use crate::exec::Batch;
use crate::storage::View;
use crate::storage::manifest::TableName;
use crate::types::{DataType, Value, total_cmp};

/// One live table's summary. `rows` is exact only when `rows_exact`: a tombstone is a
/// predicate, so the live count after a DELETE would need a scan.
#[derive(Debug, Clone, PartialEq)]
pub struct TableSummary {
    pub name: TableName,
    pub engine: String,
    pub rows: u64,
    pub rows_exact: bool,
    pub bytes: u64,
    pub time_range: Option<TimeRange>,
    pub columns: Vec<ColumnSummary>,
}

/// Min and max of a table's first `TIMESTAMP` column.
#[derive(Debug, Clone, PartialEq)]
pub struct TimeRange {
    pub column: String,
    pub min: Value,
    pub max: Value,
}

/// A schema column and how many rows carry a value for it.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnSummary {
    pub name: String,
    pub ty: DataType,
    pub non_null: u64,
}

fn widen(range: &mut Option<(Value, Value)>, min: &Value, max: &Value) {
    match range {
        None => *range = Some((min.clone(), max.clone())),
        Some((lo, hi)) => {
            if total_cmp(min, lo) == Some(Ordering::Less) {
                *lo = min.clone();
            }
            if total_cmp(max, hi) == Some(Ordering::Greater) {
                *hi = max.clone();
            }
        }
    }
}

impl View {
    /// One summary per live table, in manifest order. Never scans segment data.
    pub fn tables(&self) -> Vec<TableSummary> {
        let manifest = self.snapshot().manifest();
        manifest
            .tables
            .iter()
            .map(|t| {
                let buffered: &[Arc<Batch>] = self.buffered(&t.name);
                let ts = t
                    .schema
                    .iter()
                    .position(|f| f.field.ty == DataType::Timestamp);
                let mut range: Option<(Value, Value)> = None;
                let mut rows: u64 = buffered.iter().map(|b| b.rows() as u64).sum();
                let mut bytes = 0u64;
                let mut columns: Vec<ColumnSummary> = t
                    .schema
                    .iter()
                    .map(|f| ColumnSummary {
                        name: f.field.name.clone(),
                        ty: f.field.ty.clone(),
                        non_null: 0,
                    })
                    .collect();

                for seg in &t.segments {
                    rows += seg.rows;
                    bytes += seg.bytes;
                    for (i, f) in t.schema.iter().enumerate() {
                        // Match by field id: a reused segment has its own id order (D0013).
                        let Some(pos) = seg.field_ids.iter().position(|id| *id == f.id) else {
                            continue;
                        };
                        let Some(stats) = seg.columns.get(pos) else {
                            continue;
                        };
                        columns[i].non_null += (stats.rows - stats.null_count) as u64;
                        if Some(i) == ts
                            && let (Some(min), Some(max)) = (&stats.min, &stats.max)
                        {
                            widen(&mut range, min, max);
                        }
                    }
                }

                for b in buffered {
                    for (i, f) in t.schema.iter().enumerate() {
                        let Some(col) = b.column_by_name(&f.field.name) else {
                            continue;
                        };
                        let stats = col.stats();
                        columns[i].non_null += (stats.rows - stats.null_count) as u64;
                        if Some(i) == ts
                            && let (Some(min), Some(max)) = (&stats.min, &stats.max)
                        {
                            widen(&mut range, min, max);
                        }
                    }
                }

                let time_range = ts.zip(range).map(|(i, (min, max))| TimeRange {
                    column: t.schema[i].field.name.clone(),
                    min,
                    max,
                });
                TableSummary {
                    name: t.name.clone(),
                    engine: t.engine.clone(),
                    rows,
                    rows_exact: t.tombstones.is_empty(),
                    bytes,
                    time_range,
                    columns,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Column, Field};
    use crate::storage::manifest::{CmpOp, Predicate};
    use crate::storage::{Alter, Store, StoreOptions, TableSpec};
    use std::path::PathBuf;
    use std::time::Duration;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "adelie-catalog-{}-{tag}-{nanos}",
            std::process::id()
        ))
    }

    fn events() -> TableName {
        TableName::new("d", "events")
    }

    fn fields() -> Vec<Field> {
        vec![
            Field {
                name: "id".to_string(),
                ty: DataType::Int64,
            },
            Field {
                name: "ts".to_string(),
                ty: DataType::Timestamp,
            },
            Field {
                name: "note".to_string(),
                ty: DataType::Int64,
            },
        ]
    }

    /// Row `i` has timestamp `i * 10`; `note` is NULL for even ids.
    fn batch(ids: &[i64]) -> Batch {
        let id: Vec<Value> = ids.iter().map(|i| Value::Int64(*i)).collect();
        let ts: Vec<Value> = ids.iter().map(|i| Value::Timestamp(*i * 10)).collect();
        let note: Vec<Value> = ids
            .iter()
            .map(|i| {
                if i % 2 == 0 {
                    Value::Null
                } else {
                    Value::Int64(*i)
                }
            })
            .collect();
        Batch::new(
            fields(),
            vec![
                Column::from_values(&DataType::Int64, &id).unwrap(),
                Column::from_values(&DataType::Timestamp, &ts).unwrap(),
                Column::from_values(&DataType::Int64, &note).unwrap(),
            ],
        )
        .unwrap()
    }

    fn open(dir: &std::path::Path) -> Store {
        let opts = StoreOptions {
            flush_interval: Duration::from_secs(3600),
            ..StoreOptions::default()
        };
        let store = Store::open(dir, opts).unwrap();
        store
            .create_table(TableSpec::new(events(), fields()))
            .unwrap();
        store
    }

    // The long flush interval parks the write, so flush from this side until it acks.
    fn write_flushed(store: &Store, ids: &[i64]) {
        std::thread::scope(|s| {
            let h = s.spawn(|| store.write(&events(), batch(ids)).unwrap());
            while !h.is_finished() {
                store.flush().unwrap();
                std::thread::sleep(Duration::from_millis(2));
            }
        });
    }

    fn push_pending(store: &Store, ids: &[i64]) {
        store
            .shared
            .state
            .lock()
            .unwrap()
            .pending
            .entry(events())
            .or_default()
            .push(Arc::new(batch(ids)));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn rows_cover_segments_and_buffered_batches_in_manifest_order() {
        let dir = temp_dir("rows");
        let store = open(&dir);
        let other = TableName::new("d", "zzz");
        store
            .create_table(TableSpec::new(other.clone(), fields()))
            .unwrap();
        write_flushed(&store, &[1, 2, 3]);
        write_flushed(&store, &[4, 5]);
        push_pending(&store, &[6, 7, 8, 9]);

        let tables = store.snapshot().tables();
        let names: Vec<_> = tables.iter().map(|t| t.name.clone()).collect();
        assert_eq!(names, vec![events(), other]);
        assert_eq!(tables[0].rows, 9);
        assert!(tables[0].rows_exact);
        assert!(tables[0].bytes > 0);
        assert_eq!(tables[1].rows, 0);
        assert_eq!(tables[1].time_range, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn non_null_and_time_range_count_values_not_rows() {
        let dir = temp_dir("nulls");
        let store = open(&dir);
        write_flushed(&store, &[1, 2, 3]);
        push_pending(&store, &[10, 11]);

        let t = &store.snapshot().tables()[0];
        let non_null: Vec<u64> = t.columns.iter().map(|c| c.non_null).collect();
        // note is NULL for ids 2 and 10.
        assert_eq!(non_null, vec![5, 5, 3]);
        let range = t.time_range.clone().unwrap();
        assert_eq!(range.column, "ts");
        assert_eq!(range.min, Value::Timestamp(10));
        assert_eq!(range.max, Value::Timestamp(110));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn a_table_without_a_timestamp_has_no_time_range() {
        let dir = temp_dir("no-ts");
        let store = Store::open(&dir, StoreOptions::default()).unwrap();
        let name = TableName::new("d", "plain");
        let f = vec![Field {
            name: "n".to_string(),
            ty: DataType::Int64,
        }];
        store
            .create_table(TableSpec::new(name.clone(), f.clone()))
            .unwrap();
        let col = Column::from_values(&DataType::Int64, &[Value::Int64(1)]).unwrap();
        store
            .write(&name, Batch::new(f, vec![col]).unwrap())
            .unwrap();
        assert_eq!(store.snapshot().tables()[0].time_range, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn a_delete_makes_the_row_count_inexact() {
        let dir = temp_dir("delete");
        let store = open(&dir);
        write_flushed(&store, &[1, 2, 3]);
        store
            .delete(
                &events(),
                vec![Predicate {
                    column: "id".to_string(),
                    op: CmpOp::Eq,
                    value: Value::Int64(1),
                }],
            )
            .unwrap();
        assert!(!store.snapshot().tables()[0].rows_exact);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn non_null_survives_a_migration_that_reuses_segments() {
        let dir = temp_dir("migrate");
        let store = open(&dir);
        write_flushed(&store, &[1, 2, 3, 4]);
        // Dropping the first column shifts every later field's position in the schema.
        store
            .migrate(&events(), vec![Alter::DropColumn("id".to_string())])
            .unwrap();

        let t = &store.snapshot().tables()[0];
        let got: Vec<(&str, u64)> = t
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.non_null))
            .collect();
        assert_eq!(got, vec![("ts", 4), ("note", 2)]);
        assert_eq!(t.rows, 4);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
