//! A hand-rolled, iterative lexer (SPEC §8): no recursion anywhere, so hostile input cannot
//! blow the stack here even before the parser's own depth cap applies. Keywords are
//! case-insensitive and are not a separate token kind — `Token::Ident` already lowercases an
//! unquoted word, and the parser compares that text against the (lowercase) keywords it
//! expects at each point, which lets words like `key` or `format` also serve as plain names.

use super::parser::ParseError;

/// One lexical token. `Number` and `Str` keep their resolved text; `Ident` is lowercased
/// unquoted source text (root D0016 wants the *parser's* numbers as source text, done at
/// this layer since the lexer already holds it).
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// An unquoted word, lowercased. May be a keyword — the parser decides.
    Ident(String),
    /// A `"..."` identifier: case and characters preserved, `""` escaping `"`.
    QuotedIdent(String),
    /// An integer or decimal literal, source text verbatim (e.g. `"1.50"`, `".5"`, `"1e3"`).
    Number(String),
    /// A `'...'` string literal's contents, with `''` already resolved to `'`.
    Str(String),
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    /// `||`
    Concat,
    /// `::`
    DoubleColon,
    LParen,
    RParen,
    /// `[`, only used by `type_name`'s `('[' ']')*` array-dimension suffix.
    LBracket,
    /// `]`
    RBracket,
    Comma,
    Dot,
    Semicolon,
    Eof,
}

/// A token plus the byte offset of its first character in the source. `ParseError` turns an
/// offset into a line and column.
#[derive(Debug, Clone, PartialEq)]
pub struct SpannedToken {
    pub token: Token,
    pub offset: usize,
}

