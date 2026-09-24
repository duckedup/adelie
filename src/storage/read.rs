//! `View::scan`: reads a table's live segments off disk, realigned onto the reading table's
//! schema by field id, and appends its buffered batches.

use std::path::Path;
use std::sync::Arc;

use crate::exec::{Batch, Column};
use crate::storage::manifest::{Manifest, SchemaField, SegmentEntry, Snapshot, TableName};
use crate::storage::segment;
use crate::types::Value;

use super::Error;

/// Every row of `name`: each live segment realigned onto the table's current schema (all
/// columns, in schema order), then `buffered` in arrival order. A missing segment file is
/// `Error::SnapshotExpired`.
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
        batches.extend(read_segment_as(snapshot.root(), seg, &table.schema)?);
    }
    for b in buffered {
        batches.push((**b).clone());
    }
    Ok(batches)
}

/// Reads one segment as a table with `schema` sees it: each schema field comes from the file
/// column with the same field id (`seg.file_ids()`), or is all NULL when the file has none. A
/// file column the schema lacks is never decoded. Found by its own recorded directory (`seg.dir`).
pub(crate) fn read_segment_as(
    root: &Path,
    seg: &SegmentEntry,
    schema: &[SchemaField],
) -> Result<Vec<Batch>, Error> {
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
    let file_ids = seg.file_ids();
    if file_ids.len() != reader.fields().len() {
        return Err(Error::Usage(format!(
            "segment {}: manifest lists {} file columns but the file has {}",
            seg.id,
            file_ids.len(),
            reader.fields().len()
        )));
    }

    // For each schema field, the file column (if any) that carries its id; `FieldId(0)` and an
    // id no schema field wants are simply never matched, so that file column is never decoded.
    let mut file_index: Vec<Option<usize>> = Vec::with_capacity(schema.len());
    for sf in schema {
        let idx = file_ids.iter().position(|&id| id.0 != 0 && id == sf.id);
        if let Some(i) = idx {
            let file_field = &reader.fields()[i];
            if file_field.ty != sf.field.ty {
                return Err(Error::Usage(format!(
                    "segment {}: file column {} is {} but schema field {} is {}",
                    seg.id, file_field.name, file_field.ty, sf.field.name, sf.field.ty
                )));
            }
        }
        file_index.push(idx);
    }
    let projection: Vec<usize> = file_index.iter().filter_map(|x| *x).collect();

    let mut batches = Vec::with_capacity(reader.row_groups().len());
    for rg in 0..reader.row_groups().len() {
        let rows = reader.row_groups()[rg].rows as usize;
        let projected = reader.read_row_group(rg, &projection)?;
        let mut next_matched = projected.columns().iter();
        let mut fields = Vec::with_capacity(schema.len());
        let mut columns = Vec::with_capacity(schema.len());
        for (i, sf) in schema.iter().enumerate() {
            fields.push(sf.field.clone());
            columns.push(match file_index[i] {
                Some(_) => next_matched
                    .next()
                    .expect("one column per matched field")
                    .clone(),
                None => Column::from_values(&sf.field.ty, &vec![Value::Null; rows])
                    .map_err(|e| Error::Usage(e.to_string()))?,
            });
        }
        batches.push(Batch::new(fields, columns).map_err(|e| Error::Usage(e.to_string()))?);
    }
    Ok(batches)
}

