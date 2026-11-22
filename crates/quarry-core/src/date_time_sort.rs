//! Fixed-format Gregorian chronology keys. Parsing never consults a locale or
//! the machine's timezone, and never changes the original cell spelling.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;

use crate::encoding::RecordScanner;
use crate::export::source_matches_stamp;
use crate::{QuarryError, Session, parse_encoded_record};

/// Default upper bound on data rows sampled for bounded date/time format suggestions.
pub const DEFAULT_DATE_FORMAT_SUGGESTION_MAX_ROWS: usize = 1_000;
/// Default upper bound on source + edited bytes sampled for date/time format suggestions.
pub const DEFAULT_DATE_FORMAT_SUGGESTION_MAX_BYTES: usize = 256 * 1024;

/// Bounded sampling limits for suggesting candidate `DateTimeFormat` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateFormatSuggestionConfig {
    pub max_rows: usize,
    pub max_bytes: usize,
}

impl Default for DateFormatSuggestionConfig {
    fn default() -> Self {
        Self {
            max_rows: DEFAULT_DATE_FORMAT_SUGGESTION_MAX_ROWS,
            max_bytes: DEFAULT_DATE_FORMAT_SUGGESTION_MAX_BYTES,
        }
    }
}

/// Advisory result of sampling a column's effective values against all fixed `DateTimeFormat`
/// grammars. Callers must never auto-select from this suggestion; explicit format selection
/// and full sort-time validation remain mandatory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateFormatSuggestion {
    /// Every `DateTimeFormat` (in `DateTimeFormat::ALL` order) that parsed every sampled
    /// nonblank value without error. Empty when no nonblank values were sampled or no
    /// single format matched every sampled nonblank value.
    pub matching_formats: Vec<DateTimeFormat>,
    /// Data rows inspected within `max_rows` and `max_bytes`.
    pub sampled_rows: usize,
    /// Nonblank (after ASCII whitespace trimming) values tested.
    pub sampled_nonblank_values: usize,
    /// Bytes inspected across sampled source data and edited cell overlays.
    pub sampled_bytes: usize,
    /// True when sampling stopped before EOF due to `max_rows` or `max_bytes`.
    pub truncated: bool,
}

impl DateFormatSuggestion {
    /// True when more than one fixed format matches all sampled nonblank values
    /// (for example `01/02/2020`, which is valid under both `MM/DD/YYYY` and `DD/MM/YYYY`).
    pub fn is_ambiguous(&self) -> bool {
        self.matching_formats.len() > 1
    }

    /// Returns the sole matching format iff exactly one format matched all sampled nonblank values.
    pub fn unambiguous_format(&self) -> Option<DateTimeFormat> {
        if self.matching_formats.len() == 1 {
            Some(self.matching_formats[0])
        } else {
            None
        }
    }
}

/// Explicit input grammars; none infer date order or a machine timezone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateTimeFormat {
    YearMonthDay,
    MonthDayYear,
    DayMonthYear,
    DateTime,
    Rfc3339,
}

impl DateTimeFormat {
    pub const ALL: [Self; 5] = [
        Self::YearMonthDay,
        Self::MonthDayYear,
        Self::DayMonthYear,
        Self::DateTime,
        Self::Rfc3339,
    ];

    /// Display the required field order or timezone semantics in format pickers.
    pub const fn label(self) -> &'static str {
        match self {
            Self::YearMonthDay => "YYYY-MM-DD",
            Self::MonthDayYear => "MM/DD/YYYY",
            Self::DayMonthYear => "DD/MM/YYYY",
            Self::DateTime => "YYYY-MM-DD HH:MM:SS (no timezone)",
            Self::Rfc3339 => "RFC 3339 (with timezone)",
        }
    }

    /// Return the stable spelling accepted by the validation CLI's `--date-format`.
    pub const fn cli_name(self) -> &'static str {
        match self {
            Self::YearMonthDay => "ymd",
            Self::MonthDayYear => "mdy",
            Self::DayMonthYear => "dmy",
            Self::DateTime => "datetime",
            Self::Rfc3339 => "rfc3339",
        }
    }

    /// Explain ambiguous-date interpretation and the supported timestamp subset.
    pub const fn description(self) -> &'static str {
        match self {
            Self::YearMonthDay => {
                "For example 2026-03-04. Four-digit year, two-digit month and day."
            }
            Self::MonthDayYear => {
                "For example 03/04/2026 means March 4. Two-digit month and day, four-digit year."
            }
            Self::DayMonthYear => {
                "For example 03/04/2026 means April 3. Two-digit day and month, four-digit year."
            }
            Self::DateTime => {
                "For example 2026-03-04 09:30:00. A space or T separates date and time. Optional 1–9 fractional digits. Compare clock values without applying a timezone."
            }
            Self::Rfc3339 => {
                "For example 2026-03-04T09:30:00Z or 2026-03-04T09:30:00+02:00. Optional 1–9 fractional digits. Compare UTC instants; equal instants keep their order. Uppercase T and Z required. Leap seconds and unknown offset -00:00 are unsupported."
            }
        }
    }
}

