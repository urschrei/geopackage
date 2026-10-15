//! Property tests over `DATE` and `DATETIME` values: construction, the epoch
//! conversions, formatting, and the two parsers.
//!
//! The calendar oracle is the test's own: the Gregorian leap-year rule and
//! Howard Hinnant's `days_from_civil`, independent of the jiff arithmetic the
//! module delegates to. The parser oracles are the grammars the docs state,
//! checked byte by byte, over strings drawn from a pattern that produces both
//! the accepted shapes and near misses.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geopackage_core::datetime::{Date, DateTime, DateTimeError};
use hegel::generators;

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Whether `(year, month, day)` is a date of the proleptic Gregorian calendar
/// that four digits can write.
fn is_valid_date(year: i64, month: i64, day: i64) -> bool {
    (0..=9999).contains(&year)
        && (1..=12).contains(&month)
        && (1..=days_in_month(year, month)).contains(&day)
}

/// Days from 1970-01-01 to a civil date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Draws a valid date from the whole range a `Date` can hold.
fn draw_date(tc: &hegel::TestCase) -> Date {
    let year = tc.draw(generators::integers::<u16>().min_value(0).max_value(9999));
    let month = tc.draw(generators::integers::<u8>().min_value(1).max_value(12));
    let last = days_in_month(i64::from(year), i64::from(month));
    let day = tc.draw(
        generators::integers::<u8>()
            .min_value(1)
            .max_value(u8::try_from(last).unwrap_or(28)),
    );
    Date::new(year, month, day)
        .map_err(|error| format!("{year}-{month}-{day}: {error}"))
        .unwrap()
}

/// Draws a valid `DATETIME` with any precision and any offset that the
/// lenient parser accepts (up to 23:59 either side), or none.
fn draw_datetime(tc: &hegel::TestCase) -> DateTime {
    let offset_minutes = if tc.draw(generators::booleans()) {
        None
    } else {
        Some(
            tc.draw(
                generators::integers::<i16>()
                    .min_value(-(23 * 60 + 59))
                    .max_value(23 * 60 + 59),
            ),
        )
    };
    DateTime {
        date: draw_date(tc),
        hour: tc.draw(generators::integers::<u8>().min_value(0).max_value(23)),
        minute: tc.draw(generators::integers::<u8>().min_value(0).max_value(59)),
        second: tc.draw(generators::integers::<u8>().min_value(0).max_value(59)),
        nanosecond: tc.draw(
            generators::integers::<u32>()
                .min_value(0)
                .max_value(999_999_999),
        ),
        offset_minutes,
    }
}

/// `Date::new` accepts exactly the dates of the calendar in years 0 to 9999,
/// over the whole domain of its arguments.
#[hegel::test]
fn date_new_accepts_exactly_the_calendar_dates(tc: hegel::TestCase) {
    let year = tc.draw(generators::integers::<u16>());
    let month = tc.draw(generators::integers::<u8>());
    let day = tc.draw(generators::integers::<u8>());
    let result = Date::new(year, month, day);
    let valid = is_valid_date(i64::from(year), i64::from(month), i64::from(day));
    match result {
        Ok(date) => {
            assert!(valid, "{year}-{month}-{day} was accepted");
            assert_eq!((date.year(), date.month(), date.day()), (year, month, day));
        }
        Err(error) => {
            assert!(!valid, "{year}-{month}-{day} was rejected: {error}");
            assert!(matches!(error, DateTimeError::OutOfRange(_)));
        }
    }
}

/// `days_since_epoch` is Hinnant's `days_from_civil`, and
/// `from_days_since_epoch` inverts it.
#[hegel::test]
fn days_since_epoch_counts_calendar_days(tc: hegel::TestCase) {
    let date = draw_date(&tc);
    let days = date.days_since_epoch();
    assert_eq!(
        i64::from(days),
        days_from_civil(
            i64::from(date.year()),
            i64::from(date.month()),
            i64::from(date.day())
        )
    );
    assert_eq!(Date::from_days_since_epoch(days), Ok(date));
}

