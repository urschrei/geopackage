//! `DATE` / `DATETIME` column value parsing and formatting.
//!
//! GeoPackage stores dates and datetimes as ISO 8601 TEXT. The 1.4 spec kept
//! the strict datetime form `YYYY-MM-DDTHH:MM:SS.SSSZ` (UTC, millisecond
//! precision — the shape `strftime('%Y-%m-%dT%H:%M:%fZ', ...)` produces).
//! Files in the wild also contain second-precision values, other fractional
//! widths, numeric UTC offsets, and space separators; [`DateTime::parse_lenient`]
//! accepts those.
//!
//! These are calendar-validated value types, not a datetime library: no time
//! zone database, no arithmetic. Convert to your preferred datetime crate at
//! the boundary.

use std::fmt;

/// A `DATE` value: `YYYY-MM-DD`, proleptic Gregorian, calendar-validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Date {
    year: u16,
    month: u8,
    day: u8,
}

impl Date {
    /// Construct a calendar-validated date. Years are limited to 0–9999 by
    /// the four-digit text form.
    pub fn new(year: u16, month: u8, day: u8) -> Result<Self, DateTimeError> {
        if year > 9999 {
            return Err(DateTimeError::OutOfRange("year"));
        }
        if !(1..=12).contains(&month) {
            return Err(DateTimeError::OutOfRange("month"));
        }
        if day < 1 || day > days_in_month(year, month) {
            return Err(DateTimeError::OutOfRange("day"));
        }
        Ok(Self { year, month, day })
    }

    /// Parse `YYYY-MM-DD`.
    pub fn parse(s: &str) -> Result<Self, DateTimeError> {
        // Slice pattern: matches iff the input is exactly 10 bytes with `-`
        // separators at positions 4 and 7; any other shape is `Malformed`.
        let &[y0, y1, y2, y3, b'-', m0, m1, b'-', d0, d1] = s.as_bytes() else {
            return Err(DateTimeError::Malformed);
        };
        Self::new(
            parse_digits(&[y0, y1, y2, y3])? as u16,
            parse_digits(&[m0, m1])? as u8,
            parse_digits(&[d0, d1])? as u8,
        )
    }

    /// Year (0–9999).
    pub fn year(&self) -> u16 {
        self.year
    }

    /// Month (1–12).
    pub fn month(&self) -> u8 {
        self.month
    }

    /// Day of month (1–31).
    pub fn day(&self) -> u8 {
        self.day
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// A `DATETIME` value: a [`Date`], a time of day with nanosecond storage
/// precision, and an optional UTC offset.
///
/// The strict spec form always has `offset_minutes == Some(0)` (the trailing
/// `Z`). `None` means the text carried no zone designator at all (lenient
/// parses only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DateTime {
    /// Calendar date component.
    pub date: Date,
    /// Hour (0–23).
    pub hour: u8,
    /// Minute (0–59).
    pub minute: u8,
    /// Second (0–59).
    pub second: u8,
    /// Sub-second component in nanoseconds (0–999 999 999).
    pub nanosecond: u32,
    /// UTC offset in minutes; `Some(0)` for `Z`, `None` when absent.
    pub offset_minutes: Option<i16>,
}

impl DateTime {
    /// Parse the strict 1.4 form `YYYY-MM-DDTHH:MM:SS.SSSZ` and nothing else.
    pub fn parse_strict(s: &str) -> Result<Self, DateTimeError> {
        // Exactly 24 bytes with `T` at 10, `.` at 19, `Z` at 23; the digit
        // and calendar validation is delegated to `parse_lenient`.
        if !matches!(
            s.as_bytes(),
            [
                _,
                _,
                _,
                _,
                _,
                _,
                _,
                _,
                _,
                _,
                b'T',
                _,
                _,
                _,
                _,
                _,
                _,
                _,
                _,
                b'.',
                _,
                _,
                _,
                b'Z'
            ]
        ) {
            return Err(DateTimeError::Malformed);
        }
        let dt = Self::parse_lenient(s)?;
        debug_assert_eq!(dt.offset_minutes, Some(0));
        Ok(dt)
    }

