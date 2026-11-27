use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use crate::filter::{FilterOperator, FilterPredicate, FilterQuery};
use crate::{CaseSensitivity, QuarryError};

/// Maximum UTF-8 byte length accepted for a filter expression string.
pub const MAX_FILTER_EXPR_BYTES: usize = 64 * 1024;

/// Maximum number of individual predicates accepted from a single filter expression.
pub const MAX_FILTER_EXPR_PREDICATES: usize = 256;

/// Half-open UTF-8 byte range `[start, end)` within a filter expression string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterExprSpan {
    pub start: usize,
    pub end: usize,
}

impl FilterExprSpan {
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
}

/// Parse, resolution, or representability error for a filter expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterExprError {
    pub span: FilterExprSpan,
    pub message: String,
}

impl FilterExprError {
    fn new(span: FilterExprSpan, message: impl Into<String>) -> Self {
        Self {
            span,
            message: message.into(),
        }
    }
}

impl fmt::Display for FilterExprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid filter expression at byte {}: {}",
            self.span.start, self.message
        )
    }
}

impl Error for FilterExprError {}

/// Column metadata used to resolve header names and validate `#N` indices in filter expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterColumnSchema<'a> {
    pub headers: &'a [Vec<u8>],
    pub column_count: Option<usize>,
}

impl<'a> FilterColumnSchema<'a> {
    pub const fn headerless(column_count: Option<usize>) -> Self {
        Self {
            headers: &[],
            column_count,
        }
    }

    pub const fn with_headers(headers: &'a [Vec<u8>], column_count: Option<usize>) -> Self {
        Self {
            headers,
            column_count,
        }
    }

    fn resolve_index(
        &self,
        one_based: usize,
        span: FilterExprSpan,
    ) -> Result<usize, FilterExprError> {
        if one_based == 0 {
            return Err(FilterExprError::new(span, "column indices start at #1"));
        }
        if let Some(limit) = self.column_count
            && one_based > limit
        {
            return Err(FilterExprError::new(
                span,
                format!("column #{one_based} is out of range (file has {limit} columns)"),
            ));
        }
        Ok(one_based - 1)
    }

    fn resolve_name(&self, name: &str, span: FilterExprSpan) -> Result<usize, FilterExprError> {
        if self.headers.is_empty() {
            return Err(FilterExprError::new(
                span,
                format!(
                    "column name '{name}' cannot be resolved because the file has no header row; use #1, #2, ... instead"
                ),
            ));
        }
        let target = name.as_bytes();
        let mut matches = Vec::new();
        for (index, header) in self.headers.iter().enumerate() {
            if header.as_slice() == target {
                matches.push(index);
            }
        }
        match matches.as_slice() {
            [] => Err(FilterExprError::new(
                span,
                format!("unknown column '{name}'"),
            )),
            [only] => Ok(*only),
            multiple => {
                let refs = multiple
                    .iter()
                    .map(|idx| format!("#{}", idx + 1))
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(FilterExprError::new(
                    span,
                    format!(
                        "ambiguous column name '{name}' matches columns {refs}; use #{} or #{} instead",
                        multiple[0] + 1,
                        multiple[1] + 1
                    ),
                ))
            }
        }
    }

    fn preferred_header_label(
        &self,
        column: usize,
        max_bytes: usize,
    ) -> Option<(&'a str, usize, bool)> {
        let raw = self.headers.get(column)?;
        if raw.is_empty() || raw.len() > max_bytes {
            return None;
        }
        let name = std::str::from_utf8(raw).ok()?;
        if name.bytes().any(|b| b < 0x20 || b == 0x7F) {
            return None;
        }
        let is_bare = is_bare_identifier(name);
        let formatted_len = if is_bare {
            name.len()
        } else {
            let extra_brackets = name.bytes().filter(|&b| b == b']').count();
            2usize
                .saturating_add(name.len())
                .saturating_add(extra_brackets)
        };
        if formatted_len > max_bytes {
            return None;
        }
        if !self.is_unique_header(raw) {
            return None;
        }
        Some((name, formatted_len, is_bare))
    }

    fn format_column_bounded(&self, column: usize, max_bytes: usize) -> String {
        if let Some((name, formatted_len, is_bare)) = self.preferred_header_label(column, max_bytes)
        {
            if is_bare {
                return name.to_owned();
            }
            let mut out = String::with_capacity(formatted_len);
            out.push('[');
            for ch in name.chars() {
                if ch == ']' {
                    out.push_str("]]");
                } else {
                    out.push(ch);
                }
            }
            out.push(']');
            return out;
        }
        format!("#{}", column + 1)
    }

    fn is_unique_header(&self, target: &[u8]) -> bool {
        let mut count = 0usize;
        for header in self.headers {
            if header.as_slice() == target {
                count += 1;
                if count > 1 {
                    return false;
                }
            }
        }
        count == 1
    }
}

fn char_end_at(input: &str, byte_index: usize) -> usize {
    if byte_index >= input.len() {
        return input.len();
    }
    input[byte_index..]
        .chars()
        .next()
        .map_or(input.len(), |ch| byte_index + ch.len_utf8())
}

fn decimal_digits(mut value: usize) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

