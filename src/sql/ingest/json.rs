//! COPY's NDJSON reader (SPEC §8, §16.3): one hand-rolled JSON parser per line, then
//! flattening nested objects into dotted column names on the way into a `RowBuilder`.

use std::io::BufRead;

use crate::exec::{Batch, Field};
use crate::types::{DataType, Value, companion_name};

use super::{IngestError, RowBuilder};

/// A JSON value, parsed from one NDJSON line. `Number` keeps its source text, so it can be
/// typed exactly for the target column instead of round-tripping through `f64`.
#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

/// Nested objects and arrays deeper than this are `Malformed`, which keeps the recursive
/// parser and flattener off the stack no matter how deep the input claims to be.
const MAX_JSON_DEPTH: usize = 64;

/// Reads one JSON object per line into batches of `batch_rows` rows, calling `sink` for
/// each. Returns the total number of data rows read; blank lines are skipped.
pub fn read_ndjson(
    input: &mut dyn BufRead,
    fields: &[Field],
    batch_rows: usize,
    sink: &mut dyn FnMut(Batch) -> Result<(), IngestError>,
) -> Result<u64, IngestError> {
    let mut builder = RowBuilder::new(fields);
    let mut line = String::new();
    let mut row: u64 = 0;

    loop {
        line.clear();
        let n = input
            .read_line(&mut line)
            .map_err(|e| IngestError::Malformed {
                row: row + 1,
                message: e.to_string(),
            })?;
        if n == 0 {
            break;
        }
        let Some(value) = parse_line(&line, row + 1)? else {
            continue; // blank line
        };
        let Json::Object(members) = value else {
            return Err(IngestError::Malformed {
                row: row + 1,
                message: "top-level JSON value must be an object".to_string(),
            });
        };
        for (key, v) in &members {
            set_leaf_or_descend(&mut builder, fields, "", key, v, row + 1)?;
        }
        builder.end_row()?;
        row += 1;
        if builder.rows() >= batch_rows {
            sink(builder.finish()?)?;
        }
    }
    if builder.rows() > 0 {
        sink(builder.finish()?)?;
    }
    Ok(row)
}

/// A leaf: `column, v` is the dotted path so far and the JSON value it points to; `key` is
/// the current step. Whitespace-only lines are `None`.
fn parse_line(line: &str, row: u64) -> Result<Option<Json>, IngestError> {
    let trimmed = line.trim_matches(|c: char| c.is_ascii_whitespace());
    if trimmed.is_empty() {
        return Ok(None);
    }
    let mut parser = Parser {
        bytes: trimmed.as_bytes(),
        pos: 0,
    };
    let value = parser
        .parse_value(0)
        .map_err(|message| malformed(row, message))?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(malformed(row, "trailing data after JSON value"));
    }
    Ok(Some(value))
}

fn malformed(row: u64, message: &str) -> IngestError {
    IngestError::Malformed {
        row,
        message: message.to_string(),
    }
}

/// SPEC §16.3: the literal key is checked against the field list *before* descending, so a
/// key like `service.name` addresses that column directly rather than nesting `service`.
fn set_leaf_or_descend(
    builder: &mut RowBuilder,
    fields: &[Field],
    prefix: &str,
    key: &str,
    value: &Json,
    row: u64,
) -> Result<(), IngestError> {
    let name = if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    };
    if let Some(idx) = builder.index_of(&name) {
        return set_leaf(builder, fields, idx, value, row);
    }
    match value {
        Json::Object(members) => {
            for (k, v) in members {
                set_leaf_or_descend(builder, fields, &name, k, v, row)?;
            }
            Ok(())
        }
        _ => Err(IngestError::UnknownColumn { column: name, row }),
    }
}

/// `null` leaves the column unset (NULL); everything else follows the leaf-mapping rules
/// (SPEC §16.3): bools via `set_value`, numbers and strings via `set_text` so they type
/// against the target column, arrays into LIST columns or their companion, objects never.
fn set_leaf(
    builder: &mut RowBuilder,
    fields: &[Field],
    idx: usize,
    value: &Json,
    row: u64,
) -> Result<(), IngestError> {
    match value {
        Json::Null => Ok(()),
        Json::Bool(b) => builder.set_value(idx, Value::Bool(*b)),
        Json::Number(text) => builder.set_text(idx, text),
        Json::String(s) => builder.set_text(idx, s),
        Json::Array(items) => set_array_leaf(builder, fields, idx, items, row),
        Json::Object(_) => Err(IngestError::DoesNotFit {
            column: fields[idx].name.clone(),
            value: format!("{value:?}"),
            row,
        }),
    }
}

