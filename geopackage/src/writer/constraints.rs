use geopackage_core::schema::{ColumnConstraint, ConstraintKind};
use rusqlite::types::ToSqlOutput;
use rusqlite::{CachedStatement, Connection};

use crate::{Layer, Result, Value, ValueRef as CellRef};

use super::feature_writer::ValueColumn;

/// The `gpkg_schema` constraints a writer checks its values against, one entry
/// per value column and `None` where a column has none.
///
/// Empty, and so free per row, unless
/// [`OpenOptions::enforce_column_constraints`](crate::OpenOptions::enforce_column_constraints)
/// was set: the constraints are advisory in the format ("These restrictions
/// MAY be enforced by SQL triggers or by code in applications that update
/// GeoPackage data values"), so enforcing them is the caller's decision rather
/// than this crate's.
pub(crate) struct ColumnConstraints<'conn> {
    pub(crate) per_column: Vec<Option<ColumnConstraint>>,
    /// `SELECT ?1 GLOB ?2`, prepared once, for the glob form.
    ///
    /// The pattern language is SQLite's, defined by whatever the engine
    /// holding the file does, so the engine answers rather than a copy of its
    /// rules living here. That also gives numbers SQLite's own text coercion,
    /// which is what a trigger enforcing the same constraint would apply.
    /// `None` when no column carries a glob, which is the usual case.
    pub(crate) glob: Option<CachedStatement<'conn>>,
}

impl<'conn> ColumnConstraints<'conn> {
    /// Resolve each value column's constraint, once per writer.
    pub(crate) fn read(
        layer: &Layer<'_>,
        conn: &'conn Connection,
        value_columns: &[ValueColumn],
    ) -> Result<Self> {
        if !layer.gpkg().enforces_column_constraints() {
            return Ok(Self::none());
        }
        let described = layer.gpkg().data_columns(layer.table_name())?;
        let mut per_column = Vec::with_capacity(value_columns.len());
        for column in value_columns {
            let constraint_name = described
                .iter()
                .find(|described| described.column_name == column.name)
                .and_then(|described| described.constraint_name.as_deref());
            per_column.push(match constraint_name {
                Some(name) => layer.gpkg().column_constraint(name)?,
                None => None,
            });
        }
        let needs_glob = per_column
            .iter()
            .flatten()
            .any(|constraint| matches!(constraint.kind, ConstraintKind::Glob(_)));
        let glob = match needs_glob {
            true => Some(conn.prepare_cached("SELECT ?1 GLOB ?2")?),
            false => None,
        };
        Ok(Self { per_column, glob })
    }

    /// Nothing enforced.
    fn none() -> Self {
        Self {
            per_column: Vec::new(),
            glob: None,
        }
    }

    /// Whether anything at all is enforced, so the row paths can skip the walk.
    pub(crate) fn is_empty(&self) -> bool {
        self.per_column.iter().all(Option::is_none)
    }

    pub(crate) fn at(&self, index: usize) -> Option<&ColumnConstraint> {
        self.per_column.get(index).and_then(Option::as_ref)
    }

    /// Whether `value` satisfies the constraint on the column at `index`.
    ///
    /// The range and enum forms are decided here; the glob form is put to
    /// SQLite.
    pub(crate) fn satisfied(&mut self, index: usize, value: Checkable<'_>) -> Result<bool> {
        let Self { per_column, glob } = self;
        let Some(Some(constraint)) = per_column.get(index) else {
            return Ok(true);
        };
        match (&constraint.kind, value) {
            (_, Checkable::Null | Checkable::Unchecked) => Ok(true),
            (ConstraintKind::Range { .. }, Checkable::Text(_)) => Ok(false),
            (ConstraintKind::Range { .. }, Checkable::Integer(number)) => {
                // i64 to f64 loses precision above 2^53, and a bound that far
                // out is not one anybody wrote deliberately.
                Ok(in_range(&constraint.kind, number as f64))
            }
            (ConstraintKind::Range { .. }, Checkable::Real(number)) => {
                Ok(in_range(&constraint.kind, number))
            }
            (ConstraintKind::Enum(members), value) => Ok(match value {
                Checkable::Text(text) => members.iter().any(|member| member == text),
                // The members are text, and the spec's own sample enumerates
                // the numbers 1, 3, 5, 7 and 9, so a number is compared by its
                // decimal form.
                Checkable::Integer(number) => members.contains(&number.to_string()),
                Checkable::Real(number) => members.contains(&number.to_string()),
                Checkable::Null | Checkable::Unchecked => true,
            }),
            (ConstraintKind::Glob(pattern), value) => {
                let statement = glob
                    .as_mut()
                    .expect("a glob constraint means the statement was prepared");
                let matched: i64 = match value {
                    Checkable::Text(text) => {
                        statement.query_one(rusqlite::params![text, pattern], |r| r.get(0))?
                    }
                    Checkable::Integer(number) => {
                        statement.query_one(rusqlite::params![number, pattern], |r| r.get(0))?
                    }
                    Checkable::Real(number) => {
                        statement.query_one(rusqlite::params![number, pattern], |r| r.get(0))?
                    }
                    Checkable::Null | Checkable::Unchecked => 1,
                };
                Ok(matched != 0)
            }
        }
    }
}

