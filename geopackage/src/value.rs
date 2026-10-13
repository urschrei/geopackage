//! Typed column values: mapping SQLite storage classes to GeoPackage column
//! types.
//!
//! [`Value`] and its borrowed counterpart [`ValueRef`] are the non-geometry
//! cell types. A [`crate::Feature`] returns `ValueRef`s that point into the
//! buffer holding its row, and the write path accepts them. `Value` is the
//! owned form, for a value that has to outlive its row or the call it was
//! passed to.
//!
//! Conversion from a stored SQLite value depends on the column's declared
//! [`ColumnType`]. An `INTEGER`-declared column yields [`Value::Integer`], a
//! `DATETIME`-declared column parses its text into a
//! [`geopackage_core::datetime::DateTime`], and so on. A storage class that is
//! incompatible with the declared type is reported as
//! [`Error::ValueTypeMismatch`] rather than silently coerced.
//!
//! Two cases fall in between: a `BOOLEAN` column containing an integer other
//! than `0` or `1`, and an integer in a `FLOAT`/`DOUBLE` column. Both can be
//! read as the declared type, but both are non-conformant. You choose whether
//! to read or reject them through [`ConversionOptions::storage`]
//! ([`StorageStrictness`]). The default reads them, because other
//! implementations read files containing them without complaint.
//!
//! Geometry columns are not represented here; they are read through the feature
//! API.

use crate::{Error, GeoPackage, Result};
use geopackage_core::datetime::{Date, DateTime};
use geopackage_core::ident;
use geopackage_core::types::ColumnType;
use rusqlite::types::ValueRef as SqlValueRef;

/// A typed, non-geometry column value.
///
/// Storage width is not preserved. Every declared integer width
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

/// A borrowed [`Value`]: the same cases, with text and binary borrowed from
/// whatever stores the row's bytes.
///
/// The read path returns these. A [`crate::Feature`] keeps its text and blob
/// cells in one buffer, not as a separate `String` or `Vec<u8>` each, so there
/// is no stored `Value` to borrow. Instead, a `ValueRef` points into that
/// buffer. Call [`ValueRef::to_value`] for a `Value` that outlives the
/// feature.
///
/// The variants without a borrow hold their value directly, because [`Date`]
/// and [`DateTime`] are `Copy` and smaller than a pointer pair.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum ValueRef<'a> {
    /// SQL `NULL`.
    Null,
    /// A `BOOLEAN` value (`0`/`1` stored as INTEGER).
    Boolean(bool),
    /// An integer value of any declared width.
    Integer(i64),
    /// A floating-point value.
    Float(f64),
    /// A text value.
    Text(&'a str),
    /// A binary value.
    Blob(&'a [u8]),
    /// A `DATE` value.
    Date(Date),
    /// A `DATETIME` value.
    DateTime(DateTime),
}

impl<'a> ValueRef<'a> {
    /// Copies this into an owned [`Value`].
    ///
    /// This isn't named `to_owned`, because `ValueRef` is `Copy` and so already
    /// has a `ToOwned::to_owned` that returns another `ValueRef`. An inherent
    /// method of that name would shadow it, and return a different type
    /// depending on whether you called it as a method or through the trait.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::from(*self)
    }

    /// Returns the text, if this is a [`ValueRef::Text`].
    ///
    /// The result borrows from whatever stores the row's bytes, not from this
    /// value, so it outlives the `ValueRef` it came from.
    #[must_use]
    pub fn as_str(&self) -> Option<&'a str> {
        match *self {
            ValueRef::Text(s) => Some(s),
            _ => None,
        }
    }

    /// Returns the bytes, if this is a [`ValueRef::Blob`].
    ///
    /// The result borrows as in [`Self::as_str`].
    #[must_use]
    pub fn as_blob(&self) -> Option<&'a [u8]> {
        match *self {
            ValueRef::Blob(b) => Some(b),
            _ => None,
        }
    }

    /// Returns `true` if this is [`ValueRef::Null`].
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(*self, ValueRef::Null)
    }

    /// Returns the boolean, if this is a [`ValueRef::Boolean`].
    ///
    /// An integer column containing `0` or `1` still reads as
    /// [`ValueRef::Integer`], because the column's declared type decides the
    /// conversion, not the value's contents.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match *self {
            ValueRef::Boolean(b) => Some(b),
            _ => None,
        }
    }

    /// Returns the integer, if this is a [`ValueRef::Integer`].
    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            ValueRef::Integer(i) => Some(i),
            _ => None,
        }
    }

    /// Returns the float, if this is a [`ValueRef::Float`].
    ///
    /// An `INTEGER`-declared column reads as [`ValueRef::Integer`] even when
    /// the value would widen losslessly, and this method does not convert it.
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            ValueRef::Float(f) => Some(f),
            _ => None,
        }
    }

    /// Returns the date, if this is a [`ValueRef::Date`].
    #[must_use]
    pub fn as_date(&self) -> Option<Date> {
        match *self {
            ValueRef::Date(d) => Some(d),
            _ => None,
        }
    }

    /// Returns the datetime, if this is a [`ValueRef::DateTime`].
    #[must_use]
    pub fn as_datetime(&self) -> Option<DateTime> {
        match *self {
            ValueRef::DateTime(dt) => Some(dt),
            _ => None,
        }
    }
}

