//! Bounded, zero-heavy-dependency Cell Inspector lexers, streaming RFC 8259 JSON
//! pretty-printer, background line-start indexer, and hover-to-peek builder.
//!
//! All analysis is strictly bounded by byte, line, nesting, and token budgets so
//! adversarial inputs (such as a 10 MiB single-line cell or deeply nested JSON)
//! never stall the UI or exhaust memory.

use std::borrow::Cow;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::theme::{Palette, Rgba, ThemeAppearance};
use crate::word_wrap::utf8_bounded_prefix_len;

/// Maximum prefix inspected on the UI thread to classify cell format.
pub const MAX_FORMAT_DETECT_BYTES: usize = 4096;

/// Maximum bytes materialized for a Hover-to-Peek tooltip (zero file I/O).
pub const MAX_PEEK_BYTES: usize = 1024;

/// Maximum lines included in a Hover-to-Peek tooltip.
pub const MAX_PEEK_LINES: usize = 8;

/// Maximum characters per line in a Hover-to-Peek tooltip.
pub const MAX_PEEK_LINE_CHARS: usize = 120;

/// Maximum bytes decoded, tokenized, and laid out per displayed line in the Cell Inspector.
pub const MAX_INSPECTOR_LINE_BYTES: usize = 4096;

/// Maximum syntax highlight token spans emitted per displayed line.
pub const MAX_INSPECTOR_TOKENS_PER_LINE: usize = 256;

/// Maximum number of line-start offsets indexed for a single cell.
pub const MAX_INSPECTOR_LINE_OFFSETS: usize = 1_000_000;

/// Maximum input bytes eligible for background JSON pretty-printing.
pub const MAX_PRETTY_JSON_INPUT_BYTES: usize = 4 * 1024 * 1024;

/// Maximum output bytes produced by background JSON pretty-printing.
pub const MAX_PRETTY_JSON_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// Maximum JSON container nesting depth permitted by the iterative parser.
pub const MAX_JSON_NESTING_DEPTH: usize = 256;

/// Detected semantic content format for a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CellFormat {
    Json,
    StackTrace,
    Markdown,
    PlainText,
    InvalidUtf8,
}

impl CellFormat {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Json => "JSON",
            Self::StackTrace => "Stack trace",
            Self::Markdown => "Markdown",
            Self::PlainText => "Plain text",
            Self::InvalidUtf8 => "Invalid UTF-8",
        }
    }
}

/// Semantic token role emitted by the bounded per-line lexers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenKind {
    Plain,
    JsonKey,
    JsonString,
    JsonNumber,
    JsonBoolNull,
    Punctuation,
    StackHeader,
    StackLocation,
    StackSymbol,
    MarkdownHeading,
    MarkdownFence,
    MarkdownCode,
    MarkdownMarker,
}

/// A single highlighted byte range within a displayed line slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenSpan {
    pub range: Range<usize>,
    pub kind: TokenKind,
}

/// Result of tokenizing one bounded line slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenizedLine {
    pub text: String,
    pub spans: Vec<TokenSpan>,
    pub truncated_bytes: usize,
    pub next_in_code_fence: bool,
}

/// A single bounded line extracted from raw cell bytes using [`LineIndex`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedLineSlice<'a> {
    pub text: Cow<'a, str>,
    pub total_line_bytes: usize,
    pub truncated_bytes: usize,
    pub had_invalid_utf8: bool,
}

/// Compact line-start index built on a background worker thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineIndex {
    /// Byte offset where each line begins (always starts with `0` for non-empty buffers).
    starts: Vec<usize>,
    /// Line indices whose trimmed text opens or closes a Markdown code fence (```` ``` ```` / `~~~`).
    fence_lines: Vec<usize>,
    total_bytes: usize,
    truncated_lines: bool,
}

impl LineIndex {
    /// Builds a line-start index for `bytes`, checking `cancel` periodically.
    /// Returns `None` if cancelled before completion.
    pub fn build_cancellable(bytes: &[u8], cancel: &AtomicBool) -> Option<Self> {
        if bytes.is_empty() {
            return Some(Self {
                starts: vec![0],
                fence_lines: Vec::new(),
                total_bytes: 0,
                truncated_lines: false,
            });
        }

        let mut starts = vec![0usize];
        let mut fence_lines = Vec::new();
        let mut truncated_lines = false;
        let mut line_start = 0usize;
        let mut line_idx = 0usize;

        for (i, &b) in bytes.iter().enumerate() {
            if (i & 0xFFFF) == 0 && cancel.load(Ordering::Relaxed) {
                return None;
            }
            if b == b'\n' {
                if is_fence_delimiter_bytes(&bytes[line_start..i]) {
                    fence_lines.push(line_idx);
                }
                if i + 1 < bytes.len() {
                    if starts.len() >= MAX_INSPECTOR_LINE_OFFSETS {
                        truncated_lines = true;
                        break;
                    }
                    starts.push(i + 1);
                    line_start = i + 1;
                    line_idx += 1;
                } else {
                    line_start = bytes.len();
                }
            }
        }
        if !truncated_lines
            && line_start < bytes.len()
            && is_fence_delimiter_bytes(&bytes[line_start..])
        {
            fence_lines.push(line_idx);
        }

        Some(Self {
            starts,
            fence_lines,
            total_bytes: bytes.len(),
            truncated_lines,
        })
    }

