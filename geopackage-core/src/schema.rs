//! The `gpkg_schema` extension's model (Annex F.9): column descriptions and
//! the constraints their values may carry.
//!
//! Two tables. `gpkg_data_columns` describes a column: a human-readable name,
//! a title, a description, a MIME type for a BLOB column, and optionally the
//! name of a constraint. `gpkg_data_column_constraints` holds the constraints,
//! keyed by that name, in three forms: a numeric `range`, an `enum` of allowed
//! values, or a `glob` pattern.
//!
//! The spec is explicit that these constraints are advisory as far as the file
//! format goes: "These restrictions MAY be enforced by SQL triggers or by code
//! in applications that update GeoPackage data values." This module supplies
//! the model and the matching; the `geopackage` crate decides when to apply it.

use std::fmt;

/// Registered extension name for column descriptions and value constraints
/// (Annex F.9).
pub const EXTENSION_NAME: &str = "gpkg_schema";
/// `gpkg_extensions.definition` value for [`EXTENSION_NAME`].
pub const EXTENSION_DEFINITION: &str = "http://www.geopackage.org/spec140/#extension_schema";
/// `gpkg_extensions.scope` value for [`EXTENSION_NAME`].
pub const EXTENSION_SCOPE: &str = "read-write";

/// A row of `gpkg_data_columns`: what one column of one table means.
///
/// Every field but the column's own name is optional, and a row carrying
/// nothing but a `name` is both legal and common: the extension exists to
/// supplement `sqlite_master` and `PRAGMA table_info`, not to replace them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataColumn {
    /// The column this describes.
    pub column_name: String,
    /// A human-readable identifier, such as a short name.
    pub name: Option<String>,
    /// A human-readable formal title.
    pub title: Option<String>,
    /// A human-readable description.
    pub description: Option<String>,
    /// The MIME type of the column's content, for a BLOB column.
    pub mime_type: Option<String>,
    /// The name of the constraint the column's values are subject to, which
    /// [`ColumnConstraint::name`] matches (Requirement 106).
    pub constraint_name: Option<String>,
}

/// A constraint on a column's values, assembled from the
/// `gpkg_data_column_constraints` rows sharing one `constraint_name`.
///
/// An `enum` occupies one row per member and a `range` or `glob` exactly one
/// row (Requirement 109), so this is a constraint rather than a row.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnConstraint {
    /// The name rows reference this constraint by. Lowercase, per the column
    /// description in the spec's table definition.
    pub name: String,
    /// What the constraint allows.
    pub kind: ConstraintKind,
    /// The constraint's description, or for an `enum`, the description of
    /// whichever member row carried one.
    pub description: Option<String>,
}

/// The three constraint forms Requirement 108 allows.
#[derive(Debug, Clone, PartialEq)]
pub enum ConstraintKind {
    /// `range`: a numeric interval, each end inclusive or exclusive.
    ///
    /// Requirement 111 makes both ends NOT NULL with `min` less than `max`,
    /// and Requirement 112 makes both inclusivity flags 0 or 1.
    Range {
        /// The lower bound.
        min: f64,
        /// Whether a value equal to `min` satisfies the constraint.
        min_is_inclusive: bool,
        /// The upper bound.
        max: f64,
        /// Whether a value equal to `max` satisfies the constraint.
        max_is_inclusive: bool,
    },
    /// `enum`: the set of allowed values, one per row, compared as text
    /// (Requirement 114 makes each row's `value` NOT NULL).
    Enum(Vec<String>),
    /// `glob`: a pattern the value has to match, in SQLite's `GLOB` syntax.
    Glob(String),
}

impl ConstraintKind {
    /// The `constraint_type` column value.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Range { .. } => "range",
            Self::Enum(_) => "enum",
            Self::Glob(_) => "glob",
        }
    }
}

impl ColumnConstraint {
    /// Whether `value`, rendered as text, satisfies the constraint.
    ///
    /// A `range` is checked against [`ColumnConstraint::allows_number`]
    /// instead: a range is numeric, and text has no place in one.
    pub fn allows_text(&self, value: &str) -> bool {
        match &self.kind {
            ConstraintKind::Range { .. } => false,
            ConstraintKind::Enum(members) => members.iter().any(|member| member == value),
            ConstraintKind::Glob(pattern) => glob_match(pattern, value),
        }
    }

