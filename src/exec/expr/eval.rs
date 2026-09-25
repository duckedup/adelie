//! Evaluates `Expr` against a `Batch`, producing the output column. Filled by U2 (adelie-1st).

use super::Expr;
use crate::exec::{Batch, Column, ExecError};

pub fn eval(expr: &Expr, batch: &Batch) -> Result<Column, ExecError> {
    unimplemented!("adelie-1st U2")
}
