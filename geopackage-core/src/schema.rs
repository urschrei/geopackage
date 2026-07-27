//! The `gpkg_schema` extension's model (Annex F.9): column descriptions and
//! the constraints their values may carry.
//!
//! Two tables. `gpkg_data_columns` describes a column: a human-readable name,
//! a title, a description, a MIME type for a BLOB column, and optionally the
//! name of a constraint. `gpkg_data_column_constraints` holds the constraints,
//! keyed by that name, in three forms: a numeric `range`, an `enum` of allowed
//! values, or a `glob` pattern.
//!
//! This module is the model only: what a file says. Deciding whether a value
//! satisfies a constraint belongs to the `geopackage` crate, because one of
//! the three forms cannot be decided here. A `glob` is a pattern in SQLite's
//! `GLOB` syntax, whose definition is whatever the engine holding the file
//! does with it, so the engine is asked rather than a copy of its rules being
//! kept here. See `geopackage::OpenOptions::enforce_column_constraints`.
//!
//! The spec is explicit that these constraints are advisory as far as the file
//! format goes: "These restrictions MAY be enforced by SQL triggers or by code
//! in applications that update GeoPackage data values."

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
    ///
    /// The order is the order the rows came back in, and carries no meaning:
    /// the spec calls this a set, and round-tripping a file through GDAL
    /// reorders the members. Compare two enums as sets rather than by
    /// [`PartialEq`] if the file has been through another implementation.
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