    pub fn len(&self) -> usize {
        self.starts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    pub fn truncated_lines(&self) -> bool {
        self.truncated_lines
    }

    /// Returns whether `line_idx` starts inside a fenced Markdown code block in O(log F).
    pub fn in_code_fence_before(&self, line_idx: usize) -> bool {
        self.fence_lines.partition_point(|&l| l < line_idx) % 2 == 1
    }

    /// Returns the raw byte range (excluding trailing `\r`/`\n`) for `line_idx`.
    pub fn line_byte_range(&self, bytes: &[u8], line_idx: usize) -> Option<Range<usize>> {
        let start = *self.starts.get(line_idx)?;
        let raw_end = self
            .starts
            .get(line_idx + 1)
            .copied()
            .unwrap_or(bytes.len())
            .min(bytes.len());
        if start > raw_end {
            return Some(raw_end..raw_end);
        }
        let mut end = raw_end;
        if end > start && bytes[end - 1] == b'\n' {
            end -= 1;
        }
        if end > start && bytes[end - 1] == b'\r' {
            end -= 1;
        }
        Some(start..end)
    }

    /// Extracts at most `byte_budget` bytes of `line_idx` as displayable UTF-8 text.
    ///
    /// A 10 MiB single-line cell only touches at most `byte_budget` bytes here.
    pub fn line_slice<'a>(
        &self,
        bytes: &'a [u8],
        line_idx: usize,
        byte_budget: usize,
    ) -> Option<BoundedLineSlice<'a>> {
        let range = self.line_byte_range(bytes, line_idx)?;
        let full = &bytes[range];
        let total_line_bytes = full.len();
        let cap = byte_budget.max(1);
        let prefix_len = if full.len() <= cap {
            full.len()
        } else {
            bounded_slice_end(full, cap)
        };
        let slice = &full[..prefix_len];
        let truncated_bytes = total_line_bytes.saturating_sub(prefix_len);
        match std::str::from_utf8(slice) {
            Ok(valid) => Some(BoundedLineSlice {
                text: Cow::Borrowed(valid),
                total_line_bytes,
                truncated_bytes,
                had_invalid_utf8: false,
            }),
            Err(_) => Some(BoundedLineSlice {
                text: String::from_utf8_lossy(slice),
                total_line_bytes,
                truncated_bytes,
                had_invalid_utf8: true,
            }),
        }
    }
}

fn bounded_slice_end(bytes: &[u8], cap: usize) -> usize {
    if bytes.len() <= cap {
        return bytes.len();
    }
    // Avoid splitting a valid multi-byte UTF-8 sequence at `cap`.
    let candidate = &bytes[..cap];
    match std::str::from_utf8(candidate) {
        Ok(_) => cap,
        Err(err) => {
            if err.error_len().is_none() && err.valid_up_to() > 0 {
                err.valid_up_to()
            } else {
                cap
            }
        }
    }
}

fn is_fence_delimiter_bytes(line: &[u8]) -> bool {
    let trimmed = trim_ascii_start(line);
    trimmed.starts_with(b"```") || trimmed.starts_with(b"~~~")
}

fn trim_ascii_start(mut bytes: &[u8]) -> &[u8] {
    while let Some((&first, rest)) = bytes.split_first() {
        if first.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    bytes
}

/// Classifies `raw` into a [`CellFormat`] using at most [`MAX_FORMAT_DETECT_BYTES`].
pub fn detect_cell_format(raw: &[u8]) -> CellFormat {
    if raw.is_empty() {
        return CellFormat::PlainText;
    }
    let prefix_len = bounded_slice_end(raw, MAX_FORMAT_DETECT_BYTES);
    let prefix = &raw[..prefix_len];
    let Ok(text) = std::str::from_utf8(prefix) else {
        return CellFormat::InvalidUtf8;
    };

    let trimmed = text.trim_start_matches(|c: char| c.is_ascii_whitespace());
    if looks_like_json_prefix(trimmed) {
        return CellFormat::Json;
    }
    if looks_like_stack_trace(text) {
        return CellFormat::StackTrace;
    }
    if looks_like_markdown(text) {
        return CellFormat::Markdown;
    }
    CellFormat::PlainText
}

fn looks_like_json_prefix(trimmed: &str) -> bool {
    let bytes = trimmed.as_bytes();
    let Some(&first) = bytes.first() else {
        return false;
    };
    if first != b'{' && first != b'[' {
        return false;
    }
    let rest = trimmed[1..].trim_start_matches(|c: char| c.is_ascii_whitespace());
    let Some(&second) = rest.as_bytes().first() else {
        return true;
    };
    if first == b'{' {
        matches!(second, b'"' | b'}')
    } else {
        matches!(
            second,
            b']' | b'"' | b'{' | b'[' | b'-' | b'0'..=b'9' | b't' | b'f' | b'n'
        )
    }
}

fn looks_like_stack_trace(text: &str) -> bool {
    let mut has_java_at = false;
    let mut has_python_header = false;
    let mut has_python_file = false;
    let mut has_rust_panic = false;
    let mut has_rust_frame = false;

    for line in text.lines().take(32) {
        let trimmed = line.trim_start();
        if trimmed.starts_with("Traceback (most recent call last):") {
            has_python_header = true;
        }
        if trimmed.starts_with("File \"") && trimmed.contains(", line ") {
            has_python_file = true;
        }
        if (line.starts_with('\t') || line.starts_with("  "))
            && trimmed.starts_with("at ")
            && (trimmed.contains(".java:")
                || trimmed.contains(".kt:")
                || trimmed.contains(".scala:")
                || trimmed.contains(".rs:")
                || trimmed.contains("(Native Method)")
                || trimmed.contains("(Unknown Source)"))
        {
            has_java_at = true;
        }
        if trimmed.starts_with("Caused by:")
            && (trimmed.contains("Exception") || trimmed.contains("Error"))
        {
            has_java_at = true;
        }
        if trimmed.starts_with("thread '") && trimmed.contains("panicked at") {
            has_rust_panic = true;
        }
        if trimmed == "stack backtrace:" {
            has_rust_frame = true;
        }
        if is_rust_numbered_frame(trimmed) {
            has_rust_frame = true;
        }
    }

    has_java_at || has_python_header || has_python_file || has_rust_panic || has_rust_frame
}

fn is_rust_numbered_frame(trimmed: &str) -> bool {
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    i > 0
        && i + 2 < bytes.len()
        && bytes[i] == b':'
        && bytes[i + 1] == b' '
        && trimmed[i + 2..].contains("::")
}

fn looks_like_markdown(text: &str) -> bool {
    let mut has_heading = false;
    let mut has_fence = false;
    let mut list_or_quote_lines = 0usize;
    for line in text.lines().take(32) {
        let trimmed = line.trim_start();
        if is_markdown_heading(trimmed) {
            has_heading = true;
        }
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            has_fence = true;
        }
        if trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed.starts_with("> ")
            || is_ordered_list_marker(trimmed).is_some()
        {
            list_or_quote_lines += 1;
        }
    }
    has_heading || has_fence || list_or_quote_lines >= 2
}

fn is_markdown_heading(trimmed: &str) -> bool {
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    (1..=6).contains(&hashes) && trimmed.as_bytes().get(hashes) == Some(&b' ')
}

fn is_ordered_list_marker(trimmed: &str) -> Option<usize> {
    let digits = trimmed.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 || digits > 6 {
        return None;
    }
    let rest = &trimmed.as_bytes()[digits..];
    if rest.len() >= 2 && rest[0] == b'.' && rest[1] == b' ' {
        Some(digits + 2)
    } else {
        None
    }
}