fn index_label_len(one_based: usize) -> usize {
    1 + decimal_digits(one_based)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenKind {
    ColumnIndex(usize),
    Ident(String),
    BracketIdent(String),
    StringLit(Vec<u8>),
    RegexLit(Vec<u8>),
    NumberLit(Vec<u8>),
    EqEq,
    NotEq,
    RegexOp,
    Gt,
    Gte,
    Lt,
    Lte,
    And,
    Or,
    Not,
    Bang,
    Contains,
    Between,
    RegexKw,
    LParen,
    RParen,
}

#[derive(Debug, Clone)]
struct Token {
    kind: TokenKind,
    span: FilterExprSpan,
}

fn is_ident_start(ch: char) -> bool {
    ch == '_' || ch.is_alphabetic()
}

fn is_ident_continue(ch: char) -> bool {
    ch == '_' || ch.is_alphanumeric()
}

fn is_reserved_keyword(ident: &str) -> bool {
    ident.eq_ignore_ascii_case("and")
        || ident.eq_ignore_ascii_case("or")
        || ident.eq_ignore_ascii_case("not")
        || ident.eq_ignore_ascii_case("contains")
        || ident.eq_ignore_ascii_case("between")
        || ident.eq_ignore_ascii_case("regex")
}

fn is_bare_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    is_ident_start(first) && chars.all(is_ident_continue) && !is_reserved_keyword(name)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn lex(input: &str) -> Result<Vec<Token>, FilterExprError> {
    if input.len() > MAX_FILTER_EXPR_BYTES {
        return Err(FilterExprError::new(
            FilterExprSpan::new(0, input.len()),
            format!("filter expression exceeds the {MAX_FILTER_EXPR_BYTES}-byte limit"),
        ));
    }

    let bytes = input.as_bytes();
    let mut tokens = Vec::new();
    let mut pos = 0usize;

    while pos < bytes.len() {
        let ch = input[pos..].chars().next().expect("valid utf8 boundary");
        if ch.is_whitespace() {
            pos += ch.len_utf8();
            continue;
        }

        let start = pos;
        match ch {
            '(' => {
                pos += 1;
                tokens.push(Token {
                    kind: TokenKind::LParen,
                    span: FilterExprSpan::new(start, pos),
                });
            }
            ')' => {
                pos += 1;
                tokens.push(Token {
                    kind: TokenKind::RParen,
                    span: FilterExprSpan::new(start, pos),
                });
            }
            '=' => {
                if bytes.get(pos + 1) == Some(&b'=') {
                    pos += 2;
                    tokens.push(Token {
                        kind: TokenKind::EqEq,
                        span: FilterExprSpan::new(start, pos),
                    });
                } else if bytes.get(pos + 1) == Some(&b'~') {
                    pos += 2;
                    tokens.push(Token {
                        kind: TokenKind::RegexOp,
                        span: FilterExprSpan::new(start, pos),
                    });
                } else {
                    return Err(FilterExprError::new(
                        FilterExprSpan::new(start, start + 1),
                        "unexpected '='; use '==' for exact text equality",
                    ));
                }
            }
            '!' => {
                if bytes.get(pos + 1) == Some(&b'=') {
                    pos += 2;
                    tokens.push(Token {
                        kind: TokenKind::NotEq,
                        span: FilterExprSpan::new(start, pos),
                    });
                } else {
                    pos += 1;
                    tokens.push(Token {
                        kind: TokenKind::Bang,
                        span: FilterExprSpan::new(start, pos),
                    });
                }
            }
            '~' => {
                if bytes.get(pos + 1) == Some(&b'=') {
                    return Err(FilterExprError::new(
                        FilterExprSpan::new(start, start + 2),
                        "unexpected '~='; use '=~' for regular expression matching",
                    ));
                }
                return Err(FilterExprError::new(
                    FilterExprSpan::new(start, start + 1),
                    "unexpected '~'; use '=~' for regular expression matching",
                ));
            }
            '>' => {
                if bytes.get(pos + 1) == Some(&b'=') {
                    pos += 2;
                    tokens.push(Token {
                        kind: TokenKind::Gte,
                        span: FilterExprSpan::new(start, pos),
                    });
                } else {
                    pos += 1;
                    tokens.push(Token {
                        kind: TokenKind::Gt,
                        span: FilterExprSpan::new(start, pos),
                    });
                }
            }
            '<' => {
                if bytes.get(pos + 1) == Some(&b'=') {
                    pos += 2;
                    tokens.push(Token {
                        kind: TokenKind::Lte,
                        span: FilterExprSpan::new(start, pos),
                    });
                } else {
                    pos += 1;
                    tokens.push(Token {
                        kind: TokenKind::Lt,
                        span: FilterExprSpan::new(start, pos),
                    });
                }
            }
            '#' => {
                pos += 1;
                let digits_start = pos;
                while pos < bytes.len() && bytes[pos].is_ascii_digit() {
                    pos += 1;
                }
                if pos == digits_start {
                    return Err(FilterExprError::new(
                        FilterExprSpan::new(start, pos),
                        "expected 1-based column number after '#' (for example #1)",
                    ));
                }
                if pos < bytes.len()
                    && let Some(next_ch) = input[pos..].chars().next()
                    && is_ident_continue(next_ch)
                {
                    let mut end = pos;
                    while end < bytes.len() {
                        let c = input[end..].chars().next().unwrap();
                        if is_ident_continue(c) {
                            end += c.len_utf8();
                        } else {
                            break;
                        }
                    }
                    return Err(FilterExprError::new(
                        FilterExprSpan::new(start, end),
                        format!(
                            "invalid column index '{}'; expected digits after '#'",
                            &input[start..end]
                        ),
                    ));
                }
                let idx_str = &input[digits_start..pos];
                let idx = idx_str.parse::<usize>().map_err(|_| {
                    FilterExprError::new(
                        FilterExprSpan::new(start, pos),
                        format!("column index '#{idx_str}' is out of range"),
                    )
                })?;
                if idx == 0 {
                    return Err(FilterExprError::new(
                        FilterExprSpan::new(start, pos),
                        "column indices start at #1",
                    ));
                }
                tokens.push(Token {
                    kind: TokenKind::ColumnIndex(idx),
                    span: FilterExprSpan::new(start, pos),
                });
            }
            '[' => {
                pos += 1;
                let mut name = String::new();
                let mut closed = false;
                while pos < bytes.len() {
                    let c = input[pos..].chars().next().expect("valid utf8");
                    if c == '\n' || c == '\r' {
                        return Err(FilterExprError::new(
                            FilterExprSpan::new(start, pos),
                            "bracketed column names cannot contain raw line breaks; use #N instead",
                        ));
                    }
                    if (c as u32) < 0x20 || c == '\x7F' {
                        return Err(FilterExprError::new(
                            FilterExprSpan::new(pos, pos + 1),
                            "bracketed column names cannot contain control characters; use #N instead",
                        ));
                    }
                    if c == ']' {
                        if bytes.get(pos + 1) == Some(&b']') {
                            name.push(']');
                            pos += 2;
                            continue;
                        }
                        pos += 1;
                        closed = true;
                        break;
                    }
                    name.push(c);
                    pos += c.len_utf8();
                }
                if !closed {
                    return Err(FilterExprError::new(
                        FilterExprSpan::new(start, pos),
                        "unterminated bracketed column name; expected ']'",
                    ));
                }
                if name.is_empty() {
                    return Err(FilterExprError::new(
                        FilterExprSpan::new(start, pos),
                        "bracketed column name must not be empty",
                    ));
                }
                tokens.push(Token {
                    kind: TokenKind::BracketIdent(name),
                    span: FilterExprSpan::new(start, pos),
                });
            }
            '"' => {
                pos += 1;
                let mut value = Vec::new();
                let mut closed = false;
                while pos < bytes.len() {
                    let b = bytes[pos];
                    if b == b'\n' || b == b'\r' {
                        return Err(FilterExprError::new(
                            FilterExprSpan::new(start, pos),
                            "unterminated string literal; use \\n or \\r for line breaks",
                        ));
                    }
                    if b < 0x20 || b == 0x7F {
                        return Err(FilterExprError::new(
                            FilterExprSpan::new(pos, pos + 1),
                            "unescaped control character in string literal; use \\t or \\xHH instead",
                        ));
                    }
                    if b == b'"' {
                        pos += 1;
                        closed = true;
                        break;
                    }
                    if b == b'\\' {
                        let esc_start = pos;
                        pos += 1;
                        let Some(&esc) = bytes.get(pos) else {
                            return Err(FilterExprError::new(
                                FilterExprSpan::new(start, pos),
                                "unterminated escape sequence in string literal",
                            ));
                        };
                        match esc {
                            b'"' => {
                                value.push(b'"');
                                pos += 1;
                            }
                            b'\\' => {
                                value.push(b'\\');
                                pos += 1;
                            }
                            b'n' => {
                                value.push(b'\n');
                                pos += 1;
                            }
                            b'r' => {
                                value.push(b'\r');
                                pos += 1;
                            }
                            b't' => {
                                value.push(b'\t');
                                pos += 1;
                            }
                            b'x' => {
                                let Some(hi) = bytes.get(pos + 1).copied().and_then(hex_value)
                                else {
                                    return Err(FilterExprError::new(
                                        FilterExprSpan::new(esc_start, char_end_at(input, pos + 1)),
                                        "invalid hex escape; expected \\xHH with two hexadecimal digits",
                                    ));
                                };
                                let Some(lo) = bytes.get(pos + 2).copied().and_then(hex_value)
                                else {
                                    return Err(FilterExprError::new(
                                        FilterExprSpan::new(esc_start, char_end_at(input, pos + 2)),
                                        "invalid hex escape; expected \\xHH with two hexadecimal digits",
                                    ));
                                };
                                value.push((hi << 4) | lo);
                                pos += 3;
                            }
                            _ => {
                                let bad_ch = input[pos..].chars().next().unwrap();
                                return Err(FilterExprError::new(
                                    FilterExprSpan::new(esc_start, pos + bad_ch.len_utf8()),
                                    format!(
                                        "unknown escape sequence '\\{bad_ch}' in string literal (supported: \\\", \\\\, \\n, \\r, \\t, \\xHH)"
                                    ),
                                ));
                            }
                        }
                    } else {
                        let c = input[pos..].chars().next().expect("valid utf8");
                        value.extend_from_slice(c.encode_utf8(&mut [0u8; 4]).as_bytes());
                        pos += c.len_utf8();
                    }
                }
                if !closed {
                    return Err(FilterExprError::new(
                        FilterExprSpan::new(start, pos),
                        "unterminated string literal",
                    ));
                }
                tokens.push(Token {
                    kind: TokenKind::StringLit(value),
                    span: FilterExprSpan::new(start, pos),
                });
            }
            '\'' => {
                return Err(FilterExprError::new(
                    FilterExprSpan::new(start, start + 1),
                    "single-quoted strings are not supported; use double quotes (\"...\")",
                ));
            }
            '/' => {
                pos += 1;
                let mut pattern = Vec::new();
                let mut closed = false;
                while pos < bytes.len() {
                    let b = bytes[pos];
                    if b == b'\n' || b == b'\r' {
                        return Err(FilterExprError::new(
                            FilterExprSpan::new(start, pos),
                            "unterminated regex literal; use \\n or \\r inside /.../",
                        ));
                    }
                    if b < 0x20 || b == 0x7F {
                        return Err(FilterExprError::new(
                            FilterExprSpan::new(pos, pos + 1),
                            "unescaped control character in regex literal; use escape sequences like \\t or \\xHH instead",
                        ));
                    }
                    if b == b'/' {
                        pos += 1;
                        closed = true;
                        break;
                    }
                    if b == b'\\' {
                        if bytes.get(pos + 1) == Some(&b'/') {
                            pattern.push(b'/');
                            pos += 2;
                            continue;
                        }
                        if bytes.get(pos + 1) == Some(&b'\\') {
                            pattern.extend_from_slice(b"\\\\");
                            pos += 2;
                            continue;
                        }
                    }
                    let c = input[pos..].chars().next().expect("valid utf8");
                    pattern.extend_from_slice(c.encode_utf8(&mut [0u8; 4]).as_bytes());
                    pos += c.len_utf8();
                }
                if !closed {
                    return Err(FilterExprError::new(
                        FilterExprSpan::new(start, pos),
                        "unterminated regex literal; expected closing '/'",
                    ));
                }
                tokens.push(Token {
                    kind: TokenKind::RegexLit(pattern),
                    span: FilterExprSpan::new(start, pos),
                });
            }
            c if c.is_ascii_digit()
                || (c == '.' && bytes.get(pos + 1).is_some_and(u8::is_ascii_digit))
                || ((c == '+' || c == '-')
                    && (bytes.get(pos + 1).is_some_and(u8::is_ascii_digit)
                        || (bytes.get(pos + 1) == Some(&b'.')
                            && bytes.get(pos + 2).is_some_and(u8::is_ascii_digit)))) =>
            {
                if matches!(bytes[pos], b'+' | b'-') {
                    pos += 1;
                }
                let mut saw_digit = false;
                while pos < bytes.len() && bytes[pos].is_ascii_digit() {
                    saw_digit = true;
                    pos += 1;
                }
                if pos < bytes.len() && bytes[pos] == b'.' {
                    pos += 1;
                    while pos < bytes.len() && bytes[pos].is_ascii_digit() {
                        saw_digit = true;
                        pos += 1;
                    }
                }
                if !saw_digit {
                    return Err(FilterExprError::new(
                        FilterExprSpan::new(start, pos),
                        format!("invalid numeric literal '{}'", &input[start..pos]),
                    ));
                }
                if pos < bytes.len() && matches!(bytes[pos], b'e' | b'E') {
                    pos += 1;
                    if pos < bytes.len() && matches!(bytes[pos], b'+' | b'-') {
                        pos += 1;
                    }
                    let exp_start = pos;
                    while pos < bytes.len() && bytes[pos].is_ascii_digit() {
                        pos += 1;
                    }
                    if pos == exp_start {
                        return Err(FilterExprError::new(
                            FilterExprSpan::new(start, pos),
                            format!(
                                "invalid numeric literal '{}': missing exponent digits",
                                &input[start..pos]
                            ),
                        ));
                    }
                }
                // Reject trailing junk attached directly to a number (e.g. `123abc` or `1.2.3`).
                if pos < bytes.len() {
                    let next = input[pos..].chars().next().unwrap();
                    if next == '.' || is_ident_continue(next) {
                        let mut end = pos;
                        while end < bytes.len() {
                            let c = input[end..].chars().next().unwrap();
                            if c == '.' || is_ident_continue(c) {
                                end += c.len_utf8();
                            } else {
                                break;
                            }
                        }
                        return Err(FilterExprError::new(
                            FilterExprSpan::new(start, end),
                            format!("invalid numeric literal '{}'", &input[start..end]),
                        ));
                    }
                }
                tokens.push(Token {
                    kind: TokenKind::NumberLit(bytes[start..pos].to_vec()),
                    span: FilterExprSpan::new(start, pos),
                });
            }
            c if is_ident_start(c) => {
                pos += c.len_utf8();
                while pos < bytes.len() {
                    let next = input[pos..].chars().next().expect("valid utf8");
                    if is_ident_continue(next) {
                        pos += next.len_utf8();
                    } else {
                        break;
                    }
                }
                let text = &input[start..pos];
                let kind = if text.eq_ignore_ascii_case("and") {
                    TokenKind::And
                } else if text.eq_ignore_ascii_case("or") {
                    TokenKind::Or
                } else if text.eq_ignore_ascii_case("not") {
                    TokenKind::Not
                } else if text.eq_ignore_ascii_case("contains") {
                    TokenKind::Contains
                } else if text.eq_ignore_ascii_case("between") {
                    TokenKind::Between
                } else if text.eq_ignore_ascii_case("regex") {
                    TokenKind::RegexKw
                } else {
                    TokenKind::Ident(text.to_owned())
                };
                tokens.push(Token {
                    kind,
                    span: FilterExprSpan::new(start, pos),
                });
            }
            other => {
                return Err(FilterExprError::new(
                    FilterExprSpan::new(start, start + other.len_utf8()),
                    format!("unexpected character '{other}'"),
                ));
            }
        }
    }

    Ok(tokens)
}

#[derive(Debug)]
struct ParsedAtom {
    predicate: FilterPredicate,
    column_span: FilterExprSpan,
    span: FilterExprSpan,
}

#[derive(Debug)]
struct ParsedClause {
    column: usize,
    atoms: Vec<ParsedAtom>,
    span: FilterExprSpan,
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    input_len: usize,
    atom_count: usize,
    schema: &'a FilterColumnSchema<'a>,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a Token> {
        self.tokens.get(self.pos)
    }

    fn eof_span(&self) -> FilterExprSpan {
        FilterExprSpan::new(self.input_len, self.input_len)
    }

    fn next(&mut self) -> Option<&'a Token> {
        let tok = self.tokens.get(self.pos)?;
        self.pos += 1;
        Some(tok)
    }

    fn parse_all(&mut self) -> Result<Vec<ParsedClause>, FilterExprError> {
        if self.tokens.is_empty() {
            return Err(FilterExprError::new(
                FilterExprSpan::new(0, self.input_len),
                "filter expression must not be empty",
            ));
        }

        let mut clauses = Vec::new();
        clauses.push(self.parse_clause()?);
        while let Some(tok) = self.peek() {
            match tok.kind {
                TokenKind::And => {
                    self.next();
                    clauses.push(self.parse_clause()?);
                }
                TokenKind::LParen | TokenKind::RParen => {
                    return Err(FilterExprError::new(
                        tok.span,
                        "parentheses are not supported; write AND between columns and OR between rules on the same column",
                    ));
                }
                TokenKind::Not | TokenKind::Bang => {
                    return Err(FilterExprError::new(
                        tok.span,
                        "NOT is not supported; use != for text exclusion",
                    ));
                }
                _ => {
                    return Err(FilterExprError::new(
                        tok.span,
                        "expected AND, OR, or end of filter expression",
                    ));
                }
            }
        }
        Ok(clauses)
    }

    fn parse_clause(&mut self) -> Result<ParsedClause, FilterExprError> {
        let first = self.parse_atom()?;
        let column = first.predicate.column;
        let clause_start = first.span.start;
        let mut clause_end = first.span.end;
        let mut atoms = vec![first];

        while let Some(tok) = self.peek() {
            if tok.kind != TokenKind::Or {
                break;
            }
            if atoms[0].predicate.operator == FilterOperator::NotEquals {
                return Err(FilterExprError::new(
                    atoms[0].span,
                    "'!=' cannot be combined with OR; use AND to exclude values",
                ));
            }
            self.next();
            let next_atom = self.parse_atom()?;
            if next_atom.predicate.operator == FilterOperator::NotEquals {
                return Err(FilterExprError::new(
                    next_atom.span,
                    "'!=' cannot be combined with OR; use AND to exclude values",
                ));
            }
            if next_atom.predicate.column != column {
                return Err(FilterExprError::new(
                    next_atom.column_span,
                    "OR can only combine rules on the same column; use AND between different columns",
                ));
            }
            clause_end = next_atom.span.end;
            atoms.push(next_atom);
        }

        Ok(ParsedClause {
            column,
            atoms,
            span: FilterExprSpan::new(clause_start, clause_end),
        })
    }

    fn parse_atom(&mut self) -> Result<ParsedAtom, FilterExprError> {
        if self.atom_count >= MAX_FILTER_EXPR_PREDICATES {
            let span = self
                .peek()
                .map(|tok| tok.span)
                .unwrap_or_else(|| self.eof_span());
            return Err(FilterExprError::new(
                span,
                format!(
                    "filter expression exceeds the maximum of {MAX_FILTER_EXPR_PREDICATES} predicates"
                ),
            ));
        }
        self.atom_count += 1;

        let Some(col_tok) = self.next() else {
            return Err(FilterExprError::new(
                self.eof_span(),
                "expected column reference (name, [column name], or #N)",
            ));
        };
        let column = match &col_tok.kind {
            TokenKind::ColumnIndex(idx) => self.schema.resolve_index(*idx, col_tok.span)?,
            TokenKind::Ident(name) | TokenKind::BracketIdent(name) => {
                self.schema.resolve_name(name, col_tok.span)?
            }
            TokenKind::LParen | TokenKind::RParen => {
                return Err(FilterExprError::new(
                    col_tok.span,
                    "parentheses are not supported; write AND between columns and OR between rules on the same column",
                ));
            }
            TokenKind::Not | TokenKind::Bang => {
                return Err(FilterExprError::new(
                    col_tok.span,
                    "NOT is not supported; use != for text exclusion",
                ));
            }
            TokenKind::And
            | TokenKind::Or
            | TokenKind::Contains
            | TokenKind::Between
            | TokenKind::RegexKw => {
                return Err(FilterExprError::new(
                    col_tok.span,
                    "unexpected keyword where a column reference was expected; wrap column names matching keywords in brackets (for example [and])",
                ));
            }
            _ => {
                return Err(FilterExprError::new(
                    col_tok.span,
                    "expected column reference (name, [column name], or #N)",
                ));
            }
        };

        let Some(op_tok) = self.next() else {
            return Err(FilterExprError::new(
                self.eof_span(),
                "expected operator (==, !=, CONTAINS, =~, >, >=, <, <=, or BETWEEN)",
            ));
        };

        match op_tok.kind {
            TokenKind::EqEq => {
                let (value, end) = self.expect_string("==")?;
                Ok(ParsedAtom {
                    predicate: FilterPredicate {
                        column,
                        operator: FilterOperator::Equals,
                        value,
                        upper_bound: None,
                    },
                    column_span: col_tok.span,
                    span: FilterExprSpan::new(col_tok.span.start, end),
                })
            }
            TokenKind::NotEq => {
                let (value, end) = self.expect_string("!=")?;
                Ok(ParsedAtom {
                    predicate: FilterPredicate {
                        column,
                        operator: FilterOperator::NotEquals,
                        value,
                        upper_bound: None,
                    },
                    column_span: col_tok.span,
                    span: FilterExprSpan::new(col_tok.span.start, end),
                })
            }
            TokenKind::Contains => {
                let (value, end) = self.expect_string("CONTAINS")?;
                Ok(ParsedAtom {
                    predicate: FilterPredicate {
                        column,
                        operator: FilterOperator::Contains,
                        value,
                        upper_bound: None,
                    },
                    column_span: col_tok.span,
                    span: FilterExprSpan::new(col_tok.span.start, end),
                })
            }
            TokenKind::RegexOp | TokenKind::RegexKw => {
                let (value, end) = self.expect_regex_pattern()?;
                Ok(ParsedAtom {
                    predicate: FilterPredicate {
                        column,
                        operator: FilterOperator::Regex,
                        value,
                        upper_bound: None,
                    },
                    column_span: col_tok.span,
                    span: FilterExprSpan::new(col_tok.span.start, end),
                })
            }
            TokenKind::Gt => {
                let (value, end) = self.expect_number(">")?;
                Ok(ParsedAtom {
                    predicate: FilterPredicate {
                        column,
                        operator: FilterOperator::GreaterThan,
                        value,
                        upper_bound: None,
                    },
                    column_span: col_tok.span,
                    span: FilterExprSpan::new(col_tok.span.start, end),
                })
            }
            TokenKind::Gte => {
                let (value, end) = self.expect_number(">=")?;
                Ok(ParsedAtom {
                    predicate: FilterPredicate {
                        column,
                        operator: FilterOperator::GreaterThanOrEqual,
                        value,
                        upper_bound: None,
                    },
                    column_span: col_tok.span,
                    span: FilterExprSpan::new(col_tok.span.start, end),
                })
            }
            TokenKind::Lt => {
                let (value, end) = self.expect_number("<")?;
                Ok(ParsedAtom {
                    predicate: FilterPredicate {
                        column,
                        operator: FilterOperator::LessThan,
                        value,
                        upper_bound: None,
                    },
                    column_span: col_tok.span,
                    span: FilterExprSpan::new(col_tok.span.start, end),
                })
            }
            TokenKind::Lte => {
                let (value, end) = self.expect_number("<=")?;
                Ok(ParsedAtom {
                    predicate: FilterPredicate {
                        column,
                        operator: FilterOperator::LessThanOrEqual,
                        value,
                        upper_bound: None,
                    },
                    column_span: col_tok.span,
                    span: FilterExprSpan::new(col_tok.span.start, end),
                })
            }
            TokenKind::Between => {
                let (lower, _) = self.expect_number("BETWEEN")?;
                let Some(and_tok) = self.next() else {
                    return Err(FilterExprError::new(
                        self.eof_span(),
                        "expected AND and upper numeric bound after BETWEEN lower bound",
                    ));
                };
                if and_tok.kind != TokenKind::And {
                    return Err(FilterExprError::new(
                        and_tok.span,
                        "expected AND between lower and upper bounds in BETWEEN expression",
                    ));
                }
                let (upper, end) = self.expect_number("BETWEEN ... AND")?;
                Ok(ParsedAtom {
                    predicate: FilterPredicate {
                        column,
                        operator: FilterOperator::Between,
                        value: lower,
                        upper_bound: Some(upper),
                    },
                    column_span: col_tok.span,
                    span: FilterExprSpan::new(col_tok.span.start, end),
                })
            }
            TokenKind::LParen | TokenKind::RParen => Err(FilterExprError::new(
                op_tok.span,
                "parentheses are not supported; write AND between columns and OR between rules on the same column",
            )),
            TokenKind::Not | TokenKind::Bang => Err(FilterExprError::new(
                op_tok.span,
                "NOT is not supported; use != for text exclusion",
            )),
            _ => Err(FilterExprError::new(
                op_tok.span,
                "expected operator (==, !=, CONTAINS, =~, >, >=, <, <=, or BETWEEN)",
            )),
        }
    }

    fn expect_string(&mut self, op_label: &str) -> Result<(Vec<u8>, usize), FilterExprError> {
        let Some(tok) = self.next() else {
            return Err(FilterExprError::new(
                self.eof_span(),
                format!("expected quoted string after '{op_label}'"),
            ));
        };
        match &tok.kind {
            TokenKind::StringLit(bytes) => Ok((bytes.clone(), tok.span.end)),
            TokenKind::NumberLit(_) => Err(FilterExprError::new(
                tok.span,
                format!(
                    "text operator '{op_label}' requires a double-quoted string (for example {op_label} \"123\"); use >, >=, <, <=, or BETWEEN for numeric comparisons"
                ),
            )),
            _ => Err(FilterExprError::new(
                tok.span,
                format!("expected double-quoted string after '{op_label}'"),
            )),
        }
    }

    fn expect_regex_pattern(&mut self) -> Result<(Vec<u8>, usize), FilterExprError> {
        let Some(tok) = self.next() else {
            return Err(FilterExprError::new(
                self.eof_span(),
                "expected regex pattern (\"...\" or /.../) after '=~'",
            ));
        };
        match &tok.kind {
            TokenKind::StringLit(bytes) | TokenKind::RegexLit(bytes) => {
                Ok((bytes.clone(), tok.span.end))
            }
            _ => Err(FilterExprError::new(
                tok.span,
                "expected double-quoted string or /pattern/ regex literal after '=~'",
            )),
        }
    }

    fn expect_number(&mut self, op_label: &str) -> Result<(Vec<u8>, usize), FilterExprError> {
        let Some(tok) = self.next() else {
            return Err(FilterExprError::new(
                self.eof_span(),
                format!("expected unquoted number after '{op_label}'"),
            ));
        };
        match &tok.kind {
            TokenKind::NumberLit(bytes) => Ok((bytes.clone(), tok.span.end)),
            TokenKind::StringLit(_) => Err(FilterExprError::new(
                tok.span,
                format!(
                    "numeric operator '{op_label}' requires an unquoted number, not a quoted string (use == for exact text equality)"
                ),
            )),
            _ => Err(FilterExprError::new(
                tok.span,
                format!(
                    "expected unquoted number after '{op_label}' (for example 100, -12.5, or 1e3)"
                ),
            )),
        }
    }
}

