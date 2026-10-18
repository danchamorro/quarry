//! Explicit, lossless text encodings while retaining physical source offsets.

use std::borrow::Cow;
use std::fmt;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use crate::QuarryError;

/// Supported file encodings. Auto detection is represented by None when opening.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Encoding {
    #[default]
    Utf8,
    Utf16Le,
    Utf16Be,
    Windows1252,
    Latin1,
}

// Unicode Consortium CP1252.TXT. Undefined byte positions remain errors.
const WINDOWS_1252: [u32; 32] = [
    0x20ac, 0xffff, 0x201a, 0x0192, 0x201e, 0x2026, 0x2020, 0x2021, 0x02c6, 0x2030, 0x0160, 0x2039,
    0x0152, 0xffff, 0x017d, 0xffff, 0xffff, 0x2018, 0x2019, 0x201c, 0x201d, 0x2022, 0x2013, 0x2014,
    0x02dc, 0x2122, 0x0161, 0x203a, 0x0153, 0xffff, 0x017e, 0x0178,
];

impl Encoding {
    pub const ALL: [Self; 5] = [
        Self::Utf8,
        Self::Utf16Le,
        Self::Utf16Be,
        Self::Windows1252,
        Self::Latin1,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf16Le => "UTF-16 LE",
            Self::Utf16Be => "UTF-16 BE",
            Self::Windows1252 => "Windows-1252",
            Self::Latin1 => "ISO-8859-1",
        }
    }

    pub(crate) fn is_utf16(self) -> bool {
        matches!(self, Self::Utf16Le | Self::Utf16Be)
    }

    pub(crate) fn width(self) -> usize {
        if self.is_utf16() { 2 } else { 1 }
    }

    pub(crate) fn ends_with_ascii(self, bytes: &[u8], ascii: u8) -> bool {
        if self.is_utf16() {
            bytes.len() >= 2
                && self.unit([bytes[bytes.len() - 2], bytes[bytes.len() - 1]]) == u16::from(ascii)
        } else {
            bytes.last() == Some(&ascii)
        }
    }

    pub(crate) fn bom(self) -> &'static [u8] {
        match self {
            Self::Utf8 => b"\xef\xbb\xbf",
            Self::Utf16Le => b"\xff\xfe",
            Self::Utf16Be => b"\xfe\xff",
            Self::Windows1252 | Self::Latin1 => b"",
        }
    }

    pub(crate) fn strip_bom(self, bytes: &[u8]) -> (&[u8], &[u8]) {
        let bom = self.bom();
        if !bom.is_empty() && bytes.starts_with(bom) {
            (bom, &bytes[bom.len()..])
        } else {
            (&[], bytes)
        }
    }

    pub(crate) fn detect(file: &mut File, requested: Option<Self>) -> Result<Self, QuarryError> {
        let position = file.stream_position()?;
        file.seek(SeekFrom::Start(0))?;
        let mut prefix = [0; 4];
        let mut used = 0;
        while used < prefix.len() {
            let count = file.read(&mut prefix[used..])?;
            if count == 0 {
                break;
            }
            used += count;
        }
        file.seek(SeekFrom::Start(position))?;
        let prefix = &prefix[..used];
        if requested.is_none()
            && (prefix.starts_with(b"\xff\xfe\0\0") || prefix.starts_with(b"\0\0\xfe\xff"))
        {
            return Err(QuarryError::Encoding(
                "UTF-32 is not supported. Convert this file to UTF-8 or UTF-16 before opening it."
                    .into(),
            ));
        }
        let detected = if prefix.starts_with(Self::Utf8.bom()) {
            Some(Self::Utf8)
        } else if prefix.starts_with(Self::Utf16Le.bom()) {
            Some(Self::Utf16Le)
        } else if prefix.starts_with(Self::Utf16Be.bom()) {
            Some(Self::Utf16Be)
        } else {
            None
        };
        if let Some(requested) = requested {
            if !matches!(requested, Self::Windows1252 | Self::Latin1)
                && detected.is_some_and(|detected| detected != requested)
            {
                return Err(QuarryError::Encoding(format!(
                    "The file's byte-order mark conflicts with {}. Choose Auto or the matching encoding.",
                    requested.label(),
                )));
            }
            Ok(requested)
        } else {
            Ok(detected.unwrap_or(Self::Utf8))
        }
    }

    /// Decode a record without replacing malformed input. UTF-8 keeps the existing raw-byte path.
    pub(crate) fn decode(self, bytes: &[u8], limit: usize) -> Result<Cow<'_, [u8]>, QuarryError> {
        if bytes.len() > limit {
            return Err(QuarryError::RecordTooLarge { limit });
        }
        if self == Self::Utf8 {
            return Ok(Cow::Borrowed(bytes));
        }
        let mut text = String::with_capacity(bytes.len().min(limit));
        if self.is_utf16() {
            if !bytes.len().is_multiple_of(2) {
                return Err(self.invalid(
                    bytes.len().saturating_sub(1) as u64,
                    "incomplete UTF-16 code unit",
                ));
            }
            let mut position = 0;
            while position < bytes.len() {
                let unit = self.unit([bytes[position], bytes[position + 1]]);
                let scalar = if (0xd800..=0xdbff).contains(&unit) {
                    if position + 3 >= bytes.len() {
                        return Err(self.invalid(position as u64, "unpaired high surrogate"));
                    }
                    let low = self.unit([bytes[position + 2], bytes[position + 3]]);
                    if !(0xdc00..=0xdfff).contains(&low) {
                        return Err(self.invalid(position as u64, "unpaired high surrogate"));
                    }
                    position += 2;
                    0x10000 + ((u32::from(unit) - 0xd800) << 10) + u32::from(low) - 0xdc00
                } else if (0xdc00..=0xdfff).contains(&unit) {
                    return Err(self.invalid(position as u64, "unpaired low surrogate"));
                } else {
                    u32::from(unit)
                };
                append(
                    &mut text,
                    char::from_u32(scalar).expect("validated scalar"),
                    limit,
                )?;
                position += 2;
            }
        } else {
            for (position, &byte) in bytes.iter().enumerate() {
                let scalar = if self == Self::Windows1252 && (0x80..=0x9f).contains(&byte) {
                    let mapped = WINDOWS_1252[usize::from(byte - 0x80)];
                    if mapped == 0xffff {
                        return Err(
                            self.invalid(position as u64, &format!("undefined byte 0x{byte:02X}"))
                        );
                    }
                    mapped
                } else {
                    u32::from(byte)
                };
                append(
                    &mut text,
                    char::from_u32(scalar).expect("single-byte scalar"),
                    limit,
                )?;
            }
        }
        Ok(Cow::Owned(text.into_bytes()))
    }

    pub(crate) fn encoded_len(self, bytes: &[u8]) -> Result<usize, QuarryError> {
        if self == Self::Utf8 {
            return Ok(bytes.len());
        }
        let text = std::str::from_utf8(bytes).map_err(|_| QuarryError::Encoding(
            "Cannot encode invalid working text. Choose the correct source encoding before editing.".into(),
        ))?;
        if self.is_utf16() {
            Ok(text.encode_utf16().count().saturating_mul(2))
        } else {
            for character in text.chars() {
                self.byte(character)?;
            }
            Ok(text.chars().count())
        }
    }

    pub(crate) fn encode(self, bytes: &[u8], limit: usize) -> Result<Cow<'_, [u8]>, QuarryError> {
        let length = self.encoded_len(bytes)?;
        if length > limit {
            return Err(QuarryError::RecordTooLarge { limit });
        }
        if self == Self::Utf8 {
            return Ok(Cow::Borrowed(bytes));
        }
        let text = std::str::from_utf8(bytes).expect("encoded_len checked UTF-8");
        let mut output = Vec::with_capacity(length);
        if self.is_utf16() {
            for unit in text.encode_utf16() {
                output.extend_from_slice(&if self == Self::Utf16Le {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                });
            }
        } else {
            for character in text.chars() {
                output.push(self.byte(character)?);
            }
        }
        Ok(Cow::Owned(output))
    }

    fn byte(self, character: char) -> Result<u8, QuarryError> {
        let scalar = character as u32;
        if self == Self::Latin1 && scalar <= 0xff
            || self == Self::Windows1252 && (scalar <= 0x7f || (0xa0..=0xff).contains(&scalar))
        {
            return Ok(scalar as u8);
        }
        if self == Self::Windows1252
            && let Some(index) = WINDOWS_1252
                .iter()
                .position(|&value| value == scalar && value != 0xffff)
        {
            return Ok(0x80 + index as u8);
        }
        Err(QuarryError::Encoding(format!(
            "Cannot save U+{scalar:04X} in {}. Use Save As UTF-8 to preserve this character.",
            self.label(),
        )))
    }

    fn unit(self, bytes: [u8; 2]) -> u16 {
        if self == Self::Utf16Le {
            u16::from_le_bytes(bytes)
        } else {
            u16::from_be_bytes(bytes)
        }
    }

    fn invalid(self, offset: u64, reason: &str) -> QuarryError {
        QuarryError::Encoding(format!(
            "Invalid {} at byte {offset}: {reason}. Choose the correct encoding; no text was replaced.",
            self.label(),
        ))
    }

    pub(crate) fn chunk_size(self, requested: usize) -> usize {
        if self.is_utf16() {
            requested.max(2) & !1
        } else {
            requested.max(1)
        }
    }

    /// Keep UTF-16 callbacks inside the current raw-byte chunk, including after a short read.
    pub(crate) fn read_chunk(
        self,
        file: &mut (impl Read + Seek),
        buffer: &mut [u8],
    ) -> Result<usize, QuarryError> {
        if self.is_utf16() && (buffer.len() < 2 || !buffer.len().is_multiple_of(2)) {
            return Err(QuarryError::InvalidOption(
                "UTF-16 read buffers must contain whole code units",
            ));
        }
        let mut count = file.read(buffer)?;
        if self.is_utf16() && !count.is_multiple_of(2) {
            let extra = file.read(&mut buffer[count..count + 1])?;
            if extra == 0 {
                return Err(self.invalid(
                    file.stream_position()?.saturating_sub(1),
                    "incomplete UTF-16 code unit",
                ));
            }
            count += extra;
        }
        Ok(count)
    }
}

