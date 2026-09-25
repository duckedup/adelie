//! adelie's type system: `DataType`, `Value`, total order, SQL equality, canonical text and
//! lossless coercion (SPEC §3, §14). Pure: no IO, so Miri can run it directly (SPEC §6, §13).

mod datatype;
mod fit;
mod order;
mod parse;
mod text;
mod value;

pub use datatype::*;
pub use fit::{COMPANION_SUFFIX, coerce, companion_name, fits};
pub use order::{sql_eq, total_cmp};
pub use value::*;

/// The civil-date conversions, shared with `exec::expr::time` (adelie-1st.1) so it does not
/// duplicate Howard Hinnant's algorithm.
pub(crate) use parse::days_from_civil;
pub(crate) use text::civil_from_days;
