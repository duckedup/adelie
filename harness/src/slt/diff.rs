//! Runs each record's SQL against two engines and compares them to each other; the
//! record's own expected block, if any, is never consulted.

use super::render::render_row;
use super::{ColType, Condition, Directive, Failure, Record, Report, lines_match};
use crate::engine::{Engine, Outcome};

pub fn diff(a: &mut dyn Engine, b: &mut dyn Engine, records: &[Record]) -> Report {
    let (a_name, b_name) = (a.name().to_string(), b.name().to_string());
    let mut report = Report::default();
    for record in records {
        if record.directive == Directive::Halt {
            break;
        }
        if is_skipped(&record.conditions, &a_name, &b_name) {
            report.skipped += 1;
            continue;
        }
        match check(a, b, &a_name, &b_name, record) {
            Ok(()) => report.passed += 1,
            Err(failure) => report.failures.push(failure),
        }
    }
    report
}

/// Skipped if a `skipif` names either engine, or an `onlyif` names one that is not in the pair.
fn is_skipped(conditions: &[Condition], a_name: &str, b_name: &str) -> bool {
    conditions.iter().any(|c| matches!(c, Condition::SkipIf(n) if n == a_name || n == b_name))
        || conditions.iter().any(|c| matches!(c, Condition::OnlyIf(n) if n != a_name && n != b_name))
}

fn check(a: &mut dyn Engine, b: &mut dyn Engine, a_name: &str, b_name: &str, record: &Record) -> Result<(), Failure> {
    let ra = a.run(&record.sql);
    let rb = b.run(&record.sql);
    match (&record.directive, ra, rb) {
        (_, Err(_), Err(_)) => Ok(()),
        (_, Ok(_), Err(eb)) => Err(fail(record, a_name, "ok", &format!("error: {eb}"))),
        (_, Err(ea), Ok(_)) => Err(fail(record, a_name, &format!("error: {ea}"), "ok")),
        (Directive::Query { types, sort, .. }, Ok(oa), Ok(ob)) => {
            let ra_lines = render_outcome(&oa, types);
            let rb_lines = render_outcome(&ob, types);
            if lines_match(*sort, &ra_lines, &rb_lines) {
                Ok(())
            } else {
                Err(fail(record, a_name, &ra_lines.join("\n"), &rb_lines.join("\n")))
            }
        }
        (_, Ok(_), Ok(_)) => Ok(()),
    }
}

fn render_outcome(outcome: &Outcome, types: &[ColType]) -> Vec<String> {
    match outcome {
        Outcome::Statement => vec!["<statement>".to_string()],
        Outcome::Rows(rows) => rows.iter().map(|r| render_row(r, types)).collect(),
    }
}

fn fail(record: &Record, a_name: &str, a_text: &str, b_text: &str) -> Failure {
    Failure {
        line: record.line,
        sql: record.sql.clone(),
        expected: format!("{a_name}: {a_text}"),
        actual: b_text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{EngineError, Value};
    use crate::fake::FakeEngine;
    use crate::slt::parse;

    #[test]
    fn agreeing_fakes_pass() {
        let mut a = FakeEngine::new("a").answer("SELECT 1", Ok(Outcome::Rows(vec![vec![Value::Int(1)]])));
        let mut b = FakeEngine::new("b").answer("SELECT 1", Ok(Outcome::Rows(vec![vec![Value::Int(1)]])));
        let records = parse("query I\nSELECT 1\n----\nignored").unwrap();
        let report = diff(&mut a, &mut b, &records);
        assert!(report.ok());
        assert_eq!(report.passed, 1);
    }

    #[test]
    fn differing_query_reports_that_record() {
        let mut a = FakeEngine::new("a").answer("SELECT 1", Ok(Outcome::Rows(vec![vec![Value::Int(1)]])));
        let mut b = FakeEngine::new("b").answer("SELECT 1", Ok(Outcome::Rows(vec![vec![Value::Int(2)]])));
        let records = parse("query I\nSELECT 1\n----\nignored").unwrap();
        let report = diff(&mut a, &mut b, &records);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].line, 1);
    }

    #[test]
    fn one_erroring_and_one_not_fails() {
        let mut a = FakeEngine::new("a").answer("bad", Err(EngineError("nope".to_string())));
        let mut b = FakeEngine::new("b").answer("bad", Ok(Outcome::Statement));
        let records = parse("statement ok\nbad").unwrap();
        let report = diff(&mut a, &mut b, &records);
        assert_eq!(report.failures.len(), 1);
    }

    #[test]
    fn skipif_either_name_is_skipped() {
        let mut a = FakeEngine::new("a").answer("SELECT 1", Ok(Outcome::Statement));
        let mut b = FakeEngine::new("b").answer("SELECT 1", Ok(Outcome::Statement));
        let records = parse("skipif b\nstatement ok\nSELECT 1").unwrap();
        let report = diff(&mut a, &mut b, &records);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.passed, 0);
    }
}
