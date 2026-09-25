//! Kleene (three-valued) boolean logic over BOOL columns (SPEC §7 syntax).

use crate::exec::{Bitmap, Column, OwnedValues};
use crate::types::{DataType, Value};

/// Builds a BOOL column row by row from Kleene outcomes (`None` is NULL), the way
/// `ColumnBuilder::push_null` lazily backfills validity on the first NULL.
pub(crate) struct BoolBuilder {
    bits: Bitmap,
    validity: Option<Bitmap>,
    len: usize,
}

impl BoolBuilder {
    pub(crate) fn new() -> Self {
        BoolBuilder {
            bits: Bitmap::new_valid(0),
            validity: None,
            len: 0,
        }
    }

    pub(crate) fn push(&mut self, outcome: Option<bool>) {
        self.bits.push(outcome.unwrap_or(false));
        if let Some(v) = &mut self.validity {
            v.push(outcome.is_some());
        } else if outcome.is_none() {
            let mut v = Bitmap::new_valid(self.len);
            v.push(false);
            self.validity = Some(v);
        }
        self.len += 1;
    }

    pub(crate) fn finish(self) -> Column {
        bool_column(self.bits, self.validity)
    }
}

pub(crate) fn bool_column(values: Bitmap, validity: Option<Bitmap>) -> Column {
    Column::from_parts(DataType::Bool, OwnedValues::Bool(values), validity)
        .expect("a BOOL bitmap and matching validity always build a valid column")
}

fn row_bool(col: &Column, i: usize) -> Option<bool> {
    if col.is_null(i) {
        None
    } else if let Value::Bool(b) = col.get(i) {
        Some(b)
    } else {
        unreachable!("boolean kernel called on a non-BOOL column")
    }
}

fn kleene_and(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

fn kleene_or(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

pub(crate) fn and(a: &Column, b: &Column) -> Column {
    assert_eq!(
        a.len(),
        b.len(),
        "boolean AND: length mismatch {} vs {}",
        a.len(),
        b.len()
    );
    let mut out = BoolBuilder::new();
    for i in 0..a.len() {
        out.push(kleene_and(row_bool(a, i), row_bool(b, i)));
    }
    out.finish()
}

pub(crate) fn or(a: &Column, b: &Column) -> Column {
    assert_eq!(
        a.len(),
        b.len(),
        "boolean OR: length mismatch {} vs {}",
        a.len(),
        b.len()
    );
    let mut out = BoolBuilder::new();
    for i in 0..a.len() {
        out.push(kleene_or(row_bool(a, i), row_bool(b, i)));
    }
    out.finish()
}

pub(crate) fn not(a: &Column) -> Column {
    let mut out = BoolBuilder::new();
    for i in 0..a.len() {
        out.push(row_bool(a, i).map(|b| !b));
    }
    out.finish()
}

/// A bit set iff the row is valid and true (NULL and false both count as not-truthy).
pub(crate) fn truthy(col: &Column) -> Bitmap {
    let mut out = Bitmap::new_valid(0);
    for i in 0..col.len() {
        out.push(row_bool(col, i).unwrap_or(false));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bool_col(vals: &[Option<bool>]) -> Column {
        let values: Vec<Value> = vals
            .iter()
            .map(|v| v.map_or(Value::Null, Value::Bool))
            .collect();
        Column::from_values(&DataType::Bool, &values).unwrap()
    }

    fn row(col: &Column, i: usize) -> Option<bool> {
        row_bool(col, i)
    }

    /// Every (a, b) pair over {true, false, NULL}, for `and`/`or`'s full truth table.
    fn every_pair() -> (Column, Column) {
        let vals = [Some(true), Some(false), None];
        let mut a_vals = Vec::new();
        let mut b_vals = Vec::new();
        for &x in &vals {
            for &y in &vals {
                a_vals.push(x);
                b_vals.push(y);
            }
        }
        (bool_col(&a_vals), bool_col(&b_vals))
    }

    #[test]
    fn and_truth_table() {
        let (a, b) = every_pair();
        let out = and(&a, &b);
        let expected = [
            Some(true),
            Some(false),
            None,
            Some(false),
            Some(false),
            Some(false),
            None,
            Some(false),
            None,
        ];
        for i in 0..9 {
            assert_eq!(row(&out, i), expected[i], "and row {i}");
        }
    }

    #[test]
    fn or_truth_table() {
        let (a, b) = every_pair();
        let out = or(&a, &b);
        let expected = [
            Some(true),
            Some(true),
            Some(true),
            Some(true),
            Some(false),
            None,
            Some(true),
            None,
            None,
        ];
        for i in 0..9 {
            assert_eq!(row(&out, i), expected[i], "or row {i}");
        }
    }

    #[test]
    fn not_of_null_is_null() {
        let col = bool_col(&[Some(true), Some(false), None]);
        let out = not(&col);
        assert_eq!(row(&out, 0), Some(false));
        assert_eq!(row(&out, 1), Some(true));
        assert_eq!(row(&out, 2), None);
    }

    #[test]
    fn truthy_treats_false_and_null_as_not_truthy() {
        let col = bool_col(&[Some(true), Some(false), None]);
        let bm = truthy(&col);
        assert!(bm.get(0));
        assert!(!bm.get(1));
        assert!(!bm.get(2));
    }
}
