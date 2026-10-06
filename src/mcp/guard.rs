//! The guardrails as pure functions: the write gate, the sample cap and per-call options.

use super::Guardrails;
use crate::exec::ExecOptions;
use crate::sql::{self, Options};

const DEFAULT_SAMPLE: usize = 10;

/// Err with the message a caller sees when `query` may not run.
pub(super) fn check_sql(guard: &Guardrails, query: &str) -> Result<(), String> {
    let read_only = sql::is_read_only(query).map_err(|e| e.to_string())?;
    if !read_only && !guard.allow_writes {
        return Err("writes are disabled; restart with --allow-writes".to_string());
    }
    Ok(())
}

/// `n` defaults to 10 and never exceeds `max_rows`.
pub(super) fn sample_rows(n: Option<usize>, max_rows: usize) -> usize {
    n.unwrap_or(DEFAULT_SAMPLE).min(max_rows)
}

pub(super) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The query's time and memory budget, from the guardrails.
pub(super) fn options(guard: &Guardrails) -> Options {
    Options {
        exec: ExecOptions {
            timeout: Some(guard.timeout),
            memory_limit: guard.memory_limit,
            ..ExecOptions::default()
        },
        ..Options::default()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::surface::Limits;

    fn guard(allow_writes: bool) -> Guardrails {
        Guardrails {
            allow_writes,
            limits: Limits {
                max_rows: 200,
                max_bytes: 1 << 16,
            },
            timeout: Duration::from_secs(5),
            memory_limit: 1 << 20,
        }
    }

    /// Fails if a write, alone or hidden behind a SELECT, gets past a read-only guard.
    #[test]
    fn read_only_guard_rejects_writes() {
        for q in ["INSERT INTO t VALUES (1)", "SELECT 1; DELETE FROM t"] {
            let err = check_sql(&guard(false), q).unwrap_err();
            assert_eq!(
                err, "writes are disabled; restart with --allow-writes",
                "{q}"
            );
            assert!(check_sql(&guard(true), q).is_ok(), "{q}");
        }
        assert!(check_sql(&guard(false), "SELECT 1").is_ok());
        assert!(check_sql(&guard(false), "SELEC 1").is_err());
    }

    /// Fails if `n` above the row cap is passed through.
    #[test]
    fn sample_is_clamped_to_max_rows() {
        assert_eq!(sample_rows(None, 200), 10);
        assert_eq!(sample_rows(None, 5), 5);
        assert_eq!(sample_rows(Some(1_000_000), 200), 200);
        assert_eq!(sample_rows(Some(3), 200), 3);
    }

    #[test]
    fn identifiers_are_quoted() {
        assert_eq!(quote_ident("t"), "\"t\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn options_carry_the_budget() {
        let o = options(&guard(false));
        assert_eq!(o.exec.timeout, Some(Duration::from_secs(5)));
        assert_eq!(o.exec.memory_limit, 1 << 20);
    }
}