    /// Whether the number `value` satisfies the constraint.
    ///
    /// An `enum` or `glob` is compared against the number's text form by the
    /// caller, since both are text constraints and the spec's own sample
    /// enumerates the numbers 1, 3, 5, 7 and 9 as text.
    pub fn allows_number(&self, value: f64) -> bool {
        match &self.kind {
            ConstraintKind::Range {
                min,
                min_is_inclusive,
                max,
                max_is_inclusive,
            } => {
                let above = if *min_is_inclusive {
                    value >= *min
                } else {
                    value > *min
                };
                let below = if *max_is_inclusive {
                    value <= *max
                } else {
                    value < *max
                };
                above && below
            }
            ConstraintKind::Enum(_) | ConstraintKind::Glob(_) => false,
        }
    }
}

impl fmt::Display for ConstraintKind {
    /// A one-line rendering, for an error message naming what was violated.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Range {
                min,
                min_is_inclusive,
                max,
                max_is_inclusive,
            } => write!(
                f,
                "range {}{min}, {max}{}",
                if *min_is_inclusive { '[' } else { '(' },
                if *max_is_inclusive { ']' } else { ')' }
            ),
            Self::Enum(members) => write!(f, "enum of {} value(s)", members.len()),
            Self::Glob(pattern) => write!(f, "glob {pattern:?}"),
        }
    }
}

/// Match `text` against a SQLite `GLOB` pattern.
///
/// The pattern language is SQLite's, not the shell's, and this follows
/// `patternCompare` in SQLite's `func.c` rather than a general description of
/// globbing, because the constraint is written to be checked by SQLite and the
/// two have to agree. What that pins down:
///
/// - `*` matches any run of characters, including none; `?` matches exactly
///   one character. Both count characters, not bytes.
/// - `[...]` matches one character from the set. A leading `^` inverts it. A
///   `]` immediately after the `[` or the `^` is a member rather than the
///   terminator. `a-z` is a range; a `-` that has no character before it, or
///   is followed by the terminator, is a member.
/// - A `[` with no closing `]` matches nothing at all. It is not treated as a
///   literal `[`, which is the point most re-implementations get wrong.
/// - There is no escape character. `GLOB` takes no `ESCAPE` clause, so a
///   literal `*` is written `[*]`.
/// - Matching is case sensitive, unlike `LIKE`.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    // Where to resume from if the tail turns out not to match: the `*` most
    // recently passed, and how much of the text it had consumed.
    let mut star: Option<(usize, usize)> = None;

    while let Some(&current) = text.get(t) {
        // Consuming one character of text, if this element can.
        let consumed = match pattern.get(p) {
            Some('*') => {
                star = Some((p, t));
                p += 1;
                continue;
            }
            Some('?') => true,
            Some('[') => match class_match(&pattern, p, current) {
                // An unterminated class matches nothing, and backtracking
                // cannot change that, so the whole match fails here.
                None => return false,
                Some((matched, next)) => {
                    if matched {
                        p = next;
                        t += 1;
                        continue;
                    }
                    false
                }
            },
            Some(&literal) => literal == current,
            None => false,
        };
        if consumed {
            p += 1;
            t += 1;
            continue;
        }
        // This element did not match, or the pattern ran out with text left.
        // Give the last `*` one more character and try the tail again.
        match star {
            Some((star_p, star_t)) if star_t < text.len() => {
                p = star_p + 1;
                t = star_t + 1;
                star = Some((star_p, star_t + 1));
            }
            _ => return false,
        }
    }
    // The text is spent, so what is left of the pattern has to be able to
    // match nothing, which only a run of `*` can.
    pattern.iter().skip(p).all(|element| *element == '*')
}

