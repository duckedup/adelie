//! adelie's executor building blocks: `Column`, `Batch` and the kernels operators share
//! (SPEC §7, §14). Pure: no IO, so Miri can run it directly (SPEC §6, §13).
//! Operators, the scheduler and the memory budget live here too; the module stays pure —
//! storage implements `TableSource`.

mod agg;
mod batch;
mod bitmap;
mod coalesce;
mod column;
mod context;
mod execute;
pub mod expr;
mod join;
pub(crate) mod kernels;
mod operator;
pub(crate) mod ops;
mod pipeline;
mod plan;
mod stats;

pub use agg::output_type as agg_output_type;
pub use batch::{Batch, BatchError, Field};
pub use bitmap::Bitmap;
pub use coalesce::coalesce_companion;
pub use column::{Column, ColumnBuilder, ColumnError};
pub(crate) use column::{ColumnValues, OwnedValues};
pub use context::{CancelToken, ExecContext, ExecError, ExecOptions, Reservation};
pub use execute::{QueryResult, QueryStats, execute};
pub use expr::{ArithOp, CmpOp, Expr, ScalarFunc};
pub use operator::{BatchSource, MorselSource, ScanSpec, ScanStats, TableSource};
pub use plan::{AggCall, AggFunc, JoinKind, Plan, SortKey};
pub use stats::ColumnStats;

/// Operators process column batches of about this many rows (SPEC §7). A target, not a cap.
pub const BATCH_ROWS: usize = 4096;
