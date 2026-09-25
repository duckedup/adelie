//! The exec `TableSource` for `View` (SPEC §7 scan contract): resolves an engine's segment
//! set, prunes by manifest stats and zone maps/skip indexes, then applies tombstones.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::exec::kernels;
use crate::exec::{
    Batch, Bitmap, Column, ExecContext, ExecError, Expr, Field, MorselSource, ScanSpec, ScanStats,
    TableSource,
};
use crate::storage::engines::engine_by_name;
use crate::storage::manifest::{self, SchemaField, SegmentEntry, TableEntry, TableName, Tombstone};
use crate::storage::segment::{self, IndexKind, SkipIndex};
use crate::types::Value;

use super::prune;
use super::read::{map_file_columns, realign};
use super::{Error, View};

/// Wraps a storage `Error` as the `TableSource`'s own `ExecError::Source`, so `View::query`
/// (U11) can downcast it back; `SnapshotExpired` and every other variant survive the round trip.
fn src(e: Error) -> ExecError {
    ExecError::Source(Box::new(e))
}

/// `manifest::CmpOp` and `exec::CmpOp` name the same six operators one-to-one.
fn map_cmp(op: manifest::CmpOp) -> crate::exec::CmpOp {
    use crate::exec::CmpOp as E;
    use manifest::CmpOp as M;
    match op {
        M::Eq => E::Eq,
        M::Ne => E::Ne,
        M::Lt => E::Lt,
        M::Le => E::Le,
        M::Gt => E::Gt,
        M::Ge => E::Ge,
    }
}

/// One table's scan: a live segment per morsel (pruned, in manifest order), then one morsel
/// per buffered batch.
pub(crate) struct TableScan<'a> {
    fields: Vec<Field>,
    /// Schema position of each output column, in `fields` order.
    columns: Vec<usize>,
    table: &'a TableEntry,
    segments: Vec<&'a SegmentEntry>,
    buffered: &'a [Arc<Batch>],
    /// Pruning only: the scan never filters by this (SPEC §7).
    predicate: Option<Expr>,
    root: PathBuf,
    segments_total: usize,
    segments_pruned: usize,
    row_groups_total: AtomicUsize,
    row_groups_pruned: AtomicUsize,
    rows_read: AtomicU64,
}

impl<'a> MorselSource for TableScan<'a> {
    fn fields(&self) -> &[Field] {
        &self.fields
    }

    fn morsels(&self) -> usize {
        self.segments.len() + self.buffered.len()
    }

    fn read(&self, morsel: usize, ctx: &ExecContext) -> Result<Vec<Batch>, ExecError> {
        ctx.check()?;
        match self.segments.get(morsel) {
            Some(&seg) => self.read_segment(seg, ctx),
            None => {
                let b = &self.buffered[morsel - self.segments.len()];
                Ok(vec![self.project_buffered(b)])
            }
        }
    }

    fn stats(&self) -> ScanStats {
        ScanStats {
            segments_total: self.segments_total,
            segments_pruned: self.segments_pruned,
            row_groups_total: self.row_groups_total.load(Ordering::Relaxed),
            row_groups_pruned: self.row_groups_pruned.load(Ordering::Relaxed),
            rows_read: self.rows_read.load(Ordering::Relaxed),
        }
    }
}

impl<'a> TableScan<'a> {
    /// A buffered batch is already in `table.fields()` order (`Store::write_many` validates
    /// it). No tombstones: `Store::delete` flushes first, so every buffered row is newer than
    /// every tombstone.
    fn project_buffered(&self, b: &Batch) -> Batch {
        let columns: Vec<Column> = self.columns.iter().map(|&i| b.column(i).clone()).collect();
        Batch::new(self.fields.clone(), columns)
            .expect("a buffered batch already matches the table schema")
    }