fn append(text: &mut String, character: char, limit: usize) -> Result<(), QuarryError> {
    if text.len().saturating_add(character.len_utf8()) > limit {
        return Err(QuarryError::RecordTooLarge { limit });
    }
    text.push(character);
    Ok(())
}

impl fmt::Display for Encoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

impl std::str::FromStr for Encoding {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "utf8" | "utf-8" => Ok(Self::Utf8),
            "utf16le" | "utf-16le" | "utf-16-le" => Ok(Self::Utf16Le),
            "utf16be" | "utf-16be" | "utf-16-be" => Ok(Self::Utf16Be),
            "windows1252" | "windows-1252" | "cp1252" => Ok(Self::Windows1252),
            "latin1" | "latin-1" | "iso-8859-1" => Ok(Self::Latin1),
            _ => Err("encoding must be utf8, utf16le, utf16be, windows1252, or latin1".into()),
        }
    }
}

/// Encoding-aware structural scanner; record ends remain physical source byte offsets.
#[derive(Debug, Clone)]
pub(crate) struct RecordScanner {
    inner: quarry_delimited::RecordScanner,
    encoding: Encoding,
    delimiter: u8,
    at_start: bool,
    high_surrogate: Option<u64>,
    shadow: Vec<u8>,
}

impl RecordScanner {
    pub(crate) fn new(delimiter: u8, encoding: Encoding) -> Result<Self, QuarryError> {
        Self::at_offset(delimiter, 0, encoding)
    }

