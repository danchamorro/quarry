//! Byte-oriented delimited record scanning and parsing.

mod dialect;
mod simd;

use std::borrow::Cow;
use std::error::Error;
use std::fmt;

use memchr::{memchr, memchr2, memchr3};

pub use dialect::{ByteEncodingClass, EscapeRule, ScannerDialect};
pub use simd::{SimdBackend, active_simd_backend};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseErrorKind {
    InvalidDelimiter,
    InvalidDialect,
    FieldLimitExceeded(usize),
    UnexpectedQuote,
    UnexpectedAfterQuote(u8),
    UnterminatedQuote,
    ExpectedLfAfterCr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseError {
    pub offset: u64,
    pub kind: ParseErrorKind,
}

impl ParseError {
    pub(crate) fn new(offset: u64, kind: ParseErrorKind) -> Self {
        Self { offset, kind }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            ParseErrorKind::InvalidDelimiter => write!(f, "invalid delimiter"),
            ParseErrorKind::InvalidDialect => write!(f, "invalid dialect"),
            ParseErrorKind::FieldLimitExceeded(limit) => {
                write!(f, "record exceeds the configured {limit}-field limit")
            }
            ParseErrorKind::UnexpectedQuote => {
                write!(f, "unexpected quote at byte {}", self.offset)
            }
            ParseErrorKind::UnexpectedAfterQuote(byte) => write!(
                f,
                "unexpected byte 0x{byte:02x} after closing quote at byte {}",
                self.offset
            ),
            ParseErrorKind::UnterminatedQuote => {
                write!(f, "unterminated quoted field at byte {}", self.offset)
            }
            ParseErrorKind::ExpectedLfAfterCr => {
                write!(f, "expected LF after CR at byte {}", self.offset)
            }
        }
    }
}

impl Error for ParseError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanState {
    FieldStart,
    Unquoted,
    Quoted,
    QuotedEscape,
    AfterQuote,
    AfterQuoteCr,
}

/// Stateful scanner shared by viewport reads and structural indexing.
pub type RecordScanner = StructuralScanner<false>;

/// The width-tracking variant counts unquoted field separators without retaining field data.
#[derive(Debug, Clone)]
pub struct StructuralScanner<const TRACK_WIDTH: bool> {
    separators: usize,
    maximum_width: usize,
    dialect: ScannerDialect,
    backend: SimdBackend,
    state: ScanState,
    record_start: u64,
    bom_prefix: [u8; 3],
    bom_prefix_len: usize,
    bom_checked: bool,
}

impl<const TRACK_WIDTH: bool> StructuralScanner<TRACK_WIDTH> {
    pub fn new(delimiter: u8) -> Result<Self, ParseError> {
        Self::at_offset(delimiter, 0)
    }

    pub fn at_offset(delimiter: u8, record_start: u64) -> Result<Self, ParseError> {
        Self::with_dialect(
            ScannerDialect::rfc4180(delimiter),
            record_start,
            ByteEncodingClass::Utf8,
        )
    }

    /// Scan bytes whose encoding has no UTF-8 signature (for example a legacy code page).
    pub fn at_offset_without_bom(delimiter: u8, record_start: u64) -> Result<Self, ParseError> {
        Self::with_dialect_without_bom(
            ScannerDialect::rfc4180(delimiter),
            record_start,
            ByteEncodingClass::SingleByteAsciiSuperset,
        )
    }

    /// Create a scanner with a parameterized dialect and encoding class.
    pub fn with_dialect(
        dialect: ScannerDialect,
        record_start: u64,
        encoding: ByteEncodingClass,
    ) -> Result<Self, ParseError> {
        Self::with_dialect_and_backend(dialect, record_start, encoding, active_simd_backend())
    }

    /// Create a scanner with a parameterized dialect without checking for a UTF-8 BOM.
    pub fn with_dialect_without_bom(
        dialect: ScannerDialect,
        record_start: u64,
        encoding: ByteEncodingClass,
    ) -> Result<Self, ParseError> {
        let mut scanner = Self::with_dialect(dialect, record_start, encoding)?;
        scanner.bom_checked = true;
        Ok(scanner)
    }

    /// Create a scanner pinned to a specific SIMD backend (falls back to `Scalar` if unsupported).
    pub fn with_dialect_and_backend(
        dialect: ScannerDialect,
        record_start: u64,
        encoding: ByteEncodingClass,
        requested_backend: SimdBackend,
    ) -> Result<Self, ParseError> {
        dialect.validate(encoding)?;
        let backend = if requested_backend.is_supported_on_current_cpu() {
            requested_backend
        } else {
            SimdBackend::Scalar
        };
        Ok(Self {
            separators: 0,
            maximum_width: 0,
            dialect,
            backend,
            state: ScanState::FieldStart,
            record_start,
            bom_prefix: [0; 3],
            bom_prefix_len: 0,
            bom_checked: record_start != 0 || encoding != ByteEncodingClass::Utf8,
        })
    }

