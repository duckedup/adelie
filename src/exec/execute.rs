//! Plan execution entry point (SPEC §7). `execute` is filled by U11 (adelie-1st); this unit
//! pins the result types every later unit returns.

use super::context::{ExecError, ExecOptions};
use super::operator::{ScanStats, TableSource};
use super::plan::Plan;
use super::{Batch, Field};

pub struct QueryResult {
    pub fields: Vec<Field>,
    pub batches: Vec<Batch>,
    pub stats: QueryStats,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryStats {
    pub scan: ScanStats,
    pub peak_memory: usize,
}

pub fn execute(
    source: &dyn TableSource,
    plan: &Plan,
    opts: &ExecOptions,
) -> Result<QueryResult, ExecError> {
    unimplemented!("adelie-1st U11")
}