/// An array into a LIST column types each element by the element type; anywhere else it is
/// routed to the declared companion as JSON text, or `DoesNotFit`.
fn set_array_leaf(
    builder: &mut RowBuilder,
    fields: &[Field],
    idx: usize,
    items: &[Json],
    row: u64,
) -> Result<(), IngestError> {
    if let DataType::List(lt) = &fields[idx].ty {
        let elem_ty = lt.element().clone();
        let mut values = Vec::with_capacity(items.len());
        for item in items {
            values.push(json_to_element(item, &elem_ty, row)?);
        }
        return builder.set_value(idx, Value::List(values));
    }
    let text = to_json_text(&Json::Array(items.to_vec()));
    let comp_name = companion_name(&fields[idx].name);
    match builder.index_of(&comp_name) {
        Some(comp) => {
            builder.set_value(idx, Value::Null)?;
            builder.set_text(comp, &text)
        }
        None => Err(IngestError::DoesNotFit {
            column: fields[idx].name.clone(),
            value: text,
            row,
        }),
    }
}

/// An array element, typed against a LIST's element type the same way a scalar leaf would
/// be: bools directly, everything else as text (exact for DECIMAL, never through `f64`).
fn json_to_element(item: &Json, elem_ty: &DataType, row: u64) -> Result<Value, IngestError> {
    match item {
        Json::Null => Ok(Value::Null),
        Json::Bool(b) => Ok(Value::Bool(*b)),
        Json::Number(text) => {
            Value::from_text(text, elem_ty).ok_or_else(|| IngestError::Malformed {
                row,
                message: format!("{text} does not fit list element type {elem_ty}"),
            })
        }
        Json::String(s) => Value::from_text(s, elem_ty).ok_or_else(|| IngestError::Malformed {
            row,
            message: format!("{s:?} does not fit list element type {elem_ty}"),
        }),
        Json::Array(_) | Json::Object(_) => Err(IngestError::Malformed {
            row,
            message: "list elements must be scalar".to_string(),
        }),
    }
}