impl From<ValueRef<'_>> for Value {
    fn from(value: ValueRef<'_>) -> Self {
        match value {
            ValueRef::Null => Value::Null,
            ValueRef::Boolean(b) => Value::Boolean(b),
            ValueRef::Integer(i) => Value::Integer(i),
            ValueRef::Float(f) => Value::Float(f),
            ValueRef::Text(s) => Value::Text(s.to_owned()),
            ValueRef::Blob(b) => Value::Blob(b.to_vec()),
            ValueRef::Date(d) => Value::Date(d),
            ValueRef::DateTime(dt) => Value::DateTime(dt),
        }
    }
}

impl<'a> From<&'a Value> for ValueRef<'a> {
    fn from(value: &'a Value) -> Self {
        match value {
            Value::Null => ValueRef::Null,
            Value::Boolean(b) => ValueRef::Boolean(*b),
            Value::Integer(i) => ValueRef::Integer(*i),
            Value::Float(f) => ValueRef::Float(*f),
            Value::Text(s) => ValueRef::Text(s),
            Value::Blob(b) => ValueRef::Blob(b),
            Value::Date(d) => ValueRef::Date(*d),
            Value::DateTime(dt) => ValueRef::DateTime(*dt),
        }
    }
}

/// Compares a borrowed value against an owned one without copying either.
///
/// This works on bare values only. `Option` has no cross-type `PartialEq` in
/// the standard library, so `feature.value("x")`, an `Option<ValueRef>`, does
/// not compare against an `Option<Value>`. Write the expected side as a
/// `ValueRef` in that case.
impl PartialEq<Value> for ValueRef<'_> {
    fn eq(&self, other: &Value) -> bool {
        *self == ValueRef::from(other)
    }
}

impl PartialEq<ValueRef<'_>> for Value {
    fn eq(&self, other: &ValueRef<'_>) -> bool {
        ValueRef::from(self) == *other
    }
}

/// How [`Value`] conversion interprets `DATETIME` text.
///
/// `DATE` text is always parsed with [`Date::parse`]. Only `DATETIME` has a
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

/// How [`Value`] conversion treats a stored value that its declared type does
/// not strictly permit but that can still be read as that type.
///
/// These cases come from SQLite's storage model rather than from the GeoPackage
/// types. SQLite stores what it is given under a column's type affinity, and
/// `BOOLEAN` has no affinity at all, so a `BOOLEAN` column can contain any
/// integer whatever the spec says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum StorageStrictness {
    /// Read the value as its declared type where possible. Any non-zero
    /// INTEGER in a `BOOLEAN` column is `true`, and an INTEGER in a
    /// `FLOAT`/`DOUBLE` column widens losslessly to [`Value::Float`].
    ///
    /// This is the default. Other implementations read files containing these
    /// non-conformant values without complaint, so rejecting them by default
    /// would make this crate the odd one out on files it can read perfectly
    /// well.
    #[default]
    Lenient,
    /// Reject both. A `BOOLEAN` column may contain only `0` or `1`
    /// ([`Error::NonBooleanInteger`]), and a `FLOAT`/`DOUBLE` column may
    /// contain only REAL ([`Error::ValueTypeMismatch`]).
    Strict,
}