/// Tokenizes a single line according to `format`, enforcing `byte_budget` and
/// [`MAX_INSPECTOR_TOKENS_PER_LINE`].
pub fn tokenize_line(
    line: &str,
    format: CellFormat,
    in_code_fence: bool,
    byte_budget: usize,
) -> TokenizedLine {
    let cap = byte_budget.max(1);
    let prefix_len = utf8_bounded_prefix_len(line.as_bytes(), cap);
    let slice = &line[..prefix_len];
    let truncated_bytes = line.len().saturating_sub(prefix_len);

    let (spans, next_in_code_fence) = match format {
        CellFormat::Json => (tokenize_json_line(slice), false),
        CellFormat::StackTrace => (tokenize_stack_trace_line(slice), false),
        CellFormat::Markdown => tokenize_markdown_line(slice, in_code_fence),
        CellFormat::PlainText | CellFormat::InvalidUtf8 => (Vec::new(), false),
    };

    TokenizedLine {
        text: slice.to_owned(),
        spans,
        truncated_bytes,
        next_in_code_fence,
    }
}

fn push_span(spans: &mut Vec<TokenSpan>, range: Range<usize>, kind: TokenKind) {
    if range.is_empty() || spans.len() >= MAX_INSPECTOR_TOKENS_PER_LINE {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.kind == kind
        && last.range.end == range.start
    {
        last.range.end = range.end;
        return;
    }
    spans.push(TokenSpan { range, kind });
}

fn tokenize_json_line(line: &str) -> Vec<TokenSpan> {
    let mut spans = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;

    while i < bytes.len() && spans.len() < MAX_INSPECTOR_TOKENS_PER_LINE {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        match b {
            b'{' | b'}' | b'[' | b']' | b':' | b',' => {
                push_span(&mut spans, i..i + 1, TokenKind::Punctuation);
                i += 1;
            }
            b'"' => {
                let start = i;
                i += 1;
                let mut escaped = false;
                while i < bytes.len() {
                    let ch = bytes[i];
                    if escaped {
                        escaped = false;
                    } else if ch == b'\\' {
                        escaped = true;
                    } else if ch == b'"' {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                let mut look = i;
                while look < bytes.len() && bytes[look].is_ascii_whitespace() {
                    look += 1;
                }
                let kind = if bytes.get(look) == Some(&b':') {
                    TokenKind::JsonKey
                } else {
                    TokenKind::JsonString
                };
                push_span(&mut spans, start..i, kind);
            }
            b'-' | b'0'..=b'9' => {
                let start = i;
                i += 1;
                while i < bytes.len()
                    && matches!(bytes[i], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
                {
                    i += 1;
                }
                push_span(&mut spans, start..i, TokenKind::JsonNumber);
            }
            b't' if bytes[i..].starts_with(b"true")
                && !bytes.get(i + 4).is_some_and(u8::is_ascii_alphanumeric) =>
            {
                push_span(&mut spans, i..i + 4, TokenKind::JsonBoolNull);
                i += 4;
            }
            b'f' if bytes[i..].starts_with(b"false")
                && !bytes.get(i + 5).is_some_and(u8::is_ascii_alphanumeric) =>
            {
                push_span(&mut spans, i..i + 5, TokenKind::JsonBoolNull);
                i += 5;
            }
            b'n' if bytes[i..].starts_with(b"null")
                && !bytes.get(i + 4).is_some_and(u8::is_ascii_alphanumeric) =>
            {
                push_span(&mut spans, i..i + 4, TokenKind::JsonBoolNull);
                i += 4;
            }
            _ => {
                let ch_len = line[i..].chars().next().map_or(1, char::len_utf8);
                i += ch_len;
            }
        }
    }

    spans
}

fn tokenize_stack_trace_line(line: &str) -> Vec<TokenSpan> {
    let mut spans = Vec::new();
    let leading = line.len() - line.trim_start().len();
    let trimmed = &line[leading..];

    if trimmed.is_empty() {
        return spans;
    }

    // Python traceback header / Rust panic / Java Caused by / Exception header.
    if trimmed.starts_with("Traceback (most recent call last):")
        || trimmed.starts_with("Caused by:")
        || (trimmed.starts_with("thread '") && trimmed.contains("panicked at"))
        || trimmed == "stack backtrace:"
        || is_exception_header_line(trimmed)
    {
        push_span(&mut spans, leading..line.len(), TokenKind::StackHeader);
        return spans;
    }

    // Python frame: `  File "/path/foo.py", line 42, in func_name`
    if let Some(rest) = trimmed.strip_prefix("File \"") {
        let file_start = leading + 5;
        if let Some(close_quote) = rest.find('"') {
            let file_end = file_start + 1 + close_quote + 1;
            let mut loc_end = file_end;
            let after_quote = &line[file_end..];
            if let Some(line_rest) = after_quote.strip_prefix(", line ") {
                let digits = line_rest.bytes().take_while(u8::is_ascii_digit).count();
                loc_end = file_end + ", line ".len() + digits;
            }
            push_span(&mut spans, file_start..loc_end, TokenKind::StackLocation);
            if let Some(in_idx) = line[loc_end..].find(", in ") {
                let sym_start = loc_end + in_idx + ", in ".len();
                if sym_start < line.len() {
                    push_span(&mut spans, sym_start..line.len(), TokenKind::StackSymbol);
                }
            }
            return spans;
        }
    }

    // Java / Kotlin / Rust `at ...` frame.
    if let Some(after_at) = trimmed.strip_prefix("at ") {
        let at_start = leading;
        let sym_start = at_start + 3;
        if let Some(open_paren) = after_at.find('(')
            && after_at.ends_with(')')
        {
            let paren_abs = sym_start + open_paren;
            push_span(&mut spans, sym_start..paren_abs, TokenKind::StackSymbol);
            push_span(&mut spans, paren_abs..line.len(), TokenKind::StackLocation);
            return spans;
        }
        // Rust backtrace location line: `             at /path/file.rs:12:5`
        if after_at.contains(':') && (after_at.contains('/') || after_at.contains('\\')) {
            push_span(&mut spans, sym_start..line.len(), TokenKind::StackLocation);
            return spans;
        }
        push_span(&mut spans, sym_start..line.len(), TokenKind::StackSymbol);
        return spans;
    }

    // Rust numbered frame: `   0: crate::module::func`
    if is_rust_numbered_frame(trimmed)
        && let Some(colon) = trimmed.find(": ")
    {
        let sym_start = leading + colon + 2;
        push_span(&mut spans, sym_start..line.len(), TokenKind::StackSymbol);
    }

    spans
}

fn is_exception_header_line(trimmed: &str) -> bool {
    let head = trimmed.split(':').next().unwrap_or(trimmed);
    !head.is_empty()
        && !head.contains(' ')
        && (head.ends_with("Exception") || head.ends_with("Error") || head.ends_with("Throwable"))
}

fn tokenize_markdown_line(line: &str, in_code_fence: bool) -> (Vec<TokenSpan>, bool) {
    let mut spans = Vec::new();
    let leading = line.len() - line.trim_start().len();
    let trimmed = &line[leading..];

    if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
        push_span(&mut spans, leading..line.len(), TokenKind::MarkdownFence);
        return (spans, !in_code_fence);
    }
    if in_code_fence {
        if !line.is_empty() {
            push_span(&mut spans, 0..line.len(), TokenKind::MarkdownCode);
        }
        return (spans, true);
    }
    if is_markdown_heading(trimmed) {
        push_span(&mut spans, leading..line.len(), TokenKind::MarkdownHeading);
        return (spans, false);
    }

    let mut content_start = leading;
    if trimmed.starts_with("> ") || trimmed.starts_with("- ") || trimmed.starts_with("* ") {
        push_span(&mut spans, leading..leading + 2, TokenKind::MarkdownMarker);
        content_start = leading + 2;
    } else if let Some(marker_len) = is_ordered_list_marker(trimmed) {
        push_span(
            &mut spans,
            leading..leading + marker_len,
            TokenKind::MarkdownMarker,
        );
        content_start = leading + marker_len;
    }

    // Highlight inline `code` spans on the remaining text.
    let bytes = line.as_bytes();
    let mut i = content_start;
    while i < bytes.len() && spans.len() < MAX_INSPECTOR_TOKENS_PER_LINE {
        if bytes[i] == b'`' {
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i] != b'`' {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'`' {
                i += 1;
                push_span(&mut spans, start..i, TokenKind::MarkdownCode);
                continue;
            }
        }
        i += 1;
    }

    (spans, false)
}

/// Exact error location returned when JSON pretty-printing fails on invalid JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonPrettyError {
    pub message: String,
    pub byte_offset: usize,
    /// 1-based line number in the raw cell text.
    pub line: usize,
    /// 1-based column number (Unicode scalar offset on `line`).
    pub column: usize,
}

impl JsonPrettyError {
    pub fn display_summary(&self) -> String {
        format!(
            "Invalid JSON at line {}, column {} (byte {}): {}",
            self.line, self.column, self.byte_offset, self.message
        )
    }
}

/// Outcome of background cancellable JSON pretty-printing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrettyJsonResult {
    Ready {
        text: Arc<String>,
        lines: LineIndex,
    },
    Invalid(JsonPrettyError),
    TooLarge {
        actual_bytes: usize,
        limit_bytes: usize,
    },
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonContainer {
    Object {
        expect_key_or_end: bool,
        count: usize,
    },
    Array {
        count: usize,
    },
}

/// Iterative, cancellable RFC 8259 streaming JSON validator and pretty-printer.
///
/// Preserves duplicate object keys in source order, preserves exact numeric
/// lexemes verbatim (e.g. `1.00`, `1e500`, arbitrary-precision integers), and
/// never recurses on the call stack.
pub fn pretty_print_json_streaming(input: &[u8], cancel: &AtomicBool) -> PrettyJsonResult {
    if input.len() > MAX_PRETTY_JSON_INPUT_BYTES {
        return PrettyJsonResult::TooLarge {
            actual_bytes: input.len(),
            limit_bytes: MAX_PRETTY_JSON_INPUT_BYTES,
        };
    }
    let Ok(text) = std::str::from_utf8(input) else {
        return PrettyJsonResult::Invalid(JsonPrettyError {
            message: "cell contains invalid UTF-8 bytes".into(),
            byte_offset: 0,
            line: 1,
            column: 1,
        });
    };

    let bytes = text.as_bytes();
    let mut out = String::with_capacity(bytes.len().min(64 * 1024));
    let mut stack: Vec<JsonContainer> = Vec::new();
    let mut pos = 0usize;
    let mut expect_value = true;
    let mut expect_colon = false;
    let mut expect_comma_or_end = false;
    let mut steps = 0usize;

    let make_err = |offset: usize, msg: &str| {
        let (line, column) = line_col_at_offset(text, offset);
        PrettyJsonResult::Invalid(JsonPrettyError {
            message: msg.to_owned(),
            byte_offset: offset,
            line,
            column,
        })
    };

    loop {
        steps += 1;
        if (steps & 0x3FF) == 0 && cancel.load(Ordering::Relaxed) {
            return PrettyJsonResult::Cancelled;
        }
        if out.len() > MAX_PRETTY_JSON_OUTPUT_BYTES {
            return PrettyJsonResult::TooLarge {
                actual_bytes: out.len(),
                limit_bytes: MAX_PRETTY_JSON_OUTPUT_BYTES,
            };
        }

        skip_json_ws(bytes, &mut pos);

        if expect_colon {
            if bytes.get(pos) != Some(&b':') {
                return make_err(pos, "expected ':' after object key");
            }
            pos += 1;
            out.push_str(": ");
            expect_colon = false;
            expect_value = true;
            continue;
        }

        if expect_comma_or_end {
            let Some(&container) = stack.last() else {
                if pos == bytes.len() {
                    break;
                }
                return make_err(pos, "unexpected trailing characters after JSON value");
            };
            match bytes.get(pos) {
                Some(&b',') => {
                    pos += 1;
                    out.push(',');
                    match container {
                        JsonContainer::Object { .. } => {
                            if let Some(JsonContainer::Object {
                                expect_key_or_end, ..
                            }) = stack.last_mut()
                            {
                                *expect_key_or_end = true;
                            }
                            expect_comma_or_end = false;
                            // Peek next non-ws to reject trailing comma before `}`.
                            skip_json_ws(bytes, &mut pos);
                            if bytes.get(pos) == Some(&b'}') {
                                return make_err(pos, "trailing comma before '}'");
                            }
                            push_indent(&mut out, stack.len());
                        }
                        JsonContainer::Array { .. } => {
                            expect_comma_or_end = false;
                            expect_value = true;
                            skip_json_ws(bytes, &mut pos);
                            if bytes.get(pos) == Some(&b']') {
                                return make_err(pos, "trailing comma before ']'");
                            }
                            push_indent(&mut out, stack.len());
                        }
                    }
                    continue;
                }
                Some(&b'}') if matches!(container, JsonContainer::Object { .. }) => {
                    pos += 1;
                    let popped = stack.pop().unwrap();
                    if let JsonContainer::Object { count, .. } = popped
                        && count > 0
                    {
                        push_indent(&mut out, stack.len());
                    }
                    out.push('}');
                    expect_comma_or_end = true;
                    continue;
                }
                Some(&b']') if matches!(container, JsonContainer::Array { .. }) => {
                    pos += 1;
                    let popped = stack.pop().unwrap();
                    if let JsonContainer::Array { count } = popped
                        && count > 0
                    {
                        push_indent(&mut out, stack.len());
                    }
                    out.push(']');
                    expect_comma_or_end = true;
                    continue;
                }
                _ => {
                    let expected = match container {
                        JsonContainer::Object { .. } => "expected ',' or '}'",
                        JsonContainer::Array { .. } => "expected ',' or ']'",
                    };
                    return make_err(pos, expected);
                }
            }
        }

        if let Some(&JsonContainer::Object {
            expect_key_or_end: true,
            count,
        }) = stack.last()
        {
            match bytes.get(pos) {
                Some(&b'}') if count == 0 => {
                    pos += 1;
                    stack.pop();
                    out.push('}');
                    expect_comma_or_end = true;
                    continue;
                }
                Some(&b'"') => {
                    if count == 0 {
                        push_indent(&mut out, stack.len());
                    }
                    let end = match scan_json_string(bytes, pos) {
                        Ok(end) => end,
                        Err((err_off, msg)) => return make_err(err_off, msg),
                    };
                    out.push_str(&text[pos..end]);
                    pos = end;
                    if let Some(JsonContainer::Object {
                        expect_key_or_end,
                        count,
                    }) = stack.last_mut()
                    {
                        *expect_key_or_end = false;
                        *count += 1;
                    }
                    expect_colon = true;
                    continue;
                }
                _ => {
                    return make_err(
                        pos,
                        if count == 0 {
                            "expected object key string or '}'"
                        } else {
                            "expected object key string"
                        },
                    );
                }
            }
        }

        if !expect_value {
            return make_err(pos, "unexpected JSON state");
        }

        let depth = stack.len();
        if let Some(JsonContainer::Array { count }) = stack.last_mut() {
            if bytes.get(pos) == Some(&b']') && *count == 0 {
                pos += 1;
                stack.pop();
                out.push(']');
                expect_value = false;
                expect_comma_or_end = true;
                continue;
            }
            if *count == 0 {
                push_indent(&mut out, depth);
            }
            *count += 1;
        }

        let Some(&b) = bytes.get(pos) else {
            return make_err(pos, "unexpected end of JSON input");
        };

        match b {
            b'{' => {
                if stack.len() >= MAX_JSON_NESTING_DEPTH {
                    return make_err(pos, "JSON nesting exceeds 256-level limit");
                }
                pos += 1;
                out.push('{');
                stack.push(JsonContainer::Object {
                    expect_key_or_end: true,
                    count: 0,
                });
                expect_value = false;
            }
            b'[' => {
                if stack.len() >= MAX_JSON_NESTING_DEPTH {
                    return make_err(pos, "JSON nesting exceeds 256-level limit");
                }
                pos += 1;
                out.push('[');
                stack.push(JsonContainer::Array { count: 0 });
                expect_value = true;
            }
            b'"' => {
                let end = match scan_json_string(bytes, pos) {
                    Ok(end) => end,
                    Err((err_off, msg)) => return make_err(err_off, msg),
                };
                out.push_str(&text[pos..end]);
                pos = end;
                expect_value = false;
                expect_comma_or_end = true;
            }
            b'-' | b'0'..=b'9' => {
                let end = match scan_json_number(bytes, pos) {
                    Ok(end) => end,
                    Err((err_off, msg)) => return make_err(err_off, msg),
                };
                out.push_str(&text[pos..end]);
                pos = end;
                expect_value = false;
                expect_comma_or_end = true;
            }
            b't' => {
                if !bytes[pos..].starts_with(b"true") {
                    return make_err(pos, "invalid token; expected 'true'");
                }
                out.push_str("true");
                pos += 4;
                expect_value = false;
                expect_comma_or_end = true;
            }
            b'f' => {
                if !bytes[pos..].starts_with(b"false") {
                    return make_err(pos, "invalid token; expected 'false'");
                }
                out.push_str("false");
                pos += 5;
                expect_value = false;
                expect_comma_or_end = true;
            }
            b'n' => {
                if !bytes[pos..].starts_with(b"null") {
                    return make_err(pos, "invalid token; expected 'null'");
                }
                out.push_str("null");
                pos += 4;
                expect_value = false;
                expect_comma_or_end = true;
            }
            _ => {
                return make_err(pos, "unexpected character while expecting a JSON value");
            }
        }
    }

    if out.len() > MAX_PRETTY_JSON_OUTPUT_BYTES {
        return PrettyJsonResult::TooLarge {
            actual_bytes: out.len(),
            limit_bytes: MAX_PRETTY_JSON_OUTPUT_BYTES,
        };
    }

    let Some(lines) = LineIndex::build_cancellable(out.as_bytes(), cancel) else {
        return PrettyJsonResult::Cancelled;
    };
    PrettyJsonResult::Ready {
        text: Arc::new(out),
        lines,
    }
}

fn push_indent(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn skip_json_ws(bytes: &[u8], pos: &mut usize) {
    while let Some(&b) = bytes.get(*pos) {
        if matches!(b, b' ' | b'\t' | b'\r' | b'\n') {
            *pos += 1;
        } else {
            break;
        }
    }
}

fn scan_json_string(bytes: &[u8], start: usize) -> Result<usize, (usize, &'static str)> {
    debug_assert_eq!(bytes[start], b'"');
    let mut i = start + 1;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' {
            return Ok(i + 1);
        }
        if b < 0x20 {
            return Err((i, "unescaped control character in JSON string"));
        }
        if b == b'\\' {
            i += 1;
            let Some(&esc) = bytes.get(i) else {
                return Err((i, "unterminated escape sequence in JSON string"));
            };
            match esc {
                b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {
                    i += 1;
                }
                b'u' => {
                    if i + 4 >= bytes.len() {
                        return Err((i, "incomplete \\uXXXX escape in JSON string"));
                    }
                    for j in 1..=4 {
                        if !bytes[i + j].is_ascii_hexdigit() {
                            return Err((i + j, "invalid hex digit in \\uXXXX escape"));
                        }
                    }
                    i += 5;
                }
                _ => {
                    return Err((i, "invalid escape sequence in JSON string"));
                }
            }
        } else {
            i += 1;
        }
    }
    Err((start, "unterminated JSON string"))
}

fn scan_json_number(bytes: &[u8], start: usize) -> Result<usize, (usize, &'static str)> {
    let mut i = start;
    if bytes.get(i) == Some(&b'-') {
        i += 1;
    }
    match bytes.get(i) {
        Some(&b'0') => {
            i += 1;
            if bytes.get(i).is_some_and(u8::is_ascii_digit) {
                return Err((i, "leading zeros are not allowed in JSON numbers"));
            }
        }
        Some(b'1'..=b'9') => {
            i += 1;
            while bytes.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
        }
        _ => return Err((i, "expected digit in JSON number")),
    }
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        if !bytes.get(i).is_some_and(u8::is_ascii_digit) {
            return Err((i, "expected digit after decimal point in JSON number"));
        }
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !bytes.get(i).is_some_and(u8::is_ascii_digit) {
            return Err((i, "expected digit in JSON number exponent"));
        }
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
    }
    Ok(i)
}