/// Match one character against the `[...]` class starting at `open`.
///
/// Returns whether the character is in the class and the index just past the
/// closing `]`, or `None` if there is no closing `]`.
fn class_match(pattern: &[char], open: usize, c: char) -> Option<(bool, usize)> {
    let mut i = open + 1;
    let invert = pattern.get(i) == Some(&'^');
    if invert {
        i += 1;
    }
    let mut seen = false;
    // A `]` in the first position is a member, not the terminator.
    if pattern.get(i) == Some(&']') {
        seen = c == ']';
        i += 1;
    }
    // The character before a `-`, and so the start of a range. Cleared after
    // each range so that `a-z-0` reads as the range `a-z` then the members
    // `-` and `0`, which is what SQLite does.
    let mut prior: Option<char> = None;
    while let Some(&current) = pattern.get(i) {
        if current == ']' {
            return Some((seen != invert, i + 1));
        }
        let next = pattern.get(i + 1).copied();
        if current == '-' && next != Some(']') && next.is_some() && prior.is_some() {
            let (low, high) = (prior.unwrap_or(current), next.unwrap_or(current));
            if c >= low && c <= high {
                seen = true;
            }
            prior = None;
            i += 2;
        } else {
            if c == current {
                seen = true;
            }
            prior = Some(current);
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_and_wildcards() {
        assert!(glob_match("abc", "abc"));
        assert!(!glob_match("abc", "abd"));
        assert!(glob_match("a*c", "abbbc"));
        assert!(glob_match("a*c", "ac"));
        assert!(glob_match("*", ""));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"), "? needs exactly one character");
        // Case sensitive, which is what separates GLOB from LIKE.
        assert!(!glob_match("ABC", "abc"));
    }

    #[test]
    fn character_classes() {
        assert!(glob_match("[abc]", "b"));
        assert!(!glob_match("[abc]", "d"));
        assert!(glob_match("[^abc]", "d"));
        assert!(!glob_match("[^abc]", "a"));
        assert!(glob_match("[a-z]", "q"));
        assert!(!glob_match("[a-z]", "Q"));
        // The spec's own sample constraint, which is a four-digit year.
        assert!(glob_match("[1-2][0-9][0-9][0-9]", "1984"));
        assert!(!glob_match("[1-2][0-9][0-9][0-9]", "3984"));
        assert!(!glob_match("[1-2][0-9][0-9][0-9]", "198"));
    }

    #[test]
    fn class_edge_cases() {
        // A `]` in the first position is a member.
        assert!(glob_match("[]]", "]"));
        assert!(glob_match("[]a]", "a"));
        assert!(glob_match("[^]]", "a"));
        assert!(!glob_match("[^]]", "]"));
        // A `-` with nothing before it, or nothing after it, is a member.
        assert!(glob_match("[-a]", "-"));
        assert!(glob_match("[a-]", "-"));
        // No closing bracket: matches nothing, rather than a literal `[`.
        assert!(!glob_match("[abc", "[abc"));
        assert!(!glob_match("[abc", "a"));
        // Which is how a literal `*` is written, there being no escape.
        assert!(glob_match("[*]", "*"));
        assert!(!glob_match("[*]", "x"));
    }

    #[test]
    fn backtracking_does_not_stop_at_the_first_candidate() {
        assert!(glob_match("*abc", "xxabcxxabc"));
        assert!(glob_match("*a*b*c", "zzazzbzzc"));
        assert!(!glob_match("*a*b*c", "zzazzbzz"));
        assert!(glob_match("a*b*", "ab"));
    }

    #[test]
    fn characters_not_bytes() {
        // Two bytes each in UTF-8, so a byte-wise `?` would need two of them.
        assert!(glob_match("?", "é"));
        assert!(glob_match("a?c", "aéc"));
        assert!(glob_match("[é]", "é"));
    }

    #[test]
    fn range_bounds_honour_their_inclusivity() {
        let closed = ColumnConstraint {
            name: "closed".to_owned(),
            kind: ConstraintKind::Range {
                min: 1.0,
                min_is_inclusive: true,
                max: 10.0,
                max_is_inclusive: true,
            },
            description: None,
        };
        assert!(closed.allows_number(1.0));
        assert!(closed.allows_number(10.0));
        assert!(!closed.allows_number(0.999));
        assert!(!closed.allows_number(10.001));

        let open = ColumnConstraint {
            name: "open".to_owned(),
            kind: ConstraintKind::Range {
                min: 1.0,
                min_is_inclusive: false,
                max: 10.0,
                max_is_inclusive: false,
            },
            description: None,
        };
        assert!(!open.allows_number(1.0));
        assert!(!open.allows_number(10.0));
        assert!(open.allows_number(1.001));
    }

    #[test]
    fn a_range_never_admits_text_and_a_pattern_never_admits_a_number() {
        let range = ColumnConstraint {
            name: "r".to_owned(),
            kind: ConstraintKind::Range {
                min: 0.0,
                min_is_inclusive: true,
                max: 1.0,
                max_is_inclusive: true,
            },
            description: None,
        };
        assert!(!range.allows_text("0.5"));

        let members = ColumnConstraint {
            name: "e".to_owned(),
            kind: ConstraintKind::Enum(vec!["1".to_owned(), "3".to_owned()]),
            description: None,
        };
        // The caller renders a number as text first; the numeric door is shut.
        assert!(!members.allows_number(1.0));
        assert!(members.allows_text("1"));
        assert!(!members.allows_text("2"));
    }
}
