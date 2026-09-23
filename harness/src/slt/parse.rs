//! Splits an slt file into blank-line-separated blocks, each of which becomes one `Record`.

use super::{ColType, Condition, Directive, ParseError, Record, SortMode};

/// Parses a whole slt file into records. `#` comment lines are dropped; blank lines
/// separate records; `hash-threshold` lines are accepted and produce no record.
pub fn parse(src: &str) -> Result<Vec<Record>, ParseError> {
    let mut records = Vec::new();
    for block in blocks(src) {
        if let Some(record) = parse_block(&block)? {
            records.push(record);
        }
    }
    Ok(records)
}

/// Groups numbered, non-comment lines into contiguous runs separated by blank lines.
fn blocks(src: &str) -> Vec<Vec<(usize, &str)>> {
    let mut blocks = Vec::new();
    let mut current = Vec::new();
    for (i, raw) in src.lines().enumerate() {
        let line = i + 1;
        if raw.trim().is_empty() {
            if !current.is_empty() {
                blocks.push(std::mem::take(&mut current));
            }
        } else if !raw.trim_start().starts_with('#') {
            current.push((line, raw));
        }
    }
    if !current.is_empty() {
        blocks.push(current);
    }
    blocks
}

fn parse_block(block: &[(usize, &str)]) -> Result<Option<Record>, ParseError> {
    let mut idx = 0;
    let mut conditions = Vec::new();
    while let Some(&(_, text)) = block.get(idx) {
        let t = text.trim();
        if let Some(rest) = t.strip_prefix("skipif ") {
            conditions.push(Condition::SkipIf(rest.trim().to_string()));
        } else if let Some(rest) = t.strip_prefix("onlyif ") {
            conditions.push(Condition::OnlyIf(rest.trim().to_string()));
        } else {
            break;
        }
        idx += 1;
    }
    let (header_line, header_text) = *block.get(idx).ok_or_else(|| ParseError {
        line: block[0].0,
        msg: "record has conditions but no body".to_string(),
    })?;
    let header = header_text.trim();
    let body = &block[idx + 1..];

    if header == "halt" {
        return Ok(Some(Record { line: header_line, conditions, sql: String::new(), directive: Directive::Halt }));
    }
    if header.starts_with("hash-threshold") {
        return Ok(None);
    }
    if header == "statement ok" {
        let sql = join_sql(body);
        return Ok(Some(Record { line: header_line, conditions, sql, directive: Directive::StatementOk }));
    }
    if let Some(rest) = header.strip_prefix("statement error") {
        let sql = join_sql(body);
        let directive = Directive::StatementError(optional_substring(rest));
        return Ok(Some(Record { line: header_line, conditions, sql, directive }));
    }
    if let Some(rest) = header.strip_prefix("query error") {
        let sql = join_sql(body);
        let directive = Directive::QueryError(optional_substring(rest));
        return Ok(Some(Record { line: header_line, conditions, sql, directive }));
    }
    if let Some(rest) = header.strip_prefix("query") {
        return parse_query(header_line, conditions, rest, body);
    }
    Err(ParseError { line: header_line, msg: format!("unknown directive: {header}") })
}

fn optional_substring(rest: &str) -> Option<String> {
    let s = rest.trim();
    if s.is_empty() { None } else { Some(s.to_string()) }
}

fn parse_query(
    header_line: usize,
    conditions: Vec<Condition>,
    rest: &str,
    body: &[(usize, &str)],
) -> Result<Option<Record>, ParseError> {
    let mut words = rest.trim().split_whitespace();
    let types_word = words
        .next()
        .ok_or_else(|| ParseError { line: header_line, msg: "query: missing column types".to_string() })?;
    let types = parse_types(types_word, header_line)?;

    let mut sort = SortMode::NoSort;
    let mut label = None;
    for word in words {
        match word {
            "nosort" => sort = SortMode::NoSort,
            "rowsort" => sort = SortMode::RowSort,
            "valuesort" => sort = SortMode::ValueSort,
            other => label = Some(other.to_string()),
        }
    }

    let sep = body.iter().position(|(_, t)| t.trim() == "----");
    let (sql_lines, expected_lines): (&[(usize, &str)], &[(usize, &str)]) = match sep {
        Some(p) => (&body[..p], &body[p + 1..]),
        None => (body, &[]),
    };
    let sql = join_sql(sql_lines);

    if expected_lines.len() == 1 && is_hash_line(expected_lines[0].1.trim()) {
        return Ok(Some(Record { line: header_line, conditions, sql, directive: Directive::QueryHashUnsupported }));
    }
    let expected = expected_lines.iter().map(|(_, t)| t.trim().to_string()).collect();
    let directive = Directive::Query { types, sort, label, expected };
    Ok(Some(Record { line: header_line, conditions, sql, directive }))
}