fn line_col_at_offset(text: &str, byte_offset: usize) -> (usize, usize) {
    let clamped = byte_offset.min(text.len());
    let mut safe_end = clamped;
    while safe_end > 0 && !text.is_char_boundary(safe_end) {
        safe_end -= 1;
    }
    let prefix = &text[..safe_end];
    let mut line = 1usize;
    let mut col = 1usize;
    for ch in prefix.chars() {
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// Completed immutable snapshot produced by [`CellInspectorJob`].
#[derive(Debug, Clone)]
pub struct CellInspectorSnapshot {
    pub generation: u64,
    pub raw: Arc<[u8]>,
    pub raw_lines: LineIndex,
    pub detected_format: CellFormat,
    pub valid_utf8: bool,
    pub pretty_json: Option<PrettyJsonResult>,
}

/// Builds an immutable [`CellInspectorSnapshot`] synchronously (intended to run on a background
/// thread or executor), returning `None` if `cancel` is signalled before completion.
pub fn build_inspector_snapshot(
    generation: u64,
    raw: Arc<[u8]>,
    cancel: &AtomicBool,
) -> Option<CellInspectorSnapshot> {
    let raw_lines = LineIndex::build_cancellable(&raw, cancel)?;
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    let valid_utf8 = std::str::from_utf8(&raw).is_ok();
    let mut detected_format = detect_cell_format(&raw);
    if !valid_utf8 {
        detected_format = CellFormat::InvalidUtf8;
    }
    let pretty_json = if detected_format == CellFormat::Json {
        let res = pretty_print_json_streaming(&raw, cancel);
        if matches!(res, PrettyJsonResult::Cancelled) {
            return None;
        }
        Some(res)
    } else {
        None
    };
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    Some(CellInspectorSnapshot {
        generation,
        raw,
        raw_lines,
        detected_format,
        valid_utf8,
        pretty_json,
    })
}

/// Cancellable background worker that builds a [`CellInspectorSnapshot`].
pub struct CellInspectorJob {
    generation: u64,
    cancel: Arc<AtomicBool>,
    result: Arc<Mutex<Option<CellInspectorSnapshot>>>,
    handle: Option<JoinHandle<()>>,
}

impl CellInspectorJob {
    /// Spawns a cancellable background thread to index `raw` and pretty-print JSON when detected.
    pub fn spawn(
        generation: u64,
        raw: Arc<[u8]>,
        on_done: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let result = Arc::new(Mutex::new(None));
        let cancel_bg = Arc::clone(&cancel);
        let result_bg = Arc::clone(&result);

        let handle = thread::Builder::new()
            .name("quarry-cell-inspector".into())
            .spawn(move || {
                let Some(snapshot) = build_inspector_snapshot(generation, raw, &cancel_bg) else {
                    return;
                };
                if let Ok(mut guard) = result_bg.lock() {
                    *guard = Some(snapshot);
                }
                if let Some(wake) = on_done {
                    wake();
                }
            })
            .ok();

        Self {
            generation,
            cancel,
            result,
            handle,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn try_take(&mut self) -> Option<CellInspectorSnapshot> {
        let taken = self.result.lock().ok().and_then(|mut g| g.take());
        if taken.is_some()
            && let Some(handle) = self.handle.take()
        {
            let _ = handle.join();
        }
        taken
    }

    /// Blocks until the background worker finishes (intended for tests).
    pub fn wait(mut self) -> Option<CellInspectorSnapshot> {
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        self.result.lock().ok().and_then(|mut g| g.take())
    }
}

impl Drop for CellInspectorJob {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Pure, bounded hover-to-peek preview computed directly from already-parsed viewport bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverPeek {
    pub format: CellFormat,
    pub preview_text: String,
    pub total_bytes: usize,
    pub truncated: bool,
}

/// Builds a bounded hover-to-peek preview from already-parsed viewport bytes `raw`.
///
/// Performs zero file I/O and returns `None` for short single-line values that fit
/// comfortably inside `col_width_px`.
pub fn build_hover_peek(raw: &[u8], col_width_px: f32, font_size_px: f32) -> Option<HoverPeek> {
    if raw.is_empty() {
        return None;
    }
    let avg_char_px = (font_size_px * 0.62).max(5.0);
    let inner_width = (col_width_px - 16.0).max(24.0);
    let fitting_chars = ((inner_width / avg_char_px).floor() as usize).max(8);

    let prefix_len = bounded_slice_end(raw, MAX_PEEK_BYTES);
    let prefix = &raw[..prefix_len];
    let has_line_break = prefix.iter().any(|&b| b == b'\n' || b == b'\r');

    if !has_line_break && raw.len() <= fitting_chars {
        return None;
    }

    let format = detect_cell_format(raw);
    let text = String::from_utf8_lossy(prefix);
    let mut lines_out = Vec::new();
    let mut truncated = raw.len() > prefix_len;

    for (idx, line) in text.lines().enumerate() {
        if idx >= MAX_PEEK_LINES {
            truncated = true;
            break;
        }
        let clean = line.trim_end_matches('\r');
        let mut chars = clean.chars();
        let taken: String = chars.by_ref().take(MAX_PEEK_LINE_CHARS).collect();
        if chars.next().is_some() {
            truncated = true;
            lines_out.push(format!("{taken}…"));
        } else {
            lines_out.push(taken);
        }
    }

    if lines_out.is_empty() {
        return None;
    }

    let mut preview_text = lines_out.join("\n");
    if truncated {
        let omitted = raw.len().saturating_sub(prefix_len);
        if omitted > 0 {
            preview_text.push_str(&format!("\n… (+{} more bytes)", omitted));
        } else {
            preview_text.push_str("\n…");
        }
    }

    Some(HoverPeek {
        format,
        preview_text,
        total_bytes: raw.len(),
        truncated,
    })
}

/// Maps a [`TokenKind`] to a WCAG AA contrast-safe color (`>= 4.5:1` on `tokens.canvas`)
/// for both built-in Light and Dark themes.
pub fn inspector_token_color(
    kind: TokenKind,
    appearance: ThemeAppearance,
    tokens: &Palette,
) -> Rgba {
    match (appearance, kind) {
        (_, TokenKind::Plain) => tokens.text,
        (_, TokenKind::Punctuation) => tokens.text_secondary,
        // Light theme (>= 4.5:1 against #FFFFFF canvas)
        (ThemeAppearance::Light, TokenKind::JsonKey | TokenKind::MarkdownHeading) => {
            Rgba::rgb(26, 79, 156) // #1A4F9C (~8.2:1 on white)
        }
        (ThemeAppearance::Light, TokenKind::JsonString | TokenKind::MarkdownCode) => {
            Rgba::rgb(22, 107, 52) // #166B34 (~6.4:1 on white)
        }
        (ThemeAppearance::Light, TokenKind::JsonNumber | TokenKind::StackLocation) => {
            Rgba::rgb(148, 72, 0) // #944800 (~6.1:1 on white)
        }
        (ThemeAppearance::Light, TokenKind::JsonBoolNull | TokenKind::StackSymbol) => {
            Rgba::rgb(112, 46, 152) // #702E98 (~7.5:1 on white)
        }
        (ThemeAppearance::Light, TokenKind::StackHeader) => tokens.error_text,
        (ThemeAppearance::Light, TokenKind::MarkdownFence | TokenKind::MarkdownMarker) => {
            tokens.text_secondary
        }
        // Dark theme (>= 4.5:1 against #141413 canvas)
        (ThemeAppearance::Dark, TokenKind::JsonKey | TokenKind::MarkdownHeading) => {
            Rgba::rgb(122, 184, 255) // #7AB8FF (~9.1:1 on dark)
        }
        (ThemeAppearance::Dark, TokenKind::JsonString | TokenKind::MarkdownCode) => {
            Rgba::rgb(124, 214, 148) // #7CD694 (~10.4:1 on dark)
        }
        (ThemeAppearance::Dark, TokenKind::JsonNumber | TokenKind::StackLocation) => {
            Rgba::rgb(245, 194, 114) // #F5C272 (~11.3:1 on dark)
        }
        (ThemeAppearance::Dark, TokenKind::JsonBoolNull | TokenKind::StackSymbol) => {
            Rgba::rgb(206, 160, 255) // #CEA0FF (~8.8:1 on dark)
        }
        (ThemeAppearance::Dark, TokenKind::StackHeader) => tokens.error_text,
        (ThemeAppearance::Dark, TokenKind::MarkdownFence | TokenKind::MarkdownMarker) => {
            tokens.text_secondary
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{DARK, LIGHT, contrast_ratio};
    use std::time::Instant;

    #[test]
    fn ten_mib_single_line_cell_stays_bounded_and_fast() {
        let raw = Arc::<[u8]>::from(vec![b'a'; 10 * 1024 * 1024]);
        let began = Instant::now();
        let job = CellInspectorJob::spawn(1, Arc::clone(&raw), None);
        let snap = job.wait().expect("inspector job must finish");
        assert_eq!(snap.raw_lines.len(), 1);
        assert_eq!(snap.detected_format, CellFormat::PlainText);

        let slice = snap
            .raw_lines
            .line_slice(&snap.raw, 0, MAX_INSPECTOR_LINE_BYTES)
            .unwrap();
        assert_eq!(slice.text.len(), MAX_INSPECTOR_LINE_BYTES);
        assert_eq!(
            slice.truncated_bytes,
            10 * 1024 * 1024 - MAX_INSPECTOR_LINE_BYTES
        );
        let tok = tokenize_line(
            &slice.text,
            snap.detected_format,
            false,
            MAX_INSPECTOR_LINE_BYTES,
        );
        assert_eq!(tok.text.len(), MAX_INSPECTOR_LINE_BYTES);
        assert!(
            began.elapsed().as_secs() < 5,
            "10 MiB single-line indexing + bounded slice must stay fast"
        );
    }

    #[test]
    fn json_pretty_print_preserves_duplicate_keys_and_exact_numeric_lexemes() {
        let input = br#"{"id":1.00,"id":999999999999999999999999999,"exp":1e500,"zero":-0.0,"nested":[{"k":"v\n"},{"k":"v2"}]}"#;
        let cancel = AtomicBool::new(false);
        let PrettyJsonResult::Ready { text, lines } = pretty_print_json_streaming(input, &cancel)
        else {
            panic!("expected valid pretty JSON");
        };
        assert!(lines.len() >= 10);
        let id_count = text.matches("\"id\":").count();
        assert_eq!(id_count, 2, "duplicate object keys must be preserved");
        assert!(text.contains("1.00"), "exact decimal precision preserved");
        assert!(
            text.contains("999999999999999999999999999"),
            "arbitrary-precision integer lexeme preserved"
        );
        assert!(text.contains("1e500"), "large exponent lexeme preserved");
        assert!(text.contains("-0.0"), "signed zero lexeme preserved");
        assert!(text.contains("\"v\\n\""), "escape sequence preserved");
    }

    #[test]
    fn invalid_json_reports_exact_line_column_and_byte_offset() {
        let input = b"{\n  \"ok\": true,\n  \"bad\": [1, 2, ]\n}";
        let cancel = AtomicBool::new(false);
        let PrettyJsonResult::Invalid(err) = pretty_print_json_streaming(input, &cancel) else {
            panic!("expected invalid JSON error");
        };
        assert_eq!(err.line, 3);
        assert_eq!(err.column, 17);
        assert_eq!(input[err.byte_offset], b']');
        assert!(err.display_summary().contains("line 3, column 17"));
    }

    #[test]
    fn deeply_nested_and_oversized_json_fail_cleanly() {
        let mut deep = vec![b'['; MAX_JSON_NESTING_DEPTH + 10];
        deep.extend(vec![b']'; MAX_JSON_NESTING_DEPTH + 10]);
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            pretty_print_json_streaming(&deep, &cancel),
            PrettyJsonResult::Invalid(_)
        ));

        let huge = vec![b' '; MAX_PRETTY_JSON_INPUT_BYTES + 1];
        assert!(matches!(
            pretty_print_json_streaming(&huge, &cancel),
            PrettyJsonResult::TooLarge { .. }
        ));
    }

    #[test]
    fn cancellation_stops_pretty_print_and_line_index() {
        let cancel = AtomicBool::new(true);
        let sample = vec![b'a'; 128 * 1024];
        assert!(LineIndex::build_cancellable(&sample, &cancel).is_none());

        let mut big_arr = String::from("[");
        for i in 0..2000 {
            if i > 0 {
                big_arr.push(',');
            }
            big_arr.push_str(&i.to_string());
        }
        big_arr.push(']');
        assert!(matches!(
            pretty_print_json_streaming(big_arr.as_bytes(), &cancel),
            PrettyJsonResult::Cancelled
        ));
    }

    #[test]
    fn detects_and_tokenizes_java_python_and_rust_stack_traces() {
        let java = b"java.lang.IllegalStateException: boom\n\tat com.acme.App.run(App.java:42)\nCaused by: java.io.IOException: disk";
        assert_eq!(detect_cell_format(java), CellFormat::StackTrace);
        let frame = tokenize_line(
            "\tat com.acme.App.run(App.java:42)",
            CellFormat::StackTrace,
            false,
            MAX_INSPECTOR_LINE_BYTES,
        );
        assert!(frame.spans.iter().any(|s| s.kind == TokenKind::StackSymbol));
        assert!(
            frame
                .spans
                .iter()
                .any(|s| s.kind == TokenKind::StackLocation)
        );

        let python = b"Traceback (most recent call last):\n  File \"app.py\", line 19, in main\nValueError: bad";
        assert_eq!(detect_cell_format(python), CellFormat::StackTrace);
        let py_frame = tokenize_line(
            "  File \"app.py\", line 19, in main",
            CellFormat::StackTrace,
            false,
            MAX_INSPECTOR_LINE_BYTES,
        );
        assert!(
            py_frame
                .spans
                .iter()
                .any(|s| s.kind == TokenKind::StackLocation)
        );
        assert!(
            py_frame
                .spans
                .iter()
                .any(|s| s.kind == TokenKind::StackSymbol)
        );

        let rust = b"thread 'main' panicked at src/main.rs:10:5:\nstack backtrace:\n   0: quarry::run\n             at /src/main.rs:10:5";
        assert_eq!(detect_cell_format(rust), CellFormat::StackTrace);
    }

    #[test]
    fn markdown_detection_and_code_fence_index_work() {
        let md = b"# Title\n\nHere is `inline` code:\n```rust\nlet x = 1;\n```\n- item";
        assert_eq!(detect_cell_format(md), CellFormat::Markdown);
        let cancel = AtomicBool::new(false);
        let idx = LineIndex::build_cancellable(md, &cancel).unwrap();
        assert!(!idx.in_code_fence_before(0));
        assert!(!idx.in_code_fence_before(3));
        assert!(idx.in_code_fence_before(4));
        assert!(!idx.in_code_fence_before(6));

        let heading = tokenize_line("# Title", CellFormat::Markdown, false, 1024);
        assert_eq!(heading.spans[0].kind, TokenKind::MarkdownHeading);
        let code = tokenize_line("let x = 1;", CellFormat::Markdown, true, 1024);
        assert_eq!(code.spans[0].kind, TokenKind::MarkdownCode);
        assert!(code.next_in_code_fence);
    }

    #[test]
    fn hover_peek_skips_short_cells_and_bounds_long_and_multiline_cells() {
        assert!(build_hover_peek(b"short", 160.0, 12.0).is_none());

        let multiline = b"line 1\nline 2";
        let peek = build_hover_peek(multiline, 240.0, 12.0).expect("multiline must peek");
        assert!(!peek.truncated);
        assert_eq!(peek.preview_text, "line 1\nline 2");

        let huge = vec![b'x'; 50_000];
        let huge_peek = build_hover_peek(&huge, 120.0, 12.0).expect("long cell must peek");
        assert!(huge_peek.truncated);
        assert!(huge_peek.preview_text.len() <= MAX_PEEK_BYTES + 64);
        assert!(huge_peek.preview_text.contains("more bytes"));
    }

    #[test]
    fn inspector_syntax_colors_meet_wcag_aa_contrast_on_light_and_dark() {
        let all_kinds = [
            TokenKind::Plain,
            TokenKind::JsonKey,
            TokenKind::JsonString,
            TokenKind::JsonNumber,
            TokenKind::JsonBoolNull,
            TokenKind::Punctuation,
            TokenKind::StackHeader,
            TokenKind::StackLocation,
            TokenKind::StackSymbol,
            TokenKind::MarkdownHeading,
            TokenKind::MarkdownFence,
            TokenKind::MarkdownCode,
            TokenKind::MarkdownMarker,
        ];
        for kind in all_kinds {
            let light_color = inspector_token_color(kind, ThemeAppearance::Light, &LIGHT);
            let light_ratio = contrast_ratio(light_color, LIGHT.canvas);
            assert!(
                light_ratio >= 4.5,
                "light contrast too low for {kind:?}: {light_ratio:.2}"
            );
            let dark_color = inspector_token_color(kind, ThemeAppearance::Dark, &DARK);
            let dark_ratio = contrast_ratio(dark_color, DARK.canvas);
            assert!(
                dark_ratio >= 4.5,
                "dark contrast too low for {kind:?}: {dark_ratio:.2}"
            );
        }
    }
}