    /// Create a scanner starting inside a quoted field (used by LF-aligned parallel chunk workers).
    pub fn at_offset_in_quote(
        dialect: ScannerDialect,
        record_start: u64,
        encoding: ByteEncodingClass,
        requested_backend: SimdBackend,
    ) -> Result<Self, ParseError> {
        if dialect.quote.is_none() {
            return Err(ParseError::new(0, ParseErrorKind::InvalidDialect));
        }
        let mut scanner =
            Self::with_dialect_and_backend(dialect, record_start, encoding, requested_backend)?;
        scanner.bom_checked = true;
        scanner.state = ScanState::Quoted;
        Ok(scanner)
    }

    /// Returns the SIMD backend active for this scanner instance.
    pub fn backend(&self) -> SimdBackend {
        self.backend
    }

    /// Returns the configured dialect.
    pub fn dialect(&self) -> ScannerDialect {
        self.dialect
    }

    /// Returns whether the scanner is currently inside a quoted field (including `QuotedEscape`).
    pub fn is_in_quote(&self) -> bool {
        matches!(self.state, ScanState::Quoted | ScanState::QuotedEscape)
    }

    /// Returns whether the scanner is at a clean record/field start outside quotes.
    pub fn is_at_field_start(&self) -> bool {
        self.state == ScanState::FieldStart
    }

    fn end_field(&mut self) {
        if TRACK_WIDTH {
            self.separators = self.separators.saturating_add(1);
        }
    }

    fn end_record(&mut self) {
        if TRACK_WIDTH {
            self.maximum_width = self.maximum_width.max(self.separators.saturating_add(1));
            self.separators = 0;
        }
    }

    pub fn record_start(&self) -> u64 {
        self.record_start
    }

    /// Scans one contiguous chunk and emits exclusive absolute record ends.
    pub fn scan_chunk(
        &mut self,
        bytes: &[u8],
        absolute_start: u64,
        mut on_record_end: impl FnMut(u64),
    ) -> Result<u64, ParseError> {
        const UTF8_BOM: &[u8; 3] = b"\xEF\xBB\xBF";

        if self.bom_checked {
            return self.scan_data_chunk(bytes, absolute_start, &mut on_record_end);
        }

        if absolute_start != self.bom_prefix_len as u64 {
            let prefix = self.bom_prefix;
            let prefix_len = self.bom_prefix_len;
            self.bom_checked = true;
            let mut records = self.scan_data_chunk(&prefix[..prefix_len], 0, &mut on_record_end)?;
            records += self.scan_data_chunk(bytes, absolute_start, &mut on_record_end)?;
            return Ok(records);
        }

        let mut index = 0;
        while index < bytes.len() && self.bom_prefix_len < UTF8_BOM.len() {
            if bytes[index] != UTF8_BOM[self.bom_prefix_len] {
                let prefix = self.bom_prefix;
                let prefix_len = self.bom_prefix_len;
                self.bom_checked = true;
                let mut records =
                    self.scan_data_chunk(&prefix[..prefix_len], 0, &mut on_record_end)?;
                records += self.scan_data_chunk(
                    &bytes[index..],
                    absolute_start + index as u64,
                    &mut on_record_end,
                )?;
                return Ok(records);
            }
            self.bom_prefix[self.bom_prefix_len] = bytes[index];
            self.bom_prefix_len += 1;
            index += 1;
        }

        if self.bom_prefix_len < UTF8_BOM.len() {
            return Ok(0);
        }

        self.bom_checked = true;
        self.record_start = UTF8_BOM.len() as u64;
        self.scan_data_chunk(
            &bytes[index..],
            absolute_start + index as u64,
            &mut on_record_end,
        )
    }

    fn scan_data_chunk(
        &mut self,
        bytes: &[u8],
        absolute_start: u64,
        on_record_end: &mut impl FnMut(u64),
    ) -> Result<u64, ParseError> {
        if self.backend == SimdBackend::Scalar || bytes.len() < 64 {
            return self.scan_scalar_slice(bytes, absolute_start, on_record_end);
        }

        let mut index = 0usize;
        let mut records = 0u64;
        let len = bytes.len();

        while index + 64 <= len {
            let block: &[u8; 64] = bytes[index..index + 64].try_into().expect("64-byte slice");
            let block_start = absolute_start + index as u64;
            records += self.scan_block_simd(block, block_start, on_record_end)?;
            index += 64;
        }

        if index < len {
            records += self.scan_scalar_slice(
                &bytes[index..],
                absolute_start + index as u64,
                on_record_end,
            )?;
        }

        Ok(records)
    }