    pub(crate) fn at_offset(
        delimiter: u8,
        offset: u64,
        encoding: Encoding,
    ) -> Result<Self, QuarryError> {
        if encoding.is_utf16() && !offset.is_multiple_of(2) {
            return Err(QuarryError::InvalidOption(
                "UTF-16 record offset must be a whole code unit",
            ));
        }
        let logical = offset / encoding.width() as u64;
        let inner = if encoding == Encoding::Utf8 {
            quarry_delimited::RecordScanner::at_offset(delimiter, logical)?
        } else {
            quarry_delimited::RecordScanner::at_offset_without_bom(delimiter, logical)?
        };
        Ok(Self {
            inner,
            encoding,
            delimiter,
            at_start: offset == 0,
            high_surrogate: None,
            shadow: Vec::new(),
        })
    }

    pub(crate) fn record_start(&self) -> u64 {
        self.inner.record_start() * self.encoding.width() as u64
    }

    pub(crate) fn scan_chunk(
        &mut self,
        bytes: &[u8],
        absolute_start: u64,
        mut on_end: impl FnMut(u64),
    ) -> Result<u64, QuarryError> {
        if !self.encoding.is_utf16() {
            return self
                .inner
                .scan_chunk(bytes, absolute_start, on_end)
                .map_err(Into::into);
        }
        if !absolute_start.is_multiple_of(2) || !bytes.len().is_multiple_of(2) {
            return Err(self
                .encoding
                .invalid(absolute_start, "incomplete UTF-16 code unit"));
        }
        self.shadow.clear();
        let mut skip = 0;
        for (index, pair) in bytes.chunks_exact(2).enumerate() {
            let offset = absolute_start + index as u64 * 2;
            let unit = self.encoding.unit([pair[0], pair[1]]);
            if self.at_start {
                self.at_start = false;
                if unit == 0xfeff {
                    self.inner =
                        quarry_delimited::RecordScanner::at_offset_without_bom(self.delimiter, 1)?;
                    skip = 1;
                    continue;
                }
            }
            if let Some(high) = self.high_surrogate.take() {
                if !(0xdc00..=0xdfff).contains(&unit) {
                    return Err(self.encoding.invalid(high, "unpaired high surrogate"));
                }
            } else if (0xd800..=0xdbff).contains(&unit) {
                self.high_surrogate = Some(offset);
            } else if (0xdc00..=0xdfff).contains(&unit) {
                return Err(self.encoding.invalid(offset, "unpaired low surrogate"));
            }
            self.shadow.push(if unit < 128 { unit as u8 } else { b'x' });
        }
        if self.shadow.is_empty() {
            return Ok(self.record_start());
        }
        self.inner
            .scan_chunk(&self.shadow, absolute_start / 2 + skip, |end| {
                on_end(end * 2)
            })
            .map(|offset| offset * 2)
            .map_err(|mut error| {
                error.offset *= 2;
                QuarryError::Parse(error)
            })
    }