/// Options controlling [`Value`] conversion from stored SQLite values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ConversionOptions {
    /// How `DATETIME` text is parsed.
    pub datetime: DateTimeParsing,
    /// How a value its declared type does not strictly permit is treated.
    pub storage: StorageStrictness,
}

impl ConversionOptions {
    /// Creates options that are strict throughout.
    ///
    /// `DATETIME` must be the exact 1.4 form, and a value its declared type
    /// does not permit is an error rather than an interpretation.
    ///
    /// This is stricter than [`Self::default`], which pairs strict `DATETIME`
    /// parsing with [`StorageStrictness::Lenient`] because that combination
    /// reads the files real writers produce. Use [`Self::with_datetime`] and
    /// [`Self::with_storage`] to mix the two.
    pub fn strict() -> Self {
        Self {
            datetime: DateTimeParsing::Strict,
            storage: StorageStrictness::Strict,
        }
    }

    /// Creates options that are lenient throughout.
    ///
    /// They accept the common `DATETIME` variants found in real files, and
    /// read values their declared type does not strictly permit.
    pub fn lenient() -> Self {
        Self {
            datetime: DateTimeParsing::Lenient,
            storage: StorageStrictness::Lenient,
        }
    }

    /// Sets how `DATETIME` text is parsed.
    #[must_use]
    pub fn with_datetime(mut self, datetime: DateTimeParsing) -> Self {
        self.datetime = datetime;
        self
    }

    /// Sets how a value its declared type does not strictly permit is
    /// treated.
    #[must_use]
    pub fn with_storage(mut self, storage: StorageStrictness) -> Self {
        self.storage = storage;
        self
    }
}