    #[inline]
    fn emit_block_records(
        &mut self,
        block_start: u64,
        unquoted_lfs: u64,
        unquoted_delims: u64,
        on_record_end: &mut impl FnMut(u64),
    ) -> u64 {
        let mut records = 0u64;
        let mut lfs = unquoted_lfs;
        if TRACK_WIDTH {
            let mut prev = 0u32;
            while lfs != 0 {
                let b = lfs.trailing_zeros();
                let seg_mask = ((1u64 << b) - 1) & (!0u64 << prev);
                let seg_delims = (unquoted_delims & seg_mask).count_ones() as usize;
                self.separators = self.separators.saturating_add(seg_delims);
                self.end_record();
                let end = block_start + u64::from(b) + 1;
                on_record_end(end);
                records += 1;
                self.record_start = end;
                prev = b + 1;
                lfs &= lfs - 1;
            }
            if prev < 64 {
                let tail_delims = (unquoted_delims >> prev).count_ones() as usize;
                self.separators = self.separators.saturating_add(tail_delims);
            }
        } else {
            while lfs != 0 {
                let b = lfs.trailing_zeros();
                let end = block_start + u64::from(b) + 1;
                on_record_end(end);
                records += 1;
                self.record_start = end;
                lfs &= lfs - 1;
            }
        }
        records
    }

    #[inline]
    fn scan_block_simd(
        &mut self,
        block: &[u8; 64],
        block_start: u64,
        on_record_end: &mut impl FnMut(u64),
    ) -> Result<u64, ParseError> {
        let need_delim =
            TRACK_WIDTH || !matches!(self.state, ScanState::FieldStart | ScanState::Unquoted);
        let masks = simd::classify_block_64(block, self.dialect, self.backend, need_delim);

        // Fast path A: unquoted block with zero quote characters.
        if masks.quote == 0 && matches!(self.state, ScanState::FieldStart | ScanState::Unquoted) {
            let records =
                self.emit_block_records(block_start, masks.lf, masks.delim, on_record_end);
            self.state = if (masks.lf >> 63) != 0 || block[63] == self.dialect.delimiter {
                ScanState::FieldStart
            } else {
                ScanState::Unquoted
            };
            return Ok(records);
        }

        // Fast path B: general 64-byte SIMD quoted classification with violation-bit scalar replay.
        let prev_in_quote = matches!(self.state, ScanState::Quoted | ScanState::QuotedEscape);
        let prev_escaped = self.state == ScanState::QuotedEscape;
        let prev_ended_after_quote = self.state == ScanState::AfterQuote;
        let prev_ended_after_quote_cr = self.state == ScanState::AfterQuoteCr;
        let prev_is_field_start = self.state == ScanState::FieldStart;

        let (unescaped_quotes, next_escaped, is_escape_byte) = match self.dialect.escape {
            EscapeRule::DoubledQuote => (masks.quote, false, false),
            EscapeRule::EscapeByte(_) => {
                let (escaped, next_esc) = simd::compute_escaped_mask(masks.escape, prev_escaped);
                (masks.quote & !escaped, next_esc, true)
            }
        };

        let prev_in_quote_mask = if prev_in_quote { !0u64 } else { 0u64 };
        let in_quote_inclusive =
            simd::prefix_xor(unescaped_quotes, self.backend) ^ prev_in_quote_mask;
        let in_quote_before = (in_quote_inclusive << 1) | u64::from(prev_in_quote);

        let opening_quotes = unescaped_quotes & !in_quote_before;
        let closing_quotes = unescaped_quotes & in_quote_before;

        // If `EscapeByte` is active and an escape byte appears outside quotes, it is ordinary data
        // rather than an escape sequence; delegate this single block to the scalar state machine.
        if is_escape_byte && (masks.escape & !in_quote_before) != 0 {
            return self.scan_scalar_slice(block, block_start, on_record_end);
        }

        let unquoted_delims = masks.delim & !in_quote_inclusive;
        let unquoted_lfs = masks.lf & !in_quote_inclusive;

        let after_quote_positions = (closing_quotes << 1) | u64::from(prev_ended_after_quote);
        let after_quote_crs = after_quote_positions & masks.cr;
        let need_lf = (after_quote_crs << 1) | u64::from(prev_ended_after_quote_cr);

        let valid_before_open = if is_escape_byte {
            ((unquoted_delims | unquoted_lfs) << 1) | u64::from(prev_is_field_start)
        } else {
            ((closing_quotes | unquoted_delims | unquoted_lfs) << 1)
                | u64::from(prev_ended_after_quote || prev_is_field_start)
        };
        let valid_after_quote = if is_escape_byte {
            masks.delim | masks.lf | masks.cr
        } else {
            unescaped_quotes | masks.delim | masks.lf | masks.cr
        };

        let bad_open = opening_quotes & !valid_before_open;
        let bad_after_quote = after_quote_positions & !valid_after_quote;
        let bad_cr = need_lf & !masks.lf;

        if (bad_open | bad_after_quote | bad_cr) != 0 {
            // Scalar error delegation: replay this 64-byte block through the scalar state machine
            // so error kind and byte offset match the pure-scalar oracle by construction.
            return self.scan_scalar_slice(block, block_start, on_record_end);
        }

        let records =
            self.emit_block_records(block_start, unquoted_lfs, unquoted_delims, on_record_end);

        let next_in_quote = (in_quote_inclusive >> 63) != 0;
        self.state = if next_in_quote {
            if is_escape_byte && next_escaped {
                ScanState::QuotedEscape
            } else {
                ScanState::Quoted
            }
        } else if (closing_quotes >> 63) != 0 {
            ScanState::AfterQuote
        } else if (after_quote_crs >> 63) != 0 {
            ScanState::AfterQuoteCr
        } else if ((unquoted_delims | unquoted_lfs) >> 63) != 0 {
            ScanState::FieldStart
        } else {
            ScanState::Unquoted
        };

        Ok(records)
    }

