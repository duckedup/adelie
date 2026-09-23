//! Runs each record's SQL against two engines and compares them to each other; the
//! record's own expected block, if any, is never consulted.

use super::render::render_row;
use super::run;
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

/// A comparison needs both sides, so a record is skipped when either engine alone would skip
/// it: stacked `onlyif`s keep `run`'s any-of meaning.
fn is_skipped(conditions: &[Condition], a_name: &str, b_name: &str) -> bool {
    run::is_skipped(conditions, a_name) || run::is_skipped(conditions, b_name)
}

fn check(
    a: &mut dyn Engine,
    b: &mut dyn Engine,
    a_name: &str,
    b_name: &str,
    record: &Record,
) -> Result<(), Failure> {
    let ra = a.run(&record.sql);
    let rb = b.run(&record.sql);
    match (&record.directive, ra, rb) {
        (_, Err(_), Err(_)) => Ok(()),
        (_, Ok(_), Err(eb)) => Err(fail(record, a_name, b_name, "ok", &format!("error: {eb}"))),
        (_, Err(ea), Ok(_)) => Err(fail(record, a_name, b_name, &format!("error: {ea}"), "ok")),
        (Directive::Query { types, sort, .. }, Ok(oa), Ok(ob)) => {
            // Rendering zips each row against `types`, so a surplus column would vanish before
            // the comparison ever saw it.
            let (wa, wb) = (
                width_mismatch(&oa, types.len()),
                width_mismatch(&ob, types.len()),
            );
            if wa.is_some() || wb.is_some() {
                let [a_text, b_text] = [wa, wb].map(|w| w.unwrap_or_else(|| "ok".to_string()));
                return Err(fail(record, a_name, b_name, &a_text, &b_text));
            }
            let ra_lines = render_outcome(&oa, types);
            let rb_lines = render_outcome(&ob, types);
            if lines_match(*sort, &ra_lines, &rb_lines) {
                Ok(())
            } else {
                Err(fail(
                    record,
                    a_name,
                    b_name,
                    &ra_lines.join("\n"),
                    &rb_lines.join("\n"),
                ))
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

fn width_mismatch(outcome: &Outcome, width: usize) -> Option<String> {
    let Outcome::Rows(rows) = outcome else {
        return None;
    };
    rows.iter()
        .find(|r| r.len() != width)
        .map(|r| format!("expected {width} columns, got {}", r.len()))
}

fn fail(record: &Record, a_name: &str, b_name: &str, a_text: &str, b_text: &str) -> Failure {
    Failure {
        line: record.line,
        sql: record.sql.clone(),
        expected: format!("{a_name}: {a_text}"),
        actual: format!("{b_name}: {b_text}"),
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
        let mut a =
            FakeEngine::new("a").answer("SELECT 1", Ok(Outcome::Rows(vec![vec![Value::Int(1)]])));
        let mut b =
            FakeEngine::new("b").answer("SELECT 1", Ok(Outcome::Rows(vec![vec![Value::Int(1)]])));
        let records = parse("query I\nSELECT 1\n----\nignored").unwrap();
        let report = diff(&mut a, &mut b, &records);
        assert!(report.ok());
        assert_eq!(report.passed, 1);
    }

    #[test]
    fn differing_query_reports_that_record() {
        let mut a =
            FakeEngine::new("a").answer("SELECT 1", Ok(Outcome::Rows(vec![vec![Value::Int(1)]])));
        let mut b =
            FakeEngine::new("b").answer("SELECT 1", Ok(Outcome::Rows(vec![vec![Value::Int(2)]])));
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

    #[test]
    fn a_surplus_column_fails_even_when_the_declared_ones_agree() {
        let mut a =
            FakeEngine::new("a").answer("SELECT 1", Ok(Outcome::Rows(vec![vec![Value::Int(1)]])));
        let mut b = FakeEngine::new("b").answer(
            "SELECT 1",
            Ok(Outcome::Rows(vec![vec![Value::Int(1), Value::Int(9)]])),
        );
        let records = parse("query I\nSELECT 1").unwrap();
        let report = diff(&mut a, &mut b, &records);
        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].actual.contains("got 2"));
    }

    #[test]
    fn stacked_onlyifs_naming_both_engines_run() {
        let mut a = FakeEngine::new("a").answer("SELECT 1", Ok(Outcome::Statement));
        let mut b = FakeEngine::new("b").answer("SELECT 1", Ok(Outcome::Statement));
        let both = parse("onlyif a\nonlyif b\nstatement ok\nSELECT 1").unwrap();
        assert_eq!(diff(&mut a, &mut b, &both).passed, 1);
        // Only one side may run it, so there is nothing to compare.
        let one = parse("onlyif a\nonlyif other\nstatement ok\nSELECT 1").unwrap();
        assert_eq!(diff(&mut a, &mut b, &one).skipped, 1);
    }
}