impl GeoPackage {
    /// Reads the values of one non-geometry column as typed [`Value`]s, in
    /// the table's natural row order.
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

/// Converts a typed [`Value`] into an owned rusqlite value for parameter
/// binding (the [`crate::Layer::select`] passthrough).
///
/// The prepared statement keeps query parameters for as long as the cursor
/// lives, which is longer than the borrow the caller passed in. This is
/// therefore one of the few places the read path has to copy. The copy happens
/// once per query, not once per row.
pub(crate) fn value_ref_to_sql(value: &ValueRef<'_>) -> rusqlite::types::Value {
    value_into_sql(value.to_value())
}

/// Binds a borrowed [`Value`] without copying it.
///
/// The scalar write path keeps the caller's values for the whole insert, so
/// text and blob cells can be bound straight out of them. Going through
/// [`value_to_sql`] instead would deep-copy every string and blob in the row,
/// once per row written. Only `DATE` and `DATETIME` are owned here, because
/// they are formatted rather than copied.
pub(crate) fn value_ref_to_bind(value: ValueRef<'_>) -> rusqlite::types::ToSqlOutput<'_> {
    use rusqlite::types::{ToSqlOutput, Value as Sql};
    match value {
        ValueRef::Null => ToSqlOutput::Borrowed(SqlValueRef::Null),
        ValueRef::Boolean(b) => ToSqlOutput::Borrowed(SqlValueRef::Integer(i64::from(b))),
        ValueRef::Integer(i) => ToSqlOutput::Borrowed(SqlValueRef::Integer(i)),
        ValueRef::Float(f) => ToSqlOutput::Borrowed(SqlValueRef::Real(f)),
        ValueRef::Text(s) => ToSqlOutput::Borrowed(SqlValueRef::Text(s.as_bytes())),
        ValueRef::Blob(b) => ToSqlOutput::Borrowed(SqlValueRef::Blob(b)),
        ValueRef::Date(d) => ToSqlOutput::Owned(Sql::Text(d.to_string())),
        ValueRef::DateTime(dt) => ToSqlOutput::Owned(Sql::Text(dt.to_string())),
    }
}

/// Like [`value_ref_to_bind`], for a caller holding an owned [`Value`].
pub(crate) fn value_to_bind(value: &Value) -> rusqlite::types::ToSqlOutput<'_> {
    use rusqlite::types::{ToSqlOutput, Value as Sql};
    match value {
        Value::Null => ToSqlOutput::Borrowed(SqlValueRef::Null),
        Value::Boolean(b) => ToSqlOutput::Borrowed(SqlValueRef::Integer(i64::from(*b))),
        Value::Integer(i) => ToSqlOutput::Borrowed(SqlValueRef::Integer(*i)),
        Value::Float(f) => ToSqlOutput::Borrowed(SqlValueRef::Real(*f)),
        Value::Text(s) => ToSqlOutput::Borrowed(SqlValueRef::Text(s.as_bytes())),
        Value::Blob(b) => ToSqlOutput::Borrowed(SqlValueRef::Blob(b)),
        Value::Date(d) => ToSqlOutput::Owned(Sql::Text(d.to_string())),
        Value::DateTime(dt) => ToSqlOutput::Owned(Sql::Text(dt.to_string())),
    }
}

/// Like [`value_ref_to_sql`], for a caller that owns its value. A string or
/// blob moves into the binding instead of being copied.
///
/// The columnar write path builds a value per cell and then binds it once, so
/// cloning there would copy every string twice before it reached SQLite.
pub(crate) fn value_into_sql(value: Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as Sql;
    match value {
        Value::Null => Sql::Null,
        Value::Boolean(b) => Sql::Integer(i64::from(b)),
        Value::Integer(i) => Sql::Integer(i),
        Value::Float(f) => Sql::Real(f),
        Value::Text(s) => Sql::Text(s),
        Value::Blob(b) => Sql::Blob(b),
        Value::Date(d) => Sql::Text(d.to_string()),
        Value::DateTime(dt) => Sql::Text(dt.to_string()),
    }
}

/// Converts a raw SQLite value into a typed [`Value`], driven by the column's
/// declared type.
///
/// `column_type` is `None` for a column whose declared type is outside the
/// spec vocabulary. Such a value is returned by its raw storage class.
pub(crate) fn value_ref_from_sql<'a>(
    value: SqlValueRef<'a>,
    column_type: Option<&ColumnType>,
    column_name: &str,
    options: ConversionOptions,
) -> Result<ValueRef<'a>> {
    // NULL is NULL irrespective of the declared type.
    if let SqlValueRef::Null = value {
        return Ok(ValueRef::Null);
    }
    let Some(declared) = column_type else {
        return untyped(value);
    };
    match declared {
        ColumnType::Boolean => match value {
            SqlValueRef::Integer(0) => Ok(ValueRef::Boolean(false)),
            SqlValueRef::Integer(1) => Ok(ValueRef::Boolean(true)),
            // The spec says a BOOLEAN column contains 0 or 1, but SQLite
            // gives the declared type no affinity of its own, so the column
            // stores whatever was inserted. Anything non-zero reads as
            // `true`, which is the C convention the writers that produce such
            // files are following.
            SqlValueRef::Integer(value) => match options.storage {
                StorageStrictness::Lenient => Ok(ValueRef::Boolean(true)),
                StorageStrictness::Strict => Err(Error::NonBooleanInteger {
                    column: column_name.to_owned(),
                    value,
                }),
            },
            other => Err(mismatch(column_name, declared, other)),
        },
        ColumnType::TinyInt
        | ColumnType::SmallInt
        | ColumnType::MediumInt
        | ColumnType::Integer => match value {
            SqlValueRef::Integer(i) => Ok(ValueRef::Integer(i)),
            other => Err(mismatch(column_name, declared, other)),
        },
        ColumnType::Float | ColumnType::Double => match value {
            SqlValueRef::Real(f) => Ok(ValueRef::Float(f)),
            // A whole number stored with integer affinity in a real column is
            // widened losslessly rather than rejected.
            //
            // Reading a table or view column does not reach this arm: `FLOAT`,
            // `DOUBLE` and `REAL` all give the column REAL affinity, which
            // converts an integer to floating point on the way in, and converts
            // again on the way out for a file whose stored bytes say otherwise.
            // It is kept as a defensive arm rather than removed, and it answers
            // to the same option as the BOOLEAN case above so that strict
            // conversion means one thing.
            SqlValueRef::Integer(i) => match options.storage {
                StorageStrictness::Lenient => Ok(ValueRef::Float(i as f64)),
                StorageStrictness::Strict => Err(mismatch(column_name, declared, value)),
            },
            other => Err(mismatch(column_name, declared, other)),
        },
        ColumnType::Text(_) => match value {
            SqlValueRef::Text(bytes) => Ok(ValueRef::Text(text_ref(bytes)?)),
            other => Err(mismatch(column_name, declared, other)),
        },
        ColumnType::Blob(_) => match value {
            SqlValueRef::Blob(bytes) => Ok(ValueRef::Blob(bytes)),
            other => Err(mismatch(column_name, declared, other)),
        },
        ColumnType::Date => match value {
            SqlValueRef::Text(bytes) => {
                let s = text_ref(bytes)?;
                Date::parse(s)
                    .map(ValueRef::Date)
                    .map_err(|source| Error::InvalidDateTimeValue {
                        column: column_name.to_owned(),
                        text: s.to_owned(),
                        source,
                    })
            }
            other => Err(mismatch(column_name, declared, other)),
        },
        ColumnType::DateTime => match value {
            SqlValueRef::Text(bytes) => {
                let s = text_ref(bytes)?;
                let parsed = match options.datetime {
                    DateTimeParsing::Strict => DateTime::parse_strict(s),
                    DateTimeParsing::Lenient => DateTime::parse_lenient(s),
                };
                parsed
                    .map(ValueRef::DateTime)
                    .map_err(|source| Error::InvalidDateTimeValue {
                        column: column_name.to_owned(),
                        text: s.to_owned(),
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

/// Converts a value by its raw storage class alone, for a declared type
/// outside the spec vocabulary.
fn untyped<'a>(value: SqlValueRef<'a>) -> Result<ValueRef<'a>> {
    Ok(match value {
        SqlValueRef::Null => ValueRef::Null,
        SqlValueRef::Integer(i) => ValueRef::Integer(i),
        SqlValueRef::Real(f) => ValueRef::Float(f),
        SqlValueRef::Text(bytes) => ValueRef::Text(text_ref(bytes)?),
        SqlValueRef::Blob(bytes) => ValueRef::Blob(bytes),
    })
}

/// Like [`value_ref_from_sql`], but produces an owned [`Value`].
///
/// The conversion itself borrows. This copies the result for callers that need
/// a value to outlive the row, such as the query-parameter path.
pub(crate) fn value_from_ref(
    value: SqlValueRef<'_>,
    column_type: Option<&ColumnType>,
    column_name: &str,
    options: ConversionOptions,
) -> Result<Value> {
    value_ref_from_sql(value, column_type, column_name, options).map(Value::from)
}

/// Borrows SQLite TEXT bytes as UTF-8.
///
/// The parsed types read through this. They consume the text and keep none of
/// it, so copying it first would allocate and drop a `String` per cell, per
/// row.
fn text_ref(bytes: &[u8]) -> Result<&str> {
    Ok(std::str::from_utf8(bytes).map_err(rusqlite::Error::from)?)
}

/// Builds an [`Error::ValueTypeMismatch`] for an incompatible storage class.
fn mismatch(column: &str, declared: &ColumnType, found: SqlValueRef<'_>) -> Error {
    Error::ValueTypeMismatch {
        column: column.to_owned(),
        declared: declared.clone(),
        found: storage_class(found),
    }
}

/// Returns the SQLite storage class name of a value, for diagnostics.
fn storage_class(value: SqlValueRef<'_>) -> &'static str {
    match value {
        SqlValueRef::Null => "NULL",
        SqlValueRef::Integer(_) => "INTEGER",
        SqlValueRef::Real(_) => "REAL",
        SqlValueRef::Text(_) => "TEXT",
        SqlValueRef::Blob(_) => "BLOB",
    }
}