    fn scan_scalar_slice(
        &mut self,
        bytes: &[u8],
        absolute_start: u64,
        on_record_end: &mut impl FnMut(u64),
    ) -> Result<u64, ParseError> {
        let mut index = 0;
        let mut records = 0;
        let delimiter = self.dialect.delimiter;
        let quote = self.dialect.quote;
        let escape = self.dialect.escape;

        while index < bytes.len() {
            match self.state {
                ScanState::FieldStart | ScanState::Unquoted => {
                    let found_rel = match quote {
                        Some(q) => memchr3(delimiter, q, b'\n', &bytes[index..]),
                        None => memchr2(delimiter, b'\n', &bytes[index..]),
                    };
                    let Some(relative) = found_rel else {
                        if self.state == ScanState::FieldStart {
                            self.state = ScanState::Unquoted;
                        }
                        break;
                    };
                    let found = index + relative;
                    if found > index && self.state == ScanState::FieldStart {
                        self.state = ScanState::Unquoted;
                        index = found;
                        continue;
                    }

                    let byte = bytes[found];
                    if byte == delimiter {
                        self.end_field();
                        self.state = ScanState::FieldStart;
                        index = found + 1;
                    } else if Some(byte) == quote {
                        if self.state == ScanState::FieldStart {
                            self.state = ScanState::Quoted;
                            index = found + 1;
                        } else {
                            return Err(ParseError::new(
                                absolute_start + found as u64,
                                ParseErrorKind::UnexpectedQuote,
                            ));
                        }
                    } else if byte == b'\n' {
                        let end = absolute_start + found as u64 + 1;
                        self.end_record();
                        on_record_end(end);
                        records += 1;
                        self.record_start = end;
                        self.state = ScanState::FieldStart;
                        index = found + 1;
                    } else {
                        unreachable!()
                    }
                }
                ScanState::Quoted => {
                    let q = quote.expect("Quoted state requires quote byte");
                    match escape {
                        EscapeRule::DoubledQuote => {
                            let Some(relative) = memchr(q, &bytes[index..]) else {
                                break;
                            };
                            index += relative + 1;
                            self.state = ScanState::AfterQuote;
                        }
                        EscapeRule::EscapeByte(esc) => {
                            let Some(relative) = memchr2(q, esc, &bytes[index..]) else {
                                break;
                            };
                            let found = index + relative;
                            index = found + 1;
                            if bytes[found] == esc {
                                self.state = ScanState::QuotedEscape;
                            } else {
                                self.state = ScanState::AfterQuote;
                            }
                        }
                    }
                }
                ScanState::QuotedEscape => {
                    // Consume any single escaped byte inside the quoted field.
                    index += 1;
                    self.state = ScanState::Quoted;
                }
                ScanState::AfterQuote => {
                    let byte = bytes[index];
                    if escape == EscapeRule::DoubledQuote && Some(byte) == quote {
                        self.state = ScanState::Quoted;
                        index += 1;
                    } else if byte == delimiter {
                        self.end_field();
                        self.state = ScanState::FieldStart;
                        index += 1;
                    } else if byte == b'\r' {
                        self.state = ScanState::AfterQuoteCr;
                        index += 1;
                    } else if byte == b'\n' {
                        let end = absolute_start + index as u64 + 1;
                        self.end_record();
                        on_record_end(end);
                        records += 1;
                        self.record_start = end;
                        self.state = ScanState::FieldStart;
                        index += 1;
                    } else {
                        return Err(ParseError::new(
                            absolute_start + index as u64,
                            ParseErrorKind::UnexpectedAfterQuote(byte),
                        ));
                    }
                }
                ScanState::AfterQuoteCr => {
                    if bytes[index] != b'\n' {
                        return Err(ParseError::new(
                            absolute_start + index as u64,
                            ParseErrorKind::ExpectedLfAfterCr,
                        ));
                    }
                    let end = absolute_start + index as u64 + 1;
                    self.end_record();
                    on_record_end(end);
                    records += 1;
                    self.record_start = end;
                    self.state = ScanState::FieldStart;
                    index += 1;
                }
            }
        }

        Ok(records)
    }

