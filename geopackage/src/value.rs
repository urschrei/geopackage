//! Typed column values: mapping SQLite storage classes to GeoPackage column
//! types.
//!
//! [`Value`] is the non-geometry cell type of the read path. Conversion from a
//! stored SQLite value is driven by the column's declared [`ColumnType`]: an
//! `INTEGER`-declared column yields [`Value::Integer`], a `DATETIME`-declared
//! column parses its text into a [`geopackage_core::datetime::DateTime`], and
//! so on. A storage class that is incompatible with the declared type is
//! reported as [`Error::ValueTypeMismatch`] rather than silently coerced.
//!
//! Geometry columns are not represented here; they are read through the
//! feature API.

use crate::{Error, GeoPackage, Result};
use geopackage_core::datetime::{Date, DateTime};
use geopackage_core::ident;
use geopackage_core::types::ColumnType;
use rusqlite::types::ValueRef;

/// A typed, non-geometry column value.
///
/// Storage width is not preserved: every declared integer width
/// (`TINYINT` … `INTEGER`) maps to [`Value::Integer`], and both `FLOAT` and
/// `DOUBLE`/`REAL` map to [`Value::Float`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Value {
    /// SQL `NULL`.
    Null,
    /// A `BOOLEAN` value (`0`/`1` stored as INTEGER).
    Boolean(bool),
    /// An integer value of any declared width.
    Integer(i64),
    /// A floating-point value.
    Float(f64),
    /// A text value.
    Text(String),
    /// A binary value.
    Blob(Vec<u8>),
    /// A `DATE` value.
    Date(Date),
    /// A `DATETIME` value.
    DateTime(DateTime),
}

/// How [`Value`] conversion interprets `DATETIME` text.
///
/// `DATE` text is always parsed with [`Date::parse`]; only `DATETIME` has a
/// strict and a lenient form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum DateTimeParsing {
    /// Accept only the strict 1.4 form `YYYY-MM-DDTHH:MM:SS.SSSZ`
    /// ([`DateTime::parse_strict`]). The default.
    #[default]
    Strict,
    /// Also accept the common ISO 8601 variants found in real files
    /// ([`DateTime::parse_lenient`]): second precision, other fractional
    /// widths, a space separator, and numeric UTC offsets.
    Lenient,
}

/// Options controlling [`Value`] conversion from stored SQLite values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ConversionOptions {
    /// How `DATETIME` text is parsed.
    pub datetime: DateTimeParsing,
}

impl ConversionOptions {
    /// Strict conversion: `DATETIME` must be the exact 1.4 form. The default.
    pub fn strict() -> Self {
        Self {
            datetime: DateTimeParsing::Strict,
        }
    }

    /// Lenient conversion: accept the common `DATETIME` variants found in real
    /// files.
    pub fn lenient() -> Self {
        Self {
            datetime: DateTimeParsing::Lenient,
        }
    }
}

impl GeoPackage {
    /// Read the values of one non-geometry column as typed [`Value`]s, in the
    /// table's natural row order.
    ///
    /// Each cell is interpreted according to the column's declared type in the
    /// table schema (see [`GeoPackage::table_schema`]). This is a building
    /// block for the feature/attribute read path.
    ///
    /// # Errors
    ///
    /// - [`Error::NoSuchTable`] / [`Error::NoSuchColumn`] if the table or
    ///   column is absent.
    /// - [`Error::GeometryValueUnsupported`] if the column is a geometry
    ///   column.
    /// - [`Error::ValueTypeMismatch`] or [`Error::InvalidDateTimeValue`] for a
    ///   cell whose stored value does not fit its declared type.
    pub fn column_values(
        &self,
        table_name: &str,
        column_name: &str,
        options: ConversionOptions,
    ) -> Result<Vec<Value>> {
        let schema = self.table_schema(table_name)?;
        let column = schema
            .column(column_name)
            .ok_or_else(|| Error::NoSuchColumn {
                table_name: table_name.to_owned(),
                column_name: column_name.to_owned(),
            })?;
        if let Some(ColumnType::Geometry(_)) = column.column_type {
            return Err(Error::GeometryValueUnsupported {
                column: column_name.to_owned(),
            });
        }
        let column_type = column.column_type.clone();

        let sql = format!(
            "SELECT {} FROM {}",
            ident::quote(column_name)?,
            ident::quote(table_name)?
        );
        let mut stmt = self.connection().prepare(&sql)?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(value_from_ref(
                row.get_ref(0)?,
                column_type.as_ref(),
                column_name,
                options,
            )?);
        }
        Ok(out)
    }
}

