//! `DATE` / `DATETIME` column value parsing and formatting.
//!
//! GeoPackage stores dates and datetimes as ISO 8601 TEXT. The 1.4 spec kept
//! the strict datetime form `YYYY-MM-DDTHH:MM:SS.SSSZ` (UTC, millisecond
//! precision: the shape `strftime('%Y-%m-%dT%H:%M:%fZ', ...)` produces).
//! Files in the wild also contain second-precision values, other fractional
//! widths, numeric UTC offsets, and space separators; [`DateTime::parse_lenient`]
//! accepts those.
//!
//! These are calendar-validated value types, not a datetime library: no time
//! zone database, no arithmetic. Convert to your preferred datetime crate at
//! the boundary.
//!
//! Calendar arithmetic (date validation, epoch conversion) is delegated to
//! [jiff](https://docs.rs/jiff); this module keeps only the GeoPackage
//! concerns, namely which text forms are accepted on read and exactly which
//! form is written back. jiff is built with no timezone database
//! (`default-features = false`, `std` only): a GeoPackage `DATETIME` is UTC by
//! definition, and this crate transforms neither coordinates nor times. No
//! jiff type appears in this crate's API, so a jiff major version bump is not
//! a breaking change here.

use jiff::civil::Date as JiffDate;
use std::fmt;

/// A `DATE` value: `YYYY-MM-DD`, proleptic Gregorian, calendar-validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Date {
    year: u16,
    month: u8,
    day: u8,
}

impl Date {
    /// Creates a calendar-validated date. Years are limited to 0–9999 by the
    /// four-digit text form.
    ///
    /// Calendar rules (leap years, month lengths, the century rules) are
    /// delegated to jiff; the year bound comes from the spec's text form, not
    /// from the calendar.
    ///
    /// # Errors
    ///
    /// [`DateTimeError::OutOfRange`] if a component is outside its calendar
    /// range.
    ///
    /// # Examples
    ///
    /// ```
    /// use geopackage_core::datetime::Date;
    ///
    /// assert!(Date::new(2024, 2, 29).is_ok());
    /// assert!(Date::new(2023, 2, 29).is_err());
    /// ```
    pub fn new(year: u16, month: u8, day: u8) -> Result<Self, DateTimeError> {
        if year > 9999 {
            return Err(DateTimeError::OutOfRange("year"));
        }
        Self::to_jiff_checked(year, month, day)?;
        Ok(Self { year, month, day })
    }

    /// Converts to a jiff date, validating the components.
    fn to_jiff_checked(year: u16, month: u8, day: u8) -> Result<JiffDate, DateTimeError> {
        if !(1..=12).contains(&month) {
            return Err(DateTimeError::OutOfRange("month"));
        }
        let year = i16::try_from(year)
            .ok()
            .ok_or(DateTimeError::OutOfRange("year"))?;
        let month = i8::try_from(month)
            .ok()
            .ok_or(DateTimeError::OutOfRange("month"))?;
        let day = i8::try_from(day)
            .ok()
            .ok_or(DateTimeError::OutOfRange("day"))?;
        JiffDate::new(year, month, day)
            .ok()
            .ok_or(DateTimeError::OutOfRange("day"))
    }

    /// Converts to a jiff date. Infallible: a [`Date`] is validated on
    /// construction.
    fn to_jiff(self) -> JiffDate {
        Self::to_jiff_checked(self.year, self.month, self.day).unwrap_or(JiffDate::ZERO)
    }

    /// Returns the number of days from the Unix epoch to this date, negative
    /// before it.
    ///
    /// This is the value an Arrow `Date32` column stores, and a convenient
    /// interchange form for any other datetime crate.
    ///
    /// # Examples
    ///
    /// ```
    /// use geopackage_core::datetime::Date;
    ///
    /// assert_eq!(Date::new(1970, 1, 1)?.days_since_epoch(), 0);
    /// assert_eq!(Date::new(1969, 12, 31)?.days_since_epoch(), -1);
    /// # Ok::<(), geopackage_core::datetime::DateTimeError>(())
    /// ```
    pub fn days_since_epoch(self) -> i32 {
        let epoch = JiffDate::new(1970, 1, 1).unwrap_or(JiffDate::ZERO);
        // Whole days, so the hour count divides exactly and truncation is not a
        // rounding decision, before or after the epoch.
        let hours = self.to_jiff().duration_since(epoch).as_hours();
        i32::try_from(hours / 24).unwrap_or(i32::MAX)
    }

    /// Returns the date `days` days from the Unix epoch; the inverse of
    /// [`Self::days_since_epoch`].
    ///
    /// # Errors
    ///
    /// [`DateTimeError::OutOfRange`] for a count outside the years this type
    /// can represent.
    pub fn from_days_since_epoch(days: i32) -> Result<Self, DateTimeError> {
        let epoch = JiffDate::new(1970, 1, 1)
            .ok()
            .ok_or(DateTimeError::OutOfRange("year"))?;
        let date = epoch
            .checked_add(jiff::Span::new().days(days))
            .ok()
            .ok_or(DateTimeError::OutOfRange("day"))?;
        Self::new(
            u16::try_from(date.year())
                .ok()
                .ok_or(DateTimeError::OutOfRange("year"))?,
            u8::try_from(date.month())
                .ok()
                .ok_or(DateTimeError::OutOfRange("month"))?,
            u8::try_from(date.day())
                .ok()
                .ok_or(DateTimeError::OutOfRange("day"))?,
        )
    }