    /// Finishes an EOF-delimited final record and validates quote state.
    pub fn finish(
        &mut self,
        absolute_end: u64,
        mut on_record_end: impl FnMut(u64),
    ) -> Result<bool, ParseError> {
        if !self.bom_checked {
            let prefix = self.bom_prefix;
            let prefix_len = self.bom_prefix_len;
            self.bom_checked = true;
            self.scan_data_chunk(&prefix[..prefix_len], 0, &mut on_record_end)?;
        }

        match self.state {
            ScanState::Quoted | ScanState::QuotedEscape => {
                return Err(ParseError::new(
                    absolute_end,
                    ParseErrorKind::UnterminatedQuote,
                ));
            }
            ScanState::AfterQuoteCr => {
                return Err(ParseError::new(
                    absolute_end,
                    ParseErrorKind::ExpectedLfAfterCr,
                ));
            }
            _ => {}
        }

        if absolute_end > self.record_start {
            self.end_record();
            on_record_end(absolute_end);
            self.record_start = absolute_end;
            self.state = ScanState::FieldStart;
            return Ok(true);
        }
        Ok(false)
    }
}

impl StructuralScanner<true> {
    /// Widest completed logical record, or zero when no record has ended.
    pub fn maximum_width(&self) -> usize {
        self.maximum_width
    }
}

pub fn parse_record(record: &[u8], delimiter: u8) -> Result<Vec<Cow<'_, [u8]>>, ParseError> {
    parse_record_with_field_limit(record, delimiter, usize::MAX)
}

pub fn parse_record_with_field_limit(
    record: &[u8],
    delimiter: u8,
    max_fields: usize,
) -> Result<Vec<Cow<'_, [u8]>>, ParseError> {
    parse_record_with_dialect_and_field_limit(
        record,
        ScannerDialect::rfc4180(delimiter),
        ByteEncodingClass::Utf8,
        max_fields,
    )
}

pub fn parse_record_with_dialect(
    record: &[u8],
    dialect: ScannerDialect,
    encoding: ByteEncodingClass,
) -> Result<Vec<Cow<'_, [u8]>>, ParseError> {
    parse_record_with_dialect_and_field_limit(record, dialect, encoding, usize::MAX)
}