/// Parses an exact `FilterQuery` expression against `schema`, validating all representability,
/// numeric, and bounded-regex rules before returning a [`FilterQuery`].
///
/// The language expresses **exactly** what [`FilterQuery`] can represent:
/// a conjunction (`AND`) of per-column groups, where `OR` combines alternative inclusion rules
/// on the same column and `!=` excludes exact text values on that column.
pub fn parse_filter_expr(
    input: &str,
    schema: &FilterColumnSchema<'_>,
    case_sensitivity: CaseSensitivity,
) -> Result<FilterQuery, FilterExprError> {
    let tokens = lex(input)?;
    let mut parser = Parser {
        tokens: &tokens,
        pos: 0,
        input_len: input.len(),
        atom_count: 0,
        schema,
    };
    let clauses = parser.parse_all()?;

    struct ColumnBucket {
        inclusion_clauses: Vec<ParsedClause>,
        exclusions: Vec<ParsedAtom>,
    }

    let mut by_column: BTreeMap<usize, ColumnBucket> = BTreeMap::new();
    for clause in clauses {
        let bucket = by_column
            .entry(clause.column)
            .or_insert_with(|| ColumnBucket {
                inclusion_clauses: Vec::new(),
                exclusions: Vec::new(),
            });
        if clause.atoms.len() == 1
            && clause.atoms[0].predicate.operator == FilterOperator::NotEquals
        {
            bucket
                .exclusions
                .push(clause.atoms.into_iter().next().unwrap());
        } else {
            bucket.inclusion_clauses.push(clause);
        }
    }

    let mut predicates = Vec::new();
    let mut predicate_spans = Vec::new();

    for (column, bucket) in by_column {
        match bucket.inclusion_clauses.len() {
            0 => {}
            1 => {
                let clause = bucket.inclusion_clauses.into_iter().next().unwrap();
                for atom in clause.atoms {
                    predicates.push(atom.predicate);
                    predicate_spans.push(atom.span);
                }
            }
            2 => {
                let mut iter = bucket.inclusion_clauses.into_iter();
                let first = iter.next().unwrap();
                let second = iter.next().unwrap();
                if first.atoms.len() == 1 && second.atoms.len() == 1 {
                    let a = &first.atoms[0].predicate;
                    let b = &second.atoms[0].predicate;
                    let folded = match (a.operator, b.operator) {
                        (FilterOperator::GreaterThanOrEqual, FilterOperator::LessThanOrEqual) => {
                            Some((a.value.clone(), b.value.clone()))
                        }
                        (FilterOperator::LessThanOrEqual, FilterOperator::GreaterThanOrEqual) => {
                            Some((b.value.clone(), a.value.clone()))
                        }
                        _ => None,
                    };
                    if let Some((lower, upper)) = folded {
                        let span = FilterExprSpan::new(
                            first.span.start.min(second.span.start),
                            first.span.end.max(second.span.end),
                        );
                        predicates.push(FilterPredicate {
                            column,
                            operator: FilterOperator::Between,
                            value: lower,
                            upper_bound: Some(upper),
                        });
                        predicate_spans.push(span);
                    } else if is_range_pair(a.operator, b.operator) {
                        let col_label = schema.format_column_bounded(column, 256);
                        return Err(FilterExprError::new(
                            second.span,
                            format!(
                                "exclusive range bounds on column '{col_label}' cannot be combined with AND; only inclusive '>= lower AND <= upper' (or BETWEEN) is supported"
                            ),
                        ));
                    } else {
                        let col_label = schema.format_column_bounded(column, 256);
                        return Err(FilterExprError::new(
                            second.span,
                            format!(
                                "cannot combine multiple inclusion clauses on column '{col_label}' with AND; use OR for same-column alternatives"
                            ),
                        ));
                    }
                } else {
                    let col_label = schema.format_column_bounded(column, 256);
                    return Err(FilterExprError::new(
                        second.span,
                        format!(
                            "cannot combine multiple inclusion clauses on column '{col_label}' with AND; use OR for same-column alternatives"
                        ),
                    ));
                }
            }
            _ => {
                let col_label = schema.format_column_bounded(column, 256);
                let bad_span = bucket.inclusion_clauses[1].span;
                return Err(FilterExprError::new(
                    bad_span,
                    format!(
                        "cannot combine multiple inclusion clauses on column '{col_label}' with AND; use OR for same-column alternatives"
                    ),
                ));
            }
        }

        for exc in bucket.exclusions {
            predicates.push(exc.predicate);
            predicate_spans.push(exc.span);
        }
    }

    let query = FilterQuery {
        predicates,
        case_sensitivity,
    };
    if let Err(err) = query.validate() {
        return Err(match err {
            QuarryError::InvalidFilter { rule, reason } => {
                let span = predicate_spans
                    .get(rule.saturating_sub(1))
                    .copied()
                    .unwrap_or_else(|| FilterExprSpan::new(0, input.len()));
                FilterExprError::new(span, reason.into_owned())
            }
            other => FilterExprError::new(FilterExprSpan::new(0, input.len()), other.to_string()),
        });
    }

    Ok(query)
}

