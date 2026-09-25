//! LIKE/ILIKE pattern matching for `Expr::Like` (SPEC §7 syntax). Filled by U2 (adelie-1st).

use crate::exec::kernels::boolean::BoolBuilder;
use crate::exec::{Column, ExecError};
use crate::types::{DataType, Value};

#[derive(Debug, Clone, Copy, PartialEq)]
enum Token {
    Any,
    One,
    Char(char),
}

/// Tokenizes once per call. `\` escapes the next character; a trailing lone `\` is `Invalid`.
fn compile(pattern: &str) -> Result<Vec<Token>, ExecError> {
    let mut tokens = Vec::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '%' => tokens.push(Token::Any),
            '_' => tokens.push(Token::One),
            '\\' => match chars.next() {
                Some(escaped) => tokens.push(Token::Char(escaped)),
                None => {
                    return Err(ExecError::Invalid(
                        "LIKE pattern ends with an escape".to_string(),
                    ));
                }
            },
            other => tokens.push(Token::Char(other)),
        }
    }
    Ok(tokens)
}

fn char_eq(a: char, b: char, case_insensitive: bool) -> bool {
    if case_insensitive {
        a.to_lowercase().eq(b.to_lowercase())
    } else {
        a == b
    }
}

/// Greedy two-pointer wildcard match with a single star position, O(n·m) worst case and no
/// recursion (a hostile pattern must not be a stack-overflow vector).
fn like_match(s: &[char], p: &[Token], case_insensitive: bool) -> bool {
    let (mut si, mut pi) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut match_from = 0usize;
    while si < s.len() {
        let literal_match = match p.get(pi) {
            Some(Token::One) => true,
            Some(Token::Char(pc)) => char_eq(*pc, s[si], case_insensitive),
            _ => false,
        };
        if literal_match {
            si += 1;
            pi += 1;
        } else if p.get(pi) == Some(&Token::Any) {
            star = Some(pi);
            match_from = si;
            pi += 1;
        } else if let Some(star_pos) = star {
            pi = star_pos + 1;
            match_from += 1;
            si = match_from;
        } else {
            return false;
        }
    }
    while p.get(pi) == Some(&Token::Any) {
        pi += 1;
    }
    pi == p.len()
}

/// The BOOL result, NULL where `col` is NULL. `col` must be STRING (`eval` only calls this
/// after typing checks it).
pub(crate) fn like(col: &Column, pattern: &str, case_insensitive: bool) -> Result<Column, ExecError> {
    if col.data_type() != &DataType::String {
        return Err(ExecError::Plan(format!(
            "LIKE operand must be STRING, found {}",
            col.data_type()
        )));
    }
    let tokens = compile(pattern)?;
    let mut out = BoolBuilder::new();
    for i in 0..col.len() {
        if col.is_null(i) {
            out.push(None);
            continue;
        }
        let Value::String(s) = col.get(i) else {
            unreachable!("checked STRING above")
        };
        let chars: Vec<char> = s.chars().collect();
        out.push(Some(like_match(&chars, &tokens, case_insensitive)));
    }
    Ok(out.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn str_col(vals: &[Option<&str>]) -> Column {
        let values: Vec<Value> = vals
            .iter()
            .map(|v| v.map_or(Value::Null, |s| Value::String(s.to_string())))
            .collect();
        Column::from_values(&DataType::String, &values).unwrap()
    }

    fn bool_at(col: &Column, i: usize) -> Option<bool> {
        if col.is_null(i) {
            None
        } else if let Value::Bool(b) = col.get(i) {
            Some(b)
        } else {
            unreachable!()
        }
    }

    #[test]
    fn percent_matches_a_run_and_is_case_sensitive_unless_ilike() {
        let col = str_col(&[Some("charlie"), Some("Charlie")]);
        let out = like(&col, "char%", false).unwrap();
        assert_eq!(bool_at(&out, 0), Some(true));
        assert_eq!(bool_at(&out, 1), Some(false));
        let out_ci = like(&col, "char%", true).unwrap();
        assert_eq!(bool_at(&out_ci, 0), Some(true));
        assert_eq!(bool_at(&out_ci, 1), Some(true));
    }

    #[test]
    fn underscore_matches_one_unicode_scalar() {
        let col = str_col(&[Some("ab"), Some("éb")]);
        let out = like(&col, "_b", false).unwrap();
        assert_eq!(bool_at(&out, 0), Some(true));
        assert_eq!(bool_at(&out, 1), Some(true));
    }

    #[test]
    fn escaped_percent_matches_only_the_literal_character() {
        let col = str_col(&[Some("a%"), Some("ab")]);
        let out = like(&col, "a\\%", false).unwrap();
        assert_eq!(bool_at(&out, 0), Some(true));
        assert_eq!(bool_at(&out, 1), Some(false));
    }

    #[test]
    fn null_row_gives_null() {
        let col = str_col(&[None]);
        let out = like(&col, "%", false).unwrap();
        assert_eq!(bool_at(&out, 0), None);
    }

    #[test]
    fn trailing_escape_is_invalid() {
        let col = str_col(&[Some("x")]);
        let err = like(&col, "a\\", false).unwrap_err();
        assert!(matches!(err, ExecError::Invalid(_)));
    }

    /// Falsify: a naive recursive backtracker would blow the stack or time out here.
    #[test]
    #[cfg_attr(miri, ignore)] // 2000-char scan, no UB surface beyond what smaller tests cover
    fn no_exponential_backtracking_on_a_long_non_matching_string() {
        let long = "a".repeat(2000);
        let col = str_col(&[Some(long.as_str())]);
        let out = like(&col, "%a%b%", false).unwrap();
        assert_eq!(bool_at(&out, 0), Some(false));
    }
}