    pub(crate) fn finish(
        &mut self,
        absolute_end: u64,
        mut on_end: impl FnMut(u64),
    ) -> Result<bool, QuarryError> {
        if let Some(offset) = self.high_surrogate {
            return Err(self.encoding.invalid(offset, "unpaired high surrogate"));
        }
        if self.encoding.is_utf16() && !absolute_end.is_multiple_of(2) {
            return Err(self
                .encoding
                .invalid(absolute_end - 1, "incomplete UTF-16 code unit"));
        }
        let width = self.encoding.width() as u64;
        self.inner
            .finish(absolute_end / width, |end| on_end(end * width))
            .map_err(|mut error| {
                error.offset *= width;
                QuarryError::Parse(error)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn golden_codecs_preserve_non_ascii_and_supplementary_characters() {
        let unicode = "Aé😀";
        for (encoding, golden) in [
            (
                Encoding::Utf16Le,
                &[0x41, 0, 0xe9, 0, 0x3d, 0xd8, 0, 0xde][..],
            ),
            (
                Encoding::Utf16Be,
                &[0, 0x41, 0, 0xe9, 0xd8, 0x3d, 0xde, 0][..],
            ),
        ] {
            assert_eq!(encoding.decode(golden, 100).unwrap(), unicode.as_bytes());
            assert_eq!(encoding.encode(unicode.as_bytes(), 100).unwrap(), golden);
        }
        assert_eq!(
            Encoding::Windows1252
                .decode(b"caf\xe9 \x80\x93x\x94", 100)
                .unwrap(),
            "café €“x”".as_bytes()
        );
        assert_eq!(
            Encoding::Windows1252
                .encode("café €“x”".as_bytes(), 100)
                .unwrap(),
            b"caf\xe9 \x80\x93x\x94".as_slice()
        );
        assert_eq!(
            Encoding::Latin1.decode(b"\x80\x85\xff", 100).unwrap(),
            "\u{80}\u{85}ÿ".as_bytes()
        );
        assert_eq!(
            Encoding::Latin1
                .encode("\u{80}\u{85}ÿ".as_bytes(), 100)
                .unwrap(),
            b"\x80\x85\xff".as_slice()
        );
    }

    #[test]
    fn malformed_and_unrepresentable_text_never_gets_replaced() {
        for bytes in [
            &[0x00, 0xd8][..],
            &[0x00, 0xdc][..],
            &[0x00, 0xd8, 0x41, 0][..],
            &[0x41][..],
        ] {
            assert!(Encoding::Utf16Le.decode(bytes, 100).is_err());
        }
        for byte in [0x81, 0x8d, 0x8f, 0x90, 0x9d] {
            assert!(Encoding::Windows1252.decode(&[byte], 100).is_err());
        }
        assert!(
            Encoding::Windows1252
                .encode("😀".as_bytes(), 100)
                .unwrap_err()
                .to_string()
                .contains("Save As UTF-8")
        );
        assert!(Encoding::Latin1.encode("€".as_bytes(), 100).is_err());
        assert!(Encoding::Latin1.decode(&[0xe9, 0xe9], 3).is_err());
        assert!(Encoding::Utf16Le.encode(b"abcd", 7).is_err());
    }

    #[test]
    fn signatures_only_are_detected_and_explicit_legacy_bytes_stay_data() {
        for (bytes, expected) in [
            (b"\xef\xbb\xbfabc".as_slice(), Encoding::Utf8),
            (b"\xff\xfeA\0".as_slice(), Encoding::Utf16Le),
            (b"\xfe\xff\0A".as_slice(), Encoding::Utf16Be),
            (b"caf\xe9".as_slice(), Encoding::Utf8),
        ] {
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(bytes).unwrap();
            file.rewind().unwrap();
            assert_eq!(Encoding::detect(&mut file, None).unwrap(), expected);
            assert_eq!(file.stream_position().unwrap(), 0);
            assert_eq!(
                Encoding::detect(&mut file, Some(Encoding::Latin1)).unwrap(),
                Encoding::Latin1
            );
            if expected != Encoding::Utf8 {
                assert!(Encoding::detect(&mut file, Some(Encoding::Utf8)).is_err());
            }
        }
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"\xff\xfe\0\0").unwrap();
        assert!(Encoding::detect(&mut file, None).is_err());
    }

    #[test]
    fn every_single_byte_roundtrips_or_is_a_documented_undefined_cp1252_byte() {
        for byte in 0..=255u8 {
            let latin = Encoding::Latin1.decode(&[byte], 8).unwrap().into_owned();
            assert_eq!(
                Encoding::Latin1.encode(&latin, 8).unwrap(),
                [byte].as_slice()
            );
            match Encoding::Windows1252.decode(&[byte], 8) {
                Ok(value) => assert_eq!(
                    Encoding::Windows1252.encode(&value, 8).unwrap(),
                    [byte].as_slice()
                ),
                Err(_) => assert!([0x81, 0x8d, 0x8f, 0x90, 0x9d].contains(&byte)),
            }
        }
    }

    #[test]
    fn utf16_scanner_preserves_raw_boundaries_in_small_aligned_reads() {
        // Generated independently from the literal records by Python's UTF-16 codec.
        let little: &[u8] = &[
            255, 254, 97, 0, 44, 0, 98, 0, 13, 0, 10, 0, 49, 0, 44, 0, 34, 0, 99, 0, 97, 0, 102, 0,
            233, 0, 32, 0, 61, 216, 0, 222, 44, 0, 13, 0, 10, 0, 120, 0, 34, 0, 13, 0, 10, 0, 50,
            0, 44, 0, 116, 0, 97, 0, 105, 0, 108, 0,
        ];
        let expected = [12, 46, 58];
        for encoding in [Encoding::Utf16Le, Encoding::Utf16Be] {
            let bytes = if encoding == Encoding::Utf16Le {
                little.to_vec()
            } else {
                little.chunks_exact(2).flat_map(|p| [p[1], p[0]]).collect()
            };
            for requested in 1..=19 {
                let mut file = tempfile::tempfile().unwrap();
                file.write_all(&bytes).unwrap();
                file.rewind().unwrap();
                let mut scanner = RecordScanner::new(b',', encoding).unwrap();
                let mut chunk = vec![0; encoding.chunk_size(requested)];
                let mut offset = 0;
                let mut ends = Vec::new();
                loop {
                    let read = encoding.read_chunk(&mut file, &mut chunk).unwrap();
                    if read == 0 {
                        break;
                    }
                    scanner
                        .scan_chunk(&chunk[..read], offset, |end| {
                            assert!(end >= offset && end <= offset + read as u64);
                            ends.push(end);
                        })
                        .unwrap();
                    offset += read as u64;
                }
                scanner.finish(offset, |end| ends.push(end)).unwrap();
                assert_eq!(ends, expected, "{encoding}, chunk {requested}");
            }
        }
    }

    #[test]
    fn legacy_prefix_is_not_a_utf8_signature_and_utf16_bom_only_is_empty() {
        let mut scanner = RecordScanner::new(b',', Encoding::Latin1).unwrap();
        let mut ends = Vec::new();
        scanner
            .scan_chunk(b"\xef\xbb\xbf\n", 0, |end| ends.push(end))
            .unwrap();
        scanner.finish(4, |end| ends.push(end)).unwrap();
        assert_eq!(ends, [4]);
        assert_eq!(
            Encoding::Latin1.decode(b"\xef\xbb\xbf", 20).unwrap(),
            "ï»¿".as_bytes()
        );
        for encoding in [Encoding::Utf16Le, Encoding::Utf16Be] {
            let mut scanner = RecordScanner::new(b',', encoding).unwrap();
            let mut ends = Vec::new();
            scanner
                .scan_chunk(encoding.bom(), 0, |end| ends.push(end))
                .unwrap();
            scanner.finish(2, |end| ends.push(end)).unwrap();
            assert!(ends.is_empty());
        }
    }
    #[test]
    fn sessions_decode_initial_and_indexed_rows_without_changing_physical_offsets() {
        let text = "city,value\r\nMünchen,1\r\nZürich,2\r\n";
        for encoding in Encoding::ALL {
            for with_bom in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let path = directory.path().join("cities.csv");
                let prefix = if with_bom { encoding.bom() } else { &[] };
                let mut bytes = prefix.to_vec();
                bytes.extend_from_slice(&encoding.encode(text.as_bytes(), 4096).unwrap());
                std::fs::write(&path, &bytes).unwrap();
                let session = crate::Session::open_with_encoding(
                    &path,
                    crate::OpenOptions {
                        rows: 1,
                        header_mode: crate::HeaderMode::FirstRow,
                        sample_bytes: 3,
                        bootstrap_limit: 128,
                        ..Default::default()
                    },
                    Some(encoding),
                )
                .unwrap();
                assert_eq!(session.encoding(), encoding);
                assert_eq!(
                    session.first_rows[0].fields,
                    [b"city".to_vec(), b"value".to_vec()]
                );
                let index = session
                    .start_indexing(crate::IndexConfig {
                        chunk_bytes: 3,
                        checkpoint_every: 1,
                        ..Default::default()
                    })
                    .unwrap()
                    .wait()
                    .unwrap();
                assert_eq!(index.indexed_rows(), 3);
                let rows = session.read_rows(&index, 1, 2).unwrap();
                assert_eq!(rows[0].fields[0], "München".as_bytes());
                assert_eq!(rows[1].fields[0], "Zürich".as_bytes());
                let first_data_offset =
                    prefix.len() + encoding.encode(b"city,value\r\n", 100).unwrap().len();
                assert_eq!(rows[0].offset, first_data_offset as u64);
                assert_eq!(std::fs::read(&path).unwrap(), bytes);
            }
        }
    }

    #[test]
    fn filters_and_search_use_decoded_values_and_preserve_raw_row_identity() {
        use std::collections::BTreeMap;
        use std::sync::Arc;
        for encoding in Encoding::ALL {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("search.csv");
            let mut bytes = encoding.bom().to_vec();
            bytes.extend_from_slice(
                &encoding
                    .encode(
                        "city,value\nMünchen,1\nZürich,2\nMünchen,3\n".as_bytes(),
                        4096,
                    )
                    .unwrap(),
            );
            std::fs::write(&path, &bytes).unwrap();
            let session = crate::Session::open_with_encoding(
                &path,
                crate::OpenOptions {
                    header_mode: crate::HeaderMode::FirstRow,
                    ..Default::default()
                },
                Some(encoding),
            )
            .unwrap();
            let index = session
                .start_indexing(crate::IndexConfig::default())
                .unwrap()
                .wait()
                .unwrap();
            let query = crate::FilterQuery::single(
                0,
                crate::FilterOperator::Equals,
                "München".as_bytes().to_vec(),
            );
            let filtered = session.start_filter(query.clone()).unwrap().wait().unwrap();
            assert_eq!(filtered.matches_found(), 2, "{encoding}");
            assert_eq!(
                session.read_filtered_rows(&filtered, 1, 1).unwrap()[0].row,
                3
            );
            let edits = Arc::new(BTreeMap::from([(
                (2, 0),
                Arc::new("München".as_bytes().to_vec()),
            )]));
            let filtered = session
                .start_filter_with_edits(query, edits)
                .unwrap()
                .wait()
                .unwrap();
            assert_eq!(filtered.matches_found(), 3);
            let crate::FilterReadOutcome::Complete(rows) = session
                .start_filtered_read(&filtered, 1, 1)
                .unwrap()
                .wait()
                .unwrap()
            else {
                panic!("cancelled");
            };
            assert_eq!(rows[0].row, 2);
            assert_eq!(rows[0].fields[0], "Zürich".as_bytes());
            let crate::SearchOutcome::Match(found) = session
                .start_search(
                    &index,
                    "Zürich".as_bytes().to_vec(),
                    crate::SearchPosition { row: 1, column: 0 },
                )
                .unwrap()
                .wait()
                .unwrap()
            else {
                panic!("not found in {encoding}");
            };
            assert_eq!((found.row, found.column), (2, 0));
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn filter_snapshot_rejects_a_different_encoding_of_the_same_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ascii.csv");
        std::fs::write(&path, b"city\nParis\n").unwrap();
        let options = crate::OpenOptions {
            header_mode: crate::HeaderMode::FirstRow,
            ..Default::default()
        };
        let latin =
            crate::Session::open_with_encoding(&path, options, Some(Encoding::Latin1)).unwrap();
        let windows =
            crate::Session::open_with_encoding(&path, options, Some(Encoding::Windows1252))
                .unwrap();
        let filtered = latin
            .start_filter(crate::FilterQuery::single(
                0,
                crate::FilterOperator::Equals,
                b"Paris".to_vec(),
            ))
            .unwrap()
            .wait()
            .unwrap();
        assert!(matches!(
            windows.read_filtered_rows(&filtered, 0, 1),
            Err(QuarryError::SourceChanged)
        ));
    }

    #[test]
    fn save_roundtrips_and_edits_preserve_each_native_encoding() {
        use std::collections::BTreeMap;
        for encoding in Encoding::ALL {
            for with_bom in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let source = directory.path().join("source.csv");
                let copy = directory.path().join("copy.csv");
                let text = "name,value\r\n\"café\",one\r\nZürich,two";
                let prefix = if with_bom { encoding.bom() } else { &[] };
                let mut input = prefix.to_vec();
                input.extend_from_slice(&encoding.encode(text.as_bytes(), 4096).unwrap());
                std::fs::write(&source, &input).unwrap();
                let session = crate::Session::open_with_encoding(
                    &source,
                    crate::OpenOptions {
                        header_mode: crate::HeaderMode::FirstRow,
                        ..Default::default()
                    },
                    Some(encoding),
                )
                .unwrap();
                let result = session
                    .start_save_as_with_edits(BTreeMap::new(), BTreeMap::new(), &copy)
                    .unwrap()
                    .wait()
                    .unwrap();
                assert!(matches!(result, crate::SaveAsOutcome::Complete(_)));
                assert_eq!(
                    std::fs::read(&copy).unwrap(),
                    input,
                    "roundtrip {encoding} {with_bom}"
                );
                let headers = BTreeMap::from([(0, b"ville".to_vec())]);
                let cells = BTreeMap::from([((1, 0), "Québec, été".as_bytes().to_vec())]);
                session
                    .start_save_with_edits(headers, cells)
                    .unwrap()
                    .wait()
                    .unwrap();
                let mut expected = prefix.to_vec();
                expected.extend_from_slice(
                    &encoding
                        .encode(
                            "ville,value\r\n\"Québec, été\",one\r\nZürich,two".as_bytes(),
                            4096,
                        )
                        .unwrap(),
                );
                assert_eq!(
                    std::fs::read(&source).unwrap(),
                    expected,
                    "edit {encoding} {with_bom}"
                );
            }
        }
    }

    #[test]
    fn unrepresentable_edits_fail_atomically_and_explicit_utf8_copy_succeeds() {
        use std::collections::BTreeMap;
        for encoding in [Encoding::Windows1252, Encoding::Latin1] {
            let directory = tempfile::tempdir().unwrap();
            let source = directory.path().join("source.csv");
            let rejected = directory.path().join("rejected.csv");
            let converted = directory.path().join("utf8.csv");
            let input = b"name\ncaf\xe9\n";
            std::fs::write(&source, input).unwrap();
            let session = crate::Session::open_with_encoding(
                &source,
                crate::OpenOptions {
                    header_mode: crate::HeaderMode::FirstRow,
                    ..Default::default()
                },
                Some(encoding),
            )
            .unwrap();
            let cells = BTreeMap::from([((1, 0), "😀".as_bytes().to_vec())]);
            let error = session
                .start_save_as_with_edits(BTreeMap::new(), cells.clone(), &rejected)
                .unwrap()
                .wait()
                .unwrap_err();
            assert!(error.to_string().contains("Save As UTF-8"), "{error}");
            assert!(!rejected.exists());
            assert_eq!(std::fs::read(&source).unwrap(), input);
            session
                .start_save_as_utf8(BTreeMap::new(), cells, None, &converted)
                .unwrap()
                .wait()
                .unwrap();
            assert_eq!(std::fs::read(&converted).unwrap(), "name\n😀\n".as_bytes());
            assert_eq!(std::fs::read(&source).unwrap(), input);
        }
    }

    #[test]
    fn filtered_exports_use_decoded_matches_and_native_output_bytes() {
        use std::collections::BTreeMap;
        use std::sync::Arc;
        for encoding in Encoding::ALL {
            let directory = tempfile::tempdir().unwrap();
            let source = directory.path().join("source.csv");
            let destination = directory.path().join("filtered.csv");
            let mut input = encoding.bom().to_vec();
            input.extend_from_slice(
                &encoding
                    .encode("name,status\r\ncafé,yes\r\nZürich,no\r\n".as_bytes(), 4096)
                    .unwrap(),
            );
            std::fs::write(&source, &input).unwrap();
            let session = crate::Session::open_with_encoding(
                &source,
                crate::OpenOptions {
                    header_mode: crate::HeaderMode::FirstRow,
                    ..Default::default()
                },
                Some(encoding),
            )
            .unwrap();
            let result = session
                .start_filtered_export_with_edits(
                    crate::FilterQuery::single(
                        0,
                        crate::FilterOperator::Equals,
                        "Zürich".as_bytes().to_vec(),
                    ),
                    Arc::new(BTreeMap::from([(
                        (1, 0),
                        Arc::new("Zürich".as_bytes().to_vec()),
                    )])),
                    BTreeMap::new(),
                    &destination,
                )
                .unwrap()
                .wait()
                .unwrap();
            let crate::FilterExportOutcome::Complete(summary) = result else {
                panic!("cancelled")
            };
            let mut expected = encoding.bom().to_vec();
            expected.extend_from_slice(
                &encoding
                    .encode(
                        "name,status\r\nZürich,yes\r\nZürich,no\r\n".as_bytes(),
                        4096,
                    )
                    .unwrap(),
            );
            assert_eq!(std::fs::read(&destination).unwrap(), expected, "{encoding}");
            assert_eq!(summary.bytes_written, expected.len() as u64);
            assert_eq!(std::fs::read(&source).unwrap(), input);
        }
    }

    #[test]
    fn transformations_preserve_native_encoding_and_csv_values() {
        use std::collections::BTreeMap;
        for encoding in Encoding::ALL {
            let directory = tempfile::tempdir().unwrap();
            let source = directory.path().join("source.csv");
            let text = "left,right\r\n café|x ,été\r\nZürich|y,fin";
            let mut input = encoding.bom().to_vec();
            input.extend_from_slice(&encoding.encode(text.as_bytes(), 4096).unwrap());
            std::fs::write(&source, &input).unwrap();
            let session = crate::Session::open_with_encoding(
                &source,
                crate::OpenOptions {
                    header_mode: crate::HeaderMode::FirstRow,
                    ..Default::default()
                },
                Some(encoding),
            )
            .unwrap();
            let analysis = session
                .start_analyze_split(BTreeMap::new(), 0, b"|".to_vec(), 10)
                .unwrap()
                .wait()
                .unwrap();
            let crate::SplitAnalysisOutcome::Complete(analysis) = analysis else {
                panic!("cancelled")
            };
            assert_eq!(analysis.max_pieces, 2, "{encoding}");
            for operation in 0..7 {
                let destination = directory.path().join(format!("operation-{operation}.csv"));
                let expected = match operation {
                    0 => {
                        session
                            .start_create_working_copy(
                                BTreeMap::new(),
                                BTreeMap::new(),
                                crate::ColumnTransformation::split_with_blank_headers(
                                    0,
                                    b"|".to_vec(),
                                    2,
                                    Some(b"left".to_vec()),
                                )
                                .unwrap(),
                                &destination,
                            )
                            .unwrap()
                            .wait()
                            .unwrap();
                        "left,,right\r\n café,x ,été\r\nZürich,y,fin"
                    }
                    1 => {
                        session
                            .start_create_cleaned_working_copy(
                                BTreeMap::new(),
                                BTreeMap::new(),
                                crate::ColumnCleanup {
                                    columns: vec![0],
                                    operation: crate::CleanupOperation::Trim,
                                },
                                &destination,
                            )
                            .unwrap()
                            .wait()
                            .unwrap();
                        "left,right\r\ncafé|x,été\r\nZürich|y,fin"
                    }
                    2 => {
                        session
                            .start_create_replaced_working_copy(
                                BTreeMap::new(),
                                BTreeMap::new(),
                                crate::LiteralReplacement {
                                    needle: "é".as_bytes().to_vec(),
                                    replacement: "è".as_bytes().to_vec(),
                                    case_sensitivity: crate::CaseSensitivity::Sensitive,
                                },
                                &destination,
                            )
                            .unwrap()
                            .wait()
                            .unwrap();
                        "left,right\r\n cafè|x ,ètè\r\nZürich|y,fin"
                    }
                    3 => {
                        session
                            .start_create_working_copy_inserting_rows(
                                BTreeMap::new(),
                                BTreeMap::new(),
                                crate::RowInsertion {
                                    position: 1,
                                    count: 2,
                                    width: 2,
                                },
                                &destination,
                            )
                            .unwrap()
                            .wait()
                            .unwrap();
                        "left,right\r\n,\r\n,\r\n café|x ,été\r\nZürich|y,fin"
                    }
                    4 => {
                        session
                            .start_create_working_copy_deleting_rows(
                                BTreeMap::new(),
                                BTreeMap::new(),
                                vec![1..=1],
                                &destination,
                            )
                            .unwrap()
                            .wait()
                            .unwrap();
                        "left,right\r\nZürich|y,fin"
                    }
                    5 => {
                        session
                            .start_create_working_copy(
                                BTreeMap::new(),
                                BTreeMap::new(),
                                crate::ColumnTransformation::Join {
                                    source_columns: vec![0, 1],
                                    separator: b"-".to_vec(),
                                    output_header: Some(b"joined".to_vec()),
                                },
                                &destination,
                            )
                            .unwrap()
                            .wait()
                            .unwrap();
                        "joined\r\n café|x -été\r\nZürich|y-fin"
                    }
                    6 => {
                        session
                            .start_create_working_copy(
                                BTreeMap::new(),
                                BTreeMap::new(),
                                crate::ColumnTransformation::Arrange {
                                    source_width: 2,
                                    output_columns: vec![1, 0],
                                },
                                &destination,
                            )
                            .unwrap()
                            .wait()
                            .unwrap();
                        "right,left\r\nété, café|x \r\nfin,Zürich|y"
                    }
                    _ => unreachable!(),
                };
                let mut expected_bytes = encoding.bom().to_vec();
                expected_bytes
                    .extend_from_slice(&encoding.encode(expected.as_bytes(), 4096).unwrap());
                assert_eq!(
                    std::fs::read(&destination).unwrap(),
                    expected_bytes,
                    "{encoding} operation {operation}"
                );
                assert_eq!(std::fs::read(&source).unwrap(), input);
            }
        }
    }

    #[test]
    fn native_sort_and_duplicate_outputs_keep_values_and_physical_indexes() {
        use std::collections::BTreeMap;
        for encoding in Encoding::ALL {
            let directory = tempfile::tempdir().unwrap();
            let source = directory.path().join("source.csv");
            let mut input = encoding.bom().to_vec();
            input.extend_from_slice(
                &encoding
                    .encode("city,n\r\nZürich,2\r\ncafé,1\r\nZürich,2".as_bytes(), 4096)
                    .unwrap(),
            );
            std::fs::write(&source, &input).unwrap();
            let options = crate::OpenOptions {
                header_mode: crate::HeaderMode::FirstRow,
                ..Default::default()
            };
            let session =
                crate::Session::open_with_encoding(&source, options, Some(encoding)).unwrap();
            for (column, mode) in [
                (1, crate::SortMode::Number),
                (0, crate::SortMode::CharacterCount),
            ] {
                let output = directory.path().join(format!("sort-{column}.csv"));
                let crate::SortOutcome::Complete(mut summary) = session
                    .start_create_sorted_working_copy(
                        BTreeMap::new(),
                        BTreeMap::new(),
                        crate::SortSpec {
                            column,
                            mode,
                            direction: crate::SortDirection::Ascending,
                            case_sensitivity: crate::CaseSensitivity::Sensitive,
                        },
                        &output,
                    )
                    .unwrap()
                    .wait()
                    .unwrap()
                else {
                    panic!("cancelled");
                };
                let mut expected = encoding.bom().to_vec();
                expected.extend_from_slice(
                    &encoding
                        .encode("city,n\r\ncafé,1\r\nZürich,2\r\nZürich,2".as_bytes(), 4096)
                        .unwrap(),
                );
                assert_eq!(
                    std::fs::read(&output).unwrap(),
                    expected,
                    "{encoding} {mode:?}"
                );
                assert_eq!(summary.bytes_written, expected.len() as u64);
                let reopened =
                    crate::Session::open_with_encoding(&output, options, Some(encoding)).unwrap();
                let index = reopened
                    .adopt_completed_index(summary.output_index.take().unwrap())
                    .unwrap();
                assert_eq!(index.indexed_rows(), 4);
                assert_eq!(index.indexed_bytes(), expected.len() as u64);
                assert_eq!(
                    reopened.read_rows(&index, 1, 1).unwrap()[0].fields[0],
                    "café".as_bytes()
                );
            }
            let output = directory.path().join("dedup.csv");
            let result = session
                .start_find_duplicates(
                    BTreeMap::new(),
                    BTreeMap::new(),
                    crate::DuplicateSpec {
                        columns: vec![0],
                        case_sensitivity: crate::CaseSensitivity::Sensitive,
                    },
                    &output,
                )
                .unwrap()
                .wait()
                .unwrap();
            assert!(matches!(result, crate::DuplicateOutcome::Complete(_)));
            let mut expected = encoding.bom().to_vec();
            expected.extend_from_slice(
                &encoding
                    .encode("city,n\r\nZürich,2\r\ncafé,1\r\n".as_bytes(), 4096)
                    .unwrap(),
            );
            assert_eq!(std::fs::read(output).unwrap(), expected, "{encoding}");
            assert_eq!(std::fs::read(&source).unwrap(), input);
        }
    }

    #[test]
    fn combine_validates_encoding_and_preserves_native_output_size() {
        for encoding in Encoding::ALL {
            for has_header in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let paths: Vec<_> = ["a.csv", "b.csv"]
                    .iter()
                    .map(|n| directory.path().join(n))
                    .collect();
                for (index, value) in ["café,1", "Zürich,2"].iter().enumerate() {
                    let text = if has_header {
                        format!("ville,n\r\n{value}")
                    } else {
                        value.to_string()
                    };
                    let mut bytes = encoding.bom().to_vec();
                    bytes.extend_from_slice(&encoding.encode(text.as_bytes(), 4096).unwrap());
                    std::fs::write(&paths[index], bytes).unwrap();
                }
                let original: Vec<_> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
                let plan = crate::check_combine_files_with_encoding(
                    paths.clone(),
                    crate::CombineOptions {
                        delimiter: None,
                        has_header,
                    },
                    Some(encoding),
                )
                .unwrap()
                .wait()
                .unwrap()
                .unwrap();
                assert_eq!(plan.encoding, encoding);
                let expected_text = if has_header {
                    "ville,n\r\ncafé,1\nZürich,2\n"
                } else {
                    "café,1\nZürich,2\n"
                };
                let mut expected = encoding.bom().to_vec();
                expected
                    .extend_from_slice(&encoding.encode(expected_text.as_bytes(), 4096).unwrap());
                assert_eq!(plan.output_bytes, expected.len() as u64);
                let output = directory.path().join("combined.csv");
                let summary = plan
                    .start_write(output.clone())
                    .unwrap()
                    .wait()
                    .unwrap()
                    .unwrap();
                assert_eq!(summary.bytes_written, expected.len() as u64);
                assert_eq!(
                    std::fs::read(output).unwrap(),
                    expected,
                    "{encoding} header={has_header}"
                );
                assert_eq!(
                    paths
                        .iter()
                        .map(|p| std::fs::read(p).unwrap())
                        .collect::<Vec<_>>(),
                    original
                );
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let a = directory.path().join("utf16.csv");
        let b = directory.path().join("utf8.csv");
        std::fs::write(
            &a,
            [
                b"\xff\xfe".as_slice(),
                Encoding::Utf16Le.encode(b"n\n1\n", 50).unwrap().as_ref(),
            ]
            .concat(),
        )
        .unwrap();
        std::fs::write(&b, b"n\n2\n").unwrap();
        let error = crate::check_combine_files(vec![a, b], crate::CombineOptions::default())
            .unwrap()
            .wait()
            .unwrap_err();
        assert!(error.to_string().contains("Save As UTF-8"), "{error}");
    }
    #[test]
    fn explicit_utf16_nul_after_bom_is_not_misclassified_as_utf32() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"\xff\xfe\0\0,\0x\0\n\0").unwrap();
        assert_eq!(
            Encoding::detect(file.as_file_mut(), Some(Encoding::Utf16Le)).unwrap(),
            Encoding::Utf16Le
        );
        assert!(Encoding::detect(file.as_file_mut(), None).is_err());
    }