    fn read_segment(&self, seg: &SegmentEntry, ctx: &ExecContext) -> Result<Vec<Batch>, ExecError> {
        let schema = &self.table.schema;
        let path = manifest::Manifest::segment_path(&self.root, seg);
        // Reserve the manifest's recorded size before reading, so the budget can refuse the
        // read itself; then true it up to what the file actually holds.
        let mut res = ctx.reserve(usize::try_from(seg.bytes).unwrap_or(usize::MAX))?;
        let bytes = std::fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                src(Error::SnapshotExpired {
                    segment: path.display().to_string(),
                })
            } else {
                src(super::io_err(&path, e))
            }
        })?;
        res.resize(bytes.len())?;
        let reader =
            segment::Reader::open(path.display().to_string(), bytes).map_err(|e| src(e.into()))?;
        let file_ids = seg.file_ids();
        if file_ids.len() != reader.fields().len() {
            return Err(src(Error::Usage(format!(
                "segment {}: manifest lists {} file columns but the file has {}",
                seg.id,
                file_ids.len(),
                reader.fields().len()
            ))));
        }
        let file_index =
            map_file_columns(seg.id, schema, file_ids, reader.fields()).map_err(src)?;

        // Decode the requested columns plus any this segment's tombstones name: a tombstone
        // predicate may reach a column the query itself never asked for.
        let mut needed: Vec<usize> = self.columns.clone();
        let tombstones = self.table.tombstones_for(seg);
        for t in &tombstones {
            for p in &t.predicates {
                let pos = schema
                    .iter()
                    .position(|f| f.field.name == p.column)
                    .ok_or_else(|| {
                        src(Error::Usage(format!(
                            "tombstone at seq {} names unknown column {}",
                            t.seq, p.column
                        )))
                    })?;
                if !needed.contains(&pos) {
                    needed.push(pos);
                }
            }
        }
        needed.sort_unstable();
        let sub_schema: Vec<SchemaField> = needed.iter().map(|&i| schema[i].clone()).collect();
        let sub_file_index: Vec<Option<usize>> = needed.iter().map(|&i| file_index[i]).collect();
        let projection: Vec<usize> = sub_file_index.iter().filter_map(|x| *x).collect();

        // Indexes loaded so far, cached across this segment's row groups by directory position.
        let mut index_cache: HashMap<usize, SkipIndex> = HashMap::new();
        let mut out = Vec::with_capacity(reader.row_groups().len());
        for rg in 0..reader.row_groups().len() {
            self.row_groups_total.fetch_add(1, Ordering::Relaxed);
            let rg_rows = reader.row_groups()[rg].rows as usize;

            if let Some(pred) = &self.predicate {
                let zone_stats = |c: usize| -> Option<prune::ColumnRange<'_>> {
                    let file_i = file_index[self.columns[c]]?;
                    let chunk = &reader.row_groups()[rg].chunks[file_i];
                    Some(prune::ColumnRange {
                        rows: reader.row_groups()[rg].rows,
                        null_count: chunk.null_count,
                        min: chunk.min.as_ref(),
                        max: chunk.max.as_ref(),
                    })
                };
                if !prune::may_match(pred, &zone_stats) {
                    self.row_groups_pruned.fetch_add(1, Ordering::Relaxed);
                    continue;
                }

                let mut file_col_entry: HashMap<usize, usize> = HashMap::new();
                for (entry_idx, entry) in reader.indexes().iter().enumerate() {
                    let matches_shape =
                        matches!(entry.kind, IndexKind::Bloom | IndexKind::ValueSet)
                            && entry.columns.len() == 1
                            && (entry.row_group == Some(rg) || entry.row_group.is_none());
                    if matches_shape {
                        file_col_entry.entry(entry.columns[0]).or_insert(entry_idx);
                    }
                }
                for &entry_idx in file_col_entry.values() {
                    if let std::collections::hash_map::Entry::Vacant(e) =
                        index_cache.entry(entry_idx)
                    {
                        let loaded = reader
                            .load_index(&reader.indexes()[entry_idx])
                            .map_err(|err| src(err.into()))?;
                        e.insert(loaded);
                    }
                }
                let index_probe = |c: usize, v: &Value| -> Option<bool> {
                    let file_i = file_index[self.columns[c]]?;
                    let entry_idx = *file_col_entry.get(&file_i)?;
                    Some(index_cache[&entry_idx].might_contain(v))
                };
                if !prune::index_may_match(pred, &index_probe) {
                    self.row_groups_pruned.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            }

            let projected = reader
                .read_row_group(rg, &projection)
                .map_err(|e| src(e.into()))?;
            let full = realign(&sub_schema, &sub_file_index, &projected, rg_rows).map_err(src)?;

            let filtered = match tombstone_keep_mask(&tombstones, &full, &sub_schema, rg_rows)? {
                Some(keep) => kernels::filter_batch(&full, &keep),
                None => full,
            };
            if filtered.rows() == 0 {
                continue;
            }
            self.rows_read
                .fetch_add(filtered.rows() as u64, Ordering::Relaxed);

            let out_columns: Vec<Column> = self
                .columns
                .iter()
                .map(|&schema_idx| {
                    let pos = needed
                        .iter()
                        .position(|&x| x == schema_idx)
                        .expect("a requested column is always kept in `needed`");
                    filtered.column(pos).clone()
                })
                .collect();
            out.push(
                Batch::new(self.fields.clone(), out_columns)
                    .map_err(|e| src(Error::Usage(e.to_string())))?,
            );
        }
        Ok(out)
    }
}

