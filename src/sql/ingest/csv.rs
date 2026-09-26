//! COPY's CSV reader (SPEC §8): a hand-rolled RFC 4180 state machine over bytes, feeding a
//! `RowBuilder` batch at a time. `header = false` maps fields positionally.

use std::io::BufRead;

use crate::exec::{Batch, Field};

use super::{CsvOptions, IngestError, RowBuilder};

/// Reads every record from `input` into batches of `batch_rows` rows, calling `sink` for
/// each. Returns the total number of data rows read (not counting the header).
pub fn read_csv(
    input: &mut dyn BufRead,
    opts: &CsvOptions,
    fields: &[Field],
    batch_rows: usize,
    sink: &mut dyn FnMut(Batch) -> Result<(), IngestError>,
) -> Result<u64, IngestError> {
    let mut builder = RowBuilder::new(fields);

    let mapping: Vec<usize> = if opts.header {
        match read_record(input, opts.delimiter, 0)? {
            None => return Ok(0),
            Some(names) => header_mapping(&builder, names)?,
        }
    } else {
        builder.data_columns().to_vec()
    };

    let mut row: u64 = 0;
    while let Some(record) = read_record(input, opts.delimiter, row + 1)? {
        if record.len() != mapping.len() {
            return Err(IngestError::Malformed {
                row: row + 1,
                message: format!("expected {} fields, found {}", mapping.len(), record.len()),
            });
        }
        for (cell, &col) in record.iter().zip(&mapping) {
            if let Some(text) = cell {
                builder.set_text(col, text)?;
            }
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

/// Maps each header name to its field index via `RowBuilder::index_of`; an unquoted-empty
/// header name maps to `""`, which is `UnknownColumn` unless the table has such a field.
fn header_mapping(
    builder: &RowBuilder,
    names: Vec<Option<String>>,
) -> Result<Vec<usize>, IngestError> {
    names
        .into_iter()
        .map(|name| {
            let name = name.unwrap_or_default();
            builder.index_of(&name).ok_or(IngestError::UnknownColumn {
                column: name,
                row: 0,
            })
        })
        .collect()
}

/// One CSV field: `None` is an unquoted empty field (NULL for every type, including
/// STRING); `Some(text)` is text to hand to `RowBuilder::set_text` (`""` when quoted-empty).
type RawField = Option<String>;

/// Reads one record, transparently skipping fully empty lines (DuckDB's convention),
/// including a trailing blank line at end of file. `None` is a clean end of input.
fn read_record(
    input: &mut dyn BufRead,
    delimiter: u8,
    row: u64,
) -> Result<Option<Vec<RawField>>, IngestError> {
    loop {
        let mut fields: Vec<RawField> = Vec::new();
        let mut buf: Vec<u8> = Vec::new();
        let mut quoted = false;
        let mut in_quotes = false;
        let mut line_has_content = false;

        loop {
            let Some(b) = read_byte(input, row)? else {
                if !line_has_content && buf.is_empty() && fields.is_empty() {
                    return Ok(None);
                }
                fields.push(finish_field(&buf, quoted, row)?);
                return Ok(Some(fields));
            };

            if in_quotes {
                line_has_content = true;
                if b == b'"' {
                    if peek_byte(input, row)? == Some(b'"') {
                        read_byte(input, row)?;
                        buf.push(b'"');
                    } else {
                        in_quotes = false;
                    }
                } else {
                    buf.push(b);
                }
                continue;
            }

            match b {
                b'\n' => {
                    if !line_has_content && fields.is_empty() && buf.is_empty() {
                        break;
                    }
                    fields.push(finish_field(&buf, quoted, row)?);
                    return Ok(Some(fields));
                }
                b'\r' if peek_byte(input, row)? == Some(b'\n') => {
                    read_byte(input, row)?;
                    if !line_has_content && fields.is_empty() && buf.is_empty() {
                        break;
                    }
                    fields.push(finish_field(&buf, quoted, row)?);
                    return Ok(Some(fields));
                }
                // A lone CR (no following LF) inside an unquoted field is data, not a
                // terminator.
                b'\r' => {
                    line_has_content = true;
                    buf.push(b'\r');
                }
                b'"' if buf.is_empty() && !quoted => {
                    line_has_content = true;
                    quoted = true;
                    in_quotes = true;
                }
                _ if b == delimiter => {
                    line_has_content = true;
                    fields.push(finish_field(&buf, quoted, row)?);
                    buf.clear();
                    quoted = false;
                }
                _ => {
                    line_has_content = true;
                    buf.push(b);
                }
            }
        }
        // A fully empty line: restart and read the next record instead of returning one.
    }
}

fn finish_field(buf: &[u8], quoted: bool, row: u64) -> Result<RawField, IngestError> {
    if buf.is_empty() && !quoted {
        return Ok(None);
    }
    let text = std::str::from_utf8(buf)
        .map_err(|_| IngestError::Malformed {
            row,
            message: "invalid UTF-8".to_string(),
        })?
        .to_string();
    Ok(Some(text))
}

fn read_byte(input: &mut dyn BufRead, row: u64) -> Result<Option<u8>, IngestError> {
    let buf = input.fill_buf().map_err(|e| io_err(e, row))?;
    if buf.is_empty() {
        return Ok(None);
    }
    let b = buf[0];
    input.consume(1);
    Ok(Some(b))
}

fn peek_byte(input: &mut dyn BufRead, row: u64) -> Result<Option<u8>, IngestError> {
    let buf = input.fill_buf().map_err(|e| io_err(e, row))?;
    Ok(buf.first().copied())
}

fn io_err(e: std::io::Error, row: u64) -> IngestError {
    IngestError::Malformed {
        row,
        message: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::types::{DataType, Value};

    fn field(name: &str, ty: DataType) -> Field {
        Field {
            name: name.to_string(),
            ty,
        }
    }

    fn run(
        input: &str,
        opts: &CsvOptions,
        fields: &[Field],
        batch_rows: usize,
    ) -> (u64, Vec<Batch>) {
        let mut cursor = Cursor::new(input.as_bytes());
        let mut batches = Vec::new();
        let total = read_csv(&mut cursor, opts, fields, batch_rows, &mut |b| {
            batches.push(b);
            Ok(())
        })
        .unwrap();
        (total, batches)
    }

    fn col_values(batches: &[Batch], col: usize) -> Vec<Value> {
        batches
            .iter()
            .flat_map(|b| (0..b.rows()).map(move |i| b.column(col).get(i)))
            .collect()
    }

    #[test]
    fn quotes_delimiters_newlines_escapes_and_crlf() {
        let fields = vec![field("a", DataType::String), field("b", DataType::String)];
        let input = "a,b\r\n\"x,y\",\"line1\r\nline2\"\r\n\"say \"\"hi\"\"\",plain\r\n";
        let (total, batches) = run(input, &CsvOptions::default(), &fields, 10);
        assert_eq!(total, 2);
        assert_eq!(
            col_values(&batches, 0),
            vec![
                Value::String("x,y".to_string()),
                Value::String("say \"hi\"".to_string()),
            ]
        );
        assert_eq!(
            col_values(&batches, 1),
            vec![
                Value::String("line1\r\nline2".to_string()),
                Value::String("plain".to_string()),
            ]
        );
    }

    #[test]
    fn last_record_may_omit_its_trailing_newline() {
        let fields = vec![field("a", DataType::Int64)];
        let (total, batches) = run("a\n1\n2", &CsvOptions::default(), &fields, 10);
        assert_eq!(total, 2);
        assert_eq!(
            col_values(&batches, 0),
            vec![Value::Int64(1), Value::Int64(2)]
        );
    }

    #[test]
    fn unquoted_empty_is_null_for_string_and_int64_quoted_empty_is_empty_string_for_string() {
        let fields = vec![field("s", DataType::String), field("n", DataType::Int64)];
        let (_, batches) = run("s,n\n\"\",\n", &CsvOptions::default(), &fields, 10);
        assert_eq!(col_values(&batches, 0), vec![Value::String(String::new())]);
        assert_eq!(col_values(&batches, 1), vec![Value::Null]);
    }

    #[test]
    fn header_reorders_columns() {
        let fields = vec![field("a", DataType::Int64), field("b", DataType::Int64)];
        let (_, batches) = run("b,a\n1,2\n", &CsvOptions::default(), &fields, 10);
        assert_eq!(col_values(&batches, 0), vec![Value::Int64(2)]);
        assert_eq!(col_values(&batches, 1), vec![Value::Int64(1)]);
    }

    #[test]
    fn header_naming_an_unknown_column_errors() {
        let fields = vec![field("a", DataType::Int64)];
        let mut cursor = Cursor::new("a,z\n1,2\n".as_bytes());
        let err = read_csv(
            &mut cursor,
            &CsvOptions::default(),
            &fields,
            10,
            &mut |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(
            err,
            IngestError::UnknownColumn {
                column: "z".to_string(),
                row: 0,
            }
        );
    }

    #[test]
    fn header_false_field_count_mismatch_is_malformed_naming_the_row() {
        let fields = vec![field("a", DataType::Int64), field("b", DataType::Int64)];
        let opts = CsvOptions {
            header: false,
            delimiter: b',',
        };
        let mut cursor = Cursor::new("1,2\n3\n".as_bytes());
        let err = read_csv(&mut cursor, &opts, &fields, 10, &mut |_| Ok(())).unwrap_err();
        assert_eq!(
            err,
            IngestError::Malformed {
                row: 2,
                message: "expected 2 fields, found 1".to_string(),
            }
        );
    }

    #[test]
    fn pipe_delimiter() {
        let fields = vec![field("a", DataType::Int64), field("b", DataType::Int64)];
        let opts = CsvOptions {
            header: true,
            delimiter: b'|',
        };
        let (_, batches) = run("a|b\n1|2\n", &opts, &fields, 10);
        assert_eq!(col_values(&batches, 0), vec![Value::Int64(1)]);
        assert_eq!(col_values(&batches, 1), vec![Value::Int64(2)]);
    }

    #[test]
    fn batch_rows_of_two_over_five_rows_gives_three_batches() {
        let fields = vec![field("a", DataType::Int64)];
        let (total, batches) = run("a\n1\n2\n3\n4\n5\n", &CsvOptions::default(), &fields, 2);
        assert_eq!(total, 5);
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].rows(), 2);
        assert_eq!(batches[1].rows(), 2);
        assert_eq!(batches[2].rows(), 1);
    }

    #[test]
    fn an_unparseable_int_cell_lands_in_the_declared_companion() {
        let fields = vec![
            field("x", DataType::Int64),
            field("x::string", DataType::String),
        ];
        let (_, batches) = run("x\nabc\n", &CsvOptions::default(), &fields, 10);
        assert_eq!(col_values(&batches, 0), vec![Value::Null]);
        assert_eq!(
            col_values(&batches, 1),
            vec![Value::String("abc".to_string())]
        );
    }
}
