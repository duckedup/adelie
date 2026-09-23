//! Renders a `Value` as text under a declared column type, per the sqllogictest dialect's
//! rendering table.

use super::ColType;
use crate::engine::Value;

pub(super) fn render_row(row: &[Value], types: &[ColType]) -> String {
    row.iter()
        .zip(types)
        .map(|(v, t)| render_cell(v, *t))
        .collect::<Vec<_>>()
        .join(" ")
}

fn render_cell(value: &Value, ty: ColType) -> String {
    if let Value::Float(f) = value {
        if f.is_nan() {
            return "NaN".to_string();
        }
        if f.is_infinite() {
            return if *f > 0.0 {
                "inf".to_string()
            } else {
                "-inf".to_string()
            };
        }
    }
    match value {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => match ty {
            ColType::Int | ColType::Text => n.to_string(),
            ColType::Real => format!("{:.3}", *n as f64),
        },
        Value::Float(f) => match ty {
            ColType::Int => (f.trunc() as i64).to_string(),
            ColType::Real => {
                let s = format!("{f:.3}");
                if s == "-0.000" {
                    "0.000".to_string()
                } else {
                    s
                }
            }
            ColType::Text => format!("{f}"),
        },
        Value::Text(s) => match ty {
            ColType::Int | ColType::Real => s.clone(),
            ColType::Text => {
                if s.is_empty() {
                    "(empty)".to_string()
                } else {
                    s.clone()
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_is_always_null() {
        for ty in [ColType::Int, ColType::Real, ColType::Text] {
            assert_eq!(render_cell(&Value::Null, ty), "NULL");
        }
    }

    #[test]
    fn bool_is_true_false_everywhere() {
        for ty in [ColType::Int, ColType::Real, ColType::Text] {
            assert_eq!(render_cell(&Value::Bool(true), ty), "true");
            assert_eq!(render_cell(&Value::Bool(false), ty), "false");
        }
    }

    #[test]
    fn int_renders_per_column() {
        assert_eq!(render_cell(&Value::Int(7), ColType::Int), "7");
        assert_eq!(render_cell(&Value::Int(7), ColType::Real), "7.000");
        assert_eq!(render_cell(&Value::Int(7), ColType::Text), "7");
    }

    #[test]
    fn float_renders_per_column() {
        assert_eq!(render_cell(&Value::Float(2.5), ColType::Int), "2");
        assert_eq!(render_cell(&Value::Float(2.5), ColType::Real), "2.500");
        assert_eq!(render_cell(&Value::Float(2.5), ColType::Text), "2.5");
    }

    #[test]
    fn negative_zero_real_is_positive() {
        assert_eq!(
            render_cell(&Value::Float(-0.0000001), ColType::Real),
            "0.000"
        );
    }

    #[test]
    fn non_finite_float_overrides_every_column() {
        for ty in [ColType::Int, ColType::Real, ColType::Text] {
            assert_eq!(render_cell(&Value::Float(f64::NAN), ty), "NaN");
            assert_eq!(render_cell(&Value::Float(f64::INFINITY), ty), "inf");
            assert_eq!(render_cell(&Value::Float(f64::NEG_INFINITY), ty), "-inf");
        }
    }

    #[test]
    fn text_renders_per_column() {
        assert_eq!(
            render_cell(&Value::Text("hi".to_string()), ColType::Int),
            "hi"
        );
        assert_eq!(
            render_cell(&Value::Text("hi".to_string()), ColType::Real),
            "hi"
        );
        assert_eq!(
            render_cell(&Value::Text("hi".to_string()), ColType::Text),
            "hi"
        );
    }

    #[test]
    fn empty_text_in_text_column() {
        assert_eq!(
            render_cell(&Value::Text(String::new()), ColType::Text),
            "(empty)"
        );
        assert_eq!(render_cell(&Value::Text(String::new()), ColType::Int), "");
    }

    #[test]
    fn row_joins_with_single_spaces() {
        let row = vec![Value::Int(1), Value::Text("a".to_string())];
        assert_eq!(render_row(&row, &[ColType::Int, ColType::Text]), "1 a");
    }
}