    /// Parses `YYYY-MM-DD`.
    ///
    /// # Errors
    ///
    /// [`DateTimeError::Malformed`] for any other shape;
    /// [`DateTimeError::OutOfRange`] for a component outside its calendar
    /// range.
    ///
    /// # Examples
    ///
    /// ```
    /// use geopackage_core::datetime::Date;
    ///
    /// let date = Date::parse("2026-07-24")?;
    /// assert_eq!((date.year(), date.month(), date.day()), (2026, 7, 24));
    /// assert!(Date::parse("2026/07/24").is_err());
    /// # Ok::<(), geopackage_core::datetime::DateTimeError>(())
    /// ```
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

    /// Returns the year (0–9999).
    pub fn year(&self) -> u16 {
        self.year
    }

    /// Returns the month (1–12).
    pub fn month(&self) -> u8 {
        self.month
    }

    /// Returns the day of month (1–31).
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
/// `Z`). `None` means the text had no zone designator at all (lenient parses
/// only).
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
    /// Returns the number of microseconds from the Unix epoch to this instant,
    /// negative before it.
    ///
    /// This is the value an Arrow `Timestamp(Microsecond, "UTC")` column
    /// stores. A value with a UTC offset is normalised to UTC; text with no
    /// zone designator, which only lenient parsing produces, is read as UTC,
    /// as the spec defines for `DATETIME` columns.
    ///
    /// Sub-microsecond precision is truncated. The strict spec form is
    /// millisecond precision, so truncation affects only lenient input with
    /// more than six fractional digits.
    ///
    /// # Errors
    ///
    /// [`DateTimeError::OutOfRange`] for an instant outside what a microsecond
    /// count can address.
    pub fn micros_since_epoch(self) -> Result<i64, DateTimeError> {
        let time = jiff::civil::Time::new(
            i8::try_from(self.hour)
                .ok()
                .ok_or(DateTimeError::OutOfRange("hour"))?,
            i8::try_from(self.minute)
                .ok()
                .ok_or(DateTimeError::OutOfRange("minute"))?,
            i8::try_from(self.second)
                .ok()
                .ok_or(DateTimeError::OutOfRange("second"))?,
            i32::try_from(self.nanosecond)
                .ok()
                .ok_or(DateTimeError::OutOfRange("nanosecond"))?,
        )
        .ok()
        .ok_or(DateTimeError::OutOfRange("time"))?;
        let stamp = self
            .date
            .to_jiff()
            .to_datetime(time)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .ok()
            .ok_or(DateTimeError::OutOfRange("datetime"))?
            .timestamp()
            .as_microsecond();
        // The offset says how far ahead of UTC the written time is, so removing
        // it is what normalises the instant.
        let offset = i64::from(self.offset_minutes.unwrap_or(0)) * 60 * 1_000_000;
        stamp
            .checked_sub(offset)
            .ok_or(DateTimeError::OutOfRange("datetime"))
    }

    /// Returns the UTC instant `micros` microseconds from the Unix epoch; the
    /// inverse of [`Self::micros_since_epoch`].
    ///
    /// # Errors
    ///
    /// [`DateTimeError::OutOfRange`] for a count outside the years this type
    /// can represent.
    pub fn from_micros_since_epoch(micros: i64) -> Result<Self, DateTimeError> {
        let civil = jiff::Timestamp::from_microsecond(micros)
            .ok()
            .ok_or(DateTimeError::OutOfRange("datetime"))?
            .to_zoned(jiff::tz::TimeZone::UTC)
            .datetime();
        Ok(Self {
            date: Date::new(
                u16::try_from(civil.year())
                    .ok()
                    .ok_or(DateTimeError::OutOfRange("year"))?,
                u8::try_from(civil.month())
                    .ok()
                    .ok_or(DateTimeError::OutOfRange("month"))?,
                u8::try_from(civil.day())
                    .ok()
                    .ok_or(DateTimeError::OutOfRange("day"))?,
            )?,
            hour: u8::try_from(civil.hour())
                .ok()
                .ok_or(DateTimeError::OutOfRange("hour"))?,
            minute: u8::try_from(civil.minute())
                .ok()
                .ok_or(DateTimeError::OutOfRange("minute"))?,
            second: u8::try_from(civil.second())
                .ok()
                .ok_or(DateTimeError::OutOfRange("second"))?,
            nanosecond: u32::try_from(civil.subsec_nanosecond())
                .ok()
                .ok_or(DateTimeError::OutOfRange("nanosecond"))?,
            // Always UTC: that is what the column means, and what
            // `micros_since_epoch` normalised to.
            offset_minutes: Some(0),
        })
    }

