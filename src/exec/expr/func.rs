//! `ScalarFunc` (contract C1, adelie-1st.1): named functions the binder lowers `Expr::Func`
//! calls to, their per-signature typing table, and the dispatcher that runs their kernels.

use crate::exec::{Column, ExecError};
use crate::types::DataType;

use super::Regex;
use super::{regexp, string, time};

#[derive(Debug, Clone, PartialEq)]
pub enum ScalarFunc {
    Lower,
    Upper,
    Length,
    Substr,
    Concat,
    RegexpMatch(Regex),
    DateTrunc(TruncUnit),
    TimeBucket { width_ns: i64, origin_ns: i64 },
    Extract(DatePart),
    CoalesceText,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TruncUnit {
    Microsecond,
    Millisecond,
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DatePart {
    Year,
    Quarter,
    Month,
    Week,
    Day,
    DayOfWeek,
    DayOfYear,
    Hour,
    Minute,
    Second,
    Millisecond,
    Microsecond,
    Epoch,
}

fn func_name(func: &ScalarFunc) -> &'static str {
    match func {
        ScalarFunc::Lower => "lower",
        ScalarFunc::Upper => "upper",
        ScalarFunc::Length => "length",
        ScalarFunc::Substr => "substr",
        ScalarFunc::Concat => "concat",
        ScalarFunc::RegexpMatch(_) => "regexp_match",
        ScalarFunc::DateTrunc(_) => "date_trunc",
        ScalarFunc::TimeBucket { .. } => "time_bucket",
        ScalarFunc::Extract(_) => "extract",
        ScalarFunc::CoalesceText => "coalesce_text",
    }
}

fn signature_error(func: &ScalarFunc, args: &[DataType], want: &str) -> ExecError {
    let found: Vec<String> = args.iter().map(ToString::to_string).collect();
    ExecError::Plan(format!(
        "{}({}) has no signature ({want})",
        func_name(func),
        found.join(", ")
    ))
}

fn expect_args(func: &ScalarFunc, args: &[DataType], want: &[DataType]) -> Result<(), ExecError> {
    if args == want {
        Ok(())
    } else {
        let want_str: Vec<String> = want.iter().map(ToString::to_string).collect();
        Err(signature_error(func, args, &want_str.join(", ")))
    }
}

/// Checks arity and arg types (SPEC's per-function signature table); `ExecError::Plan` names
/// the function and the signature it failed on mismatch.
pub(super) fn func_type(func: &ScalarFunc, args: &[DataType]) -> Result<DataType, ExecError> {
    match func {
        ScalarFunc::Lower | ScalarFunc::Upper => {
            expect_args(func, args, &[DataType::String])?;
            Ok(DataType::String)
        }
        ScalarFunc::Length => {
            expect_args(func, args, &[DataType::String])?;
            Ok(DataType::Int64)
        }
        ScalarFunc::Substr => match args {
            [DataType::String, DataType::Int64]
            | [DataType::String, DataType::Int64, DataType::Int64] => Ok(DataType::String),
            _ => Err(signature_error(func, args, "STRING, INT64[, INT64]")),
        },
        ScalarFunc::Concat => {
            expect_args(func, args, &[DataType::String, DataType::String])?;
            Ok(DataType::String)
        }
        ScalarFunc::RegexpMatch(_) => {
            expect_args(func, args, &[DataType::String])?;
            Ok(DataType::Bool)
        }
        ScalarFunc::DateTrunc(_) => {
            expect_args(func, args, &[DataType::Timestamp])?;
            Ok(DataType::Timestamp)
        }
        ScalarFunc::TimeBucket { width_ns, .. } => {
            expect_args(func, args, &[DataType::Timestamp])?;
            if *width_ns <= 0 {
                return Err(ExecError::Plan(format!(
                    "time_bucket width_ns must be positive, found {width_ns}"
                )));
            }
            Ok(DataType::Timestamp)
        }
        ScalarFunc::Extract(_) => match args {
            [DataType::Timestamp] | [DataType::Date] => Ok(DataType::Int64),
            _ => Err(signature_error(func, args, "TIMESTAMP or DATE")),
        },
        ScalarFunc::CoalesceText => match args {
            [_, DataType::String] => Ok(DataType::String),
            [_, other] => Err(ExecError::Plan(format!(
                "coalesce_text companion must be STRING, found {other}"
            ))),
            _ => Err(signature_error(func, args, "any, STRING")),
        },
    }
}

/// Dispatches to each function's kernel. `func_type` has already checked arity and types, so
/// every kernel here can assume its args are well-typed.
pub(super) fn eval_func(
    func: &ScalarFunc,
    cols: &[Column],
    rows: usize,
) -> Result<Column, ExecError> {
    assert!(
        cols.iter().all(|c| c.len() == rows),
        "eval_func: an argument column does not have `rows` rows"
    );
    match func {
        ScalarFunc::Lower => string::lower(&cols[0]),
        ScalarFunc::Upper => string::upper(&cols[0]),
        ScalarFunc::Length => string::length(&cols[0]),
        ScalarFunc::Substr => string::substr(&cols[0], &cols[1], cols.get(2)),
        ScalarFunc::Concat => string::concat(&cols[0], &cols[1]),
        ScalarFunc::RegexpMatch(re) => regexp::eval_match(&cols[0], re),
        ScalarFunc::DateTrunc(unit) => time::date_trunc(&cols[0], unit),
        ScalarFunc::TimeBucket {
            width_ns,
            origin_ns,
        } => time::time_bucket(&cols[0], *width_ns, *origin_ns),
        ScalarFunc::Extract(part) => time::extract(&cols[0], part),
        ScalarFunc::CoalesceText => Ok(crate::exec::coalesce_companion(&cols[0], &cols[1])?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(precision: u8, scale: u8) -> DataType {
        DataType::decimal(precision, scale).unwrap()
    }

    #[test]
    fn lower_upper_length_want_exactly_one_string() {
        for func in [ScalarFunc::Lower, ScalarFunc::Upper, ScalarFunc::Length] {
            assert!(func_type(&func, &[DataType::String]).is_ok());
            assert!(matches!(func_type(&func, &[]), Err(ExecError::Plan(_))));
            assert!(matches!(
                func_type(&func, &[DataType::Int64]),
                Err(ExecError::Plan(_))
            ));
        }
    }

    #[test]
    fn substr_accepts_two_or_three_args_rejects_others() {
        assert_eq!(
            func_type(&ScalarFunc::Substr, &[DataType::String, DataType::Int64]).unwrap(),
            DataType::String
        );
        assert_eq!(
            func_type(
                &ScalarFunc::Substr,
                &[DataType::String, DataType::Int64, DataType::Int64]
            )
            .unwrap(),
            DataType::String
        );
        assert!(func_type(&ScalarFunc::Substr, &[DataType::String]).is_err());
    }

    #[test]
    fn concat_wants_two_strings() {
        assert_eq!(
            func_type(&ScalarFunc::Concat, &[DataType::String, DataType::String]).unwrap(),
            DataType::String
        );
        assert!(func_type(&ScalarFunc::Concat, &[DataType::String, DataType::Int64]).is_err());
    }

    #[test]
    fn regexp_match_wants_one_string_returns_bool() {
        let re = Regex::compile("a+").unwrap();
        assert_eq!(
            func_type(&ScalarFunc::RegexpMatch(re), &[DataType::String]).unwrap(),
            DataType::Bool
        );
    }

    #[test]
    fn date_trunc_wants_timestamp() {
        assert_eq!(
            func_type(
                &ScalarFunc::DateTrunc(TruncUnit::Day),
                &[DataType::Timestamp]
            )
            .unwrap(),
            DataType::Timestamp
        );
        assert!(func_type(&ScalarFunc::DateTrunc(TruncUnit::Day), &[DataType::Date]).is_err());
    }

    #[test]
    fn time_bucket_rejects_non_positive_width() {
        let func = ScalarFunc::TimeBucket {
            width_ns: 0,
            origin_ns: 0,
        };
        assert!(func_type(&func, &[DataType::Timestamp]).is_err());
        let func = ScalarFunc::TimeBucket {
            width_ns: -1,
            origin_ns: 0,
        };
        assert!(func_type(&func, &[DataType::Timestamp]).is_err());
    }

    #[test]
    fn extract_accepts_timestamp_or_date() {
        let func = ScalarFunc::Extract(DatePart::Year);
        assert_eq!(
            func_type(&func, &[DataType::Timestamp]).unwrap(),
            DataType::Int64
        );
        assert_eq!(
            func_type(&func, &[DataType::Date]).unwrap(),
            DataType::Int64
        );
        assert!(func_type(&func, &[DataType::String]).is_err());
    }

    #[test]
    fn coalesce_text_rejects_a_non_string_companion() {
        let func = ScalarFunc::CoalesceText;
        assert_eq!(
            func_type(&func, &[DataType::Int64, DataType::String]).unwrap(),
            DataType::String
        );
        let err = func_type(&func, &[DataType::Int64, DataType::Int64]).unwrap_err();
        assert!(matches!(err, ExecError::Plan(_)));
        assert!(func_type(&func, &[DataType::Int64, dt(5, 2)]).is_err());
    }
}
