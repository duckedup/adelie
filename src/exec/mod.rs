//! adelie's executor building blocks: `Column`, `Batch` and the kernels operators share
//! (SPEC §7, §14). Pure: no IO, so Miri can run it directly (SPEC §6, §13).

mod batch;
mod bitmap;
mod coalesce;
mod column;
mod stats;

pub use batch::{Batch, BatchError, Field};
pub use bitmap::Bitmap;
pub use coalesce::coalesce_companion;
pub use column::{Column, ColumnBuilder, ColumnError};
pub use stats::ColumnStats;

/// Operators process column batches of about this many rows (SPEC §7). A target, not a cap.
pub const BATCH_ROWS: usize = 4096;