    /// Parses the strict 1.4 form `YYYY-MM-DDTHH:MM:SS.SSSZ` and nothing else.
    ///
    /// # Examples
    ///
    /// ```
    /// use geopackage_core::datetime::DateTime;
    ///
    /// let dt = DateTime::parse_strict("2026-07-24T12:34:56.789Z")?;
    /// assert_eq!(dt.nanosecond, 789_000_000);
    /// assert!(DateTime::parse_strict("2026-07-24T12:34:56Z").is_err());
    /// # Ok::<(), geopackage_core::datetime::DateTimeError>(())
    /// ```
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

    /// Parses common ISO 8601 datetime variants found in real files:
    /// `T` or space separator, optional `.` + 1–9 fractional-second digits,
    /// and an optional `Z` or `±HH:MM` / `±HHMM` offset.
    ///
    /// # Examples
    ///
    /// ```
    /// use geopackage_core::datetime::DateTime;
    ///
    /// let dt = DateTime::parse_lenient("2026-07-24 12:34:56+02:00")?;
    /// assert_eq!(dt.offset_minutes, Some(120));
    /// # Ok::<(), geopackage_core::datetime::DateTimeError>(())
    /// ```
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
        if self.nanosecond.is_multiple_of(1_000_000) {
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

/// Builds a signed UTC offset in minutes from a `+`/`-` sign byte, two hour
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

#[cfg(test)]
mod epoch_tests {
    use super::*;

    /// Reference values from Python's `datetime`, an independent implementation
    /// of the same calendar. These were written against a hand-rolled
    /// `days_from_civil` and are kept unchanged now that jiff does the
    /// arithmetic, so they check the delegation rather than restate it.
    #[test]
    fn days_since_epoch_matches_the_calendar() {
        let days = |y, m, d| Date::new(y, m, d).unwrap().days_since_epoch();
        assert_eq!(days(1970, 1, 1), 0);
        assert_eq!(days(1969, 12, 31), -1, "dates before the epoch go negative");
        assert_eq!(days(2026, 7, 25), 20659);
        assert_eq!(days(1900, 1, 1), -25567);
        // 1900 is not a leap year and 2000 is: the century rules have to be
        // right on both sides, which is where a hand-rolled conversion fails.
        assert_eq!(days(2000, 2, 29), 11016);
        assert_eq!(days(2000, 3, 1), 11017);
    }

    #[test]
    fn micros_since_epoch_matches_the_calendar() {
        let micros = |text: &str| {
            DateTime::parse_strict(text)
                .unwrap()
                .micros_since_epoch()
                .unwrap()
        };
        assert_eq!(micros("1970-01-01T00:00:00.000Z"), 0);
        assert_eq!(micros("2026-07-24T12:34:56.789Z"), 1_784_896_496_789_000);
        assert_eq!(micros("1969-12-31T23:59:59.000Z"), -1_000_000);
    }

    #[test]
    fn a_utc_offset_is_normalised_away() {
        // Lenient parsing accepts a numeric offset. The same instant written
        // two ways must give the same number of microseconds.
        let utc = DateTime::parse_strict("2026-07-24T12:34:56.000Z")
            .unwrap()
            .micros_since_epoch()
            .unwrap();
        let offset = DateTime::parse_lenient("2026-07-24T14:34:56+02:00")
            .unwrap()
            .micros_since_epoch()
            .unwrap();
        assert_eq!(utc, offset);
    }

    /// Both conversions round-trip, which is what the columnar read and write
    /// paths rely on when a layer is copied through Arrow.
    #[test]
    fn the_conversions_round_trip() {
        for (y, m, d) in [(1970, 1, 1), (1969, 12, 31), (2026, 7, 25), (1900, 1, 1)] {
            let date = Date::new(y, m, d).unwrap();
            assert_eq!(
                Date::from_days_since_epoch(date.days_since_epoch()).unwrap(),
                date
            );
        }
        for text in [
            "1970-01-01T00:00:00.000Z",
            "2026-07-24T12:34:56.789Z",
            "1969-12-31T23:59:59.000Z",
        ] {
            let stamp = DateTime::parse_strict(text).unwrap();
            let micros = stamp.micros_since_epoch().unwrap();
            assert_eq!(
                DateTime::from_micros_since_epoch(micros)
                    .unwrap()
                    .to_string(),
                text
            );
        }
    }

    /// The calendar rules now come from jiff, so the cases a hand-rolled
    /// implementation gets wrong are worth keeping pointed at it.
    #[test]
    fn the_calendar_is_still_validated() {
        Date::new(2026, 2, 30).unwrap_err();
        Date::new(1900, 2, 29).unwrap_err();
        Date::new(2000, 2, 29).unwrap();
        Date::new(2026, 13, 1).unwrap_err();
        Date::new(2026, 0, 1).unwrap_err();
        Date::new(2026, 1, 0).unwrap_err();
        Date::new(10_000, 1, 1).unwrap_err();
    }
}