/// Convert a typed [`Value`] into an owned rusqlite value for parameter
/// binding (the [`crate::Layer::select`] passthrough).
///
/// [`Value::Date`] and [`Value::DateTime`] bind as their canonical text form
/// (the strict spec spelling); [`Value::Boolean`] binds as the integer `0`/`1`.
/// This keeps rusqlite types out of the public API: callers pass our [`Value`]
/// enum and conversion happens here.
pub(crate) fn value_to_sql(value: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as Sql;
    match value {
        Value::Null => Sql::Null,
        Value::Boolean(b) => Sql::Integer(i64::from(*b)),
        Value::Integer(i) => Sql::Integer(*i),
        Value::Float(f) => Sql::Real(*f),
        Value::Text(s) => Sql::Text(s.clone()),
        Value::Blob(b) => Sql::Blob(b.clone()),
        Value::Date(d) => Sql::Text(d.to_string()),
        Value::DateTime(dt) => Sql::Text(dt.to_string()),
    }
}

/// Convert a raw SQLite value into a typed [`Value`], driven by the column's
/// declared type.
///
/// `column_type` is `None` for a column whose declared type is outside the
/// spec vocabulary; such a value is surfaced by its raw storage class.
pub(crate) fn value_from_ref(
    value: ValueRef<'_>,
    column_type: Option<&ColumnType>,
    column_name: &str,
    options: ConversionOptions,
) -> Result<Value> {
    // NULL is NULL irrespective of the declared type.
    if let ValueRef::Null = value {
        return Ok(Value::Null);
    }
    let Some(declared) = column_type else {
        return untyped(value);
    };
    match declared {
        ColumnType::Boolean => match value {
            ValueRef::Integer(i) => Ok(Value::Boolean(i != 0)),
            other => Err(mismatch(column_name, declared, other)),
        },
        ColumnType::TinyInt
        | ColumnType::SmallInt
        | ColumnType::MediumInt
        | ColumnType::Integer => match value {
            ValueRef::Integer(i) => Ok(Value::Integer(i)),
            other => Err(mismatch(column_name, declared, other)),
        },
        ColumnType::Float | ColumnType::Double => match value {
            ValueRef::Real(f) => Ok(Value::Float(f)),
            // A whole number stored with integer affinity in a real column is
            // widened losslessly rather than rejected.
            ValueRef::Integer(i) => Ok(Value::Float(i as f64)),
            other => Err(mismatch(column_name, declared, other)),
        },
        ColumnType::Text(_) => match value {
            ValueRef::Text(bytes) => Ok(Value::Text(text(bytes)?)),
            other => Err(mismatch(column_name, declared, other)),
        },
        ColumnType::Blob(_) => match value {
            ValueRef::Blob(bytes) => Ok(Value::Blob(bytes.to_vec())),
            other => Err(mismatch(column_name, declared, other)),
        },
        ColumnType::Date => match value {
            ValueRef::Text(bytes) => {
                let s = text(bytes)?;
                Date::parse(&s)
                    .map(Value::Date)
                    .map_err(|source| Error::InvalidDateTimeValue {
                        column: column_name.to_owned(),
                        text: s,
                        source,
                    })
            }
            other => Err(mismatch(column_name, declared, other)),
        },
        ColumnType::DateTime => match value {
            ValueRef::Text(bytes) => {
                let s = text(bytes)?;
                let parsed = match options.datetime {
                    DateTimeParsing::Strict => DateTime::parse_strict(&s),
                    DateTimeParsing::Lenient => DateTime::parse_lenient(&s),
                };
                parsed
                    .map(Value::DateTime)
                    .map_err(|source| Error::InvalidDateTimeValue {
                        column: column_name.to_owned(),
                        text: s,
                        source,
                    })
            }
            other => Err(mismatch(column_name, declared, other)),
        },
        ColumnType::Geometry(_) => Err(Error::GeometryValueUnsupported {
            column: column_name.to_owned(),
        }),
        // `ColumnType` is `#[non_exhaustive]`; a spec type added in future is
        // surfaced by its raw storage class rather than crashing.
        _ => untyped(value),
    }
}

/// Surface a value by its raw storage class, used when the declared type is
/// outside the spec vocabulary.
fn untyped(value: ValueRef<'_>) -> Result<Value> {
    Ok(match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Integer(i),
        ValueRef::Real(f) => Value::Float(f),
        ValueRef::Text(bytes) => Value::Text(text(bytes)?),
        ValueRef::Blob(bytes) => Value::Blob(bytes.to_vec()),
    })
}

/// Decode SQLite TEXT bytes as UTF-8.
fn text(bytes: &[u8]) -> Result<String> {
    Ok(std::str::from_utf8(bytes)
        .map_err(rusqlite::Error::from)?
        .to_owned())
}

/// Build a [`Error::ValueTypeMismatch`] for an incompatible storage class.
fn mismatch(column: &str, declared: &ColumnType, found: ValueRef<'_>) -> Error {
    Error::ValueTypeMismatch {
        column: column.to_owned(),
        declared: declared.clone(),
        found: storage_class(found),
    }
}

/// The SQLite storage class name of a value, for diagnostics.
fn storage_class(value: ValueRef<'_>) -> &'static str {
    match value {
        ValueRef::Null => "NULL",
        ValueRef::Integer(_) => "INTEGER",
        ValueRef::Real(_) => "REAL",
        ValueRef::Text(_) => "TEXT",
        ValueRef::Blob(_) => "BLOB",
    }
}