pub fn parse_record_with_dialect_and_field_limit(
    record: &[u8],
    dialect: ScannerDialect,
    encoding: ByteEncodingClass,
    max_fields: usize,
) -> Result<Vec<Cow<'_, [u8]>>, ParseError> {
    dialect.validate(encoding)?;
    let record = strip_record_ending(record);
    let delimiter = dialect.delimiter;
    let quote = dialect.quote;
    let escape = dialect.escape;

    let mut fields: Vec<Cow<'_, [u8]>> = Vec::new();
    let mut start = 0;

    loop {
        if fields.len() == max_fields {
            return Err(ParseError::new(
                start as u64,
                ParseErrorKind::FieldLimitExceeded(max_fields),
            ));
        }
        if start == record.len() {
            fields.push(Cow::Borrowed(&[]));
            break;
        }

        if Some(record[start]) == quote {
            let q = quote.unwrap();
            let content_start = start + 1;
            let mut cursor = content_start;
            let mut segment_start = content_start;
            let mut unescaped: Option<Vec<u8>> = None;

            loop {
                match escape {
                    EscapeRule::DoubledQuote => {
                        let Some(relative) = memchr(q, &record[cursor..]) else {
                            return Err(ParseError::new(
                                record.len() as u64,
                                ParseErrorKind::UnterminatedQuote,
                            ));
                        };
                        let quote_pos = cursor + relative;
                        if record.get(quote_pos + 1) == Some(&q) {
                            let output = unescaped.get_or_insert_with(|| {
                                Vec::with_capacity(quote_pos - content_start + 1)
                            });
                            output.extend_from_slice(&record[segment_start..quote_pos]);
                            output.push(q);
                            cursor = quote_pos + 2;
                            segment_start = cursor;
                            continue;
                        }
                        let field = if let Some(mut output) = unescaped {
                            output.extend_from_slice(&record[segment_start..quote_pos]);
                            Cow::Owned(output)
                        } else {
                            Cow::Borrowed(&record[content_start..quote_pos])
                        };
                        fields.push(field);

                        let after_quote = quote_pos + 1;
                        if after_quote == record.len() {
                            return Ok(fields);
                        }
                        if record[after_quote] != delimiter {
                            return Err(ParseError::new(
                                after_quote as u64,
                                ParseErrorKind::UnexpectedAfterQuote(record[after_quote]),
                            ));
                        }
                        start = after_quote + 1;
                        break;
                    }
                    EscapeRule::EscapeByte(esc) => {
                        let Some(relative) = memchr2(q, esc, &record[cursor..]) else {
                            return Err(ParseError::new(
                                record.len() as u64,
                                ParseErrorKind::UnterminatedQuote,
                            ));
                        };
                        let pos = cursor + relative;
                        if record[pos] == esc {
                            let Some(&escaped_byte) = record.get(pos + 1) else {
                                return Err(ParseError::new(
                                    record.len() as u64,
                                    ParseErrorKind::UnterminatedQuote,
                                ));
                            };
                            let output = unescaped
                                .get_or_insert_with(|| Vec::with_capacity(pos - content_start + 1));
                            output.extend_from_slice(&record[segment_start..pos]);
                            output.push(escaped_byte);
                            cursor = pos + 2;
                            segment_start = cursor;
                            continue;
                        }
                        let field = if let Some(mut output) = unescaped {
                            output.extend_from_slice(&record[segment_start..pos]);
                            Cow::Owned(output)
                        } else {
                            Cow::Borrowed(&record[content_start..pos])
                        };
                        fields.push(field);

                        let after_quote = pos + 1;
                        if after_quote == record.len() {
                            return Ok(fields);
                        }
                        if record[after_quote] != delimiter {
                            return Err(ParseError::new(
                                after_quote as u64,
                                ParseErrorKind::UnexpectedAfterQuote(record[after_quote]),
                            ));
                        }
                        start = after_quote + 1;
                        break;
                    }
                }
            }
        } else {
            let end = memchr(delimiter, &record[start..])
                .map(|relative| start + relative)
                .unwrap_or(record.len());
            if let Some(q) = quote
                && let Some(relative) = memchr(q, &record[start..end])
            {
                return Err(ParseError::new(
                    (start + relative) as u64,
                    ParseErrorKind::UnexpectedQuote,
                ));
            }
            fields.push(Cow::Borrowed(&record[start..end]));
            if end == record.len() {
                return Ok(fields);
            }
            start = end + 1;
        }
    }

    Ok(fields)
}

