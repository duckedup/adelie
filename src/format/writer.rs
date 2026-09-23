//! `SegmentWriter` (SPEC §5, D0008): buffers pushed batches into row groups, encodes each
//! column chunk, builds requested skip indexes, and frames the footer/trailer on `finish`.

use std::cmp::Ordering;
use std::io::Write;

use crate::exec::{Batch, Column, ColumnStats, Field};
use crate::types::{Value, total_cmp};

use super::crc::crc32c;
use super::directory::RawIndexEntry;
use super::encode::{Encoding, encode_column};
use super::error::FormatError;
use super::footer::{Footer, RawChunk, RawRowGroup, bound_stats, encode_footer, write_trailer};
use super::index::{self, IndexKind};
use super::{DEFAULT_ROW_GROUP_ROWS, FORMAT_VERSION, HEADER_LEN, MAX_DECODE_ROWS, SEGMENT_MAGIC};

/// Options a `SegmentWriter` is built with. A pin skips encoding selection for that column;
/// an index request builds one skip structure per row group for that column.
#[derive(Debug, Clone, PartialEq)]
pub struct WriterOptions {
    pub row_group_rows: usize,
    pub pins: Vec<(String, Encoding)>,
    pub indexes: Vec<(String, IndexKind)>,
}

impl Default for WriterOptions {
    fn default() -> Self {
        WriterOptions { row_group_rows: DEFAULT_ROW_GROUP_ROWS, pins: Vec::new(), indexes: Vec::new() }
    }
}

/// A finished segment's summary: total rows, row group count, file size, the footer's own
/// CRC, and per-column stats over the whole segment (SPEC §5).
#[derive(Debug, Clone, PartialEq)]
pub struct SegmentMeta {
    pub rows: u64,
    pub row_groups: usize,
    pub bytes: u64,
    pub footer_crc: u32,
    pub columns: Vec<ColumnStats>,
}

/// Accumulates one column's segment-wide stats from each row group's bounded `(min, max)`.
/// A max dropped by truncation (SPEC §5, `bound_stats`) in any row group stays dropped.
struct StatsAcc {
    rows: usize,
    null_count: usize,
    min: Option<Value>,
    max: Option<Value>,
    max_dropped: bool,
}

impl StatsAcc {
    fn new() -> Self {
        StatsAcc { rows: 0, null_count: 0, min: None, max: None, max_dropped: false }
    }

    /// Sums rows/nulls unconditionally; folds `min`/`max` in only for a row group that has
    /// at least one valid (non-null) row, via `total_cmp`.
    fn merge(&mut self, rg_stats: &ColumnStats, min: Option<Value>, max: Option<Value>) {
        self.rows += rg_stats.rows;
        self.null_count += rg_stats.null_count;
        if rg_stats.rows <= rg_stats.null_count {
            return;
        }
        if let Some(v) = min {
            if self.min.as_ref().is_none_or(|m| total_cmp(&v, m) == Some(Ordering::Less)) {
                self.min = Some(v);
            }
        }
        if self.max_dropped {
            return;
        }
        match max {
            Some(v) => {
                if self.max.as_ref().is_none_or(|m| total_cmp(&v, m) == Some(Ordering::Greater)) {
                    self.max = Some(v);
                }
            }
            None => self.max_dropped = true,
        }
    }

    fn finish(self) -> ColumnStats {
        ColumnStats {
            rows: self.rows,
            null_count: self.null_count,
            min: self.min,
            max: if self.max_dropped { None } else { self.max },
        }
    }
}

/// Writes one `.seg` file (SPEC §5). Batches are never split: a flushed row group holds
/// `[row_group_rows, row_group_rows + last_batch_rows − 1]` rows. Deterministic, and `out`
/// is never flushed or synced (durability is the caller's job).
pub struct SegmentWriter<W: Write> {
    out: W,
    pos: u64,
    fields: Vec<Field>,
    opts: WriterOptions,
    pending: Vec<Vec<Column>>,
    pending_rows: usize,
    row_groups: Vec<RawRowGroup>,
    indexes: Vec<RawIndexEntry>,
    stats: Vec<StatsAcc>,
    pins: Vec<Option<Encoding>>,
    index_reqs: Vec<(usize, IndexKind)>,
}