fn is_range_pair(a: FilterOperator, b: FilterOperator) -> bool {
    let is_lower = |op| {
        matches!(
            op,
            FilterOperator::GreaterThan | FilterOperator::GreaterThanOrEqual
        )
    };
    let is_upper = |op| {
        matches!(
            op,
            FilterOperator::LessThan | FilterOperator::LessThanOrEqual
        )
    };
    (is_lower(a) && is_upper(b)) || (is_upper(a) && is_lower(b))
}

#[derive(Clone, Copy)]
enum ColumnLabel<'a> {
    Index { one_based: usize, len: usize },
    Bare(&'a str),
    Bracketed { name: &'a str, len: usize },
}

impl<'a> ColumnLabel<'a> {
    fn len(self) -> usize {
        match self {
            Self::Index { len, .. } | Self::Bracketed { len, .. } => len,
            Self::Bare(name) => name.len(),
        }
    }

    fn first_byte(self) -> u8 {
        match self {
            Self::Index { .. } => b'#',
            Self::Bracketed { .. } => b'[',
            Self::Bare(name) => name.as_bytes()[0],
        }
    }

    fn last_byte(self) -> u8 {
        match self {
            Self::Index { one_based, .. } => b'0' + ((one_based % 10) as u8),
            Self::Bracketed { .. } => b']',
            Self::Bare(name) => *name.as_bytes().last().unwrap(),
        }
    }

    fn write_to(self, out: &mut String) {
        match self {
            Self::Index { one_based, .. } => {
                use std::fmt::Write;
                let _ = write!(out, "#{one_based}");
            }
            Self::Bare(name) => out.push_str(name),
            Self::Bracketed { name, .. } => {
                out.push('[');
                for ch in name.chars() {
                    if ch == ']' {
                        out.push_str("]]");
                    } else {
                        out.push(ch);
                    }
                }
                out.push(']');
            }
        }
    }
}

fn token_boundary_needs_space(prev_byte: u8, next_byte: u8) -> bool {
    let prev_is_word = prev_byte.is_ascii_alphanumeric()
        || prev_byte == b'_'
        || prev_byte == b'.'
        || prev_byte >= 0x80;
    let next_is_word = next_byte.is_ascii_alphanumeric()
        || next_byte == b'_'
        || next_byte == b'.'
        || next_byte >= 0x80;
    prev_is_word && next_is_word
}

fn escaped_char_len(ch: char) -> usize {
    match ch {
        '"' | '\\' | '\n' | '\r' | '\t' => 2,
        c if (c as u32) < 0x20 || c == '\x7F' => 4,
        c => c.len_utf8(),
    }
}

fn string_literal_len(bytes: &[u8]) -> usize {
    let mut len = 2usize; // opening and closing quotes
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        match std::str::from_utf8(&bytes[cursor..]) {
            Ok(valid) => {
                for ch in valid.chars() {
                    len = len.saturating_add(escaped_char_len(ch));
                }
                break;
            }
            Err(err) => {
                let valid_len = err.valid_up_to();
                if valid_len > 0 {
                    let valid = std::str::from_utf8(&bytes[cursor..cursor + valid_len]).unwrap();
                    for ch in valid.chars() {
                        len = len.saturating_add(escaped_char_len(ch));
                    }
                    cursor += valid_len;
                }
                let bad_len = err.error_len().unwrap_or(bytes.len() - cursor);
                len = len.saturating_add(bad_len.saturating_mul(4));
                cursor += bad_len;
            }
        }
    }
    len
}

fn write_string_literal(out: &mut String, bytes: &[u8]) {
    out.push('"');
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        match std::str::from_utf8(&bytes[cursor..]) {
            Ok(valid) => {
                for ch in valid.chars() {
                    push_escaped_char(out, ch);
                }
                break;
            }
            Err(err) => {
                let valid_len = err.valid_up_to();
                if valid_len > 0 {
                    let valid = std::str::from_utf8(&bytes[cursor..cursor + valid_len]).unwrap();
                    for ch in valid.chars() {
                        push_escaped_char(out, ch);
                    }
                    cursor += valid_len;
                }
                let bad_len = err.error_len().unwrap_or(bytes.len() - cursor);
                for &b in &bytes[cursor..cursor + bad_len] {
                    use std::fmt::Write;
                    let _ = write!(out, "\\x{b:02X}");
                }
                cursor += bad_len;
            }
        }
    }
    out.push('"');
}

fn push_escaped_char(out: &mut String, ch: char) {
    match ch {
        '"' => out.push_str("\\\""),
        '\\' => out.push_str("\\\\"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        c if (c as u32) < 0x20 || c == '\x7F' => {
            use std::fmt::Write;
            let _ = write!(out, "\\x{:02X}", c as u8);
        }
        c => out.push(c),
    }
}

/// Returns the exact `/.../` literal byte length if `bytes` can round-trip losslessly
/// through the regex literal lexer without adding extra backslashes.
fn regex_literal_len(bytes: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut len = 2usize; // opening and closing '/'
    let mut backslashes = 0usize;
    for &b in text.as_bytes() {
        if b < 0x20 || b == 0x7F {
            return None;
        }
        if b == b'/' {
            if backslashes % 2 == 1 {
                return None;
            }
            len = len.saturating_add(2); // \/
            backslashes = 0;
        } else if b == b'\\' {
            len = len.saturating_add(1);
            backslashes += 1;
        } else {
            len = len.saturating_add(1);
            backslashes = 0;
        }
    }
    if backslashes % 2 == 1 {
        return None;
    }
    Some(len)
}

fn write_regex_literal(out: &mut String, bytes: &[u8]) {
    let text = std::str::from_utf8(bytes).expect("validated utf8 regex literal");
    out.push('/');
    for ch in text.chars() {
        if ch == '/' {
            out.push_str("\\/");
        } else {
            out.push(ch);
        }
    }
    out.push('/');
}

fn numeric_literal_len(bytes: &[u8]) -> usize {
    let trimmed = bytes.trim_ascii();
    match std::str::from_utf8(trimmed) {
        Ok(s) => s.len(),
        Err(_) => String::from_utf8_lossy(trimmed).len(),
    }
}

fn write_numeric_literal(out: &mut String, bytes: &[u8]) {
    let trimmed = bytes.trim_ascii();
    match std::str::from_utf8(trimmed) {
        Ok(s) => out.push_str(s),
        Err(_) => out.push_str(&String::from_utf8_lossy(trimmed)),
    }
}

fn numeric_first_byte(bytes: &[u8]) -> u8 {
    bytes.trim_ascii().first().copied().unwrap_or(b'0')
}

fn numeric_last_byte(bytes: &[u8]) -> u8 {
    bytes.trim_ascii().last().copied().unwrap_or(b'0')
}

#[derive(Clone, Copy)]
enum PrintTier {
    PrettyHeaders,
    PrettyIndex,
    Compact,
    CompactReordered,
}

fn measure_group_with_label(
    col_label: ColumnLabel<'_>,
    inclusions: &[&FilterPredicate],
    exclusions: &[&FilterPredicate],
    prev_clause_last: Option<u8>,
    compact: bool,
    expand_between: bool,
) -> (usize, Option<u8>) {
    let mut len = 0usize;
    let mut prev_clause = prev_clause_last;
    if !inclusions.is_empty() {
        if let Some(prev) = prev_clause {
            len = len.saturating_add(emit_joiner(
                prev,
                "AND",
                col_label.first_byte(),
                compact,
                None,
            ));
        }
        let mut prev_or_last: Option<u8> = None;
        for pred in inclusions {
            if let Some(prev) = prev_or_last {
                len = len.saturating_add(emit_joiner(
                    prev,
                    "OR",
                    col_label.first_byte(),
                    compact,
                    None,
                ));
            }
            let (pred_len, last) = emit_predicate(pred, col_label, compact, expand_between, None);
            len = len.saturating_add(pred_len);
            prev_or_last = Some(last);
        }
        prev_clause = prev_or_last;
    }
    for exc in exclusions {
        if let Some(prev) = prev_clause {
            len = len.saturating_add(emit_joiner(
                prev,
                "AND",
                col_label.first_byte(),
                compact,
                None,
            ));
        }
        let (pred_len, last) = emit_predicate(exc, col_label, compact, false, None);
        len = len.saturating_add(pred_len);
        prev_clause = Some(last);
    }
    (len, prev_clause)
}

fn choose_column_label<'a>(
    schema: &FilterColumnSchema<'a>,
    column: usize,
    one_based: usize,
    inclusions: &[&FilterPredicate],
    exclusions: &[&FilterPredicate],
    prev_clause_last: Option<u8>,
    tier: PrintTier,
) -> ColumnLabel<'a> {
    let idx_len = index_label_len(one_based);
    let idx_label = ColumnLabel::Index {
        one_based,
        len: idx_len,
    };
    match tier {
        PrintTier::PrettyIndex => idx_label,
        PrintTier::PrettyHeaders => {
            if let Some((name, formatted_len, is_bare)) =
                schema.preferred_header_label(column, MAX_FILTER_EXPR_BYTES)
            {
                if is_bare {
                    ColumnLabel::Bare(name)
                } else {
                    ColumnLabel::Bracketed {
                        name,
                        len: formatted_len,
                    }
                }
            } else {
                idx_label
            }
        }
        PrintTier::Compact | PrintTier::CompactReordered => {
            // A header label can only beat or tie `#N` in compact mode (including separator
            // savings such as `[a-]CONTAINS"..."` vs `#100 CONTAINS"..."`) if its formatted
            // length is at most `idx_len + 1`.
            if let Some((name, formatted_len, is_bare)) =
                schema.preferred_header_label(column, idx_len.saturating_add(1))
            {
                let candidate = if is_bare {
                    ColumnLabel::Bare(name)
                } else {
                    ColumnLabel::Bracketed {
                        name,
                        len: formatted_len,
                    }
                };
                let (idx_cost, _) = measure_group_with_label(
                    idx_label,
                    inclusions,
                    exclusions,
                    prev_clause_last,
                    true,
                    false,
                );
                let (cand_cost, _) = measure_group_with_label(
                    candidate,
                    inclusions,
                    exclusions,
                    prev_clause_last,
                    true,
                    false,
                );
                if cand_cost < idx_cost {
                    candidate
                } else {
                    idx_label
                }
            } else {
                idx_label
            }
        }
    }
}