fn strip_record_ending(mut record: &[u8]) -> &[u8] {
    if record.ends_with(b"\n") {
        record = &record[..record.len() - 1];
        if record.ends_with(b"\r") {
            record = &record[..record.len() - 1];
        }
    }
    record
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::{
        ByteEncodingClass, EscapeRule, ParseErrorKind, RecordScanner, ScannerDialect, SimdBackend,
        parse_record, parse_record_with_dialect, parse_record_with_field_limit,
    };

    fn fields(input: &[u8]) -> Vec<Vec<u8>> {
        parse_record(input, b',')
            .unwrap()
            .into_iter()
            .map(Cow::into_owned)
            .collect()
    }

    #[test]
    fn parses_required_csv_forms() {
        assert_eq!(fields(b"a,b,c\n"), [b"a", b"b", b"c"]);
        assert_eq!(fields(b"1,2,3\n"), [b"1", b"2", b"3"]);
        assert_eq!(
            fields(b"\"hello, world\",2,3\n"),
            [b"hello, world".as_slice(), b"2", b"3"]
        );
        assert_eq!(
            fields(b"\"a \"\"quoted\"\" value\",2,3\n"),
            [b"a \"quoted\" value".as_slice(), b"2", b"3"]
        );
        assert_eq!(
            fields(b"\"first\nsecond\",2,3\r\n"),
            [b"first\nsecond".as_slice(), b"2", b"3"]
        );
    }

    #[test]
    fn handles_empty_records_fields_and_trailing_delimiters() {
        assert_eq!(fields(b"\n"), [b"".as_slice()]);
        assert_eq!(fields(b",a,,\n"), [b"".as_slice(), b"a", b"", b""]);
        assert_eq!(fields(b"\"\",\"\"\n"), [b"".as_slice(), b""]);
    }

    #[test]
    fn scanner_is_correct_at_every_chunk_boundary() {
        let input = b"a,b\n\"quoted\nvalue\",\"escaped \"\"quote\"\"\"\r\nx,y";
        let expected = [4_u64, 40, input.len() as u64];

        for split in 0..=input.len() {
            let mut scanner = RecordScanner::new(b',').unwrap();
            let mut ends = Vec::new();
            scanner
                .scan_chunk(&input[..split], 0, |end| ends.push(end))
                .unwrap();
            scanner
                .scan_chunk(&input[split..], split as u64, |end| ends.push(end))
                .unwrap();
            scanner
                .finish(input.len() as u64, |end| ends.push(end))
                .unwrap();
            assert_eq!(ends, expected, "split {split}");
        }
    }

    #[test]
    fn scanner_ignores_an_initial_bom_for_quoted_multiline_framing_across_tiny_chunks() {
        let first = b"\xEF\xBB\xBF\"one\ncontinued\",two\n";
        let second = b"last,row\n";
        let input = [first.as_slice(), second.as_slice()].concat();
        let mut scanner = RecordScanner::new(b',').unwrap();
        let mut ends = Vec::new();

        for (offset, byte) in input.iter().enumerate() {
            scanner
                .scan_chunk(std::slice::from_ref(byte), offset as u64, |end| {
                    ends.push(end)
                })
                .unwrap();
        }
        assert!(
            !scanner
                .finish(input.len() as u64, |end| ends.push(end))
                .unwrap()
        );
        assert_eq!(ends, [first.len() as u64, input.len() as u64]);
    }

    #[test]
    fn scanner_does_not_ignore_a_bom_on_a_later_record() {
        let input = b"a,b\n\xEF\xBB\xBF\"c\",d\n";
        let mut scanner = RecordScanner::new(b',').unwrap();
        assert_eq!(
            scanner.scan_chunk(input, 0, |_| {}).unwrap_err().kind,
            ParseErrorKind::UnexpectedQuote
        );
    }

    #[test]
    fn bounded_parser_stops_before_parsing_the_first_excess_field() {
        assert_eq!(
            parse_record_with_field_limit(b"a,b,\"unterminated", b',', 2)
                .unwrap_err()
                .kind,
            ParseErrorKind::FieldLimitExceeded(2)
        );
    }

    #[test]
    fn handles_large_fields_without_per_byte_objects() {
        let field = vec![b'x'; 1_048_576];
        let mut record = field.clone();
        record.extend_from_slice(b",tail\n");
        let parsed = parse_record(&record, b',').unwrap();
        assert_eq!(parsed[0].as_ref(), field);
        assert_eq!(parsed[1].as_ref(), b"tail");
    }

    #[test]
    fn escaped_fields_allocate_in_proportion_to_the_record() {
        let mut record = Vec::new();
        for field in 0..128 {
            if field > 0 {
                record.push(b',');
            }
            record.extend_from_slice(b"\"a\"\"b\"");
        }

        let parsed = parse_record(&record, b',').unwrap();
        let total_capacity = parsed
            .iter()
            .map(|field| match field {
                Cow::Borrowed(_) => 0,
                Cow::Owned(bytes) => bytes.capacity(),
            })
            .sum::<usize>();

        assert_eq!(parsed.len(), 128);
        assert!(parsed.iter().all(|field| field.as_ref() == b"a\"b"));
        assert!(total_capacity <= record.len() * 4);
    }

    #[test]
    fn exposes_malformed_quote_sequences() {
        assert_eq!(
            parse_record(b"bad\"quote,2", b',').unwrap_err().kind,
            ParseErrorKind::UnexpectedQuote
        );
        assert!(matches!(
            parse_record(b"\"bad\"x,2", b',').unwrap_err().kind,
            ParseErrorKind::UnexpectedAfterQuote(b'x')
        ));

        let mut scanner = RecordScanner::new(b',').unwrap();
        scanner.scan_chunk(b"\"unfinished", 0, |_| {}).unwrap();
        assert_eq!(
            scanner.finish(11, |_| {}).unwrap_err().kind,
            ParseErrorKind::UnterminatedQuote
        );
    }

    #[test]
    fn a_bom_only_file_has_no_csv_record() {
        for chunk_bytes in 1..=3 {
            let mut scanner = RecordScanner::new(b',').unwrap();
            let mut ends = Vec::new();
            let bytes = b"\xef\xbb\xbf";
            for (chunk, offset) in bytes.chunks(chunk_bytes).zip((0..).step_by(chunk_bytes)) {
                scanner
                    .scan_chunk(chunk, offset as u64, |end| ends.push(end))
                    .unwrap();
            }
            assert!(!scanner.finish(3, |end| ends.push(end)).unwrap());
            assert!(ends.is_empty());

            // Single-byte encodings treat EF BB BF as ordinary data bytes rather than a UTF-8 BOM.
            let mut latin1 = super::StructuralScanner::<true>::with_dialect(
                ScannerDialect::rfc4180(b','),
                0,
                ByteEncodingClass::SingleByteAsciiSuperset,
            )
            .unwrap();
            let mut latin1_ends = Vec::new();
            for (chunk, offset) in bytes.chunks(chunk_bytes).zip((0..).step_by(chunk_bytes)) {
                latin1
                    .scan_chunk(chunk, offset as u64, |end| latin1_ends.push(end))
                    .unwrap();
            }
            assert!(latin1.finish(3, |end| latin1_ends.push(end)).unwrap());
            assert_eq!(latin1_ends, vec![3]);
            assert_eq!(latin1.maximum_width(), 1);
        }
    }

    #[test]
    fn dialect_validation_rejects_invalid_and_non_ascii_utf8_structural_bytes() {
        assert_eq!(
            ScannerDialect::rfc4180(b'\n')
                .validate(ByteEncodingClass::Utf8)
                .unwrap_err()
                .kind,
            ParseErrorKind::InvalidDelimiter
        );
        assert_eq!(
            ScannerDialect::rfc4180(0x80)
                .validate(ByteEncodingClass::Utf8)
                .unwrap_err()
                .kind,
            ParseErrorKind::InvalidDelimiter
        );
        assert_eq!(
            ScannerDialect::new(b',', Some(b'\r'), EscapeRule::DoubledQuote)
                .validate(ByteEncodingClass::Utf8)
                .unwrap_err()
                .kind,
            ParseErrorKind::InvalidDialect
        );
        assert_eq!(
            ScannerDialect::new(b',', Some(b'"'), EscapeRule::EscapeByte(b'"'))
                .validate(ByteEncodingClass::Utf8)
                .unwrap_err()
                .kind,
            ParseErrorKind::InvalidDialect
        );
        assert_eq!(
            ScannerDialect::new(b',', None, EscapeRule::EscapeByte(b'\\'))
                .validate(ByteEncodingClass::Utf8)
                .unwrap_err()
                .kind,
            ParseErrorKind::InvalidDialect
        );
    }

    #[test]
    fn parameterized_escape_byte_and_unquoted_dialects_parse_cleanly() {
        let bs = ScannerDialect::new(b',', Some(b'\''), EscapeRule::EscapeByte(b'\\'));
        let parsed: Vec<Vec<u8>> =
            parse_record_with_dialect(b"'it\\'s\\\\ok',raw\\field\n", bs, ByteEncodingClass::Utf8)
                .unwrap()
                .into_iter()
                .map(Cow::into_owned)
                .collect();
        assert_eq!(parsed, vec![b"it's\\ok".to_vec(), b"raw\\field".to_vec()]);

        let unquoted = ScannerDialect::new(b'\t', None, EscapeRule::DoubledQuote);
        let parsed: Vec<Vec<u8>> =
            parse_record_with_dialect(b"a\"b\tc\"d\n", unquoted, ByteEncodingClass::Utf8)
                .unwrap()
                .into_iter()
                .map(Cow::into_owned)
                .collect();
        assert_eq!(parsed, vec![b"a\"b".to_vec(), b"c\"d".to_vec()]);

        let scanner = RecordScanner::with_dialect_and_backend(
            bs,
            0,
            ByteEncodingClass::Utf8,
            SimdBackend::Scalar,
        )
        .unwrap();
        assert_eq!(scanner.backend(), SimdBackend::Scalar);
    }
}