    /// Parse common ISO 8601 datetime variants found in real files:
    /// `T` or space separator, optional `.` + 1–9 fractional-second digits,
    /// and an optional `Z` or `±HH:MM` / `±HHMM` offset.
    pub fn parse_lenient(s: &str) -> Result<Self, DateTimeError> {
        // The fixed 19-byte prefix `YYYY-MM-DDsHH:MM:SS` (s is `T` or space),
        // with `rest` capturing an optional fractional part and/or zone. The
        // pattern validates every separator; the calendar and clock ranges are
        // checked below. Any other shape is `Malformed`.
        let &[
            y0,
            y1,
            y2,
            y3,
            b'-',
            mo0,
            mo1,
            b'-',
            d0,
            d1,
            b'T' | b' ',
            h0,
            h1,
            b':',
            mi0,
            mi1,
            b':',
            s0,
            s1,
            ref rest @ ..,
        ] = s.as_bytes()
        else {
            return Err(DateTimeError::Malformed);
        };
        let date = Date::new(
            parse_digits(&[y0, y1, y2, y3])? as u16,
            parse_digits(&[mo0, mo1])? as u8,
            parse_digits(&[d0, d1])? as u8,
        )?;
        let hour = parse_digits(&[h0, h1])? as u8;
        let minute = parse_digits(&[mi0, mi1])? as u8;
        let second = parse_digits(&[s0, s1])? as u8;
        if hour > 23 || minute > 59 || second > 59 {
            return Err(DateTimeError::OutOfRange("time of day"));
        }

        // Optional `.` + 1-9 fractional-second digits.
        let (nanosecond, after_frac): (u32, &[u8]) = match rest.split_first() {
            Some((&b'.', tail)) => {
                let n_digits = tail.iter().take_while(|&&c| c.is_ascii_digit()).count();
                if n_digits == 0 || n_digits > 9 {
                    return Err(DateTimeError::Malformed);
                }
                let (frac_bytes, remainder) = tail.split_at(n_digits);
                let frac = parse_digits(frac_bytes)?;
                (frac * 10u32.pow(9 - n_digits as u32), remainder)
            }
            _ => (0, rest),
        };

        // Optional zone: `Z`, `±HH:MM`, `±HHMM`, or `±HH`.
        let offset_minutes = match after_frac {
            [] => None,
            [b'Z'] => Some(0),
            &[sign @ (b'+' | b'-'), oh0, oh1, b':', om0, om1] => {
                make_offset(sign, [oh0, oh1], parse_digits(&[om0, om1])?)?
            }
            &[sign @ (b'+' | b'-'), oh0, oh1, om0, om1] => {
                make_offset(sign, [oh0, oh1], parse_digits(&[om0, om1])?)?
            }
            &[sign @ (b'+' | b'-'), oh0, oh1] => make_offset(sign, [oh0, oh1], 0)?,
            _ => return Err(DateTimeError::Malformed),
        };

        Ok(Self {
            date,
            hour,
            minute,
            second,
            nanosecond,
            offset_minutes,
        })
    }
}

impl fmt::Display for DateTime {
    /// Formats in the strict spec shape: millisecond precision, and `Z` for
    /// UTC/unzoned values. Sub-millisecond digits are printed only when
    /// present; non-zero offsets print as `±HH:MM`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}T{:02}:{:02}:{:02}",
            self.date, self.hour, self.minute, self.second
        )?;
        if self.nanosecond % 1_000_000 == 0 {
            write!(f, ".{:03}", self.nanosecond / 1_000_000)?;
        } else {
            write!(f, ".{:09}", self.nanosecond)?;
        }
        match self.offset_minutes {
            None | Some(0) => f.write_str("Z"),
            Some(o) => {
                let (sign, abs) = if o < 0 { ('-', -o) } else { ('+', o) };
                write!(f, "{sign}{:02}:{:02}", abs / 60, abs % 60)
            }
        }
    }
}

/// Errors from `DATE`/`DATETIME` parsing.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum DateTimeError {
    /// The text does not match the expected shape.
    #[error("malformed date/datetime text")]
    Malformed,
    /// A component is outside its calendar or clock range.
    #[error("{0} out of range")]
    OutOfRange(&'static str),
}