/// Carry a bounded parser reason for the sort scanner to attach row/column context.
fn invalid(reason: &'static str) -> QuarryError {
    QuarryError::InvalidOption(reason)
}

/// Parse an ASCII field; callers supply at most nine digits, so arithmetic fits i64.
fn digits(value: &[u8]) -> Result<i64, QuarryError> {
    value.iter().try_fold(0, |number, byte| {
        if byte.is_ascii_digit() {
            Ok(number * 10 + i64::from(byte - b'0'))
        } else {
            Err(invalid(
                "expected ASCII digits in the selected date/time format",
            ))
        }
    })
}

/// Validate an exact-width date and count Gregorian days since 0001-01-01.
/// Timestamp formats use the same year/month/day grammar as `YearMonthDay`.
fn date_days(value: &[u8], format: DateTimeFormat) -> Result<i64, QuarryError> {
    if value.len() != 10 {
        return Err(invalid("expected a date with exactly ten characters"));
    }
    let (year, month, day) = match format {
        DateTimeFormat::MonthDayYear | DateTimeFormat::DayMonthYear => {
            if value[2] != b'/' || value[5] != b'/' {
                return Err(invalid("expected slash-separated date fields"));
            }
            let first = digits(&value[..2])?;
            let second = digits(&value[3..5])?;
            let (month, day) = if format == DateTimeFormat::MonthDayYear {
                (first, second)
            } else {
                (second, first)
            };
            (digits(&value[6..])?, month, day)
        }
        _ => {
            if value[4] != b'-' || value[7] != b'-' {
                return Err(invalid("expected hyphen-separated YYYY-MM-DD"));
            }
            (
                digits(&value[..4])?,
                digits(&value[5..7])?,
                digits(&value[8..])?,
            )
        }
    };
    if !(1..=9999).contains(&year) || !(1..=12).contains(&month) {
        return Err(invalid("year must be 0001–9999 and month must be 01–12"));
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    const MONTH_DAYS: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let month_index = (month - 1) as usize;
    let max_day = MONTH_DAYS[month_index] + i64::from(leap && month == 2);
    if !(1..=max_day).contains(&day) {
        return Err(invalid("day is not valid for the selected month and year"));
    }
    // Number of days since 0001-01-01 in the proleptic Gregorian calendar.
    // Bounded four-digit years keep all arithmetic well inside i64.
    let prior_year = year - 1;
    Ok(prior_year * 365 + prior_year / 4 - prior_year / 100
        + prior_year / 400
        + MONTH_DAYS[..month_index].iter().sum::<i64>()
        + i64::from(leap && month > 2)
        + day
        - 1)
}

/// Encode chronology as sign-flipped big-endian seconds followed by nanoseconds.
/// Blank keys are empty. Offsets normalize to UTC; clock values stay timezone-free.
/// Invalid nonblank values fail without changing the source field's spelling.
pub(crate) fn date_time_key(value: &[u8], format: DateTimeFormat) -> Result<Vec<u8>, QuarryError> {
    let value = value.trim_ascii();
    if value.is_empty() {
        return Ok(Vec::new());
    }
    // Reject huge/invalid fields before doing work proportional to their length.
    // Longest accepted timestamp: 19 + '.' + 9 digits + 6 offset bytes.
    if value.len() > 35 {
        return Err(invalid(
            "value is longer than the selected date/time format",
        ));
    }
    let timestamp = matches!(format, DateTimeFormat::DateTime | DateTimeFormat::Rfc3339);
    let mut seconds;
    let mut nanos = 0_u32;
    if timestamp {
        if value.len() < 19
            || value[13] != b':'
            || value[16] != b':'
            || !(value[10] == b'T' || (format == DateTimeFormat::DateTime && value[10] == b' '))
        {
            return Err(invalid("expected YYYY-MM-DD followed by HH:MM:SS"));
        }
        seconds = date_days(&value[..10], format)? * 86_400;
        let hour = digits(&value[11..13])?;
        let minute = digits(&value[14..16])?;
        let second = digits(&value[17..19])?;
        if hour > 23 || minute > 59 || second > 59 {
            return Err(invalid(
                "time must be 00:00:00–23:59:59; leap seconds are unsupported",
            ));
        }
        seconds += hour * 3600 + minute * 60 + second;
        let mut tail = &value[19..];
        if tail.first() == Some(&b'.') {
            tail = &tail[1..];
            let count = tail.iter().take_while(|byte| byte.is_ascii_digit()).count();
            if !(1..=9).contains(&count) {
                return Err(invalid("fractional seconds require 1–9 digits"));
            }
            nanos = digits(&tail[..count])? as u32 * 10_u32.pow(9 - count as u32);
            tail = &tail[count..];
        }
        if format == DateTimeFormat::Rfc3339 {
            if tail != b"Z" {
                if tail.len() != 6 || !matches!(tail[0], b'+' | b'-') || tail[3] != b':' {
                    return Err(invalid("expected Z or an explicit +HH:MM or -HH:MM offset"));
                }
                let hours = digits(&tail[1..3])?;
                let minutes = digits(&tail[4..6])?;
                if hours > 23 || minutes > 59 || tail == b"-00:00" {
                    return Err(invalid(
                        "offset must be within 23:59; unknown offset -00:00 is unsupported",
                    ));
                }
                let offset = hours * 3600 + minutes * 60;
                seconds -= if tail[0] == b'+' { offset } else { -offset };
            }
        } else if !tail.is_empty() {
            return Err(invalid(
                "the selected timestamp format does not accept a timezone",
            ));
        }
    } else {
        seconds = date_days(value, format)? * 86_400;
    }
    let mut key = Vec::with_capacity(12);
    // Sign flip preserves signed chronological order in an unsigned byte key,
    // including offset-normalized instants before 0001-01-01.
    key.extend_from_slice(&((seconds as u64) ^ (1 << 63)).to_be_bytes());
    key.extend_from_slice(&nanos.to_be_bytes());
    Ok(key)
}

struct FormatSampler {
    candidates: [bool; 5],
    sampled_rows: usize,
    sampled_nonblank_values: usize,
}

impl FormatSampler {
    fn new() -> Self {
        Self {
            candidates: [true; 5],
            sampled_rows: 0,
            sampled_nonblank_values: 0,
        }
    }

    fn observe(&mut self, value: &[u8]) {
        self.sampled_rows = self.sampled_rows.saturating_add(1);
        let trimmed = value.trim_ascii();
        if trimmed.is_empty() {
            return;
        }
        self.sampled_nonblank_values = self.sampled_nonblank_values.saturating_add(1);
        for (index, &format) in DateTimeFormat::ALL.iter().enumerate() {
            if self.candidates[index] && date_time_key(trimmed, format).is_err() {
                self.candidates[index] = false;
            }
        }
    }

    fn any_remaining(&self) -> bool {
        self.candidates.iter().any(|&ok| ok)
    }

    fn finish(self, sampled_bytes: usize, truncated: bool) -> DateFormatSuggestion {
        let matching_formats = if self.sampled_nonblank_values == 0 {
            Vec::new()
        } else {
            DateTimeFormat::ALL
                .iter()
                .enumerate()
                .filter_map(|(index, &format)| self.candidates[index].then_some(format))
                .collect()
        };
        DateFormatSuggestion {
            matching_formats,
            sampled_rows: self.sampled_rows,
            sampled_nonblank_values: self.sampled_nonblank_values,
            sampled_bytes,
            truncated,
        }
    }
}

/// Evaluate an iterator of effective field values within strict row and byte bounds and
/// return every `DateTimeFormat` that parses all sampled nonblank values.
pub fn suggest_date_time_formats_from_values<'a>(
    values: impl IntoIterator<Item = &'a [u8]>,
    config: DateFormatSuggestionConfig,
) -> DateFormatSuggestion {
    if config.max_rows == 0 || config.max_bytes == 0 {
        return DateFormatSuggestion {
            matching_formats: Vec::new(),
            sampled_rows: 0,
            sampled_nonblank_values: 0,
            sampled_bytes: 0,
            truncated: true,
        };
    }
    let mut sampler = FormatSampler::new();
    let mut sampled_bytes = 0_usize;
    let mut truncated = false;
    for value in values {
        if sampler.sampled_rows >= config.max_rows {
            truncated = true;
            break;
        }
        let next_bytes = sampled_bytes.saturating_add(value.len());
        if next_bytes > config.max_bytes {
            truncated = true;
            break;
        }
        sampled_bytes = next_bytes;
        sampler.observe(value);
        if !sampler.any_remaining() {
            break;
        }
    }
    sampler.finish(sampled_bytes, truncated)
}