    #[test]
    fn native_eof_insertion_reports_exact_published_byte_count() {
        use std::collections::BTreeMap;
        for encoding in [
            Encoding::Utf16Le,
            Encoding::Utf16Be,
            Encoding::Windows1252,
            Encoding::Latin1,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let source = directory.path().join("source.csv");
            let dest = directory.path().join("insert.csv");
            let input = encoding
                .encode("name,value\ncafé,1".as_bytes(), 4096)
                .unwrap();
            std::fs::write(&source, &input).unwrap();
            let session = crate::Session::open_with_encoding(
                &source,
                crate::OpenOptions {
                    header_mode: crate::HeaderMode::FirstRow,
                    ..Default::default()
                },
                Some(encoding),
            )
            .unwrap();
            let job = session
                .start_create_working_copy_inserting_rows(
                    BTreeMap::new(),
                    BTreeMap::new(),
                    crate::RowInsertion {
                        position: 2,
                        count: 2,
                        width: 2,
                    },
                    &dest,
                )
                .unwrap();
            while !job.progress().done {
                std::thread::yield_now();
            }
            let progress = job.progress();
            let crate::SaveAsOutcome::Complete(summary) = job.wait().unwrap() else {
                panic!("cancelled")
            };
            let output = std::fs::read(&dest).unwrap();
            assert_eq!(
                output,
                encoding
                    .encode("name,value\ncafé,1\n,\n,\n".as_bytes(), 4096)
                    .unwrap()
                    .as_ref()
            );
            assert_eq!(summary.bytes_written, output.len() as u64);
            assert_eq!(progress.bytes_written, summary.bytes_written);
        }
    }
}