/// Build a signed UTC offset in minutes from a `+`/`-` sign byte, two hour
/// digits, and an already-parsed minute value. Returns `Some(offset)`, matching
/// the caller's `offset_minutes` field, or an `OutOfRange` error.
fn make_offset(sign: u8, hour_digits: [u8; 2], minute: u32) -> Result<Option<i16>, DateTimeError> {
    let hour = parse_digits(&hour_digits)?;
    if hour > 23 || minute > 59 {
        return Err(DateTimeError::OutOfRange("utc offset"));
    }
    let signum: i16 = if sign == b'+' { 1 } else { -1 };
    Ok(Some(signum * (hour as i16 * 60 + minute as i16)))
}

fn parse_digits(b: &[u8]) -> Result<u32, DateTimeError> {
    let mut v: u32 = 0;
    for &c in b {
        if !c.is_ascii_digit() {
            return Err(DateTimeError::Malformed);
        }
        v = v * 10 + (c - b'0') as u32;
    }
    Ok(v)
}

fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: u16) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_parse_and_display() {
        let d = Date::parse("2026-07-24").unwrap();
        assert_eq!((d.year(), d.month(), d.day()), (2026, 7, 24));
        assert_eq!(d.to_string(), "2026-07-24");

        Date::parse("2024-02-29").unwrap(); // 2024 is a leap year
        assert_eq!(
            Date::parse("2023-02-29"),
            Err(DateTimeError::OutOfRange("day"))
        );
        assert!(
            Date::parse("1900-02-29").is_err(),
            "1900 is not a leap year"
        );
        assert!(Date::parse("2000-02-29").is_ok(), "2000 is a leap year");
        assert_eq!(
            Date::parse("2026-13-01"),
            Err(DateTimeError::OutOfRange("month"))
        );
        assert_eq!(Date::parse("2026-7-24"), Err(DateTimeError::Malformed));
        assert_eq!(Date::parse("2026/07/24"), Err(DateTimeError::Malformed));
    }

    #[test]
    fn strict_datetime() {
        let dt = DateTime::parse_strict("2026-07-24T12:34:56.789Z").unwrap();
        assert_eq!(dt.hour, 12);
        assert_eq!(dt.nanosecond, 789_000_000);
        assert_eq!(dt.offset_minutes, Some(0));
        assert_eq!(dt.to_string(), "2026-07-24T12:34:56.789Z");

        // Everything the strict form does not allow.
        for s in [
            "2026-07-24T12:34:56Z",
            "2026-07-24T12:34:56.789",
            "2026-07-24 12:34:56.789Z",
            "2026-07-24T12:34:56.789+00:00",
            "2026-07-24T12:34:56.7890Z",
        ] {
            assert!(DateTime::parse_strict(s).is_err(), "{s}");
        }
    }

    #[test]
    fn lenient_datetime() {
        let no_frac = DateTime::parse_lenient("2026-07-24T12:34:56Z").unwrap();
        assert_eq!(no_frac.nanosecond, 0);
        assert_eq!(no_frac.to_string(), "2026-07-24T12:34:56.000Z");

        let no_zone = DateTime::parse_lenient("2026-07-24 12:34:56.5").unwrap();
        assert_eq!(no_zone.nanosecond, 500_000_000);
        assert_eq!(no_zone.offset_minutes, None);

        let offset = DateTime::parse_lenient("2026-07-24T12:34:56+02:00").unwrap();
        assert_eq!(offset.offset_minutes, Some(120));
        assert_eq!(offset.to_string(), "2026-07-24T12:34:56.000+02:00");
        let compact = DateTime::parse_lenient("2026-07-24T12:34:56-0930").unwrap();
        assert_eq!(compact.offset_minutes, Some(-(9 * 60 + 30)));

        let nanos = DateTime::parse_lenient("2026-07-24T12:34:56.123456789Z").unwrap();
        assert_eq!(nanos.nanosecond, 123_456_789);
        assert_eq!(nanos.to_string(), "2026-07-24T12:34:56.123456789Z");

        for s in [
            "2026-07-24T24:00:00Z",
            "2026-07-24T12:60:00Z",
            "2026-07-24T12:34:56.Z",
            "2026-07-24T12:34:56.1234567890Z",
            "2026-07-24T12:34:56+25:00",
            "not a datetime",
            "",
        ] {
            assert!(DateTime::parse_lenient(s).is_err(), "{s}");
        }
    }
}