impl Session {
    /// Sample bounded rows and bytes of effective (edited) values for `column` and return every
    /// `DateTimeFormat` that parses all sampled nonblank values. Never auto-selects a format;
    /// full validation still runs during sort execution.
    pub fn suggest_date_time_formats(
        &self,
        column: usize,
        cell_edits: &BTreeMap<(u64, usize), Vec<u8>>,
        config: DateFormatSuggestionConfig,
    ) -> Result<DateFormatSuggestion, QuarryError> {
        self.suggest_date_time_formats_with_lookup(
            column,
            |row, col| cell_edits.get(&(row, col)).map(Vec::as_slice),
            config,
        )
    }

    /// Borrow-friendly variant of `suggest_date_time_formats` that consults unsaved edits through
    /// `edit_lookup` without cloning the edit overlay.
    pub fn suggest_date_time_formats_with_lookup<'a>(
        &self,
        column: usize,
        mut edit_lookup: impl FnMut(u64, usize) -> Option<&'a [u8]>,
        config: DateFormatSuggestionConfig,
    ) -> Result<DateFormatSuggestion, QuarryError> {
        if config.max_rows == 0 || config.max_bytes == 0 {
            return Err(QuarryError::InvalidOption(
                "date format suggestion max_rows and max_bytes must be non-zero",
            ));
        }
        let mut file = File::open(&self.path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                QuarryError::SourceChanged
            } else {
                error.into()
            }
        })?;
        if !source_matches_stamp(&file, &self.path, &self.source_stamp)? {
            return Err(QuarryError::SourceChanged);
        }
        let encoding = self.encoding;
        let delimiter = self.dialect.delimiter;
        let has_header = self.dialect.has_header;
        if encoding.is_utf16() && config.max_bytes < 2 {
            return Ok(DateFormatSuggestion {
                matching_formats: Vec::new(),
                sampled_rows: 0,
                sampled_nonblank_values: 0,
                sampled_bytes: 0,
                truncated: self.file_size > 0,
            });
        }
        let mut scanner = RecordScanner::new(delimiter, encoding)?;
        let chunk_cap = encoding
            .chunk_size(config.max_bytes.min(64 * 1024))
            .min(if encoding.is_utf16() {
                config.max_bytes & !1
            } else {
                config.max_bytes
            })
            .max(if encoding.is_utf16() { 2 } else { 1 });
        let mut chunk = vec![0_u8; chunk_cap];
        let mut record = Vec::new();
        let mut offset = 0_u64;
        let mut physical_row = 0_u64;
        let mut source_bytes_read = 0_usize;
        let mut edit_bytes_read = 0_usize;
        let mut sampler = FormatSampler::new();
        let mut truncated = false;
        let mut stop = false;
        let mut reached_eof = false;

        let mut process_record = |rec: &[u8],
                                  row_num: u64,
                                  sampler: &mut FormatSampler,
                                  edit_bytes_read: &mut usize,
                                  source_bytes_read: usize,
                                  truncated: &mut bool,
                                  stop: &mut bool|
         -> Result<(), QuarryError> {
            if has_header && row_num == 0 {
                return Ok(());
            }
            if sampler.sampled_rows >= config.max_rows {
                *truncated = true;
                *stop = true;
                return Ok(());
            }
            if let Some(edited) = edit_lookup(row_num, column) {
                let next_total = source_bytes_read
                    .saturating_add(*edit_bytes_read)
                    .saturating_add(edited.len());
                if next_total > config.max_bytes {
                    *truncated = true;
                    *stop = true;
                    return Ok(());
                }
                *edit_bytes_read = edit_bytes_read.saturating_add(edited.len());
                sampler.observe(edited);
            } else {
                let fields = parse_encoded_record(rec, delimiter, row_num, encoding)?;
                let value = fields.get(column).map_or(&[][..], |field| field.as_ref());
                sampler.observe(value);
            }
            if !sampler.any_remaining() {
                *stop = true;
            }
            Ok(())
        };

        while !stop {
            let remaining = config
                .max_bytes
                .saturating_sub(source_bytes_read.saturating_add(edit_bytes_read));
            let mut to_read = remaining.min(chunk.len());
            if encoding.is_utf16() {
                to_read &= !1;
            }
            if to_read == 0 {
                if offset < self.file_size {
                    truncated = true;
                } else {
                    reached_eof = true;
                }
                break;
            }
            let read = encoding.read_chunk(&mut file, &mut chunk[..to_read])?;
            if read == 0 {
                reached_eof = true;
                break;
            }
            source_bytes_read = source_bytes_read.saturating_add(read);
            let mut segment_start = 0_usize;
            let mut deferred_error = None;
            let scan_result = scanner.scan_chunk(&chunk[..read], offset, |absolute_end| {
                let local_end = (absolute_end - offset) as usize;
                if deferred_error.is_none() && !stop {
                    record.extend_from_slice(&chunk[segment_start..local_end]);
                    if let Err(error) = process_record(
                        &record,
                        physical_row,
                        &mut sampler,
                        &mut edit_bytes_read,
                        source_bytes_read,
                        &mut truncated,
                        &mut stop,
                    ) {
                        deferred_error = Some(error);
                    }
                }
                record.clear();
                physical_row = physical_row.saturating_add(1);
                segment_start = local_end;
            });
            offset = offset.saturating_add(read as u64);
            if let Some(error) = deferred_error {
                return Err(error);
            }
            scan_result?;
            if !stop {
                record.extend_from_slice(&chunk[segment_start..read]);
                if offset >= self.file_size {
                    reached_eof = true;
                    break;
                }
            }
        }

        if reached_eof && !stop {
            let mut deferred_error = None;
            let finish_result = scanner.finish(offset, |_| {
                if deferred_error.is_none()
                    && !stop
                    && let Err(error) = process_record(
                        &record,
                        physical_row,
                        &mut sampler,
                        &mut edit_bytes_read,
                        source_bytes_read,
                        &mut truncated,
                        &mut stop,
                    )
                {
                    deferred_error = Some(error);
                }
                record.clear();
                physical_row = physical_row.saturating_add(1);
            });
            if let Some(error) = deferred_error {
                return Err(error);
            }
            finish_result?;
        } else if !reached_eof && offset < self.file_size {
            truncated = true;
        }

        Ok(sampler.finish(source_bytes_read.saturating_add(edit_bytes_read), truncated))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(value: &str, format: DateTimeFormat) -> Vec<u8> {
        date_time_key(value.as_bytes(), format).unwrap()
    }

    #[test]
    fn explicit_date_order_changes_ambiguous_interpretation() {
        assert!(
            key("03/04/2026", DateTimeFormat::MonthDayYear)
                < key("04/03/2026", DateTimeFormat::MonthDayYear)
        );
        assert!(
            key("03/04/2026", DateTimeFormat::DayMonthYear)
                > key("04/03/2026", DateTimeFormat::DayMonthYear)
        );
        assert_eq!(
            key("03/04/2026", DateTimeFormat::MonthDayYear),
            key("2026-03-04", DateTimeFormat::YearMonthDay)
        );
        assert_eq!(
            key("03/04/2026", DateTimeFormat::DayMonthYear),
            key("2026-04-03", DateTimeFormat::YearMonthDay)
        );
    }

    #[test]
    fn gregorian_boundaries_and_leap_centuries_are_exact() {
        for (earlier, later) in [
            ("0001-01-01", "0001-01-02"),
            ("1899-12-31", "1900-01-01"),
            ("1900-02-28", "1900-03-01"),
            ("2000-02-28", "2000-02-29"),
            ("2000-02-29", "2000-03-01"),
            ("2024-12-31", "2025-01-01"),
            ("9999-12-30", "9999-12-31"),
        ] {
            let before = date_days(earlier.as_bytes(), DateTimeFormat::YearMonthDay).unwrap();
            let after = date_days(later.as_bytes(), DateTimeFormat::YearMonthDay).unwrap();
            assert_eq!(after - before, 1, "{earlier} {later}");
        }
        // Known Gregorian interval: 1970-01-01 to 2000-01-01 is 10,957 days.
        assert_eq!(
            date_days(b"2000-01-01", DateTimeFormat::YearMonthDay).unwrap()
                - date_days(b"1970-01-01", DateTimeFormat::YearMonthDay).unwrap(),
            10_957
        );
        for invalid in [
            "0000-01-01",
            "1900-02-29",
            "2100-02-29",
            "2026-04-31",
            "2026-00-01",
            "2026-13-01",
            "2026-01-00",
        ] {
            assert!(
                date_time_key(invalid.as_bytes(), DateTimeFormat::YearMonthDay).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn offsets_cross_dates_and_equal_instants_share_keys() {
        let format = DateTimeFormat::Rfc3339;
        for (first, second) in [
            ("2026-01-01T01:00:00+02:00", "2025-12-31T23:00:00Z"),
            (
                "2026-03-04T09:30:00.1+02:00",
                "2026-03-04T07:30:00.100000000Z",
            ),
            ("2026-03-04T06:30:00-01:00", "2026-03-04T07:30:00+00:00"),
            ("0001-01-01T00:00:00+23:59", "0001-01-01T00:00:00+23:59"),
        ] {
            assert_eq!(key(first, format), key(second, format));
        }
        assert!(key("0001-01-01T00:00:00+01:00", format) < key("0001-01-01T00:00:00Z", format));
        assert!(
            key("2026-03-04T07:30:00.000000001Z", format) > key("2026-03-04T07:30:00Z", format)
        );
        assert!(
            key("2026-03-04T07:30:00.999999999Z", format) < key("2026-03-04T07:30:01Z", format)
        );
        assert!(key("9999-12-31T23:59:59-23:59", format) > key("9999-12-31T23:59:59Z", format));
    }

    #[test]
    fn clock_values_accept_exact_fractional_precision_without_timezone_inference() {
        let format = DateTimeFormat::DateTime;
        assert_eq!(
            key("2026-03-04 07:30:00.10", format),
            key("2026-03-04T07:30:00.100000000", format)
        );
        assert_eq!(
            key(" \t2026-03-04 07:30:00\r\n", format),
            key("2026-03-04T07:30:00", format)
        );
        for value in [
            "2026-03-04",
            "2026-03-04T07:30:00Z",
            "2026-03-04T07:30:00+00:00",
        ] {
            assert!(date_time_key(value.as_bytes(), format).is_err(), "{value}");
        }
    }

    #[test]
    fn strict_formats_reject_mixed_invalid_and_unsupported_values() {
        for (format, values) in [
            (
                DateTimeFormat::YearMonthDay,
                vec![
                    "2026-3-04",
                    "2026-03-4",
                    "03/04/2026",
                    "+2026-03-04",
                    "2026-03-04T00:00:00",
                    "２０２６-03-04",
                    "\u{a0}2026-03-04",
                ],
            ),
            (
                DateTimeFormat::MonthDayYear,
                vec!["3/04/2026", "03/04/26", "31/01/2026", "02/29/1900"],
            ),
            (
                DateTimeFormat::DayMonthYear,
                vec!["03/4/2026", "01/31/2026", "29/02/1900"],
            ),
            (
                DateTimeFormat::Rfc3339,
                vec![
                    "2026-03-04T07:30:00",
                    "2026-03-04 07:30:00Z",
                    "2026-03-04t07:30:00Z",
                    "2026-03-04T07:30:00z",
                    "2026-03-04T24:00:00Z",
                    "2026-03-04T07:60:00Z",
                    "2026-03-04T07:30:60Z",
                    "2026-03-04T07:30:00.Z",
                    "2026-03-04T07:30:00.1234567890Z",
                    "2026-03-04T07:30:00-00:00",
                    "2026-03-04T07:30:00+24:00",
                    "2026-03-04T07:30:00+00:60",
                    "2026-03-04T07:30:00+0000",
                    "2026-03-04T07:30:00ZZ",
                ],
            ),
        ] {
            for value in values {
                assert!(
                    date_time_key(value.as_bytes(), format).is_err(),
                    "{format:?}: {value}"
                );
            }
        }
        for format in DateTimeFormat::ALL {
            assert!(date_time_key(&[0xff], format).is_err());
            assert!(date_time_key(&vec![b'1'; 100_000], format).is_err());
            assert_eq!(date_time_key(b" \r\n\t", format).unwrap(), Vec::<u8>::new());
        }
    }

    #[test]
    fn suggestion_detects_unambiguous_ambiguous_blank_and_mixed_samples() {
        let cfg = DateFormatSuggestionConfig::default();

        // Unambiguous YYYY-MM-DD with blanks and ASCII whitespace.
        let ymd = suggest_date_time_formats_from_values(
            [b"".as_slice(), b"  ", b"2026-03-04", b" 2025-12-31\t"],
            cfg,
        );
        assert_eq!(ymd.matching_formats, vec![DateTimeFormat::YearMonthDay]);
        assert!(!ymd.is_ambiguous());
        assert_eq!(ymd.unambiguous_format(), Some(DateTimeFormat::YearMonthDay));
        assert_eq!(ymd.sampled_rows, 4);
        assert_eq!(ymd.sampled_nonblank_values, 2);
        assert!(!ymd.truncated);

        // Ambiguous slash dates: 01/02/2020 matches both MM/DD/YYYY and DD/MM/YYYY.
        let amb =
            suggest_date_time_formats_from_values([b"01/02/2020".as_slice(), b"03/04/2026"], cfg);
        assert_eq!(
            amb.matching_formats,
            vec![DateTimeFormat::MonthDayYear, DateTimeFormat::DayMonthYear]
        );
        assert!(amb.is_ambiguous());
        assert_eq!(amb.unambiguous_format(), None);

        // Later unambiguous row narrows to MonthDayYear.
        let mdy =
            suggest_date_time_formats_from_values([b"01/02/2020".as_slice(), b"03/25/2026"], cfg);
        assert_eq!(mdy.matching_formats, vec![DateTimeFormat::MonthDayYear]);
        assert!(!mdy.is_ambiguous());

        // DayMonthYear.
        let dmy = suggest_date_time_formats_from_values([b"25/03/2026".as_slice()], cfg);
        assert_eq!(dmy.unambiguous_format(), Some(DateTimeFormat::DayMonthYear));

        // DateTime vs Rfc3339.
        let dt = suggest_date_time_formats_from_values([b"2026-03-04 09:30:00".as_slice()], cfg);
        assert_eq!(dt.unambiguous_format(), Some(DateTimeFormat::DateTime));
        let rfc = suggest_date_time_formats_from_values([b"2026-03-04T09:30:00Z".as_slice()], cfg);
        assert_eq!(rfc.unambiguous_format(), Some(DateTimeFormat::Rfc3339));

        // All blank -> empty and not ambiguous.
        let blank = suggest_date_time_formats_from_values([b"".as_slice(), b"   "], cfg);
        assert!(blank.matching_formats.is_empty());
        assert!(!blank.is_ambiguous());
        assert_eq!(blank.sampled_nonblank_values, 0);

        // Mixed incompatible values -> empty.
        let mixed =
            suggest_date_time_formats_from_values([b"2026-03-04".as_slice(), b"03/04/2026"], cfg);
        assert!(mixed.matching_formats.is_empty());
        assert!(!mixed.is_ambiguous());
    }

    #[test]
    fn session_suggestion_respects_row_byte_bounds_edits_and_utf16() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dates.csv");
        // Row 1 & 2 are ambiguous; row 3 disambiguates MDY; row 4 is invalid text.
        std::fs::write(
            &path,
            b"id,when\n1,01/02/2020\n2,03/04/2026\n3,03/25/2026\n4,not-a-date",
        )
        .unwrap();
        let session = Session::open(
            &path,
            crate::OpenOptions {
                header_mode: crate::HeaderMode::FirstRow,
                delimiter: Some(b','),
                ..crate::OpenOptions::default()
            },
        )
        .unwrap();

        // Bounded to 2 rows -> ambiguous [MDY, DMY], truncated.
        let two = session
            .suggest_date_time_formats(
                1,
                &BTreeMap::new(),
                DateFormatSuggestionConfig {
                    max_rows: 2,
                    max_bytes: 1024,
                },
            )
            .unwrap();
        assert_eq!(
            two.matching_formats,
            vec![DateTimeFormat::MonthDayYear, DateTimeFormat::DayMonthYear]
        );
        assert!(two.is_ambiguous());
        assert!(two.truncated);

        // Bounded to 3 rows -> unambiguous MDY, truncated before invalid row 4.
        let three = session
            .suggest_date_time_formats(
                1,
                &BTreeMap::new(),
                DateFormatSuggestionConfig {
                    max_rows: 3,
                    max_bytes: 1024,
                },
            )
            .unwrap();
        assert_eq!(three.matching_formats, vec![DateTimeFormat::MonthDayYear]);
        assert!(three.truncated);

        // Unbounded rows sees invalid row 4 -> empty.
        let all = session
            .suggest_date_time_formats(1, &BTreeMap::new(), DateFormatSuggestionConfig::default())
            .unwrap();
        assert!(all.matching_formats.is_empty());

        // Unsaved edit fixes row 4 -> unambiguous MDY across entire file (not truncated).
        let edits = BTreeMap::from([((4, 1), b"04/20/2026".to_vec())]);
        let fixed = session
            .suggest_date_time_formats(1, &edits, DateFormatSuggestionConfig::default())
            .unwrap();
        assert_eq!(fixed.matching_formats, vec![DateTimeFormat::MonthDayYear]);
        assert!(!fixed.truncated);
        assert_eq!(fixed.sampled_rows, 4);

        // Exact byte cap equal to file length still reads final newline-less record.
        let file_len = std::fs::metadata(&path).unwrap().len() as usize;
        let exact_bytes = session
            .suggest_date_time_formats(
                1,
                &BTreeMap::new(),
                DateFormatSuggestionConfig {
                    max_rows: 10,
                    max_bytes: file_len,
                },
            )
            .unwrap();
        assert_eq!(exact_bytes.sampled_rows, 4);
        assert!(exact_bytes.matching_formats.is_empty());

        // Strict byte cap stops early without reading full file.
        let tiny_bytes = session
            .suggest_date_time_formats(
                1,
                &BTreeMap::new(),
                DateFormatSuggestionConfig {
                    max_rows: 100,
                    max_bytes: 24,
                },
            )
            .unwrap();
        assert!(tiny_bytes.sampled_bytes <= 24);
        assert!(tiny_bytes.truncated);
        assert!(tiny_bytes.is_ambiguous());

        // UTF-16LE with BOM.
        let utf16_path = dir.path().join("dates-utf16.csv");
        let mut utf16 = crate::Encoding::Utf16Le.bom().to_vec();
        utf16.extend_from_slice(
            &crate::Encoding::Utf16Le
                .encode(b"d\r\n2026-03-04\r\n2026-03-05\r\n", 4096)
                .unwrap(),
        );
        std::fs::write(&utf16_path, &utf16).unwrap();
        let utf16_session = Session::open_with_encoding(
            &utf16_path,
            crate::OpenOptions {
                header_mode: crate::HeaderMode::FirstRow,
                delimiter: Some(b','),
                ..crate::OpenOptions::default()
            },
            None,
        )
        .unwrap();
        let u16_sugg = utf16_session
            .suggest_date_time_formats(0, &BTreeMap::new(), DateFormatSuggestionConfig::default())
            .unwrap();
        assert_eq!(
            u16_sugg.unambiguous_format(),
            Some(DateTimeFormat::YearMonthDay)
        );
        assert!(!u16_sugg.truncated);
    }
}
