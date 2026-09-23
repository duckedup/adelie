//! `ColumnStats`: the per-column `{rows, null_count, min, max}` E3's segment footer stores
//! (SPEC §5). `Column::stats` (in `column.rs`) computes it.

use crate::types::Value;

/// A column's stats over one batch. `min`/`max` use `types::total_cmp`, not derived
/// `PartialEq`, so `NaN` and cross-scale `DECIMAL` compare the way the footer needs.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnStats {
    pub rows: usize,
    pub null_count: usize,
    pub min: Option<Value>,
    pub max: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::Column;
    use crate::types::DataType;

    #[test]
    fn stats_struct_holds_the_computed_fields() {
        let col = Column::from_values(&DataType::Int64, &[Value::Int64(1)]).unwrap();
        let stats = col.stats();
        assert_eq!(
            stats,
            ColumnStats {
                rows: 1,
                null_count: 0,
                min: Some(Value::Int64(1)),
                max: Some(Value::Int64(1))
            }
        );
    }
}
