//! Fixed-format Gregorian chronology keys. Parsing never consults a locale or
//! the machine's timezone, and never changes the original cell spelling.

use crate::QuarryError;

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
}
