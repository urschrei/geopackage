//! `geopackage_core::schema::glob_match` against SQLite's own `GLOB`.
//!
//! The `gpkg_schema` extension's glob constraints are written to be checked by
//! SQLite, so the only useful definition of "correct" for our matcher is "the
//! same answer SQLite gives". This compares the two directly over generated
//! patterns and inputs, with the alphabet weighted towards the metacharacters:
//! ordinary letters agree trivially, and every interesting disagreement is
//! about `[`, `]`, `^`, `-` or an unterminated class.
//!
//! The matcher lives in `geopackage-core`, which has no SQLite dependency, so
//! the comparison lives here.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geopackage_core::schema::glob_match;
use hegel::generators;
use rusqlite::Connection;

/// The characters patterns are drawn from: every GLOB metacharacter, plus
/// enough ordinary ones to build ranges and literals out of.
const PATTERN_ALPHABET: &[char] = &[
    '*', '?', '[', ']', '^', '-', 'a', 'b', 'c', 'z', '1', '2', '9',
];

/// What the text is drawn from. The metacharacters are here too, since a
/// literal `[` in the text is exactly the case where an unterminated class in
/// the pattern could be mistaken for one.
const TEXT_ALPHABET: &[char] = &['a', 'b', 'c', 'z', '1', '2', '9', '[', ']', '-', '^', '*'];

fn draw_string(tc: &hegel::TestCase, alphabet: &[char], max_len: usize) -> String {
    let len = tc.draw(
        generators::integers::<usize>()
            .min_value(0)
            .max_value(max_len),
    );
    (0..len)
        .map(|_| {
            let index = tc.draw(
                generators::integers::<usize>()
                    .min_value(0)
                    .max_value(alphabet.len() - 1),
            );
            *alphabet
                .get(index)
                .expect("drawn index within the alphabet")
        })
        .collect()
}

thread_local! {
    /// One connection for the whole run. Opening one per generated case cost
    /// more than everything else in this test put together, and a connection
    /// that only evaluates `GLOB` carries no state between cases.
    static ORACLE: Connection = Connection::open_in_memory().expect("in-memory SQLite");
}

/// SQLite's answer for `text GLOB pattern`.
fn sqlite_glob(pattern: &str, text: &str) -> bool {
    ORACLE.with(|conn| {
        conn.query_row("SELECT ?1 GLOB ?2", rusqlite::params![text, pattern], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
            != 0
    })
}

#[hegel::test]
fn glob_match_agrees_with_sqlite(tc: hegel::TestCase) {
    let pattern = draw_string(&tc, PATTERN_ALPHABET, 8);
    let text = draw_string(&tc, TEXT_ALPHABET, 8);
    assert_eq!(
        glob_match(&pattern, &text),
        sqlite_glob(&pattern, &text),
        "pattern {pattern:?} against text {text:?}"
    );
}

#[test]
fn the_cases_worth_naming_agree_with_sqlite() {
    // The generated cases above cover these, but a named list fails with a
    // readable message and documents what the awkward corners are.
    for (pattern, text) in [
        ("[abc", "a"),
        ("[abc", "[abc"),
        ("[]]", "]"),
        ("[]a]", "a"),
        ("[^]]", "a"),
        ("[^]]", "]"),
        ("[-a]", "-"),
        ("[a-]", "-"),
        ("[a-c]", "b"),
        ("[^a-c]", "z"),
        ("[a-z-1]", "-"),
        ("[*]", "*"),
        ("*", ""),
        ("**", "ab"),
        ("*a*b*c", "zzazzbzzc"),
        ("?", ""),
        ("", ""),
        ("", "a"),
        ("[]", "]"),
        ("[^]", "a"),
    ] {
        assert_eq!(
            glob_match(pattern, text),
            sqlite_glob(pattern, text),
            "pattern {pattern:?} against text {text:?}"
        );
    }
}
