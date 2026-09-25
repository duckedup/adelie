//! `SqlOutput` and `Rows` (contract C5): what one statement returns.

use crate::exec::{Batch, Field, QueryStats};

/// One statement's result: a row count for DDL/DML, or a `SELECT`'s rows.
#[derive(Debug)]
pub enum SqlOutput {
    Statement { rows_affected: u64 },
    Rows(Rows),
}

/// A `SELECT`'s output: the final projection's fields and batches, the executor's stats, and
/// the wsr.1 companion warnings gathered while binding (deduplicated, in first-seen order).
#[derive(Debug)]
pub struct Rows {
    pub fields: Vec<Field>,
    pub batches: Vec<Batch>,
    pub stats: QueryStats,
    pub warnings: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statement_variant_holds_rows_affected() {
        let out = SqlOutput::Statement { rows_affected: 3 };
        assert!(matches!(out, SqlOutput::Statement { rows_affected: 3 }));
    }
}
