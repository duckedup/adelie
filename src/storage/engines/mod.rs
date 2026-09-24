//! The table-engine boundary (SPEC §18, Q12): flush, merge policy, scan resolution and
//! optional index builders. The core (commits, snapshots, GC, the straddle check) is fixed;
//! only these hooks vary per engine. `append` is the only engine E4 ships.

use std::sync::Arc;

use crate::exec::{Batch, Field};
use crate::storage::manifest::TableEntry;
use crate::storage::segment::IndexKind;

use super::{Error, StoreOptions};

/// A merge plan: compact these input segment ids of `partition` together. The core validates
/// every plan (`compact::validate_merge`) before running it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePlan {
    pub partition: String,
    pub inputs: Vec<u64>,
}

/// Which segments a scan should read, and the key rows are merged on (`None`: rows are
/// independent, as `append` produces).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanPlan {
    pub segments: Vec<u64>,
    pub merge_key: Option<Vec<String>>,
}

/// A table engine (SPEC §18). Implementations are stateless: everything they need comes in
/// through the arguments.
pub trait Engine: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    /// Buffered batches, in arrival order, grouped into one `Vec<Batch>` per output segment.
    fn flush(
        &self,
        schema: &[Field],
        batches: &[Arc<Batch>],
        max_rows: usize,
    ) -> Result<Vec<Vec<Batch>>, Error>;

    /// Which live segments to compact together. The core validates the result.
    fn plan_merge(&self, table: &TableEntry, opts: &StoreOptions) -> Vec<MergePlan>;

    /// Input rows, one `Vec<Batch>` per input in seq order, merged into output segments.
    fn merge(
        &self,
        schema: &[Field],
        inputs: Vec<Vec<Batch>>,
        max_rows: usize,
    ) -> Result<Vec<Vec<Batch>>, Error>;

    /// Scan resolution over the table's unmerged segments.
    fn resolve(&self, table: &TableEntry) -> ScanPlan;

    /// Skip indexes to build for every segment this engine writes. None by default.
    fn indexes(&self, _schema: &[Field]) -> Vec<(String, IndexKind)> {
        Vec::new()
    }
}

mod append;

pub use append::Append;

/// `"append"` maps to `&Append`; every other name is unknown to the core.
pub fn engine_by_name(name: &str) -> Option<&'static dyn Engine> {
    static APPEND: Append = Append;
    match name {
        "append" => Some(&APPEND),
        _ => None,
    }
}