/// Realigns an in-memory batch written against `from` onto `to` by field id (the pending
/// buffer at a swap or REVERT). Same NULL rule as `read_segment_as`.
pub(crate) fn project_batch(
    batch: &Batch,
    from: &[SchemaField],
    to: &[SchemaField],
) -> Result<Batch, Error> {
    let rows = batch.rows();
    let mut fields = Vec::with_capacity(to.len());
    let mut columns = Vec::with_capacity(to.len());
    for tf in to {
        fields.push(tf.field.clone());
        match from.iter().position(|f| f.id == tf.id) {
            Some(i) => {
                let col = batch.column(i);
                if col.data_type() != &tf.field.ty {
                    return Err(Error::Usage(format!(
                        "project_batch: field {} is {} in the source but {} in the target",
                        tf.field.name,
                        col.data_type(),
                        tf.field.ty
                    )));
                }
                columns.push(col.clone());
            }
            None => columns.push(
                Column::from_values(&tf.field.ty, &vec![Value::Null; rows])
                    .map_err(|e| Error::Usage(e.to_string()))?,
            ),
        }
    }
    Batch::new(fields, columns).map_err(|e| Error::Usage(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Column, Field};
    use crate::storage::manifest::{Commit, Edit, FieldId, TableSpec};
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
            file_field_ids: Vec::new(),
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
            file_field_ids: Vec::new(),
        };
        let err = read_segment_as(&root, &seg, &[]).unwrap_err();
        assert!(matches!(err, Error::SnapshotExpired { .. }));
    }

    /// Writes a segment with physical columns `a`(id 1), `mid`(masked, `FieldId(0)`), `b`(id 2),
    /// then reads it as a schema that keeps `a`, drops `b`, and adds a brand-new `c`(id 3).
    fn write_projection_fixture(root: &Path) -> SegmentEntry {
        let file_fields = vec![
            Field {
                name: "a".to_string(),
                ty: DataType::Int64,
            },
            Field {
                name: "mid".to_string(),
                ty: DataType::String,
            },
            Field {
                name: "b".to_string(),
                ty: DataType::Int64,
            },
        ];
        let row = Batch::new(
            file_fields.clone(),
            vec![
                Column::from_values(&DataType::Int64, &[Value::Int64(10)]).unwrap(),
                Column::from_values(&DataType::String, &[Value::String("x".to_string())]).unwrap(),
                Column::from_values(&DataType::Int64, &[Value::Int64(20)]).unwrap(),
            ],
        )
        .unwrap();
        let mut writer = Writer::new(Vec::new(), file_fields, WriterOptions::default()).unwrap();
        writer.push(&row).unwrap();
        let (bytes, meta) = writer.finish().unwrap();
        let dir = "d/0000000000000001".to_string();
        std::fs::create_dir_all(Manifest::table_dir(root, &dir).join("_")).unwrap();
        let seg = SegmentEntry {
            id: 1,
            partition: "_".to_string(),
            seq: 1,
            rows: meta.rows,
            bytes: meta.bytes,
            footer_crc: meta.footer_crc,
            columns: meta.columns,
            side_files: Vec::new(),
            dir,
            field_ids: vec![FieldId(1), FieldId(2)],
            file_field_ids: vec![FieldId(1), FieldId(0), FieldId(2)],
        };
        std::fs::write(Manifest::segment_path(root, &seg), &bytes).unwrap();
        seg
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn read_segment_as_nulls_a_new_field_drops_an_unwanted_one_and_skips_a_masked_column() {
        let root = temp_dir("project-onto");
        let seg = write_projection_fixture(&root);
        let schema = vec![
            SchemaField {
                id: FieldId(1),
                field: Field {
                    name: "a".to_string(),
                    ty: DataType::Int64,
                },
            },
            SchemaField {
                id: FieldId(3),
                field: Field {
                    name: "c".to_string(),
                    ty: DataType::Int64,
                },
            },
        ];
        let batches = read_segment_as(&root, &seg, &schema).unwrap();
        assert_eq!(batches.len(), 1);
        let b = &batches[0];
        assert_eq!(b.fields().len(), 2);
        assert_eq!(b.column(0).get(0), Value::Int64(10));
        assert!(b.column(1).is_null(0));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn read_segment_as_rejects_a_matched_field_whose_type_changed() {
        let root = temp_dir("type-mismatch");
        let seg = write_projection_fixture(&root);
        let schema = vec![SchemaField {
            id: FieldId(1),
            field: Field {
                name: "a".to_string(),
                ty: DataType::String,
            },
        }];
        let err = read_segment_as(&root, &seg, &schema).unwrap_err();
        assert!(matches!(err, Error::Usage(_)));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn project_batch_realigns_by_field_id_dropping_and_nulling() {
        let from = vec![
            SchemaField {
                id: FieldId(1),
                field: Field {
                    name: "a".to_string(),
                    ty: DataType::Int64,
                },
            },
            SchemaField {
                id: FieldId(2),
                field: Field {
                    name: "b".to_string(),
                    ty: DataType::Int64,
                },
            },
        ];
        let to = vec![
            SchemaField {
                id: FieldId(1),
                field: Field {
                    name: "a".to_string(),
                    ty: DataType::Int64,
                },
            },
            SchemaField {
                id: FieldId(3),
                field: Field {
                    name: "c".to_string(),
                    ty: DataType::Int64,
                },
            },
        ];
        let b = Batch::new(
            from.iter().map(|f| f.field.clone()).collect(),
            vec![
                Column::from_values(&DataType::Int64, &[Value::Int64(1)]).unwrap(),
                Column::from_values(&DataType::Int64, &[Value::Int64(2)]).unwrap(),
            ],
        )
        .unwrap();
        let projected = project_batch(&b, &from, &to).unwrap();
        assert_eq!(projected.fields().len(), 2);
        assert_eq!(projected.column(0).get(0), Value::Int64(1));
        assert!(projected.column(1).is_null(0));
    }
}
