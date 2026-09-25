//! Operator, sink and source traits every exec stage implements (SPEC §7).

#![allow(dead_code)] // Operator/Sink: until adelie-1st U5 (ops) and U7 (pipeline) implement them

use super::context::{ExecContext, ExecError};
use super::expr::Expr;
use super::{Batch, Field};

/// A streaming stage: `push` may emit zero or more batches per input batch, `finish` flushes
/// whatever it buffered (a sort or join build side buffers everything and emits on finish).
pub(crate) trait Operator: Send {
    fn push(
        &mut self,
        ctx: &ExecContext,
        batch: Batch,
        out: &mut Vec<Batch>,
    ) -> Result<(), ExecError>;

    fn finish(&mut self, _ctx: &ExecContext, _out: &mut Vec<Batch>) -> Result<(), ExecError> {
        Ok(())
    }

    /// Needs no more input (e.g. a `Limit` that already has enough rows).
    fn done(&self) -> bool {
        false
    }
}

/// A per-thread partial accumulation that `merge`s into one final result (SPEC §7's
/// partial-then-merge shape: sort, top-k, hash aggregate, join build).
pub(crate) trait Sink: Send + Sized {
    fn push(&mut self, ctx: &ExecContext, batch: Batch) -> Result<(), ExecError>;

    /// Folds another thread's partial `Sink` of the same kind into this one.
    fn merge(&mut self, ctx: &ExecContext, other: Self) -> Result<(), ExecError>;

    /// Consumes the sink, rechunked.
    fn finish(self, ctx: &ExecContext) -> Result<Vec<Batch>, ExecError>;

    fn done(&self) -> bool {
        false
    }
}

/// One morsel is one segment (or one buffered batch); readable in parallel across threads.
pub trait MorselSource: Sync {
    fn fields(&self) -> &[Field];
    fn morsels(&self) -> usize;
    fn read(&self, morsel: usize, ctx: &ExecContext) -> Result<Vec<Batch>, ExecError>;

    fn stats(&self) -> ScanStats {
        ScanStats::default()
    }
}

/// Pruning counters a scan reports for `QueryStats` (SPEC §7 acceptance criteria).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanStats {
    pub segments_total: usize,
    pub segments_pruned: usize,
    pub row_groups_total: usize,
    pub row_groups_pruned: usize,
    pub rows_read: u64,
}

impl ScanStats {
    pub fn add(&mut self, other: &ScanStats) {
        self.segments_total += other.segments_total;
        self.segments_pruned += other.segments_pruned;
        self.row_groups_total += other.row_groups_total;
        self.row_groups_pruned += other.row_groups_pruned;
        self.rows_read += other.rows_read;
    }
}

/// A scan request: `columns` in output order, `predicate` for pruning only — the scan never
/// filters, so the executor always follows it with a `Filter`.
#[derive(Debug, Clone, PartialEq)]
pub struct ScanSpec {
    pub db: String,
    pub table: String,
    pub columns: Vec<String>,
    pub predicate: Option<Expr>,
}

/// Storage's side of the scan contract: opens a `ScanSpec` onto one table's live rows,
/// projected and tombstone-filtered, over `columns` in order.
pub trait TableSource: Sync {
    fn open_scan<'a>(
        &'a self,
        spec: &ScanSpec,
        ctx: &ExecContext,
    ) -> Result<Box<dyn MorselSource + 'a>, ExecError>;
}

/// In-memory batches, one morsel per batch (intermediates, tests).
pub struct BatchSource {
    fields: Vec<Field>,
    batches: Vec<Batch>,
}

impl BatchSource {
    pub fn new(fields: Vec<Field>, batches: Vec<Batch>) -> BatchSource {
        BatchSource { fields, batches }
    }
}

impl MorselSource for BatchSource {
    fn fields(&self) -> &[Field] {
        &self.fields
    }

    fn morsels(&self) -> usize {
        self.batches.len()
    }

    fn read(&self, morsel: usize, _ctx: &ExecContext) -> Result<Vec<Batch>, ExecError> {
        Ok(vec![self.batches[morsel].clone()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Column, ExecContext};
    use crate::types::{DataType, Value};

    fn one_row_batch(n: i64) -> Batch {
        let field = Field {
            name: "a".to_string(),
            ty: DataType::Int64,
        };
        let col = Column::from_values(&DataType::Int64, &[Value::Int64(n)]).unwrap();
        Batch::new(vec![field], vec![col]).unwrap()
    }

    #[test]
    fn batch_source_reports_one_morsel_per_batch() {
        let fields = vec![Field {
            name: "a".to_string(),
            ty: DataType::Int64,
        }];
        let src = BatchSource::new(fields.clone(), vec![one_row_batch(1), one_row_batch(2)]);
        assert_eq!(src.fields(), fields.as_slice());
        assert_eq!(src.morsels(), 2);
        let ctx = ExecContext::unlimited();
        let read0 = src.read(0, &ctx).unwrap();
        assert_eq!(read0.len(), 1);
        assert_eq!(read0[0].column(0).get(0), Value::Int64(1));
        let read1 = src.read(1, &ctx).unwrap();
        assert_eq!(read1[0].column(0).get(0), Value::Int64(2));
    }

    #[test]
    fn scan_stats_add_sums_every_field() {
        let mut total = ScanStats::default();
        total.add(&ScanStats {
            segments_total: 1,
            segments_pruned: 0,
            row_groups_total: 2,
            row_groups_pruned: 1,
            rows_read: 10,
        });
        total.add(&ScanStats {
            segments_total: 3,
            segments_pruned: 2,
            row_groups_total: 4,
            row_groups_pruned: 0,
            rows_read: 20,
        });
        assert_eq!(
            total,
            ScanStats {
                segments_total: 4,
                segments_pruned: 2,
                row_groups_total: 6,
                row_groups_pruned: 1,
                rows_read: 30,
            }
        );
    }
}