impl<W: Write> SegmentWriter<W> {
    /// Validates `opts` and `fields` (`Usage` on the first problem, naming the column), then
    /// writes the 8-byte header immediately.
    pub fn new(mut out: W, fields: Vec<Field>, opts: WriterOptions) -> Result<Self, FormatError> {
        if opts.row_group_rows == 0 || opts.row_group_rows > MAX_DECODE_ROWS {
            return Err(FormatError::Usage(format!(
                "row_group_rows must be in 1..={MAX_DECODE_ROWS}, got {}",
                opts.row_group_rows
            )));
        }
        for (i, field) in fields.iter().enumerate() {
            if fields[..i].iter().any(|f| f.name == field.name) {
                return Err(FormatError::Usage(format!("duplicate field name {}", field.name)));
            }
        }
        let pins = resolve_pins(&fields, &opts.pins)?;
        let index_reqs = resolve_indexes(&fields, &opts.indexes)?;

        let mut header = [0u8; HEADER_LEN];
        header[..6].copy_from_slice(&SEGMENT_MAGIC);
        header[6..8].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.write_all(&header)?;

        let pending = fields.iter().map(|_| Vec::new()).collect();
        let stats = fields.iter().map(|_| StatsAcc::new()).collect();
        Ok(SegmentWriter {
            out,
            pos: HEADER_LEN as u64,
            fields,
            opts,
            pending,
            pending_rows: 0,
            row_groups: Vec::new(),
            indexes: Vec::new(),
            stats,
            pins,
            index_reqs,
        })
    }

    /// Buffers `batch` (a no-op if it has 0 rows), flushing one row group once enough rows
    /// have accumulated. `Usage` if `batch`'s fields don't match the writer's, order included.
    pub fn push(&mut self, batch: &Batch) -> Result<(), FormatError> {
        if batch.fields() != self.fields.as_slice() {
            return Err(FormatError::Usage(format!(
                "batch fields {:?} do not match writer fields {:?}",
                batch.fields(),
                self.fields
            )));
        }
        if batch.rows() == 0 {
            return Ok(());
        }
        for (i, col) in batch.columns().iter().enumerate() {
            self.pending[i].push(col.clone());
        }
        self.pending_rows += batch.rows();
        if self.pending_rows >= self.opts.row_group_rows {
            self.flush()?;
        }
        Ok(())
    }

    /// Flushed rows plus whatever is still buffered.
    pub fn rows(&self) -> u64 {
        let flushed: u64 = self.row_groups.iter().map(|rg| rg.rows).sum();
        flushed + self.pending_rows as u64
    }

    /// Flushes any pending rows, then writes the footer and trailer. Returns `out` and a
    /// summary of the whole segment. A segment with 0 rows is still valid.
    pub fn finish(mut self) -> Result<(W, SegmentMeta), FormatError> {
        self.flush()?;
        let footer = Footer { fields: self.fields.clone(), row_groups: self.row_groups, indexes: self.indexes };
        let row_groups = footer.row_groups.len();
        let rows = footer.row_groups.iter().map(|rg| rg.rows).sum();
        let footer_bytes = encode_footer(&footer);
        let footer_crc = crc32c(&footer_bytes);
        let mut framed = Vec::new();
        write_trailer(&mut framed, SEGMENT_MAGIC, &footer_bytes);
        self.out.write_all(&framed)?;
        let bytes = self.pos + framed.len() as u64;
        let columns = self.stats.into_iter().map(StatsAcc::finish).collect();
        Ok((self.out, SegmentMeta { rows, row_groups, bytes, footer_crc, columns }))
    }

    /// One row group: every column chunk in schema order, then that row group's index blobs.
    fn flush(&mut self) -> Result<(), FormatError> {
        if self.pending_rows == 0 {
            return Ok(());
        }
        let rg_rows = self.pending_rows as u64;
        let row_group_idx = self.row_groups.len();
        let mut chunks = Vec::with_capacity(self.fields.len());
        let mut concatenated = Vec::with_capacity(self.fields.len());
        for i in 0..self.fields.len() {
            let cols = std::mem::take(&mut self.pending[i]);
            let field = &self.fields[i];
            let col = Column::concat(&field.ty, &cols)
                .map_err(|e| FormatError::Usage(format!("column {}: {e}", field.name)))?;
            let (enc, bytes) = encode_column(&col, self.pins[i]);
            let crc = crc32c(&bytes);
            let offset = self.pos;
            self.out.write_all(&bytes)?;
            self.pos += bytes.len() as u64;

            let rg_stats = col.stats();
            let (min, max) = bound_stats(&rg_stats);
            self.stats[i].merge(&rg_stats, min.clone(), max.clone());
            chunks.push(RawChunk {
                offset,
                len: bytes.len() as u64,
                crc,
                encoding: enc.id(),
                null_count: rg_stats.null_count as u64,
                min,
                max,
            });
            concatenated.push(col);
        }
        for &(ordinal, kind) in &self.index_reqs {
            if let Some(blob) = index::build(kind, &concatenated[ordinal]) {
                let crc = crc32c(&blob);
                let offset = self.pos;
                self.out.write_all(&blob)?;
                self.pos += blob.len() as u64;
                self.indexes.push(RawIndexEntry {
                    kind: kind.id(),
                    row_group: Some(row_group_idx),
                    columns: vec![ordinal],
                    offset,
                    len: blob.len() as u64,
                    crc,
                    params: Vec::new(),
                });
            }
        }
        self.row_groups.push(RawRowGroup { rows: rg_rows, chunks });
        self.pending_rows = 0;
        Ok(())
    }
}

