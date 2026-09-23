//! Runs parsed records against one engine, checking each against its own expected block.

use super::render::render_row;
use super::{Condition, Directive, Failure, Record, Report, lines_match};
use crate::engine::{Engine, Outcome};

pub fn run(engine: &mut dyn Engine, records: &[Record]) -> Report {
    let mut report = Report::default();
    for record in records {
        if record.directive == Directive::Halt {
            break;
        }
        if is_skipped(&record.conditions, engine.name()) {
            report.skipped += 1;
            continue;
        }
        match check(engine, record) {
            Ok(()) => report.passed += 1,
            Err(failure) => report.failures.push(failure),
        }
    }
    report
}

/// Skipped if any `skipif` names this engine, or an `onlyif` list exists and excludes it.
fn is_skipped(conditions: &[Condition], name: &str) -> bool {
    let mut onlyifs = Vec::new();
    for c in conditions {
        match c {
            Condition::SkipIf(n) if n == name => return true,
            Condition::OnlyIf(n) => onlyifs.push(n.as_str()),
            _ => {}
        }
    }
    !onlyifs.is_empty() && !onlyifs.contains(&name)
}

fn check(engine: &mut dyn Engine, record: &Record) -> Result<(), Failure> {
    match &record.directive {
        Directive::Halt => Ok(()),
        Directive::StatementOk => match engine.run(&record.sql) {
            Ok(_) => Ok(()),
            Err(e) => Err(fail(record, "ok", &format!("error: {e}"))),
        },
        Directive::StatementError(sub) | Directive::QueryError(sub) => check_error(engine, record, sub.as_deref()),
        Directive::QueryHashUnsupported => Err(fail(record, "", "hash results unsupported")),
        Directive::Query { types, sort, expected, .. } => match engine.run(&record.sql) {
            Err(e) => Err(fail(record, &expected.join("\n"), &format!("error: {e}"))),
            Ok(Outcome::Statement) => Err(fail(record, &expected.join("\n"), "expected rows")),
            Ok(Outcome::Rows(rows)) => {
                if let Some(row) = rows.iter().find(|r| r.len() != types.len()) {
                    let msg = format!("expected {} columns, got {}", types.len(), row.len());
                    return Err(fail(record, &expected.join("\n"), &msg));
                }
                let actual: Vec<String> = rows.iter().map(|r| render_row(r, types)).collect();
                if lines_match(*sort, expected, &actual) {
                    Ok(())
                } else {
                    Err(fail(record, &expected.join("\n"), &actual.join("\n")))
                }
            }
        },
    }
}

fn check_error(engine: &mut dyn Engine, record: &Record, sub: Option<&str>) -> Result<(), Failure> {
    let wanted = match sub {
        Some(s) => format!("error containing \"{s}\""),
        None => "error".to_string(),
    };
    match engine.run(&record.sql) {
        Ok(_) => Err(fail(record, &wanted, "ok")),
        Err(e) => match sub {
            Some(s) if !e.0.to_lowercase().contains(&s.to_lowercase()) => {
                Err(fail(record, &wanted, &format!("error: {e}")))
            }
            _ => Ok(()),
        },
    }
}

fn fail(record: &Record, expected: &str, actual: &str) -> Failure {
    Failure { line: record.line, sql: record.sql.clone(), expected: expected.to_string(), actual: actual.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EngineError;
    use crate::fake::FakeEngine;
    use crate::slt::parse;

    #[test]
    fn correct_answer_passes() {
        let mut fake = FakeEngine::new("fake").answer("SELECT 1", Ok(Outcome::Rows(vec![vec![crate::engine::Value::Int(1)]])));
        let records = parse("query I\nSELECT 1\n----\n1").unwrap();
        let report = run(&mut fake, &records);
        assert!(report.ok());
        assert_eq!(report.passed, 1);
    }

    #[test]
    fn wrong_answer_fails_at_the_right_line() {
        let mut fake = FakeEngine::new("fake").answer("SELECT 1", Ok(Outcome::Rows(vec![vec![crate::engine::Value::Int(2)]])));
        let records = parse("query I\nSELECT 1\n----\n1").unwrap();
        let report = run(&mut fake, &records);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].line, 1);
    }

    #[test]
    fn rowsort_tolerates_permutation_nosort_does_not() {
        let rows = Ok(Outcome::Rows(vec![
            vec![crate::engine::Value::Int(2)],
            vec![crate::engine::Value::Int(1)],
        ]));
        let mut sorted = FakeEngine::new("fake").answer("SELECT a", rows.clone());
        let sorted_records = parse("query I rowsort\nSELECT a\n----\n1\n2").unwrap();
        assert!(run(&mut sorted, &sorted_records).ok());

        let mut unsorted = FakeEngine::new("fake").answer("SELECT a", rows);
        let unsorted_records = parse("query I nosort\nSELECT a\n----\n1\n2").unwrap();
        assert!(!run(&mut unsorted, &unsorted_records).ok());
    }

    #[test]
    fn statement_error_substring_matches_case_insensitively() {
        let mut fake = FakeEngine::new("fake").answer("bad", Err(EngineError("a FOO happened".to_string())));
        let records = parse("statement error foo\nbad").unwrap();
        assert!(run(&mut fake, &records).ok());
    }

    #[test]
    fn statement_error_fails_on_ok() {
        let mut fake = FakeEngine::new("fake").answer("fine", Ok(Outcome::Statement));
        let records = parse("statement error\nfine").unwrap();
        assert!(!run(&mut fake, &records).ok());
    }

    #[test]
    fn query_returning_statement_fails() {
        let mut fake = FakeEngine::new("fake").answer("SELECT 1", Ok(Outcome::Statement));
        let records = parse("query I\nSELECT 1\n----\n1").unwrap();
        let report = run(&mut fake, &records);
        assert_eq!(report.failures[0].actual, "expected rows");
    }

    #[test]
    fn width_mismatch_fails() {
        let mut fake = FakeEngine::new("fake").answer(
            "SELECT a, b",
            Ok(Outcome::Rows(vec![vec![crate::engine::Value::Int(1)]])),
        );
        let records = parse("query II\nSELECT a, b\n----\n1 2").unwrap();
        let report = run(&mut fake, &records);
        assert_eq!(report.failures[0].actual, "expected 2 columns, got 1");
    }

    #[test]
    fn skipif_fake_is_skipped_not_passed() {
        let mut fake = FakeEngine::new("fake").answer("SELECT 1", Ok(Outcome::Statement));
        let records = parse("skipif fake\nstatement ok\nSELECT 1").unwrap();
        let report = run(&mut fake, &records);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.passed, 0);
    }

    #[test]
    fn halt_stops_the_run() {
        let mut fake = FakeEngine::new("fake").answer("SELECT 1", Ok(Outcome::Statement));
        let records = parse("halt\n\nstatement ok\nSELECT 1").unwrap();
        let report = run(&mut fake, &records);
        assert_eq!(report.passed, 0);
        assert_eq!(report.skipped, 0);
    }
}