/// Measure or render a single predicate under `compact` rules, returning `(len, last_byte)`.
/// If `out` is `Some`, writes exact bytes matching the measured length.
fn emit_predicate(
    predicate: &FilterPredicate,
    col_label: ColumnLabel<'_>,
    compact: bool,
    expand_between: bool,
    mut out: Option<&mut String>,
) -> (usize, u8) {
    let mut len = 0usize;
    let push_str = |out: &mut Option<&mut String>, len: &mut usize, s: &str| {
        *len = len.saturating_add(s.len());
        if let Some(buf) = out.as_deref_mut() {
            buf.push_str(s);
        }
    };
    let push_col = |out: &mut Option<&mut String>, len: &mut usize, col: ColumnLabel<'_>| {
        *len = len.saturating_add(col.len());
        if let Some(buf) = out.as_deref_mut() {
            col.write_to(buf);
        }
    };

    match predicate.operator {
        FilterOperator::Equals => {
            push_col(&mut out, &mut len, col_label);
            push_str(&mut out, &mut len, if compact { "==" } else { " == " });
            len = len.saturating_add(string_literal_len(&predicate.value));
            if let Some(buf) = out {
                write_string_literal(buf, &predicate.value);
            }
            (len, b'"')
        }
        FilterOperator::NotEquals => {
            push_col(&mut out, &mut len, col_label);
            push_str(&mut out, &mut len, if compact { "!=" } else { " != " });
            len = len.saturating_add(string_literal_len(&predicate.value));
            if let Some(buf) = out {
                write_string_literal(buf, &predicate.value);
            }
            (len, b'"')
        }
        FilterOperator::Contains => {
            push_col(&mut out, &mut len, col_label);
            if compact {
                if token_boundary_needs_space(col_label.last_byte(), b'C') {
                    push_str(&mut out, &mut len, " ");
                }
                push_str(&mut out, &mut len, "CONTAINS");
            } else {
                push_str(&mut out, &mut len, " CONTAINS ");
            }
            len = len.saturating_add(string_literal_len(&predicate.value));
            if let Some(buf) = out {
                write_string_literal(buf, &predicate.value);
            }
            (len, b'"')
        }
        FilterOperator::Regex => {
            push_col(&mut out, &mut len, col_label);
            push_str(&mut out, &mut len, if compact { "=~" } else { " =~ " });
            let str_len = string_literal_len(&predicate.value);
            let use_slash = compact
                && regex_literal_len(&predicate.value).is_some_and(|slash_len| slash_len < str_len);
            if use_slash {
                let slash_len = regex_literal_len(&predicate.value).unwrap();
                len = len.saturating_add(slash_len);
                if let Some(buf) = out {
                    write_regex_literal(buf, &predicate.value);
                }
                (len, b'/')
            } else {
                len = len.saturating_add(str_len);
                if let Some(buf) = out {
                    write_string_literal(buf, &predicate.value);
                }
                (len, b'"')
            }
        }
        FilterOperator::GreaterThan => {
            push_col(&mut out, &mut len, col_label);
            push_str(&mut out, &mut len, if compact { ">" } else { " > " });
            len = len.saturating_add(numeric_literal_len(&predicate.value));
            if let Some(buf) = out {
                write_numeric_literal(buf, &predicate.value);
            }
            (len, numeric_last_byte(&predicate.value))
        }
        FilterOperator::GreaterThanOrEqual => {
            push_col(&mut out, &mut len, col_label);
            push_str(&mut out, &mut len, if compact { ">=" } else { " >= " });
            len = len.saturating_add(numeric_literal_len(&predicate.value));
            if let Some(buf) = out {
                write_numeric_literal(buf, &predicate.value);
            }
            (len, numeric_last_byte(&predicate.value))
        }
        FilterOperator::LessThan => {
            push_col(&mut out, &mut len, col_label);
            push_str(&mut out, &mut len, if compact { "<" } else { " < " });
            len = len.saturating_add(numeric_literal_len(&predicate.value));
            if let Some(buf) = out {
                write_numeric_literal(buf, &predicate.value);
            }
            (len, numeric_last_byte(&predicate.value))
        }
        FilterOperator::LessThanOrEqual => {
            push_col(&mut out, &mut len, col_label);
            push_str(&mut out, &mut len, if compact { "<=" } else { " <= " });
            len = len.saturating_add(numeric_literal_len(&predicate.value));
            if let Some(buf) = out {
                write_numeric_literal(buf, &predicate.value);
            }
            (len, numeric_last_byte(&predicate.value))
        }
        FilterOperator::Between => {
            let upper = predicate.upper_bound.as_deref().unwrap_or(&[]);
            if compact && expand_between {
                push_col(&mut out, &mut len, col_label);
                push_str(&mut out, &mut len, ">=");
                len = len.saturating_add(numeric_literal_len(&predicate.value));
                if let Some(buf) = out.as_deref_mut() {
                    write_numeric_literal(buf, &predicate.value);
                }
                if token_boundary_needs_space(numeric_last_byte(&predicate.value), b'A') {
                    push_str(&mut out, &mut len, " ");
                }
                push_str(&mut out, &mut len, "AND");
                if token_boundary_needs_space(b'D', col_label.first_byte()) {
                    push_str(&mut out, &mut len, " ");
                }
                push_col(&mut out, &mut len, col_label);
                push_str(&mut out, &mut len, "<=");
                len = len.saturating_add(numeric_literal_len(upper));
                if let Some(buf) = out {
                    write_numeric_literal(buf, upper);
                }
                (len, numeric_last_byte(upper))
            } else {
                push_col(&mut out, &mut len, col_label);
                if compact {
                    if token_boundary_needs_space(col_label.last_byte(), b'B') {
                        push_str(&mut out, &mut len, " ");
                    }
                    push_str(&mut out, &mut len, "BETWEEN");
                    if token_boundary_needs_space(b'N', numeric_first_byte(&predicate.value)) {
                        push_str(&mut out, &mut len, " ");
                    }
                } else {
                    push_str(&mut out, &mut len, " BETWEEN ");
                }
                len = len.saturating_add(numeric_literal_len(&predicate.value));
                if let Some(buf) = out.as_deref_mut() {
                    write_numeric_literal(buf, &predicate.value);
                }
                if compact {
                    if token_boundary_needs_space(numeric_last_byte(&predicate.value), b'A') {
                        push_str(&mut out, &mut len, " ");
                    }
                    push_str(&mut out, &mut len, "AND");
                    if token_boundary_needs_space(b'D', numeric_first_byte(upper)) {
                        push_str(&mut out, &mut len, " ");
                    }
                } else {
                    push_str(&mut out, &mut len, " AND ");
                }
                len = len.saturating_add(numeric_literal_len(upper));
                if let Some(buf) = out {
                    write_numeric_literal(buf, upper);
                }
                (len, numeric_last_byte(upper))
            }
        }
    }
}

fn emit_joiner(
    prev_byte: u8,
    keyword: &str,
    next_byte: u8,
    compact: bool,
    mut out: Option<&mut String>,
) -> usize {
    if !compact {
        let total = 2 + keyword.len();
        if let Some(buf) = out {
            buf.push(' ');
            buf.push_str(keyword);
            buf.push(' ');
        }
        return total;
    }
    let mut len = 0usize;
    if token_boundary_needs_space(prev_byte, keyword.as_bytes()[0]) {
        len += 1;
        if let Some(buf) = out.as_deref_mut() {
            buf.push(' ');
        }
    }
    len += keyword.len();
    if let Some(buf) = out.as_deref_mut() {
        buf.push_str(keyword);
    }
    if token_boundary_needs_space(*keyword.as_bytes().last().unwrap(), next_byte) {
        len += 1;
        if let Some(buf) = out {
            buf.push(' ');
        }
    }
    len
}

fn predicate_ends_with_word_byte(predicate: &FilterPredicate) -> bool {
    matches!(
        predicate.operator,
        FilterOperator::GreaterThan
            | FilterOperator::GreaterThanOrEqual
            | FilterOperator::LessThan
            | FilterOperator::LessThanOrEqual
            | FilterOperator::Between
    )
}

fn try_render_grouped(
    by_column: &BTreeMap<usize, (Vec<&FilterPredicate>, Vec<&FilterPredicate>)>,
    schema: &FilterColumnSchema<'_>,
    total_predicates: usize,
    tier: PrintTier,
) -> Option<String> {
    let compact = matches!(tier, PrintTier::Compact | PrintTier::CompactReordered);
    let groups: Vec<(usize, usize, &[&FilterPredicate], &[&FilterPredicate])> =
        if matches!(tier, PrintTier::CompactReordered) {
            // Split each column's inclusion OR-group and exclusion list into independent top-level
            // AND clauses so exclusions (which always end with `"`) and text/regex inclusion groups
            // can precede numeric-ending inclusion groups on the same or different columns.
            let mut clauses = Vec::with_capacity(by_column.len() * 2);
            for (&column, (inclusions, exclusions)) in by_column {
                let one_based = column + 1;
                if !exclusions.is_empty() {
                    clauses.push((column, one_based, &[][..], exclusions.as_slice()));
                }
                if !inclusions.is_empty() {
                    clauses.push((column, one_based, inclusions.as_slice(), &[][..]));
                }
            }
            clauses.sort_by_key(|&(column, _, inclusions, exclusions)| {
                let ends_word = exclusions.is_empty()
                    && inclusions
                        .last()
                        .is_some_and(|pred| predicate_ends_with_word_byte(pred));
                (ends_word, column, exclusions.is_empty())
            });
            clauses
        } else {
            by_column
                .iter()
                .map(|(&column, (inclusions, exclusions))| {
                    (
                        column,
                        column + 1,
                        inclusions.as_slice(),
                        exclusions.as_slice(),
                    )
                })
                .collect()
        };

    // Choose each column's label in group order (respecting separator costs in compact tiers).
    let mut labels = Vec::with_capacity(groups.len());
    let mut prev_clause_last: Option<u8> = None;
    for &(column, one_based, inclusions, exclusions) in &groups {
        let label = choose_column_label(
            schema,
            column,
            one_based,
            inclusions,
            exclusions,
            prev_clause_last,
            tier,
        );
        let (_, next_last) = measure_group_with_label(
            label,
            inclusions,
            exclusions,
            prev_clause_last,
            compact,
            false,
        );
        labels.push(label);
        prev_clause_last = next_last;
    }

    // In compact tiers, greedily spend `spare_atoms` on single-inclusion `Between` expansions
    // that save the most bytes first (deterministic ascending group-index tie-breaker).
    let mut expand_group = vec![false; groups.len()];
    if compact {
        let spare_atoms = MAX_FILTER_EXPR_PREDICATES.saturating_sub(total_predicates);
        if spare_atoms > 0 {
            let mut candidates: Vec<(usize, usize)> = Vec::new();
            for (group_idx, &(_, _, inclusions, _)) in groups.iter().enumerate() {
                if inclusions.len() == 1 && inclusions[0].operator == FilterOperator::Between {
                    let label = labels[group_idx];
                    let (between_len, _) = emit_predicate(inclusions[0], label, true, false, None);
                    let (expanded_len, _) = emit_predicate(inclusions[0], label, true, true, None);
                    if expanded_len < between_len {
                        candidates.push((between_len - expanded_len, group_idx));
                    }
                }
            }
            candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            for &(_, group_idx) in candidates.iter().take(spare_atoms) {
                expand_group[group_idx] = true;
            }
        }
    }

    // Pass 1: measure exact total byte length without allocating the output string.
    let mut total_len = 0usize;
    let mut prev_clause_last: Option<u8> = None;
    for (group_idx, &(_, _, inclusions, exclusions)) in groups.iter().enumerate() {
        let col_label = labels[group_idx];
        let (group_len, next_last) = measure_group_with_label(
            col_label,
            inclusions,
            exclusions,
            prev_clause_last,
            compact,
            expand_group[group_idx],
        );
        total_len = total_len.saturating_add(group_len);
        prev_clause_last = next_last;
        if total_len > MAX_FILTER_EXPR_BYTES {
            return None;
        }
    }

    if total_len > MAX_FILTER_EXPR_BYTES {
        return None;
    }

    // Pass 2: allocate exact bounded capacity and render directly.
    let mut out = String::with_capacity(total_len);
    let mut prev_clause_last: Option<u8> = None;
    for (group_idx, &(_, _, inclusions, exclusions)) in groups.iter().enumerate() {
        let col_label = labels[group_idx];
        if !inclusions.is_empty() {
            if let Some(prev) = prev_clause_last {
                emit_joiner(prev, "AND", col_label.first_byte(), compact, Some(&mut out));
            }
            let mut prev_or_last: Option<u8> = None;
            for pred in inclusions {
                if let Some(prev) = prev_or_last {
                    emit_joiner(prev, "OR", col_label.first_byte(), compact, Some(&mut out));
                }
                let (_, last) = emit_predicate(
                    pred,
                    col_label,
                    compact,
                    expand_group[group_idx],
                    Some(&mut out),
                );
                prev_or_last = Some(last);
            }
            prev_clause_last = prev_or_last;
        }
        for exc in exclusions {
            if let Some(prev) = prev_clause_last {
                emit_joiner(prev, "AND", col_label.first_byte(), compact, Some(&mut out));
            }
            let (_, last) = emit_predicate(exc, col_label, compact, false, Some(&mut out));
            prev_clause_last = Some(last);
        }
    }

    debug_assert_eq!(out.len(), total_len);
    Some(out)
}