fn parse_types(word: &str, line: usize) -> Result<Vec<ColType>, ParseError> {
    word.chars()
        .map(|c| match c {
            'I' => Ok(ColType::Int),
            'R' => Ok(ColType::Real),
            'T' => Ok(ColType::Text),
            other => Err(ParseError { line, msg: format!("bad column type '{other}'") }),
        })
        .collect()
}

/// `N values hashing to <md5>`: five whitespace-separated words, the first a count.
fn is_hash_line(s: &str) -> bool {
    let words: Vec<&str> = s.split_whitespace().collect();
    words.len() == 5
        && words[0].parse::<u64>().is_ok()
        && words[1] == "values"
        && words[2] == "hashing"
        && words[3] == "to"
}

fn join_sql(lines: &[(usize, &str)]) -> String {
    lines.iter().map(|(_, t)| t.trim()).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statement_ok() {
        let records = parse("statement ok\nCREATE TABLE t (a INT)").unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].sql, "CREATE TABLE t (a INT)");
        assert_eq!(records[0].directive, Directive::StatementOk);
    }

    #[test]
    fn statement_error_with_substring() {
        let records = parse("statement error no such table\nDROP TABLE t").unwrap();
        assert_eq!(records[0].directive, Directive::StatementError(Some("no such table".to_string())));
    }

    #[test]
    fn statement_error_without_substring() {
        let records = parse("statement error\nDROP TABLE t").unwrap();
        assert_eq!(records[0].directive, Directive::StatementError(None));
    }

    #[test]
    fn query_with_results() {
        let src = "query IT rowsort\nSELECT a, b FROM t\n----\n1 x\n2 y";
        let records = parse(src).unwrap();
        match &records[0].directive {
            Directive::Query { types, sort, expected, label } => {
                assert_eq!(*types, vec![ColType::Int, ColType::Text]);
                assert_eq!(*sort, SortMode::RowSort);
                assert_eq!(*expected, vec!["1 x".to_string(), "2 y".to_string()]);
                assert_eq!(*label, None);
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn query_error() {
        let records = parse("query error boom\nSELECT 1/0").unwrap();
        assert_eq!(records[0].directive, Directive::QueryError(Some("boom".to_string())));
    }

    #[test]
    fn conditions_stack() {
        let src = "skipif duckdb\nonlyif adelie\nstatement ok\nSELECT 1";
        let records = parse(src).unwrap();
        assert_eq!(
            records[0].conditions,
            vec![Condition::SkipIf("duckdb".to_string()), Condition::OnlyIf("adelie".to_string())]
        );
    }

    #[test]
    fn multi_line_sql_joins_with_newlines() {
        let records = parse("statement ok\nCREATE TABLE t (\na INT\n)").unwrap();
        assert_eq!(records[0].sql, "CREATE TABLE t (\na INT\n)");
    }

    #[test]
    fn query_with_no_separator_expects_zero_rows() {
        let records = parse("query I\nSELECT a FROM empty").unwrap();
        match &records[0].directive {
            Directive::Query { expected, .. } => assert!(expected.is_empty()),
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn comment_lines_are_dropped() {
        let src = "# a comment\nstatement ok\n# mid-record comment\nSELECT 1";
        let records = parse(src).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].sql, "SELECT 1");
    }

    #[test]
    fn hash_threshold_is_accepted_and_ignored() {
        let src = "hash-threshold 8\n\nstatement ok\nSELECT 1";
        let records = parse(src).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].directive, Directive::StatementOk);
    }

    #[test]
    fn hash_block_is_unsupported() {
        let src = "query I\nSELECT a FROM t\n----\n3 values hashing to abcd1234";
        let records = parse(src).unwrap();
        assert_eq!(records[0].directive, Directive::QueryHashUnsupported);
    }

    #[test]
    fn bad_type_char_reports_the_query_line() {
        let src = "\n\nquery X\nSELECT 1";
        let err = parse(src).unwrap_err();
        assert_eq!(err.line, 3);
    }

    #[test]
    fn unknown_directive_is_a_parse_error() {
        let err = parse("bogus\nSELECT 1").unwrap_err();
        assert_eq!(err.line, 1);
    }

    #[test]
    fn halt_is_a_record() {
        let records = parse("statement ok\nSELECT 1\n\nhalt").unwrap();
        assert_eq!(records[1].directive, Directive::Halt);
    }
}