/// `OR` over every tombstone's `AND` of its predicates; `None` means nothing to delete. A NULL
/// comparison is never true (SQL), so it never marks a row deleted.
fn tombstone_keep_mask(
    tombstones: &[&Tombstone],
    full: &Batch,
    schema: &[SchemaField],
    rows: usize,
) -> Result<Option<Bitmap>, ExecError> {
    if tombstones.is_empty() {
        return Ok(None);
    }
    let mut deleted: Option<Column> = None;
    for t in tombstones {
        let mut matched: Option<Column> = None;
        for p in &t.predicates {
            let pos = schema
                .iter()
                .position(|f| f.field.name == p.column)
                .ok_or_else(|| {
                    src(Error::Usage(format!(
                        "tombstone at seq {} names unknown column {}",
                        t.seq, p.column
                    )))
                })?;
            let cmp = kernels::compare_scalar(full.column(pos), map_cmp(p.op), &p.value)?;
            matched = Some(match matched {
                Some(acc) => kernels::and(&acc, &cmp),
                None => cmp,
            });
        }
        let matched =
            matched.unwrap_or_else(|| kernels::bool_column(Bitmap::new_valid(rows), None));
        deleted = Some(match deleted {
            Some(acc) => kernels::or(&acc, &matched),
            None => matched,
        });
    }
    let deleted_mask = kernels::truthy(&deleted.expect("at least one tombstone was folded in"));
    let mut keep = Bitmap::new_valid(rows);
    for i in 0..rows {
        keep.set(i, !deleted_mask.get(i));
    }
    Ok(Some(keep))
}

impl TableSource for View {
    fn open_scan<'a>(
        &'a self,
        spec: &ScanSpec,
        _ctx: &ExecContext,
    ) -> Result<Box<dyn MorselSource + 'a>, ExecError> {
        let name = TableName::new(spec.db.clone(), spec.table.clone());
        let table = self
            .table(&name)
            .ok_or_else(|| src(Error::UnknownTable(name.to_string())))?;

        if spec.columns.is_empty() {
            return Err(ExecError::Plan(
                "scan needs at least one column".to_string(),
            ));
        }
        let mut columns = Vec::with_capacity(spec.columns.len());
        let mut fields = Vec::with_capacity(spec.columns.len());
        for col_name in &spec.columns {
            let pos = table
                .schema
                .iter()
                .position(|f| &f.field.name == col_name)
                .ok_or_else(|| ExecError::Plan(format!("unknown column {col_name}")))?;
            columns.push(pos);
            fields.push(table.schema[pos].field.clone());
        }

        let engine = engine_by_name(&table.engine)
            .ok_or_else(|| src(Error::Usage(format!("unknown engine {}", table.engine))))?;
        // `merge_key` is adelie-zit.1's `latest` read-time merge; every engine here sets None.
        let plan = engine.resolve(table);
        let mut segments: Vec<&SegmentEntry> = table
            .segments
            .iter()
            .filter(|s| plan.segments.contains(&s.id))
            .collect();

        let segments_total = segments.len();
        let mut segments_pruned = 0usize;
        if let Some(pred) = &spec.predicate {
            segments.retain(|seg| {
                let projected = seg.project_onto(&table.schema);
                let stats = |c: usize| {
                    let cs = &projected.columns[columns[c]];
                    Some(prune::ColumnRange {
                        rows: cs.rows as u64,
                        null_count: cs.null_count as u64,
                        min: cs.min.as_ref(),
                        max: cs.max.as_ref(),
                    })
                };
                let keep = prune::may_match(pred, &stats);
                if !keep {
                    segments_pruned += 1;
                }
                keep
            });
        }

        Ok(Box::new(TableScan {
            fields,
            columns,
            table,
            segments,
            buffered: self.buffered(&name),
            predicate: spec.predicate.clone(),
            root: self.snapshot().root().to_path_buf(),
            segments_total,
            segments_pruned,
            row_groups_total: AtomicUsize::new(0),
            row_groups_pruned: AtomicUsize::new(0),
            rows_read: AtomicU64::new(0),
        }))
    }
}