/// Round-trips a parsed `Json` value back to JSON text, for routing an array that does not
/// fit its column to the declared companion (SPEC §16.3).
fn to_json_text(value: &Json) -> String {
    match value {
        Json::Null => "null".to_string(),
        Json::Bool(b) => b.to_string(),
        Json::Number(text) => text.clone(),
        Json::String(s) => quote_json_string(s),
        Json::Array(items) => {
            let parts: Vec<String> = items.iter().map(to_json_text).collect();
            format!("[{}]", parts.join(","))
        }
        Json::Object(members) => {
            let parts: Vec<String> = members
                .iter()
                .map(|(k, v)| format!("{}:{}", quote_json_string(k), to_json_text(v)))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
    }
}

fn quote_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A hand-rolled recursive-descent JSON parser over one line's bytes. Depth is threaded
/// through explicitly and checked before every recursive call, so it never overflows the
/// stack no matter how deeply nested a hostile line claims to be.
struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<Json, &'static str> {
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.parse_object(depth),
            Some(b'[') => self.parse_array(depth),
            Some(b'"') => self.parse_string().map(Json::String),
            Some(b't') | Some(b'f') => self.parse_bool(),
            Some(b'n') => self.parse_null(),
            Some(b'-') | Some(b'0'..=b'9') => self.parse_number(),
            _ => Err("unexpected token"),
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<Json, &'static str> {
        if depth >= MAX_JSON_DEPTH {
            return Err("max nesting depth exceeded");
        }
        self.bump();
        let mut members = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.bump();
            return Ok(Json::Object(members));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err("expected a string key");
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.bump() != Some(b':') {
                return Err("expected ':'");
            }
            self.skip_ws();
            let value = self.parse_value(depth + 1)?;
            members.push((key, value));
            self.skip_ws();
            match self.bump() {
                Some(b',') => {}
                Some(b'}') => return Ok(Json::Object(members)),
                _ => return Err("expected ',' or '}'"),
            }
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<Json, &'static str> {
        if depth >= MAX_JSON_DEPTH {
            return Err("max nesting depth exceeded");
        }
        self.bump();
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.bump();
            return Ok(Json::Array(items));
        }
        loop {
            let value = self.parse_value(depth + 1)?;
            items.push(value);
            self.skip_ws();
            match self.bump() {
                Some(b',') => self.skip_ws(),
                Some(b']') => return Ok(Json::Array(items)),
                _ => return Err("expected ',' or ']'"),
            }
        }
    }

    fn parse_string(&mut self) -> Result<String, &'static str> {
        self.bump(); // the opening quote, already peeked by the caller
        let mut out = String::new();
        let mut seg_start = self.pos;
        loop {
            match self.peek() {
                None => return Err("unterminated string"),
                Some(b'"') => {
                    out.push_str(self.str_slice(seg_start, self.pos));
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    out.push_str(self.str_slice(seg_start, self.pos));
                    self.pos += 1;
                    self.parse_escape(&mut out)?;
                    seg_start = self.pos;
                }
                Some(b) if b < 0x20 => return Err("control character in string"),
                Some(_) => self.pos += 1,
            }
        }
    }

    /// `a..b` is always at a char boundary: every cut point is an ASCII quote or backslash,
    /// which can never fall inside a multi-byte UTF-8 sequence.
    fn str_slice(&self, a: usize, b: usize) -> &str {
        std::str::from_utf8(&self.bytes[a..b]).expect("cut at an ASCII boundary")
    }

    fn parse_escape(&mut self, out: &mut String) -> Result<(), &'static str> {
        match self.bump().ok_or("unterminated escape")? {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{8}'),
            b'f' => out.push('\u{c}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => self.parse_unicode_escape(out)?,
            _ => return Err("invalid escape"),
        }
        Ok(())
    }

    /// `\uXXXX`, joining a high/low surrogate pair into one scalar. A lone surrogate is
    /// rejected rather than producing an invalid `char`.
    fn parse_unicode_escape(&mut self, out: &mut String) -> Result<(), &'static str> {
        let cp = self.parse_hex4()?;
        if (0xDC00..=0xDFFF).contains(&cp) {
            return Err("unpaired low surrogate");
        }
        if !(0xD800..=0xDBFF).contains(&cp) {
            out.push(char::from_u32(cp).ok_or("invalid unicode escape")?);
            return Ok(());
        }
        if self.bump() != Some(b'\\') || self.bump() != Some(b'u') {
            return Err("unpaired high surrogate");
        }
        let low = self.parse_hex4()?;
        if !(0xDC00..=0xDFFF).contains(&low) {
            return Err("unpaired high surrogate");
        }
        let scalar = 0x10000 + ((cp - 0xD800) << 10) + (low - 0xDC00);
        out.push(char::from_u32(scalar).ok_or("invalid surrogate pair")?);
        Ok(())
    }

    fn parse_hex4(&mut self) -> Result<u32, &'static str> {
        let mut v: u32 = 0;
        for _ in 0..4 {
            let d = (self.bump().ok_or("truncated unicode escape")? as char)
                .to_digit(16)
                .ok_or("invalid hex digit")?;
            v = v * 16 + d;
        }
        Ok(v)
    }

    fn parse_number(&mut self) -> Result<Json, &'static str> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        self.consume_digits()?;
        if self.peek() == Some(b'.') {
            self.pos += 1;
            self.consume_digits()?;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            self.consume_digits()?;
        }
        Ok(Json::Number(self.str_slice(start, self.pos).to_string()))
    }

    fn consume_digits(&mut self) -> Result<(), &'static str> {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if self.pos == start {
            return Err("invalid number");
        }
        Ok(())
    }

    fn parse_bool(&mut self) -> Result<Json, &'static str> {
        if self.bytes[self.pos..].starts_with(b"true") {
            self.pos += 4;
            Ok(Json::Bool(true))
        } else if self.bytes[self.pos..].starts_with(b"false") {
            self.pos += 5;
            Ok(Json::Bool(false))
        } else {
            Err("invalid literal")
        }
    }

    fn parse_null(&mut self) -> Result<Json, &'static str> {
        if self.bytes[self.pos..].starts_with(b"null") {
            self.pos += 4;
            Ok(Json::Null)
        } else {
            Err("invalid literal")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::types::Decimal;

    fn field(name: &str, ty: DataType) -> Field {
        Field {
            name: name.to_string(),
            ty,
        }
    }

    fn run(input: &str, fields: &[Field]) -> (u64, Vec<Batch>) {
        let mut cursor = Cursor::new(input.as_bytes());
        let mut batches = Vec::new();
        let total = read_ndjson(&mut cursor, fields, 10, &mut |b| {
            batches.push(b);
            Ok(())
        })
        .unwrap();
        (total, batches)
    }

    fn value(batches: &[Batch], col: usize, row: usize) -> Value {
        let mut remaining = row;
        for b in batches {
            if remaining < b.rows() {
                return b.column(col).get(remaining);
            }
            remaining -= b.rows();
        }
        panic!("row {row} out of range");
    }

    #[test]
    fn nested_object_flattens_into_a_dotted_column() {
        let fields = vec![field("attributes.http.route", DataType::String)];
        let (total, batches) = run(r#"{"attributes": {"http": {"route": "/a"}}}"#, &fields);
        assert_eq!(total, 1);
        assert_eq!(value(&batches, 0, 0), Value::String("/a".to_string()));
    }

    #[test]
    fn a_literal_dotted_key_addresses_the_column_directly() {
        let fields = vec![field("service.name", DataType::String)];
        let (_, batches) = run(r#"{"service.name": "checkout"}"#, &fields);
        assert_eq!(value(&batches, 0, 0), Value::String("checkout".to_string()));
    }

    #[test]
    fn a_decimal_from_a_json_number_is_exact() {
        let fields = vec![field("price", DataType::decimal(10, 2).unwrap())];
        let (_, batches) = run(r#"{"price": 12.34}"#, &fields);
        assert_eq!(
            value(&batches, 0, 0),
            Value::Decimal(Decimal::new(1234, 2).unwrap())
        );
    }

    #[test]
    fn a_timestamp_from_a_string() {
        let fields = vec![field("ts", DataType::Timestamp)];
        let (_, batches) = run(r#"{"ts": "2024-01-01T00:00:00Z"}"#, &fields);
        assert_eq!(
            value(&batches, 0, 0),
            Value::Timestamp(1_704_067_200_000_000_000)
        );
    }

    #[test]
    fn a_list_from_a_json_array() {
        let fields = vec![field("tags", DataType::list(DataType::Int64).unwrap())];
        let (_, batches) = run(r#"{"tags": [1, 2, 3]}"#, &fields);
        assert_eq!(
            value(&batches, 0, 0),
            Value::List(vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)])
        );
    }

    #[test]
    fn multibyte_and_a_surrogate_pair_decode() {
        let fields = vec![field("s", DataType::String)];
        // The JSON text below spells the accented letter and the emoji as escapes: a
        // single code point, then a high/low surrogate pair, inside one JSON string.
        let line = "{\"s\": \"caf\\u00e9 \\ud83d\\ude00\"}";
        let (_, batches) = run(line, &fields);
        let expected = format!("caf{} {}", '\u{e9}', '\u{1f600}');
        assert_eq!(value(&batches, 0, 0), Value::String(expected));
    }

    #[test]
    fn deeply_nested_object_is_malformed_for_depth() {
        let mut line = String::new();
        for _ in 0..100 {
            line.push_str(r#"{"a":"#);
        }
        line.push('1');
        for _ in 0..100 {
            line.push('}');
        }
        let fields = vec![field("a", DataType::Int64)];
        let mut cursor = Cursor::new(line.as_bytes());
        let err = read_ndjson(&mut cursor, &fields, 10, &mut |_| Ok(())).unwrap_err();
        assert!(matches!(err, IngestError::Malformed { .. }));
    }

    #[test]
    fn a_top_level_array_is_malformed() {
        let fields = vec![field("a", DataType::Int64)];
        let mut cursor = Cursor::new(b"[1,2,3]\n".as_slice());
        let err = read_ndjson(&mut cursor, &fields, 10, &mut |_| Ok(())).unwrap_err();
        assert!(matches!(err, IngestError::Malformed { .. }));
    }

    #[test]
    fn a_blank_line_is_skipped() {
        let fields = vec![field("a", DataType::Int64)];
        let (total, batches) = run("\n{\"a\": 1}\n   \n{\"a\": 2}\n", &fields);
        assert_eq!(total, 2);
        assert_eq!(value(&batches, 0, 0), Value::Int64(1));
        assert_eq!(value(&batches, 0, 1), Value::Int64(2));
    }

    #[test]
    fn an_unknown_dotted_key_is_unknown_column() {
        let fields = vec![field("a", DataType::Int64)];
        let mut cursor = Cursor::new(br#"{"b": {"c": 1}}"#.as_slice());
        let err = read_ndjson(&mut cursor, &fields, 10, &mut |_| Ok(())).unwrap_err();
        assert_eq!(
            err,
            IngestError::UnknownColumn {
                column: "b.c".to_string(),
                row: 1,
            }
        );
    }

    #[test]
    fn an_array_into_a_non_list_column_routes_to_the_companion_as_json_text() {
        let fields = vec![
            field("a", DataType::Int64),
            field("a::string", DataType::String),
        ];
        let (_, batches) = run(r#"{"a": [1, 2]}"#, &fields);
        assert_eq!(value(&batches, 0, 0), Value::Null);
        assert_eq!(value(&batches, 1, 0), Value::String("[1,2]".to_string()));
    }
}