/// `from_days_since_epoch` accepts exactly the counts from 0000-01-01 to
/// 9999-12-31, over the whole `i32` domain.
#[hegel::test]
fn from_days_since_epoch_accepts_exactly_the_representable_days(tc: hegel::TestCase) {
    let days = tc.draw(generators::integers::<i32>());
    let (first, last) = (days_from_civil(0, 1, 1), days_from_civil(9999, 12, 31));
    let in_range = (first..=last).contains(&i64::from(days));
    match Date::from_days_since_epoch(days) {
        Ok(date) => {
            assert!(in_range, "{days} gave {date}");
            assert_eq!(date.days_since_epoch(), days);
        }
        Err(error) => {
            assert!(!in_range, "{days} was rejected: {error}");
            assert!(matches!(error, DateTimeError::OutOfRange(_)));
        }
    }
}

/// `micros_since_epoch` counts the microseconds to the instant, the written
/// time less its offset, truncating below a microsecond; and
/// `from_micros_since_epoch` gives back that instant in UTC when its year is
/// representable.
#[hegel::test]
fn micros_since_epoch_counts_to_the_utc_instant(tc: hegel::TestCase) {
    let dt = draw_datetime(&tc);
    let micros = dt.micros_since_epoch().unwrap();
    let days = days_from_civil(
        i64::from(dt.date.year()),
        i64::from(dt.date.month()),
        i64::from(dt.date.day()),
    );
    let seconds = days * 86_400
        + i64::from(dt.hour) * 3_600
        + i64::from(dt.minute) * 60
        + i64::from(dt.second)
        - i64::from(dt.offset_minutes.unwrap_or(0)) * 60;
    assert_eq!(
        micros,
        seconds * 1_000_000 + i64::from(dt.nanosecond / 1_000)
    );

    let utc_days = seconds.div_euclid(86_400);
    if (days_from_civil(0, 1, 1)..=days_from_civil(9999, 12, 31)).contains(&utc_days) {
        let back = DateTime::from_micros_since_epoch(micros).unwrap();
        assert_eq!(back.micros_since_epoch(), Ok(micros));
        assert_eq!(back.offset_minutes, Some(0));
        assert_eq!(back.nanosecond, dt.nanosecond / 1_000 * 1_000);
    }
}

/// A date formats as `YYYY-MM-DD`, and `Date::parse` reads that back.
#[hegel::test]
fn a_date_formats_and_parses_back(tc: hegel::TestCase) {
    let date = draw_date(&tc);
    let text = date.to_string();
    assert_eq!(
        text,
        format!("{:04}-{:02}-{:02}", date.year(), date.month(), date.day())
    );
    assert_eq!(Date::parse(&text), Ok(date));
}

/// A `DATETIME` formats in the strict shape when its precision is whole
/// milliseconds and it is UTC or unzoned, and `parse_lenient` reads any
/// formatted value back, with an absent offset as `Z`.
#[hegel::test]
fn a_datetime_formats_and_parses_back(tc: hegel::TestCase) {
    let dt = draw_datetime(&tc);
    let text = dt.to_string();
    let expected = DateTime {
        offset_minutes: Some(dt.offset_minutes.unwrap_or(0)),
        ..dt
    };
    assert_eq!(DateTime::parse_lenient(&text), Ok(expected), "{text}");
    let strict =
        dt.nanosecond.is_multiple_of(1_000_000) && matches!(dt.offset_minutes, None | Some(0));
    assert_eq!(DateTime::parse_strict(&text).is_ok(), strict, "{text}");
}