/// Whether `value` falls inside a range constraint, honouring both bounds'
/// inclusivity. Anything but a range is outside it.
pub(crate) fn in_range(kind: &ConstraintKind, value: f64) -> bool {
    let ConstraintKind::Range {
        min,
        min_is_inclusive,
        max,
        max_is_inclusive,
    } = kind
    else {
        return false;
    };
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

/// A value reduced to what a constraint can judge it by.
///
/// The three write paths hand values over in three forms; this is what they
/// have in common as far as a `range`, `enum` or `glob` is concerned.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Checkable<'a> {
    /// No value. A constraint says what a value may be, not that there has to
    /// be one, so NULL satisfies every constraint. A column that must hold
    /// something says so with `NOT NULL`.
    Null,
    /// An integer, compared numerically against a `range` and by its decimal
    /// form against an `enum` or `glob`: the spec's own sample enumerates the
    /// numbers 1, 3, 5, 7 and 9 as `enum` values.
    Integer(i64),
    /// A float, compared as [`Checkable::Integer`] is.
    Real(f64),
    /// Text, which no `range` admits and which an `enum` or `glob` compares
    /// directly.
    Text(&'a str),
    /// A blob, date or datetime, none of which any constraint form describes.
    /// Not checked.
    Unchecked,
}

/// The value forms a write path can hand over.
pub(crate) trait AsCheckable {
    /// This value, as a constraint sees it.
    fn as_checkable(&self) -> Checkable<'_>;
}

impl AsCheckable for Value {
    fn as_checkable(&self) -> Checkable<'_> {
        match self {
            Self::Null => Checkable::Null,
            Self::Integer(value) => Checkable::Integer(*value),
            Self::Boolean(value) => Checkable::Integer(i64::from(*value)),
            Self::Float(value) => Checkable::Real(*value),
            Self::Text(value) => Checkable::Text(value),
            Self::Blob(_) | Self::Date(_) | Self::DateTime(_) => Checkable::Unchecked,
        }
    }
}

impl AsCheckable for CellRef<'_> {
    fn as_checkable(&self) -> Checkable<'_> {
        match self {
            Self::Null => Checkable::Null,
            Self::Integer(value) => Checkable::Integer(*value),
            Self::Boolean(value) => Checkable::Integer(i64::from(*value)),
            Self::Float(value) => Checkable::Real(*value),
            Self::Text(value) => Checkable::Text(value),
            Self::Blob(_) | Self::Date(_) | Self::DateTime(_) => Checkable::Unchecked,
        }
    }
}

impl AsCheckable for ToSqlOutput<'_> {
    fn as_checkable(&self) -> Checkable<'_> {
        let value = match self {
            Self::Borrowed(value) => *value,
            Self::Owned(value) => value.into(),
            _ => return Checkable::Unchecked,
        };
        match value {
            rusqlite::types::ValueRef::Null => Checkable::Null,
            rusqlite::types::ValueRef::Integer(value) => Checkable::Integer(value),
            rusqlite::types::ValueRef::Real(value) => Checkable::Real(value),
            rusqlite::types::ValueRef::Text(bytes) => match std::str::from_utf8(bytes) {
                Ok(text) => Checkable::Text(text),
                Err(_) => Checkable::Unchecked,
            },
            rusqlite::types::ValueRef::Blob(_) => Checkable::Unchecked,
        }
    }
}
