//! Parameterized delimited-text dialect configuration and validation.

use crate::{ParseError, ParseErrorKind};

/// Escape rule applied inside quoted fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EscapeRule {
    /// RFC 4180 doubled-quote escaping (`""` represents a literal quote).
    DoubledQuote,
    /// Single-byte escape prefix scoped strictly inside quoted fields (e.g., `\"`, `\\`).
    EscapeByte(u8),
}

/// Encoding class governing SIMD structural byte safety and structural-byte validation.
///
/// Only ASCII-safe byte encodings where ASCII bytes (`< 0x80`) cannot collide with
/// multi-byte code unit continuations may use the 64-byte SIMD block classifier.
/// Multi-byte encodings such as UTF-16 stay on the scalar encoding-aware path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ByteEncodingClass {
    /// UTF-8: ASCII bytes (`< 0x80`) never appear as lead or continuation bytes.
    Utf8,
    /// Single-byte ASCII-compatible encodings (ISO-8859-1 / Latin-1, Windows-1252).
    SingleByteAsciiSuperset,
}

/// Parameterized delimited dialect shared across structural scanning and field decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScannerDialect {
    pub delimiter: u8,
    pub quote: Option<u8>,
    pub escape: EscapeRule,
}

impl ScannerDialect {
    /// Standard RFC 4180 dialect: given delimiter, `"` quote byte, and doubled-quote escaping.
    pub const fn rfc4180(delimiter: u8) -> Self {
        Self {
            delimiter,
            quote: Some(b'"'),
            escape: EscapeRule::DoubledQuote,
        }
    }

    /// Construct a parameterized dialect. Call [`Self::validate`] (or pass into a scanner/parser)
    /// to verify structural byte constraints.
    pub const fn new(delimiter: u8, quote: Option<u8>, escape: EscapeRule) -> Self {
        Self {
            delimiter,
            quote,
            escape,
        }
    }

    /// Validate structural byte rules for the given encoding class.
    ///
    /// Rejects:
    /// - CR (`\r`) or LF (`\n`) as delimiter, quote, or escape byte.
    /// - Non-ASCII structural bytes (`>= 0x80`) under UTF-8.
    /// - Duplicate structural bytes across delimiter, quote, and escape.
    /// - Quote-internal `EscapeByte` when quoting is disabled (`quote == None`).
    pub fn validate(self, encoding: ByteEncodingClass) -> Result<(), ParseError> {
        let is_line_break = |b: u8| matches!(b, b'\r' | b'\n');

        if is_line_break(self.delimiter) {
            return Err(ParseError::new(0, ParseErrorKind::InvalidDelimiter));
        }
        if encoding == ByteEncodingClass::Utf8 && !self.delimiter.is_ascii() {
            return Err(ParseError::new(0, ParseErrorKind::InvalidDelimiter));
        }

        if let Some(quote) = self.quote {
            if self.delimiter == quote {
                return Err(ParseError::new(0, ParseErrorKind::InvalidDelimiter));
            }
            if is_line_break(quote) {
                return Err(ParseError::new(0, ParseErrorKind::InvalidDialect));
            }
            if encoding == ByteEncodingClass::Utf8 && !quote.is_ascii() {
                return Err(ParseError::new(0, ParseErrorKind::InvalidDialect));
            }
        }

        if let EscapeRule::EscapeByte(esc) = self.escape {
            let Some(quote) = self.quote else {
                return Err(ParseError::new(0, ParseErrorKind::InvalidDialect));
            };
            if is_line_break(esc) || esc == self.delimiter || esc == quote {
                return Err(ParseError::new(0, ParseErrorKind::InvalidDialect));
            }
            if encoding == ByteEncodingClass::Utf8 && !esc.is_ascii() {
                return Err(ParseError::new(0, ParseErrorKind::InvalidDialect));
            }
        }

        Ok(())
    }
}
