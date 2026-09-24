//! `View::scan`: reads a table's live segments off disk and appends its buffered batches.

use std::path::Path;
use std::sync::Arc;

use crate::exec::Batch;
use crate::storage::manifest::{Manifest, SegmentEntry, Snapshot, TableName};
use crate::storage::segment;

use super::Error;

/// Every row of `name`: each live segment's row groups (all columns, in file order), then
/// `buffered` in arrival order. A missing segment file is `Error::SnapshotExpired`.
pub(crate) fn scan(
    snapshot: &Snapshot,
    name: &TableName,
    buffered: &[Arc<Batch>],
) -> Result<Vec<Batch>, Error> {
    let table = snapshot
        .manifest()
        .table(name)
        .ok_or_else(|| Error::UnknownTable(name.to_string()))?;
    let mut batches = Vec::new();
    for seg in &table.segments {
        batches.extend(read_segment(snapshot.root(), seg)?);
    }
    for b in buffered {
        batches.push((**b).clone());
    }
    Ok(batches)
}

/// Reads one segment file into every row group as a `Batch` over all its columns. Found by its
/// own recorded directory (`seg.dir`), never one derived from the table that owns it (D0012).
pub(crate) fn read_segment(root: &Path, seg: &SegmentEntry) -> Result<Vec<Batch>, Error> {
    let path = Manifest::segment_path(root, seg);
    let bytes = std::fs::read(&path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            Error::SnapshotExpired {
                segment: path.display().to_string(),
            }
        } else {
            super::io_err(&path, source)
        }
    })?;
    let reader = segment::Reader::open(path.display().to_string(), bytes)?;
    let projection: Vec<usize> = (0..reader.fields().len()).collect();
    let mut batches = Vec::with_capacity(reader.row_groups().len());
    for rg in 0..reader.row_groups().len() {
        batches.push(reader.read_row_group(rg, &projection)?);
    }
    Ok(batches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Column, Field};
    use crate::storage::manifest::{Commit, Edit, TableSpec};
    use crate::storage::segment::{Writer, WriterOptions};
    use crate::types::{DataType, Value};
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "adelie-store-read-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn schema() -> Vec<Field> {
        vec![Field {
            name: "a".to_string(),
            ty: DataType::Int64,
        }]
    }

    fn batch(vals: &[i64]) -> Batch {
        let v: Vec<Value> = vals.iter().copied().map(Value::Int64).collect();
        Batch::new(
            schema(),
            vec![Column::from_values(&DataType::Int64, &v).unwrap()],
        )
        .unwrap()
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn scan_reads_segments_then_appends_buffered_rows_in_order() {
        let root = temp_dir("scan");
        let table = TableName::new("d", "t");

        let created = Commit {
            base: 0,
            edits: vec![Edit::CreateTable {
                spec: TableSpec::new(table.clone(), schema()),
            }],
        }
        .apply(&Manifest::empty(), 0)
        .unwrap();
        let entry = created.table(&table).unwrap();
        let table_dir = entry.dir();
        let field_ids: Vec<_> = entry.schema.iter().map(|f| f.id).collect();

        let dir = Manifest::table_dir(&root, &table_dir).join("_");
        std::fs::create_dir_all(&dir).unwrap();

        let mut writer = Writer::new(Vec::new(), schema(), WriterOptions::default()).unwrap();
        writer.push(&batch(&[1, 2])).unwrap();
        let (bytes, meta) = writer.finish().unwrap();
        let seg = SegmentEntry {
            id: 1,
            partition: "_".to_string(),
            seq: 1,
            rows: meta.rows,
            bytes: meta.bytes,
            footer_crc: meta.footer_crc,
            columns: meta.columns,
            side_files: Vec::new(),
            dir: table_dir,
            field_ids,
        };
        std::fs::write(Manifest::segment_path(&root, &seg), &bytes).unwrap();

        let manifest = Commit {
            base: created.version,
            edits: vec![Edit::AddSegments {
                table: table.clone(),
                segments: vec![seg],
            }],
        }
        .apply(&created, 0)
        .unwrap();
        let snapshot = Snapshot::new(root.clone(), manifest);

        let buffered = vec![Arc::new(batch(&[3]))];
        let batches = scan(&snapshot, &table, &buffered).unwrap();
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].rows(), 2);
        assert_eq!(batches[1], batch(&[3]));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn a_missing_segment_file_is_snapshot_expired() {
        let root = temp_dir("missing-seg");
        let seg = SegmentEntry {
            id: 1,
            partition: "_".to_string(),
            seq: 1,
            rows: 0,
            bytes: 0,
            footer_crc: 0,
            columns: Vec::new(),
            side_files: Vec::new(),
            dir: "d/t".to_string(),
            field_ids: Vec::new(),
        };
        let err = read_segment(&root, &seg).unwrap_err();
        assert!(matches!(err, Error::SnapshotExpired { .. }));
    }
}