/// Prints a [`FilterQuery`] in canonical expression form against `schema`, strictly bounded to
/// [`MAX_FILTER_EXPR_BYTES`] and [`MAX_FILTER_EXPR_PREDICATES`].
///
/// Column groups are emitted in ascending column order, with each column's inclusion predicates
/// joined by `OR` and exclusions (`!=`) and separate columns joined by `AND`. Every expression
/// accepted by [`parse_filter_expr`] prints to a valid, bounded expression that round-trips
/// losslessly through [`parse_filter_expr`]. Oversized, incomplete, or empty queries return an
/// explicit [`FilterExprError`] without allocating unbounded output.
pub fn print_filter_expr(
    query: &FilterQuery,
    schema: &FilterColumnSchema<'_>,
) -> Result<String, FilterExprError> {
    if query.predicates.is_empty() {
        return Err(FilterExprError::new(
            FilterExprSpan::new(0, 0),
            "filter query has no predicates to print",
        ));
    }
    if query.predicates.len() > MAX_FILTER_EXPR_PREDICATES {
        return Err(FilterExprError::new(
            FilterExprSpan::new(0, 0),
            format!(
                "filter query has {} predicates, exceeding the {MAX_FILTER_EXPR_PREDICATES}-predicate expression limit",
                query.predicates.len()
            ),
        ));
    }

    let mut by_column: BTreeMap<usize, (Vec<&FilterPredicate>, Vec<&FilterPredicate>)> =
        BTreeMap::new();
    for predicate in &query.predicates {
        if predicate.column.checked_add(1).is_none() {
            return Err(FilterExprError::new(
                FilterExprSpan::new(0, 0),
                "column index overflows 1-based #N representation",
            ));
        }
        if predicate.operator == FilterOperator::Between && predicate.upper_bound.is_none() {
            return Err(FilterExprError::new(
                FilterExprSpan::new(0, 0),
                "BETWEEN predicate is missing an upper bound",
            ));
        }
        let entry = by_column.entry(predicate.column).or_default();
        if predicate.operator == FilterOperator::NotEquals {
            entry.1.push(predicate);
        } else {
            entry.0.push(predicate);
        }
    }

    let total_predicates = query.predicates.len();
    for tier in [
        PrintTier::PrettyHeaders,
        PrintTier::PrettyIndex,
        PrintTier::Compact,
        PrintTier::CompactReordered,
    ] {
        if let Some(rendered) = try_render_grouped(&by_column, schema, total_predicates, tier) {
            return Ok(rendered);
        }
    }

    Err(FilterExprError::new(
        FilterExprSpan::new(0, 0),
        format!(
            "filter query cannot be printed within the {MAX_FILTER_EXPR_BYTES}-byte expression limit"
        ),
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{
        FilterColumnSchema, MAX_FILTER_EXPR_BYTES, MAX_FILTER_EXPR_PREDICATES, parse_filter_expr,
        print_filter_expr,
    };
    use crate::{
        CaseSensitivity, FilterOperator, FilterPredicate, FilterQuery, HeaderMode, OpenOptions,
        Session,
    };

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

    fn headers(names: &[&[u8]]) -> Vec<Vec<u8>> {
        names.iter().map(|s| s.to_vec()).collect()
    }

    #[test]
    fn parses_all_operators_column_refs_and_canonicalizes_equivalent_forms() {
        let hdrs = headers(&[
            b"state",
            b"order id",
            b"amount",
            b"and",
            b"a]b",
            b"dup",
            b"dup",
            b"code",
        ]);
        let schema = FilterColumnSchema::with_headers(&hdrs, Some(hdrs.len()));

        let expr = "  amount >= -12.5  and  amount <= 1e3  AND  #1 == \"TX\" or state == \"FL\" AND state != \"CA\" AND [order id] contains \"inv\" AND [and] != \"\" AND [a]]b] == \"x\" AND #6 == \"first\" AND code =~ /^ERR-\\d+$/ ";
        let parsed = parse_filter_expr(expr, &schema, CaseSensitivity::Sensitive).unwrap();
        let printed = print_filter_expr(&parsed, &schema).unwrap();
        assert_eq!(
            printed,
            "state == \"TX\" OR state == \"FL\" AND state != \"CA\" AND [order id] CONTAINS \"inv\" AND amount BETWEEN -12.5 AND 1e3 AND [and] != \"\" AND [a]]b] == \"x\" AND #6 == \"first\" AND code =~ \"^ERR-\\\\d+$\""
        );

        // Idempotent canonical printing: print(parse(printed)) == printed.
        let reparsed = parse_filter_expr(&printed, &schema, CaseSensitivity::Sensitive).unwrap();
        assert_eq!(reparsed, parsed);
        assert_eq!(print_filter_expr(&reparsed, &schema).unwrap(), printed);

        // Reversed inclusive bounds (`<= upper AND >= lower`) fold to the exact same canonical Between.
        let reversed = parse_filter_expr(
            "amount <= 1e3 AND amount >= -12.5",
            &schema,
            CaseSensitivity::Sensitive,
        )
        .unwrap();
        assert_eq!(
            print_filter_expr(&reversed, &schema).unwrap(),
            "amount BETWEEN -12.5 AND 1e3"
        );
    }

    #[test]
    fn rejects_unrepresentable_ambiguous_and_malformed_expressions_with_spans() {
        let hdrs = headers(&[b"a", b"b", b"dup", b"dup"]);
        let schema = FilterColumnSchema::with_headers(&hdrs, Some(hdrs.len()));

        let cases: &[(&str, &str)] = &[
            ("", "must not be empty"),
            ("   \t  ", "must not be empty"),
            ("a == \"1\" OR b == \"2\"", "same column"),
            ("(a == \"1\")", "parentheses are not supported"),
            ("NOT a == \"1\"", "NOT is not supported"),
            ("!a == \"1\"", "NOT is not supported"),
            (
                "a != \"1\" OR a != \"2\"",
                "'!=' cannot be combined with OR",
            ),
            (
                "a == \"1\" OR a != \"2\"",
                "'!=' cannot be combined with OR",
            ),
            ("a > 1 AND a < 5", "exclusive range bounds"),
            ("a >= 1 AND a < 5", "exclusive range bounds"),
            ("a > 1 AND a <= 5", "exclusive range bounds"),
            (
                "a == \"x\" AND a == \"y\"",
                "cannot combine multiple inclusion clauses",
            ),
            (
                "a >= 1 AND a <= 5 AND a == \"3\"",
                "cannot combine multiple inclusion clauses",
            ),
            (
                "a >= 10 AND a <= 5",
                "less than or equal to its upper bound",
            ),
            (
                "dup == \"x\"",
                "ambiguous column name 'dup' matches columns #3, #4",
            ),
            ("missing == \"x\"", "unknown column 'missing'"),
            ("#0 == \"x\"", "start at #1"),
            ("#5 == \"x\"", "out of range"),
            ("a = \"x\"", "use '=='"),
            ("a ~= \"x\"", "use '=~'"),
            ("a == 'x'", "double quotes"),
            ("a == 123", "requires a double-quoted string"),
            ("a > \"123\"", "requires an unquoted number"),
            ("a BETWEEN 1 AND \"5\"", "requires an unquoted number"),
            ("a =~ \"[unclosed\"", "invalid regular expression"),
            ("a CONTAINS \"\"", "must not be empty"),
            (
                "[unterminated == \"x\"",
                "unterminated bracketed column name",
            ),
            ("[] == \"x\"", "must not be empty"),
            ("a == \"unterminated", "unterminated string literal"),
            ("a == \"bad\\q\"", "unknown escape sequence"),
            ("a == \"bad\\xZZ\"", "invalid hex escape"),
            ("a == \"bad\\xé\"", "invalid hex escape"),
            ("a == \"bad\\x0é\"", "invalid hex escape"),
            ("a == \"bad\\x", "invalid hex escape"),
            ("a == \"bad\\x0", "invalid hex escape"),
            ("a == \"raw\x01ctrl\"", "unescaped control character"),
            ("a =~ /raw\x01ctrl/", "unescaped control character"),
            ("[raw\x01hdr] == \"x\"", "control characters"),
            ("a =~ /unterminated", "unterminated regex literal"),
            ("a > 123abc", "invalid numeric literal"),
            ("a > 1e", "missing exponent digits"),
        ];

        for &(expr, expected_substr) in cases {
            let err = parse_filter_expr(expr, &schema, CaseSensitivity::Sensitive)
                .expect_err(&format!("expected error for: {expr}"));
            assert!(
                err.message.contains(expected_substr),
                "expr `{expr}`: expected `{expected_substr}`, got `{}`",
                err.message
            );
            assert!(
                expr.get(err.span.start..err.span.end).is_some(),
                "invalid or non-UTF8-boundary span {:?} for `{expr}`",
                err.span
            );
        }

        // Headerless schema rejects column names and accepts #N.
        let headerless = FilterColumnSchema::headerless(Some(2));
        let err =
            parse_filter_expr("a == \"1\"", &headerless, CaseSensitivity::Sensitive).unwrap_err();
        assert!(err.message.contains("no header row"));
        let ok = parse_filter_expr("#2 == \"1\"", &headerless, CaseSensitivity::Sensitive).unwrap();
        assert_eq!(print_filter_expr(&ok, &headerless).unwrap(), "#2 == \"1\"");

        // Expression byte cap and predicate count cap.
        let huge = format!("a == \"{}\"", "x".repeat(MAX_FILTER_EXPR_BYTES));
        assert!(parse_filter_expr(&huge, &schema, CaseSensitivity::Sensitive).is_err());

        let too_many_preds = (0..=MAX_FILTER_EXPR_PREDICATES)
            .map(|i| format!("a == \"{i}\""))
            .collect::<Vec<_>>()
            .join(" OR ");
        assert!(
            parse_filter_expr(&too_many_preds, &schema, CaseSensitivity::Sensitive)
                .unwrap_err()
                .message
                .contains("maximum")
        );

        // Pre-resolution / pre-folding predicate cap:
        // 1) 128 inclusive range pairs (256 input predicates) succeed and fold to 128 BETWEEN predicates.
        let open_schema = FilterColumnSchema::headerless(None);
        let max_pairs = (1..=128)
            .map(|c| format!("#{c} >= 1 AND #{c} <= 10"))
            .collect::<Vec<_>>()
            .join(" AND ");
        let folded_max = parse_filter_expr(&max_pairs, &open_schema, CaseSensitivity::Sensitive)
            .expect("256 input predicates should succeed");
        assert_eq!(folded_max.predicates.len(), 128);

        // 2) 129 inclusive range pairs (258 input predicates) must be rejected at atom 257 BEFORE folding.
        let over_pairs = (1..=129)
            .map(|c| format!("#{c} >= 1 AND #{c} <= 10"))
            .collect::<Vec<_>>()
            .join(" AND ");
        let over_err = parse_filter_expr(&over_pairs, &open_schema, CaseSensitivity::Sensitive)
            .expect_err("258 input predicates must be rejected even if they would fold to 129");
        assert!(over_err.message.contains("maximum"));
        assert!(
            over_pairs
                .get(over_err.span.start..over_err.span.end)
                .is_some()
        );

        // 3) Atom 257 is rejected BEFORE resolving its column name (unknown column does not mask limit).
        let mut or_chain = (0..MAX_FILTER_EXPR_PREDICATES)
            .map(|i| format!("a == \"{i}\""))
            .collect::<Vec<_>>()
            .join(" OR ");
        or_chain.push_str(" OR unknown_col_257 == \"x\"");
        let pre_res_err = parse_filter_expr(&or_chain, &schema, CaseSensitivity::Sensitive)
            .expect_err("atom 257 must fail max-predicates before name resolution");
        assert!(
            pre_res_err.message.contains("maximum"),
            "expected max-predicates error before unknown-column error, got: {}",
            pre_res_err.message
        );
        assert!(
            or_chain
                .get(pre_res_err.span.start..pre_res_err.span.end)
                .is_some()
        );
    }

    #[test]
    fn printer_bounds_huge_headers_preserves_max_byte_round_trip_and_rejects_oversized_legacy_queries()
     {
        // 1. Giant 2 MiB unique header falls back to #1 without allocating the header.
        let giant_hdrs = vec![vec![b'h'; 2 * 1024 * 1024], b"state".to_vec()];
        let giant_schema = FilterColumnSchema::with_headers(&giant_hdrs, Some(2));
        let q = parse_filter_expr(
            "#1 == \"TX\" AND state == \"FL\"",
            &giant_schema,
            CaseSensitivity::Sensitive,
        )
        .unwrap();
        let printed = print_filter_expr(&q, &giant_schema).unwrap();
        assert_eq!(printed, "#1 == \"TX\" AND state == \"FL\"");
        assert_eq!(
            parse_filter_expr(&printed, &giant_schema, CaseSensitivity::Sensitive).unwrap(),
            q
        );

        // 2. Medium 1 KiB unique header repeated across 200 predicates falls back cleanly to #1.
        let wide_hdrs = vec![vec![b'k'; 1024]];
        let wide_schema = FilterColumnSchema::with_headers(&wide_hdrs, Some(1));
        let many_preds = (0..200)
            .map(|i| format!("#1 == \"{i}\""))
            .collect::<Vec<_>>()
            .join(" OR ");
        let q_many =
            parse_filter_expr(&many_preds, &wide_schema, CaseSensitivity::Sensitive).unwrap();
        let printed_many = print_filter_expr(&q_many, &wide_schema).unwrap();
        assert!(printed_many.len() <= MAX_FILTER_EXPR_BYTES);
        assert!(printed_many.starts_with("#1 == \"0\""));
        assert_eq!(
            parse_filter_expr(&printed_many, &wide_schema, CaseSensitivity::Sensitive).unwrap(),
            q_many
        );

        // 3. Exact 64 KiB compact expression (`#1=="..."`) prints as a parseable <= 64 KiB string.
        let headerless = FilterColumnSchema::headerless(Some(10));
        let prefix = "#1==\"";
        let suffix = "\"";
        let fill = "a".repeat(MAX_FILTER_EXPR_BYTES - prefix.len() - suffix.len());
        let exact_compact = format!("{prefix}{fill}{suffix}");
        assert_eq!(exact_compact.len(), MAX_FILTER_EXPR_BYTES);
        let q_exact =
            parse_filter_expr(&exact_compact, &headerless, CaseSensitivity::Sensitive).unwrap();
        let printed_exact = print_filter_expr(&q_exact, &headerless).unwrap();
        assert!(printed_exact.len() <= MAX_FILTER_EXPR_BYTES);
        assert_eq!(
            parse_filter_expr(&printed_exact, &headerless, CaseSensitivity::Sensitive).unwrap(),
            q_exact
        );

        // 4. Exact 64 KiB compact expression on column #10 with 1-byte bare header `a` (shorter than `#10`).
        let mut short_hdrs: Vec<Vec<u8>> =
            (1..=9).map(|i| format!("col{i}").into_bytes()).collect();
        short_hdrs.push(b"a".to_vec());
        let short_schema = FilterColumnSchema::with_headers(&short_hdrs, Some(10));
        let short_prefix = "a==\"";
        let short_fill = "z".repeat(MAX_FILTER_EXPR_BYTES - short_prefix.len() - suffix.len());
        let exact_short = format!("{short_prefix}{short_fill}{suffix}");
        assert_eq!(exact_short.len(), MAX_FILTER_EXPR_BYTES);
        let q_short =
            parse_filter_expr(&exact_short, &short_schema, CaseSensitivity::Sensitive).unwrap();
        let printed_short = print_filter_expr(&q_short, &short_schema).unwrap();
        assert!(printed_short.len() <= MAX_FILTER_EXPR_BYTES);
        assert_eq!(
            parse_filter_expr(&printed_short, &short_schema, CaseSensitivity::Sensitive).unwrap(),
            q_short
        );

        // 5. Legacy queries exceeding MAX_FILTER_EXPR_PREDICATES or MAX_FILTER_EXPR_BYTES return explicit Err.
        let legacy_many = FilterQuery {
            predicates: (0..=MAX_FILTER_EXPR_PREDICATES)
                .map(|i| FilterPredicate {
                    column: 0,
                    operator: FilterOperator::Equals,
                    value: format!("v{i}").into_bytes(),
                    upper_bound: None,
                })
                .collect(),
            case_sensitivity: CaseSensitivity::Sensitive,
        };
        assert!(
            print_filter_expr(&legacy_many, &headerless)
                .unwrap_err()
                .message
                .contains("predicate expression limit")
        );

        let legacy_huge_val =
            FilterQuery::single(0, FilterOperator::Equals, vec![b'x'; MAX_FILTER_EXPR_BYTES]);
        assert!(
            print_filter_expr(&legacy_huge_val, &headerless)
                .unwrap_err()
                .message
                .contains("byte expression limit")
        );

        let empty_query = FilterQuery {
            predicates: Vec::new(),
            case_sensitivity: CaseSensitivity::Sensitive,
        };
        assert!(
            print_filter_expr(&empty_query, &headerless)
                .unwrap_err()
                .message
                .contains("no predicates")
        );

        // 6. Reject `column: usize::MAX` (overflows 1-based `#N`) and `Between` without upper_bound.
        let overflow_col = FilterQuery::single(usize::MAX, FilterOperator::Equals, b"x".to_vec());
        assert!(
            print_filter_expr(&overflow_col, &headerless)
                .unwrap_err()
                .message
                .contains("overflows")
        );

        let missing_upper = FilterQuery::single(0, FilterOperator::Between, b"1".to_vec());
        assert!(
            print_filter_expr(&missing_upper, &headerless)
                .unwrap_err()
                .message
                .contains("upper bound")
        );

        // 7. Compact label separator cost: column 100 named `a-` (`[a-]CONTAINS"..."`, no space needed
        //    vs `#100 CONTAINS"..."` which needs a space) fits exactly at MAX_FILTER_EXPR_BYTES.
        let mut hundred_hdrs: Vec<Vec<u8>> =
            (1..=99).map(|i| format!("col{i}").into_bytes()).collect();
        hundred_hdrs.push(b"a-".to_vec());
        let hundred_schema = FilterColumnSchema::with_headers(&hundred_hdrs, Some(100));
        let br_prefix = "[a-]CONTAINS\"";
        let br_fill = "q".repeat(MAX_FILTER_EXPR_BYTES - br_prefix.len() - suffix.len());
        let exact_bracket = format!("{br_prefix}{br_fill}{suffix}");
        assert_eq!(exact_bracket.len(), MAX_FILTER_EXPR_BYTES);
        let q_bracket =
            parse_filter_expr(&exact_bracket, &hundred_schema, CaseSensitivity::Sensitive).unwrap();
        let printed_bracket = print_filter_expr(&q_bracket, &hundred_schema).unwrap();
        assert!(printed_bracket.len() <= MAX_FILTER_EXPR_BYTES);
        assert_eq!(
            parse_filter_expr(
                &printed_bracket,
                &hundred_schema,
                CaseSensitivity::Sensitive
            )
            .unwrap(),
            q_bracket
        );

        // 8. Greedy spare-atom allocation picks the `Between` expansion saving the most bytes.
        let open_headerless = FilterColumnSchema::headerless(None);
        let prefix_len: usize = "#1 BETWEEN-2 AND-1 AND#2>=1 AND#2<=2 AND".len()
            + (0..252)
                .map(|i| format!("#3==\"{i}\"OR").len())
                .sum::<usize>();
        let pad_len = MAX_FILTER_EXPR_BYTES - prefix_len - "#3==\"\"".len();
        let full_greedy = format!(
            "#1 BETWEEN-2 AND-1 AND#2>=1 AND#2<=2 AND{}#3==\"{}\"",
            (0..252)
                .map(|i| format!("#3==\"{i}\"OR"))
                .collect::<String>(),
            "p".repeat(pad_len)
        );
        // Ensure exact 64 KiB input with 256 input atoms (1 + 2 + 253 = 256).
        assert_eq!(full_greedy.len(), MAX_FILTER_EXPR_BYTES);
        let q_greedy =
            parse_filter_expr(&full_greedy, &open_headerless, CaseSensitivity::Sensitive).unwrap();
        let printed_greedy = print_filter_expr(&q_greedy, &open_headerless).unwrap();
        assert!(printed_greedy.len() <= MAX_FILTER_EXPR_BYTES);
        assert_eq!(
            parse_filter_expr(
                &printed_greedy,
                &open_headerless,
                CaseSensitivity::Sensitive
            )
            .unwrap(),
            q_greedy
        );

        // 9. Reordered compact fallback preserves a 65,536-byte headerless expression where the
        //    string-ending group preceded the numeric-ending group (`#2=="..."AND#1>1`).
        let reorder_suffix = "\"AND#1>1";
        let reorder_prefix = "#2==\"";
        let reorder_fill =
            "r".repeat(MAX_FILTER_EXPR_BYTES - reorder_prefix.len() - reorder_suffix.len());
        let exact_reorder = format!("{reorder_prefix}{reorder_fill}{reorder_suffix}");
        assert_eq!(exact_reorder.len(), MAX_FILTER_EXPR_BYTES);
        let q_reorder =
            parse_filter_expr(&exact_reorder, &open_headerless, CaseSensitivity::Sensitive)
                .unwrap();
        let printed_reorder = print_filter_expr(&q_reorder, &open_headerless).unwrap();
        assert!(printed_reorder.len() <= MAX_FILTER_EXPR_BYTES);
        assert_eq!(
            parse_filter_expr(
                &printed_reorder,
                &open_headerless,
                CaseSensitivity::Sensitive
            )
            .unwrap(),
            q_reorder
        );

        // 10. Same-column exclusion-first compact fallback (`#1!="<65,523 a's>"AND#1>1`):
        //     emitting the exclusion before the numeric inclusion saves the space before `AND`.
        let excl_prefix = "#1!=\"";
        let excl_suffix = "\"AND#1>1";
        let excl_fill = "a".repeat(MAX_FILTER_EXPR_BYTES - excl_prefix.len() - excl_suffix.len());
        let exact_excl_first = format!("{excl_prefix}{excl_fill}{excl_suffix}");
        assert_eq!(exact_excl_first.len(), MAX_FILTER_EXPR_BYTES);
        let q_excl_first = parse_filter_expr(
            &exact_excl_first,
            &open_headerless,
            CaseSensitivity::Sensitive,
        )
        .unwrap();
        let printed_excl_first = print_filter_expr(&q_excl_first, &open_headerless).unwrap();
        assert!(printed_excl_first.len() <= MAX_FILTER_EXPR_BYTES);
        assert_eq!(
            parse_filter_expr(
                &printed_excl_first,
                &open_headerless,
                CaseSensitivity::Sensitive
            )
            .unwrap(),
            q_excl_first
        );
    }

    #[test]
    fn round_trip_preserves_exact_ordered_row_ids_across_all_operators_and_edge_bytes() {
        let mut csv = Vec::new();
        csv.extend_from_slice(b"state,amount,note,dup,dup\n");
        csv.extend_from_slice(b"TX,10,\"hello\nworld\",d1,e1\n");
        csv.extend_from_slice(b"FL,25,\"say \"\"hi\"\"\",d2,e2\n");
        csv.extend_from_slice(b"CA,50,plain,d3,e3\n");
        csv.extend_from_slice(b"TX,100,\xffraw\xfe,d4,e4\n");
        csv.extend_from_slice(b"NY,-5,ERR-007,d5,e5\n");
        csv.extend_from_slice(b"TX,25,skipme,d6,e6\n");

        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "quarry-filter-expr-rt-{}-{id}.csv",
            std::process::id()
        ));
        fs::write(&path, &csv).unwrap();

        let session = Session::open(
            &path,
            OpenOptions {
                delimiter: Some(b','),
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let schema = session.filter_column_schema();

        let queries = vec![
            // Interleaved columns + same-column OR + exclusion + numeric Between + duplicate header.
            FilterQuery {
                predicates: vec![
                    FilterPredicate {
                        column: 3,
                        operator: FilterOperator::NotEquals,
                        value: b"d6".to_vec(),
                        upper_bound: None,
                    },
                    FilterPredicate {
                        column: 0,
                        operator: FilterOperator::Equals,
                        value: b"TX".to_vec(),
                        upper_bound: None,
                    },
                    FilterPredicate {
                        column: 1,
                        operator: FilterOperator::Between,
                        value: b" 10 ".to_vec(),
                        upper_bound: Some(b" 100 ".to_vec()),
                    },
                    FilterPredicate {
                        column: 0,
                        operator: FilterOperator::Equals,
                        value: b"FL".to_vec(),
                        upper_bound: None,
                    },
                ],
                case_sensitivity: CaseSensitivity::Sensitive,
            },
            // Raw invalid UTF-8 bytes in Equals + regex on another column.
            FilterQuery {
                predicates: vec![
                    FilterPredicate {
                        column: 2,
                        operator: FilterOperator::Equals,
                        value: b"\xffraw\xfe".to_vec(),
                        upper_bound: None,
                    },
                    FilterPredicate {
                        column: 0,
                        operator: FilterOperator::Regex,
                        value: b"^(TX|NY)$".to_vec(),
                        upper_bound: None,
                    },
                ],
                case_sensitivity: CaseSensitivity::Sensitive,
            },
            // Multiline quoted value + escaped quote value ORed on same column.
            FilterQuery {
                predicates: vec![
                    FilterPredicate {
                        column: 2,
                        operator: FilterOperator::Equals,
                        value: b"hello\nworld".to_vec(),
                        upper_bound: None,
                    },
                    FilterPredicate {
                        column: 2,
                        operator: FilterOperator::Equals,
                        value: b"say \"hi\"".to_vec(),
                        upper_bound: None,
                    },
                ],
                case_sensitivity: CaseSensitivity::Sensitive,
            },
            // Disjoint numeric ranges on the same column + exclusion.
            FilterQuery {
                predicates: vec![
                    FilterPredicate {
                        column: 1,
                        operator: FilterOperator::LessThan,
                        value: b"0".to_vec(),
                        upper_bound: None,
                    },
                    FilterPredicate {
                        column: 1,
                        operator: FilterOperator::GreaterThan,
                        value: b"50".to_vec(),
                        upper_bound: None,
                    },
                    FilterPredicate {
                        column: 0,
                        operator: FilterOperator::NotEquals,
                        value: b"CA".to_vec(),
                        upper_bound: None,
                    },
                ],
                case_sensitivity: CaseSensitivity::Sensitive,
            },
        ];

        for query in queries {
            query.validate().unwrap();
            let printed = print_filter_expr(&query, &schema).unwrap();
            let reparsed = parse_filter_expr(&printed, &schema, query.case_sensitivity).unwrap();
            assert_eq!(print_filter_expr(&reparsed, &schema).unwrap(), printed);

            let orig_idx = session.start_filter(query).unwrap().wait().unwrap();
            let rt_idx = session.start_filter(reparsed).unwrap().wait().unwrap();
            let orig_rows: Vec<u64> = session
                .read_filtered_rows(&orig_idx, 0, 100)
                .unwrap()
                .into_iter()
                .map(|m| m.row)
                .collect();
            let rt_rows: Vec<u64> = session
                .read_filtered_rows(&rt_idx, 0, 100)
                .unwrap()
                .into_iter()
                .map(|m| m.row)
                .collect();
            assert_eq!(orig_rows, rt_rows, "printed expr: {printed}");
            assert!(
                !orig_rows.is_empty(),
                "test query should match rows: {printed}"
            );
        }

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn fuzz_structured_and_mutated_filter_expressions_never_panic_and_hold_invariants() {
        // Deterministic LCG PRNG so fuzzing is 100% reproducible in CI.
        let mut seed: u64 = 0x517C_C1B7_2722_0A95;
        let mut next_u64 = || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            seed
        };

        let hdrs = headers(&[
            b"state",
            b"order id",
            b"amount",
            b"and",
            b"dup",
            b"dup",
            b"bad\xffhdr",
            b"br]ack",
        ]);
        let schema = FilterColumnSchema::with_headers(&hdrs, Some(hdrs.len()));

        // 1. Property test over 300 random valid FilterQuery values:
        //    print -> parse -> print must always succeed and be idempotent.
        let sample_strings: &[&[u8]] = &[
            b"",
            b"TX",
            b"hello world",
            b"line1\nline2\r\t\"\\end",
            b"\x00\x1F\x7F\xFF\xFE",
            "café".as_bytes(),
        ];
        let sample_regexes: &[&[u8]] =
            &[b"^TX$", b"ERR-[0-9]{2,4}", b"a/b\\\\c", "café".as_bytes()];
        let sample_numbers: &[&[u8]] =
            &[b"-100", b"-12.5", b"0", b"42", b"1e3", b"9007199254740993"];

        for _ in 0..300 {
            let pred_count = (next_u64() as usize % 6) + 1;
            let mut predicates = Vec::new();
            let mut regex_count = 0usize;
            for _ in 0..pred_count {
                let col = (next_u64() as usize) % hdrs.len();
                let op_idx = (next_u64() as usize) % 9;
                let (operator, value, upper_bound) = match op_idx {
                    0 => (
                        FilterOperator::Equals,
                        sample_strings[(next_u64() as usize) % sample_strings.len()].to_vec(),
                        None,
                    ),
                    1 => (
                        FilterOperator::NotEquals,
                        sample_strings[(next_u64() as usize) % sample_strings.len()].to_vec(),
                        None,
                    ),
                    2 => {
                        // Contains must be non-empty.
                        let idx = 1 + ((next_u64() as usize) % (sample_strings.len() - 1));
                        (FilterOperator::Contains, sample_strings[idx].to_vec(), None)
                    }
                    3 if regex_count < 4 => {
                        regex_count += 1;
                        (
                            FilterOperator::Regex,
                            sample_regexes[(next_u64() as usize) % sample_regexes.len()].to_vec(),
                            None,
                        )
                    }
                    4 => (
                        FilterOperator::GreaterThan,
                        sample_numbers[(next_u64() as usize) % sample_numbers.len()].to_vec(),
                        None,
                    ),
                    5 => (
                        FilterOperator::GreaterThanOrEqual,
                        sample_numbers[(next_u64() as usize) % sample_numbers.len()].to_vec(),
                        None,
                    ),
                    6 => (
                        FilterOperator::LessThan,
                        sample_numbers[(next_u64() as usize) % sample_numbers.len()].to_vec(),
                        None,
                    ),
                    7 => (
                        FilterOperator::LessThanOrEqual,
                        sample_numbers[(next_u64() as usize) % sample_numbers.len()].to_vec(),
                        None,
                    ),
                    _ => {
                        let lo_idx = (next_u64() as usize) % sample_numbers.len();
                        let hi_idx =
                            lo_idx + ((next_u64() as usize) % (sample_numbers.len() - lo_idx));
                        (
                            FilterOperator::Between,
                            sample_numbers[lo_idx].to_vec(),
                            Some(sample_numbers[hi_idx].to_vec()),
                        )
                    }
                };
                predicates.push(FilterPredicate {
                    column: col,
                    operator,
                    value,
                    upper_bound,
                });
            }
            let case_sensitivity = if next_u64().is_multiple_of(2) {
                CaseSensitivity::Sensitive
            } else {
                CaseSensitivity::Insensitive
            };
            let query = FilterQuery {
                predicates,
                case_sensitivity,
            };
            query.validate().unwrap();
            let printed = print_filter_expr(&query, &schema).unwrap();
            let reparsed = parse_filter_expr(&printed, &schema, case_sensitivity)
                .unwrap_or_else(|e| panic!("failed to reparse `{printed}`: {e}"));
            let reprinted = print_filter_expr(&reparsed, &schema).unwrap();
            assert_eq!(reprinted, printed);
        }

        // 2. Random token-soup fuzzing (500 inputs): must never panic, and every error span
        //    must slice on valid UTF-8 boundaries.
        let fragments = [
            "state",
            "[order id]",
            "[and]",
            "[br]]ack]",
            "#1",
            "#0",
            "#99",
            "dup",
            "==",
            "!=",
            "=",
            "=~",
            "~=",
            "CONTAINS",
            "BETWEEN",
            "AND",
            "OR",
            "NOT",
            "!",
            "(",
            ")",
            ">",
            ">=",
            "<",
            "<=",
            "\"foo\"",
            "\"a\\nb\\xFF\"",
            "\"bad\\xé\"",
            "\"unclosed",
            "/^a+$/",
            "/bad[/",
            "10",
            "-3.5",
            "1e3",
            "1e",
            "123abc",
            "'single'",
            " ",
            "\t",
            "\n",
            "\\",
            "[]",
        ];
        for _ in 0..500 {
            let len = (next_u64() as usize) % 10;
            let mut expr = String::new();
            for j in 0..len {
                if j > 0 && !next_u64().is_multiple_of(4) {
                    expr.push(' ');
                }
                expr.push_str(fragments[(next_u64() as usize) % fragments.len()]);
            }
            match parse_filter_expr(&expr, &schema, CaseSensitivity::Sensitive) {
                Ok(q) => {
                    q.validate().unwrap();
                    let canonical = print_filter_expr(&q, &schema).unwrap();
                    let reparsed =
                        parse_filter_expr(&canonical, &schema, CaseSensitivity::Sensitive)
                            .unwrap_or_else(|e| {
                                panic!(
                                    "accepted `{expr}` printed `{canonical}` failed reparse: {e}"
                                )
                            });
                    assert_eq!(reparsed, q);
                    assert_eq!(print_filter_expr(&reparsed, &schema).unwrap(), canonical);
                }
                Err(err) => {
                    assert!(
                        expr.get(err.span.start..err.span.end).is_some(),
                        "non-UTF8-boundary span {:?} for `{expr}`",
                        err.span
                    );
                }
            }
        }

        // 3. Seeded valid-expression mutation fuzzing (500 inputs): mutates real user expressions
        //    so both accepted and rejected branches are guaranteed to execute and hold invariants.
        let seeds = [
            "state == \"TX\"",
            "state == \"TX\" OR state == \"FL\" AND state != \"CA\"",
            "[order id] CONTAINS \"inv\" AND amount >= -12.5 AND amount <= 1e3",
            "[and] == \"yes\" AND [br]]ack] =~ /^ERR-\\d+$/ AND #5 != \"x\"",
            "#1==\"TX\"OR#1==\"NY\"AND#3 BETWEEN 1 AND 100",
        ];
        let mut mutated_accepted = 0usize;
        let mut mutated_rejected = 0usize;
        for _ in 0..500 {
            let base = seeds[(next_u64() as usize) % seeds.len()];
            let expr = match next_u64() % 6 {
                0 => base.replace(" AND ", " and ").replace(" OR ", " or "),
                1 => base.replace(" == ", "==").replace(" != ", "!="),
                2 => format!("  {base}  "),
                3 => {
                    // Truncate at a random valid char boundary.
                    let boundaries: Vec<usize> = base.char_indices().map(|(i, _)| i).collect();
                    let cut = boundaries[(next_u64() as usize) % boundaries.len()];
                    base[..cut].to_owned()
                }
                4 => {
                    // Splice a random fragment into a valid char boundary.
                    let boundaries: Vec<usize> = base.char_indices().map(|(i, _)| i).collect();
                    let cut = boundaries[(next_u64() as usize) % boundaries.len()];
                    let frag = fragments[(next_u64() as usize) % fragments.len()];
                    format!("{}{frag}{}", &base[..cut], &base[cut..])
                }
                _ => base.replace("==", "=~"),
            };
            match parse_filter_expr(&expr, &schema, CaseSensitivity::Sensitive) {
                Ok(q) => {
                    mutated_accepted += 1;
                    q.validate().unwrap();
                    let canonical = print_filter_expr(&q, &schema).unwrap();
                    assert!(canonical.len() <= MAX_FILTER_EXPR_BYTES);
                    let reparsed = parse_filter_expr(
                        &canonical,
                        &schema,
                        CaseSensitivity::Sensitive,
                    )
                    .unwrap_or_else(|e| {
                        panic!(
                            "mutated accepted `{expr}` printed `{canonical}` failed reparse: {e}"
                        )
                    });
                    assert_eq!(reparsed, q);
                    assert_eq!(print_filter_expr(&reparsed, &schema).unwrap(), canonical);
                }
                Err(err) => {
                    mutated_rejected += 1;
                    assert!(
                        expr.get(err.span.start..err.span.end).is_some(),
                        "non-UTF8-boundary span {:?} for mutated `{expr}`",
                        err.span
                    );
                }
            }
        }
        assert!(
            mutated_accepted > 20 && mutated_rejected > 20,
            "mutation fuzzing must cover both branches (accepted={mutated_accepted}, rejected={mutated_rejected})"
        );
    }
}