/// Reads `n` ASCII digits at `at`, or `None`.
fn digits(bytes: &[u8], at: usize, n: usize) -> Option<i64> {
    let slice = bytes.get(at..at + n)?;
    slice.iter().try_fold(0_i64, |v, b| {
        b.is_ascii_digit().then(|| v * 10 + i64::from(b - b'0'))
    })
}

/// The byte at `at`, or `None` past the end.
fn byte(bytes: &[u8], at: usize) -> Option<u8> {
    bytes.get(at).copied()
}

/// The documented `DATE` grammar: exactly `YYYY-MM-DD`, a calendar date.
fn date_grammar(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() == 10 && date_prefix(b)
}

/// Whether `b` begins with `YYYY-MM-DD`, a calendar date.
fn date_prefix(b: &[u8]) -> bool {
    byte(b, 4) == Some(b'-')
        && byte(b, 7) == Some(b'-')
        && matches!(
            (digits(b, 0, 4), digits(b, 5, 2), digits(b, 8, 2)),
            (Some(y), Some(m), Some(d)) if is_valid_date(y, m, d)
        )
}

/// The documented lenient `DATETIME` grammar: `YYYY-MM-DD`, `T` or a space,
/// `HH:MM:SS`, an optional `.` and 1 to 9 digits, and an optional `Z`,
/// `±HH:MM`, `±HHMM` or `±HH` offset of at most 23:59, with every component in
/// range.
fn lenient_grammar(text: &str) -> bool {
    let b = text.as_bytes();
    let time = date_prefix(b)
        && matches!(byte(b, 10), Some(b'T' | b' '))
        && byte(b, 13) == Some(b':')
        && byte(b, 16) == Some(b':')
        && matches!(
            (digits(b, 11, 2), digits(b, 14, 2), digits(b, 17, 2)),
            (Some(h), Some(m), Some(s)) if h <= 23 && m <= 59 && s <= 59
        );
    let Some(mut rest) = b.get(19..).filter(|_| time) else {
        return false;
    };
    if let Some((b'.', tail)) = rest.split_first() {
        let n = tail.iter().take_while(|c| c.is_ascii_digit()).count();
        if !(1..=9).contains(&n) {
            return false;
        }
        rest = tail.get(n..).unwrap_or_default();
    }
    let offset = |hours: Option<i64>, minutes: Option<i64>| matches!((hours, minutes), (Some(h), Some(m)) if h <= 23 && m <= 59);
    match rest {
        [] | [b'Z'] => true,
        [b'+' | b'-', _, _, b':', _, _] => offset(digits(rest, 1, 2), digits(rest, 4, 2)),
        [b'+' | b'-', _, _, _, _] => offset(digits(rest, 1, 2), digits(rest, 3, 2)),
        [b'+' | b'-', _, _] => offset(digits(rest, 1, 2), Some(0)),
        _ => false,
    }
}

/// The documented strict grammar: exactly `YYYY-MM-DDTHH:MM:SS.SSSZ`.
fn strict_grammar(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() == 24
        && byte(b, 10) == Some(b'T')
        && byte(b, 19) == Some(b'.')
        && byte(b, 23) == Some(b'Z')
        && digits(b, 20, 3).is_some()
        && lenient_grammar(text)
}

/// Formats a drawn valid `DATETIME` in one of the lenient forms: either
/// separator, 0 to 9 fractional digits, and no offset, `Z`, `±HH:MM`, `±HHMM`
/// or `±HH`.
fn draw_lenient_text(tc: &hegel::TestCase) -> String {
    let dt = draw_datetime(tc);
    let separator = tc.draw(generators::sampled_from(vec!['T', ' ']));
    let mut text = format!(
        "{}{separator}{:02}:{:02}:{:02}",
        dt.date, dt.hour, dt.minute, dt.second
    );
    let digits = tc.draw(generators::integers::<usize>().min_value(0).max_value(9));
    if digits > 0 {
        text.push('.');
        text.extend(format!("{:09}", dt.nanosecond).chars().take(digits));
    }
    let offset = tc.draw(
        generators::integers::<i16>()
            .min_value(-1439)
            .max_value(1439),
    );
    let (sign, hours, minutes) = (
        if offset < 0 { '-' } else { '+' },
        offset.abs() / 60,
        offset.abs() % 60,
    );
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(4)) {
        0 => {}
        1 => text.push('Z'),
        2 => text.push_str(&format!("{sign}{hours:02}:{minutes:02}")),
        3 => text.push_str(&format!("{sign}{hours:02}{minutes:02}")),
        _ => text.push_str(&format!("{sign}{hours:02}")),
    }
    text
}

