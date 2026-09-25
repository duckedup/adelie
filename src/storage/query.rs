//! `View::query`: the storage side of the exec entry point (SPEC §7). Storage errors surface
//! as their own `Error` variant; every other failure becomes `Error::Exec`.

use crate::exec::{ExecError, ExecOptions, Plan, QueryResult, execute};

use super::{Error, View};

impl View {
    /// Runs `plan` against this view's snapshot (SPEC §7). Storage errors come back as their
    /// own variant (e.g. `SnapshotExpired`, so a reader can refresh); executor errors as
    /// `Error::Exec`.
    pub fn query(&self, plan: &Plan, opts: &ExecOptions) -> Result<QueryResult, Error> {
        match execute(self, plan, opts) {
            Ok(result) => Ok(result),
            Err(ExecError::Source(e)) => match e.downcast::<Error>() {
                Ok(e) => Err(*e),
                Err(e) => Err(Error::Exec(ExecError::Source(e))),
            },
            Err(e) => Err(Error::Exec(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::exec::{AggCall, AggFunc, Batch, Column, Field, ScanSpec};
    use crate::storage::manifest::{Manifest, TableName};
    use crate::storage::{Store, StoreOptions, TableSpec};
    use crate::types::{DataType, Value};

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("adelie-query-{tag}-{}-{nanos}", std::process::id()))
    }

    fn schema() -> Vec<Field> {
        vec![Field {
            name: "a".to_string(),
            ty: DataType::Int64,
        }]
    }

    fn table() -> TableName {
        TableName::new("d", "t")
    }

    fn open(dir: &Path) -> Store {
        let store = Store::open(dir, StoreOptions::default()).unwrap();
        store.create_table(TableSpec::new(table(), schema())).unwrap();
        store
    }

    fn batch(values: &[i64]) -> Batch {
        let vals: Vec<Value> = values.iter().copied().map(Value::Int64).collect();
        Batch::new(schema(), vec![Column::from_values(&DataType::Int64, &vals).unwrap()]).unwrap()
    }

    fn count_plan() -> Plan {
        Plan::Aggregate {
            input: Box::new(Plan::Scan(ScanSpec {
                db: "d".to_string(),
                table: "t".to_string(),
                columns: vec!["a".to_string()],
                predicate: None,
            })),
            group_by: Vec::new(),
            aggs: vec![AggCall {
                func: AggFunc::CountStar,
                args: Vec::new(),
                filter: None,
                name: "n".to_string(),
            }],
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn query_count_star_matches_the_scan_row_count() {
        let dir = temp_dir("count");
        let store = open(&dir);
        store.write(&table(), batch(&[1, 2, 3])).unwrap();
        store.write(&table(), batch(&[4])).unwrap();
        store.flush().unwrap();

        let view = store.snapshot();
        let scan_rows: usize = view.scan(&table()).unwrap().iter().map(Batch::rows).sum();

        let result = view.query(&count_plan(), &ExecOptions::default()).unwrap();
        assert_eq!(result.batches.len(), 1);
        let n = match result.batches[0].column(0).get(0) {
            Value::Int64(v) => v,
            other => panic!("expected Int64, got {other:?}"),
        };
        assert_eq!(n as usize, scan_rows);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn a_deleted_segment_file_is_snapshot_expired_not_exec() {
        // Falsify: this fails if `query`'s downcast from `ExecError::Source` is missing.
        let dir = temp_dir("deleted-segment");
        let store = open(&dir);
        store.write(&table(), batch(&[1, 2, 3])).unwrap();
        store.flush().unwrap();

        let view = store.snapshot();
        let seg = view.table(&table()).unwrap().segments[0].clone();
        std::fs::remove_file(Manifest::segment_path(&dir, &seg)).unwrap();

        let err = view.query(&count_plan(), &ExecOptions::default()).unwrap_err();
        assert!(matches!(err, Error::SnapshotExpired { .. }), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