/// Resolves each pin to a field ordinal, checking it names a real column and that the
/// encoding applies to that column's type (`Usage`, naming the column, on either failure).
fn resolve_pins(fields: &[Field], pins: &[(String, Encoding)]) -> Result<Vec<Option<Encoding>>, FormatError> {
    let mut resolved = vec![None; fields.len()];
    for (name, enc) in pins {
        let idx = fields
            .iter()
            .position(|f| &f.name == name)
            .ok_or_else(|| FormatError::Usage(format!("pin names unknown column {name}")))?;
        if !enc.applies_to(&fields[idx].ty) {
            return Err(FormatError::Usage(format!(
                "pin encoding {enc:?} does not apply to column {name}"
            )));
        }
        resolved[idx] = Some(*enc);
    }
    Ok(resolved)
}

/// Resolves each index request to a field ordinal, the same way `resolve_pins` does.
fn resolve_indexes(
    fields: &[Field],
    indexes: &[(String, IndexKind)],
) -> Result<Vec<(usize, IndexKind)>, FormatError> {
    let mut resolved = Vec::with_capacity(indexes.len());
    for (name, kind) in indexes {
        let idx = fields
            .iter()
            .position(|f| &f.name == name)
            .ok_or_else(|| FormatError::Usage(format!("index names unknown column {name}")))?;
        if !kind.applies_to(&fields[idx].ty) {
            return Err(FormatError::Usage(format!(
                "index kind {kind:?} does not apply to column {name}"
            )));
        }
        resolved.push((idx, *kind));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DataType;

    use super::super::footer::{decode_footer, read_trailer};

    fn fields() -> Vec<Field> {
        vec![Field { name: "t".to_string(), ty: DataType::Int64 }, Field { name: "s".to_string(), ty: DataType::String }]
    }

    fn batch(t: &[i64], s: &[&str]) -> Batch {
        let tvals: Vec<Value> = t.iter().copied().map(Value::Int64).collect();
        let svals: Vec<Value> = s.iter().map(|x| Value::String(x.to_string())).collect();
        Batch::new(
            fields(),
            vec![
                Column::from_values(&DataType::Int64, &tvals).unwrap(),
                Column::from_values(&DataType::String, &svals).unwrap(),
            ],
        )
        .unwrap()
    }

    fn open_written(bytes: &[u8]) -> Footer {
        let range = read_trailer(bytes, SEGMENT_MAGIC, HEADER_LEN).unwrap();
        decode_footer(&bytes[range]).unwrap()
    }

    #[test]
    fn five_batches_of_50_with_row_group_rows_100_gives_100_100_50() {
        let opts = WriterOptions { row_group_rows: 100, ..Default::default() };
        let mut w = SegmentWriter::new(Vec::new(), fields(), opts).unwrap();
        for b in 0..5 {
            let t: Vec<i64> = (0..50).map(|i| b * 50 + i).collect();
            let s: Vec<&str> = (0..50).map(|_| "x").collect();
            w.push(&batch(&t, &s)).unwrap();
        }
        let (out, meta) = w.finish().unwrap();
        assert_eq!(meta.rows, 250);
        let footer = open_written(&out);
        let rg_rows: Vec<u64> = footer.row_groups.iter().map(|rg| rg.rows).collect();
        assert_eq!(rg_rows, vec![100, 100, 50]);
    }

    #[test]
    fn chunk_stats_match_column_stats_per_row_group() {
        let opts = WriterOptions { row_group_rows: 100, ..Default::default() };
        let mut w = SegmentWriter::new(Vec::new(), fields(), opts).unwrap();
        let t: Vec<i64> = (0..100).collect();
        let s: Vec<&str> = (0..100).map(|_| "x").collect();
        w.push(&batch(&t, &s)).unwrap();
        let (out, _meta) = w.finish().unwrap();
        let footer = open_written(&out);
        let chunk = &footer.row_groups[0].chunks[0];
        assert_eq!(chunk.null_count, 0);
        assert_eq!(chunk.min, Some(Value::Int64(0)));
        assert_eq!(chunk.max, Some(Value::Int64(99)));
    }

    #[test]
    fn a_pin_records_its_encoding_and_a_control_run_differs() {
        let t: Vec<i64> = (0..1000).collect();
        let s: Vec<&str> = (0..1000).map(|_| "x").collect();

        let pinned_opts = WriterOptions {
            row_group_rows: 2000,
            pins: vec![("t".to_string(), Encoding::Plain)],
            indexes: Vec::new(),
        };
        let mut pinned = SegmentWriter::new(Vec::new(), fields(), pinned_opts).unwrap();
        pinned.push(&batch(&t, &s)).unwrap();
        let (out, _) = pinned.finish().unwrap();
        let footer = open_written(&out);
        assert_eq!(footer.row_groups[0].chunks[0].encoding, Encoding::Plain.id());

        let mut unpinned = SegmentWriter::new(Vec::new(), fields(), WriterOptions { row_group_rows: 2000, ..Default::default() }).unwrap();
        unpinned.push(&batch(&t, &s)).unwrap();
        let (out2, _) = unpinned.finish().unwrap();
        let footer2 = open_written(&out2);
        assert_ne!(footer2.row_groups[0].chunks[0].encoding, Encoding::Plain.id());
    }

    #[test]
    fn a_bloom_index_request_gives_one_entry_per_row_group_with_verifiable_crc() {
        let opts = WriterOptions {
            row_group_rows: 50,
            pins: Vec::new(),
            indexes: vec![("s".to_string(), IndexKind::Bloom)],
        };
        let mut w = SegmentWriter::new(Vec::new(), fields(), opts).unwrap();
        for b in 0..2 {
            let t: Vec<i64> = (0..50).map(|i| b * 50 + i).collect();
            let s: Vec<&str> = (0..50).map(|_| "x").collect();
            w.push(&batch(&t, &s)).unwrap();
        }
        let (out, _meta) = w.finish().unwrap();
        let footer = open_written(&out);
        assert_eq!(footer.indexes.len(), 2);
        for entry in &footer.indexes {
            assert_eq!(entry.kind, IndexKind::Bloom.id());
            let range = entry.offset as usize..(entry.offset + entry.len) as usize;
            assert_eq!(crc32c(&out[range]), entry.crc);
        }
    }

    #[test]
    fn row_group_rows_zero_is_usage() {
        let opts = WriterOptions { row_group_rows: 0, ..Default::default() };
        let err = SegmentWriter::new(Vec::new(), fields(), opts).unwrap_err();
        assert!(matches!(err, FormatError::Usage(_)));
    }

    #[test]
    fn row_group_rows_above_max_decode_rows_is_usage() {
        let opts = WriterOptions { row_group_rows: MAX_DECODE_ROWS + 1, ..Default::default() };
        let err = SegmentWriter::new(Vec::new(), fields(), opts).unwrap_err();
        assert!(matches!(err, FormatError::Usage(_)));
    }

    #[test]
    fn duplicate_field_names_is_usage() {
        let dupes = vec![Field { name: "a".to_string(), ty: DataType::Int64 }, Field { name: "a".to_string(), ty: DataType::Int64 }];
        let err = SegmentWriter::new(Vec::new(), dupes, WriterOptions::default()).unwrap_err();
        assert!(matches!(err, FormatError::Usage(_)));
    }

    #[test]
    fn pin_naming_an_unknown_column_is_usage() {
        let opts = WriterOptions { pins: vec![("nope".to_string(), Encoding::Plain)], ..Default::default() };
        let err = SegmentWriter::new(Vec::new(), fields(), opts).unwrap_err();
        assert!(matches!(err, FormatError::Usage(_)));
    }

    #[test]
    fn pin_encoding_that_does_not_apply_is_usage() {
        let opts = WriterOptions { pins: vec![("t".to_string(), Encoding::Dict)], ..Default::default() };
        let err = SegmentWriter::new(Vec::new(), fields(), opts).unwrap_err();
        assert!(matches!(err, FormatError::Usage(_)));
    }

    #[test]
    fn index_naming_an_unknown_column_is_usage() {
        let opts = WriterOptions { indexes: vec![("nope".to_string(), IndexKind::Bloom)], ..Default::default() };
        let err = SegmentWriter::new(Vec::new(), fields(), opts).unwrap_err();
        assert!(matches!(err, FormatError::Usage(_)));
    }

    #[test]
    fn index_kind_that_does_not_apply_is_usage() {
        let opts = WriterOptions { indexes: vec![("t".to_string(), IndexKind::Ngram)], ..Default::default() };
        let err = SegmentWriter::new(Vec::new(), fields(), opts).unwrap_err();
        assert!(matches!(err, FormatError::Usage(_)));
    }

    #[test]
    fn push_of_a_batch_with_different_field_order_is_usage() {
        let mut w = SegmentWriter::new(Vec::new(), fields(), WriterOptions::default()).unwrap();
        let reordered_fields = vec![fields()[1].clone(), fields()[0].clone()];
        let bad = Batch::new(
            reordered_fields,
            vec![
                Column::from_values(&DataType::String, &[Value::String("x".into())]).unwrap(),
                Column::from_values(&DataType::Int64, &[Value::Int64(1)]).unwrap(),
            ],
        )
        .unwrap();
        let err = w.push(&bad).unwrap_err();
        assert!(matches!(err, FormatError::Usage(_)));
    }

    #[test]
    fn two_writers_fed_the_same_batches_are_byte_identical() {
        let make = || {
            let opts = WriterOptions { row_group_rows: 30, ..Default::default() };
            let mut w = SegmentWriter::new(Vec::new(), fields(), opts).unwrap();
            let t: Vec<i64> = (0..70).collect();
            let s: Vec<&str> = (0..70).map(|_| "hi").collect();
            w.push(&batch(&t[..40], &s[..40])).unwrap();
            w.push(&batch(&t[40..], &s[40..])).unwrap();
            w.finish().unwrap().0
        };
        assert_eq!(make(), make());
    }

    #[test]
    fn segment_meta_columns_equals_stats_of_the_concatenated_input() {
        let opts = WriterOptions { row_group_rows: 10, ..Default::default() };
        let mut w = SegmentWriter::new(Vec::new(), fields(), opts).unwrap();
        let t: Vec<i64> = (0..20).collect();
        let s: Vec<&str> = (0..20).map(|_| "short").collect();
        w.push(&batch(&t, &s)).unwrap();
        let (_out, meta) = w.finish().unwrap();

        let tvals: Vec<Value> = t.iter().copied().map(Value::Int64).collect();
        let full = Column::from_values(&DataType::Int64, &tvals).unwrap();
        assert_eq!(meta.columns[0], full.stats());
    }

    #[test]
    fn a_long_string_in_one_row_group_drops_the_segment_max_but_keeps_a_min_prefix() {
        let opts = WriterOptions { row_group_rows: 2, ..Default::default() };
        let mut w = SegmentWriter::new(Vec::new(), fields(), opts).unwrap();
        w.push(&batch(&[1, 2], &["a", "b"])).unwrap();
        let long = "z".repeat(300);
        w.push(&batch(&[3, 4], &["c", long.as_str()])).unwrap();
        let (_out, meta) = w.finish().unwrap();
        assert_eq!(meta.columns[1].max, None);
        assert_eq!(meta.columns[1].min, Some(Value::String("a".to_string())));
    }

    #[test]
    fn empty_segment_is_8_plus_footer_plus_16_bytes_with_no_row_groups() {
        let w = SegmentWriter::new(Vec::new(), fields(), WriterOptions::default()).unwrap();
        let (out, meta) = w.finish().unwrap();
        assert_eq!(meta.row_groups, 0);
        assert_eq!(meta.rows, 0);
        assert_eq!(out.len(), meta.bytes as usize);
        let range = read_trailer(&out, SEGMENT_MAGIC, HEADER_LEN).unwrap();
        assert_eq!(range.start, HEADER_LEN);
        assert_eq!(out.len(), HEADER_LEN + range.len() + 16);
        let footer = decode_footer(&out[range]).unwrap();
        assert!(footer.row_groups.is_empty());
        assert!(footer.indexes.is_empty());
    }
}