/// Changes one byte of `text`, deletes one, or inserts one, drawing the byte
/// from digits and the separators the grammars use.
fn mutate(tc: &hegel::TestCase, text: String) -> String {
    let mut bytes = text.into_bytes();
    let at = tc.draw(
        generators::integers::<usize>()
            .min_value(0)
            .max_value(bytes.len()),
    );
    let replacement = tc.draw(generators::sampled_from(b"0123456789-:.TZ +x".to_vec()));
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(2)) {
        0 if at < bytes.len() => {
            if let Some(byte) = bytes.get_mut(at) {
                *byte = replacement;
            }
        }
        1 if at < bytes.len() => {
            bytes.remove(at);
        }
        _ => bytes.insert(at, replacement),
    }
    String::from_utf8(bytes).unwrap()
}

/// Draws a string near the accepted shapes: a valid date or `DATETIME` in a
/// lenient form, half the time with one byte changed, deleted or inserted; a
/// string from a pattern with the right structure and loose digit counts; or
/// any text.
fn draw_datetime_like(tc: &hegel::TestCase) -> String {
    const PATTERN: &str = r"[0-9]{3,5}-[0-9]{1,3}-[0-9]{1,3}([T t_][0-9]{1,3}:[0-9]{1,3}:[0-9]{1,3}(\.[0-9]{0,11})?(Z|z|[+-][0-9]{1,3}(:?[0-9]{1,3})?)?)?";
    let valid = match tc.draw(generators::integers::<u8>().min_value(0).max_value(4)) {
        0 => return tc.draw(generators::text()),
        1 => return tc.draw(generators::from_regex(PATTERN).fullmatch(true)),
        2 => draw_date(tc).to_string(),
        3 => draw_datetime(tc).to_string(),
        _ => draw_lenient_text(tc),
    };
    if tc.draw(generators::booleans()) {
        mutate(tc, valid)
    } else {
        valid
    }
}

/// `Date::parse` accepts exactly the documented `DATE` grammar.
#[hegel::test]
fn date_parse_accepts_exactly_its_grammar(tc: hegel::TestCase) {
    let text = draw_datetime_like(&tc);
    assert_eq!(Date::parse(&text).is_ok(), date_grammar(&text), "{text:?}");
}

/// `parse_lenient` accepts exactly the documented lenient grammar.
#[hegel::test]
fn parse_lenient_accepts_exactly_its_grammar(tc: hegel::TestCase) {
    let text = draw_datetime_like(&tc);
    tc.event(if lenient_grammar(&text) {
        "in the grammar"
    } else {
        "outside the grammar"
    });
    assert_eq!(
        DateTime::parse_lenient(&text).is_ok(),
        lenient_grammar(&text),
        "{text:?}"
    );
}

/// `parse_strict` accepts exactly the strict grammar, and agrees with
/// `parse_lenient` on what it accepts.
#[hegel::test]
fn parse_strict_accepts_exactly_its_grammar(tc: hegel::TestCase) {
    let text = draw_datetime_like(&tc);
    let strict = DateTime::parse_strict(&text);
    assert_eq!(strict.is_ok(), strict_grammar(&text), "{text:?}");
    if let Ok(dt) = strict {
        assert_eq!(DateTime::parse_lenient(&text), Ok(dt));
    }
}