/// Scans `source` into a token stream ending with one `Token::Eof`. Iterative: a `while`
/// loop over byte offsets, no recursive descent, so it cannot overflow the stack no matter
/// how long or malformed `source` is.
pub fn tokenize(source: &str) -> Result<Vec<SpannedToken>, ParseError> {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut out = Vec::new();
    let mut i = 0usize;

    while i < len {
        // Decoded properly (not `bytes[i] as char`) so a multi-byte lead byte classifies as
        // itself, not as the unrelated Latin-1 codepoint sharing its numeric value.
        let c = source[i..].chars().next().unwrap();

        if c == ' ' || c == '\t' || c == '\n' || c == '\r' {
            i += 1;
            continue;
        }
        if c == '-' && i + 1 < len && bytes[i + 1] as char == '-' {
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < len && bytes[i + 1] as char == '*' {
            let start = i;
            i += 2;
            let mut closed = false;
            while i + 1 < len {
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    closed = true;
                    break;
                }
                i += 1;
            }
            if !closed {
                return Err(ParseError::new(source, start, "unterminated block comment"));
            }
            continue;
        }

        let start = i;
        if c == '"' {
            let (text, next) = scan_quoted(source, start, b'"')?;
            out.push(SpannedToken {
                token: Token::QuotedIdent(text),
                offset: start,
            });
            i = next;
            continue;
        }
        if c == '\'' {
            let (text, next) = scan_quoted(source, start, b'\'')?;
            out.push(SpannedToken {
                token: Token::Str(text),
                offset: start,
            });
            i = next;
            continue;
        }
        if c.is_ascii_digit()
            || (c == '.' && i + 1 < len && (bytes[i + 1] as char).is_ascii_digit())
        {
            let next = scan_number(bytes, i);
            let text = source[start..next].to_string();
            out.push(SpannedToken {
                token: Token::Number(text),
                offset: start,
            });
            i = next;
            continue;
        }
        if is_ident_start(c) {
            let mut next = i + c.len_utf8();
            while next < len {
                let ch = source[next..].chars().next().unwrap();
                if is_ident_continue(ch) {
                    next += ch.len_utf8();
                } else {
                    break;
                }
            }
            let text = source[start..next].to_lowercase();
            out.push(SpannedToken {
                token: Token::Ident(text),
                offset: start,
            });
            i = next;
            continue;
        }

        let (token, width) = scan_operator(source, i)
            .ok_or_else(|| ParseError::new(source, start, format!("unexpected character '{c}'")))?;
        out.push(SpannedToken {
            token,
            offset: start,
        });
        i += width;
    }

    out.push(SpannedToken {
        token: Token::Eof,
        offset: len,
    });
    Ok(out)
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident_continue(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Consumes digits, an optional `.digits` fraction and an optional `[eE][+-]?digits`
/// exponent, starting at a digit or a `.` known to be followed by a digit. Returns the byte
/// offset just past the number.
fn scan_number(bytes: &[u8], start: usize) -> usize {
    let len = bytes.len();
    let mut i = start;
    while i < len && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i < len && bytes[i] == b'.' {
        i += 1;
        while i < len && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < len && (bytes[i] == b'e' || bytes[i] == b'E') {
        let mut j = i + 1;
        if j < len && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        let digits_start = j;
        while j < len && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j > digits_start {
            i = j;
        }
    }
    i
}

/// Scans a `quote`-delimited literal starting at `source[start]`, resolving a doubled quote
/// to one literal quote. Returns the resolved text and the offset just past the closing
/// quote.
fn scan_quoted(source: &str, start: usize, quote: u8) -> Result<(String, usize), ParseError> {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = start + 1;
    let mut text = String::new();
    loop {
        if i >= len {
            let what = if quote == b'"' {
                "quoted identifier"
            } else {
                "string literal"
            };
            return Err(ParseError::new(
                source,
                start,
                format!("unterminated {what}"),
            ));
        }
        if bytes[i] == quote {
            if i + 1 < len && bytes[i + 1] == quote {
                text.push(quote as char);
                i += 2;
                continue;
            }
            return Ok((text, i + 1));
        }
        let ch = source[i..].chars().next().unwrap();
        text.push(ch);
        i += ch.len_utf8();
    }
}

/// Matches one operator token at `source[i]`, longest match first. Returns its byte width.
fn scan_operator(source: &str, i: usize) -> Option<(Token, usize)> {
    let rest = &source[i..];
    let mut chars = rest.chars();
    let c0 = chars.next()?;
    let c1 = chars.next();
    match (c0, c1) {
        ('<', Some('>')) => Some((Token::NotEq, 2)),
        ('<', Some('=')) => Some((Token::LtEq, 2)),
        ('>', Some('=')) => Some((Token::GtEq, 2)),
        ('!', Some('=')) => Some((Token::NotEq, 2)),
        ('|', Some('|')) => Some((Token::Concat, 2)),
        (':', Some(':')) => Some((Token::DoubleColon, 2)),
        ('=', _) => Some((Token::Eq, 1)),
        ('<', _) => Some((Token::Lt, 1)),
        ('>', _) => Some((Token::Gt, 1)),
        ('+', _) => Some((Token::Plus, 1)),
        ('-', _) => Some((Token::Minus, 1)),
        ('*', _) => Some((Token::Star, 1)),
        ('/', _) => Some((Token::Slash, 1)),
        ('%', _) => Some((Token::Percent, 1)),
        ('(', _) => Some((Token::LParen, 1)),
        (')', _) => Some((Token::RParen, 1)),
        ('[', _) => Some((Token::LBracket, 1)),
        (']', _) => Some((Token::RBracket, 1)),
        (',', _) => Some((Token::Comma, 1)),
        ('.', _) => Some((Token::Dot, 1)),
        (';', _) => Some((Token::Semicolon, 1)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(sql: &str) -> Vec<Token> {
        tokenize(sql)
            .unwrap()
            .into_iter()
            .map(|t| t.token)
            .collect()
    }

    #[test]
    fn every_operator() {
        assert_eq!(
            toks("= <> != < <= > >= + - * / % || :: ( ) [ ] , . ;"),
            vec![
                Token::Eq,
                Token::NotEq,
                Token::NotEq,
                Token::Lt,
                Token::LtEq,
                Token::Gt,
                Token::GtEq,
                Token::Plus,
                Token::Minus,
                Token::Star,
                Token::Slash,
                Token::Percent,
                Token::Concat,
                Token::DoubleColon,
                Token::LParen,
                Token::RParen,
                Token::LBracket,
                Token::RBracket,
                Token::Comma,
                Token::Dot,
                Token::Semicolon,
                Token::Eof,
            ]
        );
    }

    #[test]
    fn keywords_lowercase_and_ident_kind() {
        assert_eq!(
            toks("SeLeCt foo_Bar"),
            vec![
                Token::Ident("select".into()),
                Token::Ident("foo_bar".into()),
                Token::Eof
            ]
        );
    }

    #[test]
    fn quoted_ident_with_escaped_quote() {
        assert_eq!(
            toks("\"a\"\"b\""),
            vec![Token::QuotedIdent("a\"b".into()), Token::Eof]
        );
    }

    #[test]
    fn quoted_ident_with_double_colon() {
        assert_eq!(
            toks("\"x::string\""),
            vec![Token::QuotedIdent("x::string".into()), Token::Eof]
        );
    }

    #[test]
    fn string_with_escaped_quote() {
        assert_eq!(toks("'it''s'"), vec![Token::Str("it's".into()), Token::Eof]);
    }

    #[test]
    fn unterminated_string_reports_position() {
        let err = tokenize("select 'abc").unwrap_err();
        assert!(err.message.contains("unterminated string"));
        assert_eq!(err.line, 1);
        assert_eq!(err.column, 8);
    }

    #[test]
    fn unterminated_block_comment_reports_position() {
        let err = tokenize("1 /* comment").unwrap_err();
        assert!(err.message.contains("unterminated block comment"));
        assert_eq!(err.line, 1);
        assert_eq!(err.column, 3);
    }

    #[test]
    fn line_comment_is_skipped() {
        assert_eq!(
            toks("1 -- comment\n2"),
            vec![
                Token::Number("1".into()),
                Token::Number("2".into()),
                Token::Eof
            ]
        );
    }

    #[test]
    fn block_comment_is_skipped() {
        assert_eq!(
            toks("1 /* c */ 2"),
            vec![
                Token::Number("1".into()),
                Token::Number("2".into()),
                Token::Eof
            ]
        );
    }

    #[test]
    fn number_forms_keep_source_text() {
        for src in ["1", "1.50", ".5", "1e3", "1.5e-3"] {
            assert_eq!(
                toks(src),
                vec![Token::Number(src.into()), Token::Eof],
                "{src}"
            );
        }
    }

    #[test]
    fn number_then_dot_ident_is_still_two_tokens() {
        assert_eq!(
            toks("t.a"),
            vec![
                Token::Ident("t".into()),
                Token::Dot,
                Token::Ident("a".into()),
                Token::Eof
            ]
        );
    }

    #[test]
    fn error_reports_line_and_column() {
        let err = tokenize("select 1,\n  $bad").unwrap_err();
        assert!(err.message.contains('$'));
        assert_eq!(err.line, 2);
        assert_eq!(err.column, 3);
    }
}