/// `View::scan`: every live row of `name`, unpruned and unlimited, morsels read sequentially
/// in index order so row order matches the pre-E5 scan (segments, then buffered, in order).
pub(crate) fn scan_all(view: &View, name: &TableName) -> Result<Vec<Batch>, Error> {
    let table = view
        .table(name)
        .ok_or_else(|| Error::UnknownTable(name.to_string()))?;
    let columns = table.schema.iter().map(|f| f.field.name.clone()).collect();
    let spec = ScanSpec {
        db: name.db.clone(),
        table: name.name.clone(),
        columns,
        predicate: None,
    };
    let ctx = ExecContext::unlimited();
    let source = view.open_scan(&spec, &ctx).map_err(unwrap_exec_error)?;
    let mut batches = Vec::new();
    for m in 0..source.morsels() {
        batches.extend(source.read(m, &ctx).map_err(unwrap_exec_error)?);
    }
    Ok(batches)
}

/// Downcasts an `ExecError::Source` back to the storage `Error` it wraps (`src` above always
/// builds it that way); anything else becomes `Error::Exec`.
fn unwrap_exec_error(e: ExecError) -> Error {
    match e {
        ExecError::Source(boxed) => match boxed.downcast::<Error>() {
            Ok(err) => *err,
            Err(other) => Error::Exec(ExecError::Source(other)),
        },
        other => Error::Exec(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Column, ExecOptions, Field};
    use crate::storage::manifest::{Commit, Edit, Manifest, Predicate, Snapshot, TableSpec};
    use crate::storage::segment::{Writer, WriterOptions};
    use crate::storage::{Store, StoreOptions};
    use crate::types::DataType;
    use std::path::Path;
    use std::time::Duration;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("adelie-scan-{tag}-{}-{nanos}", std::process::id()))
    }

    fn schema() -> Vec<Field> {
        vec![
            Field {
                name: "batch".to_string(),
                ty: DataType::UInt64,
            },
            Field {
                name: "idx".to_string(),
                ty: DataType::UInt64,
            },
        ]
    }

    fn batch(batch_no: u64, rows: u64) -> Batch {
        let b: Vec<Value> = (0..rows).map(|_| Value::UInt64(batch_no)).collect();
        let idx: Vec<Value> = (0..rows).map(Value::UInt64).collect();
        Batch::new(
            schema(),
            vec![
                Column::from_values(&DataType::UInt64, &b).unwrap(),
                Column::from_values(&DataType::UInt64, &idx).unwrap(),
            ],
        )
        .unwrap()
    }

    fn table() -> TableName {
        TableName::new("d", "t")
    }

    fn open(dir: &Path, opts: StoreOptions) -> Store {
        let store = Store::open(dir, opts).unwrap();
        store
            .create_table(TableSpec::new(table(), schema()))
            .unwrap();
        store
    }

    fn view_of(root: &Path, manifest: Manifest) -> View {
        View {
            snapshot: Arc::new(Snapshot::new(root.to_path_buf(), manifest)),
            buffered: std::collections::BTreeMap::new(),
        }
    }

    fn scan_rows(view: &View, name: &TableName) -> Vec<(u64, u64)> {
        view.scan(name)
            .unwrap()
            .iter()
            .flat_map(|b| {
                (0..b.rows()).map(|i| {
                    let batch = match b.column(0).get(i) {
                        Value::UInt64(n) => n,
                        other => panic!("expected UInt64, got {other:?}"),
                    };
                    let idx = match b.column(1).get(i) {
                        Value::UInt64(n) => n,
                        other => panic!("expected UInt64, got {other:?}"),
                    };
                    (batch, idx)
                })
            })
            .collect()
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn scan_applies_tombstones_and_keeps_seq_ordering() {
        let dir = temp_dir("tombstones");
        let store = open(&dir, StoreOptions::default());
        store.write(&table(), batch(0, 1)).unwrap();
        store.write(&table(), batch(1, 1)).unwrap();
        store
            .delete(
                &table(),
                vec![Predicate {
                    column: "batch".to_string(),
                    op: crate::storage::manifest::CmpOp::Eq,
                    value: Value::UInt64(0),
                }],
            )
            .unwrap();
        store.write(&table(), batch(0, 1)).unwrap();

        let rows = scan_rows(&store.snapshot(), &table());
        // Falsify (ignoring tombstones): the pre-delete batch-0 row would also survive, giving
        // two rows with batch == 0 instead of one.
        assert_eq!(rows.iter().filter(|&&(b, _)| b == 0).count(), 1);
        assert_eq!(rows.iter().filter(|&&(b, _)| b == 1).count(), 1);
        assert_eq!(rows.len(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn a_null_cell_is_never_deleted() {
        let dir = temp_dir("null-never-deleted");
        let store = open(&dir, StoreOptions::default());
        let one = Batch::new(
            schema(),
            vec![
                Column::from_values(&DataType::UInt64, &[Value::Null]).unwrap(),
                Column::from_values(&DataType::UInt64, &[Value::UInt64(1)]).unwrap(),
            ],
        )
        .unwrap();
        store.write(&table(), one).unwrap();
        store
            .delete(
                &table(),
                vec![Predicate {
                    column: "batch".to_string(),
                    op: crate::storage::manifest::CmpOp::Eq,
                    value: Value::UInt64(1),
                }],
            )
            .unwrap();
        let rows = store.snapshot().scan(&table()).unwrap();
        assert_eq!(rows.iter().map(Batch::rows).sum::<usize>(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn wait_until_buffered(store: &Store) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while store.snapshot().buffered(&table()).is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "write never reached the buffer"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem, spawns a thread
    fn buffered_rows_come_after_segment_rows() {
        let dir = temp_dir("buffered-after");
        let opts = StoreOptions {
            flush_interval: Duration::from_secs(3600),
            ..StoreOptions::default()
        };
        let store = Arc::new(open(&dir, opts));
        let writer = {
            let store = store.clone();
            std::thread::spawn(move || store.write(&table(), batch(0, 1)).unwrap())
        };
        wait_until_buffered(&store);
        store.flush().unwrap();
        writer.join().unwrap();

        store
            .shared
            .state
            .lock()
            .unwrap()
            .pending
            .entry(table())
            .or_default()
            .push(Arc::new(batch(1, 1)));
        let rows = scan_rows(&store.snapshot(), &table());
        assert_eq!(rows, vec![(0, 0), (1, 0)]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn int_schema() -> Vec<Field> {
        vec![Field {
            name: "a".to_string(),
            ty: DataType::Int64,
        }]
    }

    /// Builds a segment via a manual manifest (no `Store`): each of `groups` becomes one push
    /// (so `row_group_rows` at or above every group's length gives one row group per group).
    fn manual_segment(
        root: &Path,
        name: &TableName,
        groups: &[Vec<i64>],
        row_group_rows: usize,
        indexes: Vec<(String, IndexKind)>,
    ) -> (Manifest, SegmentEntry) {
        let fields = int_schema();
        let created = Commit {
            base: 0,
            edits: vec![Edit::CreateTable {
                spec: TableSpec::new(name.clone(), fields.clone()),
            }],
        }
        .apply(&Manifest::empty(), 0)
        .unwrap();
        let entry = created.table(name).unwrap();
        let table_dir = entry.dir();
        let field_ids: Vec<_> = entry.schema.iter().map(|f| f.id).collect();
        std::fs::create_dir_all(Manifest::table_dir(root, &table_dir).join("_")).unwrap();

        let opts = WriterOptions {
            row_group_rows,
            indexes,
            ..WriterOptions::default()
        };
        let mut writer = Writer::new(Vec::new(), fields.clone(), opts).unwrap();
        for g in groups {
            let values: Vec<Value> = g.iter().copied().map(Value::Int64).collect();
            let b = Batch::new(
                fields.clone(),
                vec![Column::from_values(&DataType::Int64, &values).unwrap()],
            )
            .unwrap();
            writer.push(&b).unwrap();
        }
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
        std::fs::write(Manifest::segment_path(root, &seg), &bytes).unwrap();

        let manifest = Commit {
            base: created.version,
            edits: vec![Edit::AddSegments {
                table: name.clone(),
                segments: vec![seg.clone()],
            }],
        }
        .apply(&created, 0)
        .unwrap();
        (manifest, seg)
    }

    fn query_ints(view: &View, name: &TableName, predicate: Option<Expr>) -> (Vec<i64>, ScanStats) {
        let spec = ScanSpec {
            db: name.db.clone(),
            table: name.name.clone(),
            columns: vec!["a".to_string()],
            predicate,
        };
        let ctx = ExecContext::unlimited();
        let source = view.open_scan(&spec, &ctx).unwrap();
        let mut rows = Vec::new();
        for m in 0..source.morsels() {
            for b in source.read(m, &ctx).unwrap() {
                for i in 0..b.rows() {
                    match b.column(0).get(i) {
                        Value::Int64(n) => rows.push(n),
                        other => panic!("expected Int64, got {other:?}"),
                    }
                }
            }
        }
        (rows, source.stats())
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn row_group_zone_pruning_never_filters_the_surviving_group() {
        let root = temp_dir("zone-prune");
        let name = TableName::new("d", "t");
        let groups: Vec<Vec<i64>> = (0..3i64).map(|g| (g * 4..g * 4 + 4).collect()).collect();
        let (manifest, _seg) = manual_segment(&root, &name, &groups, 4, Vec::new());
        let view = view_of(&root, manifest);

        let pred = Expr::cmp(
            crate::exec::CmpOp::Ge,
            Expr::col(0),
            Expr::lit(Value::Int64(8), DataType::Int64),
        );
        let (mut rows, stats) = query_ints(&view, &name, Some(pred));
        assert_eq!(stats.row_groups_total, 3);
        assert_eq!(stats.row_groups_pruned, 2);
        // The scan never filters: every row of the surviving group (8..12) comes back, not
        // just the ones >= 8.
        rows.sort_unstable();
        assert_eq!(rows, vec![8, 9, 10, 11]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn bloom_pruning_catches_what_zone_maps_cannot() {
        let root = temp_dir("bloom-prune");
        let name = TableName::new("d", "t");
        // Both groups span 0..100, so zone maps can't prune either; group 1 lacks 50.
        let groups = vec![vec![0, 25, 75, 99], vec![1, 2, 50, 98]];
        let (manifest, _seg) = manual_segment(
            &root,
            &name,
            &groups,
            4,
            vec![("a".to_string(), IndexKind::Bloom)],
        );
        let view = view_of(&root, manifest);

        let pred = Expr::cmp(
            crate::exec::CmpOp::Eq,
            Expr::col(0),
            Expr::lit(Value::Int64(50), DataType::Int64),
        );
        let (rows, stats) = query_ints(&view, &name, Some(pred));
        assert_eq!(stats.row_groups_total, 2);
        assert_eq!(stats.row_groups_pruned, 1);
        assert_eq!(rows, vec![1, 2, 50, 98]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn segment_pruning_from_manifest_stats() {
        let root = temp_dir("segment-prune");
        let name = TableName::new("d", "t");
        let (m1, seg1) = manual_segment(&root, &name, &[vec![1, 2, 3]], 100, Vec::new());
        let entry = m1.table(&name).unwrap();
        let field_ids: Vec<_> = entry.schema.iter().map(|f| f.id).collect();

        let fields = int_schema();
        let mut writer = Writer::new(Vec::new(), fields.clone(), WriterOptions::default()).unwrap();
        let values: Vec<Value> = vec![100, 101].into_iter().map(Value::Int64).collect();
        let b = Batch::new(
            fields,
            vec![Column::from_values(&DataType::Int64, &values).unwrap()],
        )
        .unwrap();
        writer.push(&b).unwrap();
        let (bytes, meta) = writer.finish().unwrap();
        let seg2 = SegmentEntry {
            id: 2,
            partition: "_".to_string(),
            seq: 2,
            rows: meta.rows,
            bytes: meta.bytes,
            footer_crc: meta.footer_crc,
            columns: meta.columns,
            side_files: Vec::new(),
            dir: seg1.dir.clone(),
            field_ids,
            file_field_ids: Vec::new(),
        };
        std::fs::write(Manifest::segment_path(&root, &seg2), &bytes).unwrap();
        let manifest = Commit {
            base: m1.version,
            edits: vec![Edit::AddSegments {
                table: name.clone(),
                segments: vec![seg2],
            }],
        }
        .apply(&m1, 0)
        .unwrap();
        let view = view_of(&root, manifest);

        let pred = Expr::cmp(
            crate::exec::CmpOp::Eq,
            Expr::col(0),
            Expr::lit(Value::Int64(101), DataType::Int64),
        );
        let (rows, stats) = query_ints(&view, &name, Some(pred));
        assert_eq!(stats.segments_total, 2);
        assert_eq!(stats.segments_pruned, 1);
        assert_eq!(rows, vec![100, 101]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn projection_returns_only_the_requested_column() {
        let root = temp_dir("projection");
        let name = TableName::new("d", "t");
        let (manifest, _seg) = manual_segment(&root, &name, &[vec![1, 2, 3]], 100, Vec::new());
        let view = view_of(&root, manifest);

        let spec = ScanSpec {
            db: name.db.clone(),
            table: name.name.clone(),
            columns: vec!["a".to_string()],
            predicate: None,
        };
        let ctx = ExecContext::unlimited();
        let source = view.open_scan(&spec, &ctx).unwrap();
        assert_eq!(source.fields().len(), 1);
        assert_eq!(source.fields()[0].name, "a");

        let bad = ScanSpec {
            db: name.db.clone(),
            table: name.name.clone(),
            columns: vec!["nope".to_string()],
            predicate: None,
        };
        assert!(matches!(
            view.open_scan(&bad, &ctx),
            Err(ExecError::Plan(_))
        ));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn view_at_an_old_version_scans_that_versions_rows() {
        let dir = temp_dir("view-at-scan");
        let opts = StoreOptions {
            retain_manifests: 4,
            flush_interval: Duration::from_millis(10),
            ..StoreOptions::default()
        };
        let store = open(&dir, opts);
        store.write(&table(), batch(0, 1)).unwrap();
        let v1 = store.snapshot().version();
        store.write(&table(), batch(1, 1)).unwrap();

        let rows = scan_rows(&store.view_at(v1).unwrap(), &table());
        assert_eq!(rows, vec![(0, 0)]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn a_missing_segment_file_is_snapshot_expired_through_downcast() {
        let root = temp_dir("missing-segment");
        let name = TableName::new("d", "t");
        let (manifest, seg) = manual_segment(&root, &name, &[vec![1]], 100, Vec::new());
        std::fs::remove_file(Manifest::segment_path(&root, &seg)).unwrap();
        let view = view_of(&root, manifest);

        let err = view.scan(&name).unwrap_err();
        assert!(matches!(err, Error::SnapshotExpired { .. }), "{err}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn memory_limit_below_the_file_size_is_budget_exceeded() {
        let root = temp_dir("budget");
        let name = TableName::new("d", "t");
        let (manifest, _seg) = manual_segment(&root, &name, &[vec![1, 2, 3]], 100, Vec::new());
        let view = view_of(&root, manifest);

        let spec = ScanSpec {
            db: name.db.clone(),
            table: name.name.clone(),
            columns: vec!["a".to_string()],
            predicate: None,
        };
        let ctx = ExecContext::new(&ExecOptions {
            memory_limit: 1,
            ..Default::default()
        });
        let source = view.open_scan(&spec, &ctx).unwrap();
        let err = source.read(0, &ctx).unwrap_err();
        assert!(matches!(err, ExecError::BudgetExceeded { .. }));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
