//! Arrow schema derivation: GeoPackage column types to Arrow field types.
//!
//! This is the type mapping the columnar read and write paths share. It is the
//! "impedance mismatch" the GDAL developers report in their write-up of the same
//! exercise (roadmap 05-m3), so each choice below is stated with its reason
//! rather than left to be inferred from the code.
//!
//! # Attribute types
//!
//! | Declared type | Arrow type | Note |
//! |---|---|---|
//! | `BOOLEAN` | `Boolean` | |
//! | `TINYINT`, `SMALLINT`, `MEDIUMINT`, `INT`, `INTEGER` | `Int64` | width not preserved, see below |
//! | `FLOAT`, `DOUBLE`, `REAL` | `Float64` | |
//! | `TEXT`, `TEXT(N)` | `Utf8` | the declared length is informative and is dropped |
//! | `BLOB`, `BLOB(N)` | `Binary` | |
//! | `DATE` | `Date32` | days since the epoch |
//! | `DATETIME` | `Timestamp(Microsecond, "UTC")` | see below |
//! | a geometry type | `Binary` + `geoarrow.wkb` | the geometry column |
//! | anything else | by SQLite affinity | see below |
//!
//! **Integer widths collapse to `Int64`.** SQLite does not enforce a declared
//! integer width: a `TINYINT` column holds whatever integer it was given, and
//! reading one that exceeds the width is an ordinary thing to have to do. Every
//! width therefore maps to `Int64`, which cannot truncate. This matches
//! [`crate::Value::Integer`], which collapses the same widths for the same
//! reason.
//!
//! **`DATETIME` becomes microsecond UTC.** The GeoPackage form is a UTC
//! ISO 8601 string with millisecond precision, and
//! [`geopackage_core::datetime::DateTime`] parses finer input than that, so
//! microseconds hold what the strict form can express with room for the lenient
//! form. Values are converted, not reinterpreted: the text is parsed and the
//! instant is written, so a consumer sees a timestamp rather than a string.
//!
//! **A type outside the vocabulary is mapped by SQLite's affinity rules.**
//! `VARCHAR(20)` and friends are common in files written by other tools, and
//! [`geopackage_core::types::ColumnType::parse`] returns `None` for them.
//! Refusing to read such a column would make the Arrow path useless on real
//! files, so the declared type is run through the affinity rules from
//! [SQLite section 3.1](https://www.sqlite.org/datatype3.html#determination_of_column_affinity),
//! which is exactly how SQLite itself decides what such a column holds.
//!
//! # Nullability
//!
//! A field is non-nullable only when the column is `NOT NULL`. The primary key
//! is non-nullable regardless, since SQLite's `INTEGER PRIMARY KEY` cannot hold
//! NULL.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, Date32Builder, Float64Builder, Int64Builder, StringBuilder,
    TimestampMicrosecondBuilder,
};
use arrow_array::{ArrayRef, RecordBatch, RecordBatchReader};
use arrow_schema::{ArrowError, DataType, Field, Schema, SchemaRef, TimeUnit};
use geopackage_core::datetime::{Date, DateTime};
use geopackage_core::gpb;
use geopackage_core::ident::quote;
use geopackage_core::types::ColumnType;
use rusqlite::Connection;
use rusqlite::functions::{Aggregate, Context, FunctionFlags};
use rusqlite::limits::Limit;
use rusqlite::types::ValueRef;

use crate::schema::{Column, GeometryColumn};
use crate::value::DateTimeParsing;
use crate::{Error, Layer, Result};

/// Default number of rows per [`RecordBatch`].
///
/// The same default GDAL's driver uses (`MAX_FEATURES_IN_BATCH`). Large enough
/// that per-batch overhead disappears, small enough that a batch of wide rows
/// stays a sensible allocation.
pub const DEFAULT_BATCH_SIZE: usize = 65_536;

/// The Arrow extension-name metadata key, from the Arrow columnar spec.
const EXTENSION_NAME_KEY: &str = "ARROW:extension:name";

/// The Arrow extension-metadata key, which GeoArrow uses to carry CRS and
/// related information as JSON.
const EXTENSION_METADATA_KEY: &str = "ARROW:extension:metadata";

/// The GeoArrow extension name for a WKB-encoded geometry column.
const GEOARROW_WKB: &str = "geoarrow.wkb";

/// The time unit `DATETIME` columns are represented in.
const DATETIME_UNIT: TimeUnit = TimeUnit::Microsecond;

/// Alias for the pagination key when it is not itself one of the columns, so
/// the aggregate can name it as an argument.
const KEY_ALIAS: &str = "__gpkg_key";

impl Layer<'_> {
    /// The Arrow schema this layer's rows are read into.
    ///
    /// Fields appear in the table's column order, so the geometry column sits
    /// where it sits in the table rather than being moved to one end. See the
    /// [module documentation](self) for the type mapping.
    ///
    /// # Errors
    ///
    /// [`crate::Error`] if the table schema cannot be introspected.
    pub fn arrow_schema(&self) -> Result<SchemaRef> {
        let geometry = self.geometry_column();
        let fields: Vec<Field> = self
            .schema()
            .columns
            .iter()
            .map(|column| field_for(column, geometry))
            .collect();
        Ok(Arc::new(Schema::new(fields)))
    }
}

/// The Arrow field for one table column.
fn field_for(column: &Column, geometry: Option<&GeometryColumn>) -> Field {
    let is_geometry = geometry.is_some_and(|g| g.column_name == column.name);
    if is_geometry {
        // Unwrap-free: `is_geometry` is only true when `geometry` is `Some`.
        let srs_id = geometry.map_or(0, |g| g.srs_id);
        return geometry_field(&column.name, column.not_null, srs_id);
    }
    // An `INTEGER PRIMARY KEY` is SQLite's rowid alias and can never be NULL,
    // whether or not the column carries an explicit NOT NULL.
    let nullable = !column.not_null && !column.is_primary_key();
    Field::new(&column.name, data_type_for(column), nullable)
}

/// The Arrow data type for a non-geometry column.
fn data_type_for(column: &Column) -> DataType {
    let Some(declared) = &column.column_type else {
        return affinity_type(&column.declared_type);
    };
    match declared {
        ColumnType::Boolean => DataType::Boolean,
        ColumnType::TinyInt
        | ColumnType::SmallInt
        | ColumnType::MediumInt
        | ColumnType::Integer => DataType::Int64,
        ColumnType::Float | ColumnType::Double => DataType::Float64,
        ColumnType::Text(_) => DataType::Utf8,
        ColumnType::Blob(_) => DataType::Binary,
        ColumnType::Date => DataType::Date32,
        ColumnType::DateTime => DataType::Timestamp(DATETIME_UNIT, Some("UTC".into())),
        // A geometry type name on a column that is not *the* geometry column,
        // which a conformant file does not have. Its blob is surfaced as one.
        ColumnType::Geometry(_) => DataType::Binary,
        // `ColumnType` is `#[non_exhaustive]`: a type added to the spec later
        // is mapped by affinity rather than crashing.
        _ => affinity_type(&column.declared_type),
    }
}

/// The Arrow type for a declared type outside the spec vocabulary, following
/// SQLite's column-affinity rules.
///
/// The order of the tests is part of the rule and is not alphabetical: `INT`
/// wins over everything, and the `CHAR`/`CLOB`/`TEXT` family wins over `BLOB`.
///
/// SQLite's last two rules, REAL affinity (`REAL`, `FLOA`, `DOUB`) and the
/// NUMERIC fall-through, are one branch here because both land on `Float64`.
/// For REAL that is exact. NUMERIC is the one lossy case in this function:
/// SQLite may store an integer in such a column, and an integer beyond 2^53
/// does not survive the conversion. The alternative is surfacing the column as
/// text, which is lossless and unusable, so the loss is taken and recorded.
fn affinity_type(declared: &str) -> DataType {
    let declared = declared.to_ascii_uppercase();
    let has = |needle: &str| declared.contains(needle);
    if has("INT") {
        DataType::Int64
    } else if has("CHAR") || has("CLOB") || has("TEXT") {
        DataType::Utf8
    } else if has("BLOB") || declared.is_empty() {
        DataType::Binary
    } else {
        DataType::Float64
    }
}

/// The geometry field: WKB bytes carrying the GeoArrow extension name, and the
/// CRS as extension metadata.
///
/// The bytes are the GPB blob's WKB body, which is why this encoding is the one
/// to start from: it needs no geometry parsing at all.
fn geometry_field(name: &str, not_null: bool, srs_id: i32) -> Field {
    let mut metadata = HashMap::new();
    metadata.insert(EXTENSION_NAME_KEY.to_owned(), GEOARROW_WKB.to_owned());
    metadata.insert(EXTENSION_METADATA_KEY.to_owned(), crs_metadata(srs_id));
    Field::new(name, DataType::Binary, !not_null).with_metadata(metadata)
}

/// The GeoArrow extension metadata for a geometry column: a JSON object whose
/// `crs` names the SRS.
///
/// PROJJSON is what GeoArrow prefers and what a consumer would rather have. We
/// do not carry a PROJJSON representation of an arbitrary SRS, so this emits an
/// authority code, which GeoArrow permits, for the EPSG codes that a `srs_id`
/// conventionally is. `srs_id` 0 and -1 are the spec's undefined values and
/// carry no CRS at all.
///
/// Whether to carry PROJJSON is part of the CRS-definitions question in issue
/// #23: the vendored subset behind [`geopackage_core::srs`] could not supply it
/// for an arbitrary code even if this emitted it.
fn crs_metadata(srs_id: i32) -> String {
    if srs_id <= 0 {
        return "{}".to_owned();
    }
    format!(r#"{{"crs":"EPSG:{srs_id}","crs_type":"authority_code"}}"#)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affinity_follows_sqlite_rules() {
        // The examples SQLite's own documentation gives for each affinity.
        assert_eq!(affinity_type("VARCHAR(20)"), DataType::Utf8);
        assert_eq!(affinity_type("NVARCHAR(100)"), DataType::Utf8);
        assert_eq!(affinity_type("CLOB"), DataType::Utf8);
        assert_eq!(affinity_type("BIGINT"), DataType::Int64);
        assert_eq!(affinity_type("UNSIGNED BIG INT"), DataType::Int64);
        assert_eq!(affinity_type(""), DataType::Binary);
        assert_eq!(affinity_type("DOUBLE PRECISION"), DataType::Float64);
        assert_eq!(affinity_type("NUMERIC"), DataType::Float64);
        assert_eq!(affinity_type("DECIMAL(10,5)"), DataType::Float64);
    }

    #[test]
    fn int_wins_over_the_text_family() {
        // SQLite's rule 1 is tested before rule 2, so a type containing both
        // takes INTEGER affinity. `INT` inside `POINT` is the reason the read
        // path never routes a geometry column through here.
        assert_eq!(affinity_type("INTCHAR"), DataType::Int64);
    }

    #[test]
    fn char_family_wins_over_blob() {
        assert_eq!(affinity_type("TEXTBLOB"), DataType::Utf8);
    }

    /// Reference values from Python's `datetime`, which is an independent
    /// implementation of the same calendar.
    #[test]
    fn days_since_epoch_matches_the_calendar() {
        let days = |y, m, d| days_since_epoch(Date::new(y, m, d).unwrap());
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
        let micros = |text: &str| micros_since_epoch(DateTime::parse_strict(text).unwrap());
        assert_eq!(micros("1970-01-01T00:00:00.000Z"), 0);
        assert_eq!(micros("2026-07-24T12:34:56.789Z"), 1_784_896_496_789_000);
        assert_eq!(micros("1969-12-31T23:59:59.000Z"), -1_000_000);
    }

    #[test]
    fn a_utc_offset_is_normalised_away() {
        // Lenient parsing accepts a numeric offset. The same instant written
        // two ways must give the same number of microseconds.
        let utc = micros_since_epoch(DateTime::parse_strict("2026-07-24T12:34:56.000Z").unwrap());
        let offset =
            micros_since_epoch(DateTime::parse_lenient("2026-07-24T14:34:56+02:00").unwrap());
        assert_eq!(utc, offset);
    }
}

/// Options for the columnar read path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ArrowReadOptions {
    /// Rows per [`RecordBatch`]. Defaults to [`DEFAULT_BATCH_SIZE`].
    pub batch_size: usize,
}

impl Default for ArrowReadOptions {
    fn default() -> Self {
        Self {
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }
}

impl ArrowReadOptions {
    /// Options with an explicit batch size. A size of `0` is raised to `1`.
    pub fn with_batch_size(batch_size: usize) -> Self {
        Self {
            batch_size: batch_size.max(1),
        }
    }
}

impl Layer<'_> {
    /// Read this layer as a stream of Arrow [`RecordBatch`]es.
    ///
    /// Attribute columns follow the mapping in the [module documentation](self);
    /// the geometry column is WKB carrying the `geoarrow.wkb` extension name.
    ///
    /// This does not go through [`crate::Feature`] or [`crate::Value`]. Arrow
    /// arrays are built straight from the statement's column values, which is
    /// the whole point of the path: GDAL measured its generic implementation,
    /// which does route through a per-row feature object, as *slower* than the
    /// row API it wraps (roadmap 05-m3).
    ///
    /// # Consistency
    ///
    /// Each batch is a separate query, paginated on the primary key, so a
    /// concurrent writer can change the table between batches. Wrap the read in
    /// your own transaction on [`crate::GeoPackage::connection`] if you need a
    /// stable snapshot across the whole layer (design decision D9). This shape
    /// is what lets batches be fetched by key range, which the parallel read
    /// path needs.
    ///
    /// # Errors
    ///
    /// [`Error`] if the schema cannot be introspected or the query cannot be
    /// prepared. Per-batch failures surface through the iterator.
    pub fn read_arrow(&self, options: ArrowReadOptions) -> Result<ArrowBatches<'_>> {
        let schema = self.arrow_schema()?;
        // The pagination key: the declared primary key, or SQLite's rowid for a
        // table that has none. This is the same fallback the write path uses.
        let key = match self.primary_key_column() {
            Some(pk) => quote(pk)?,
            None => "rowid".to_owned(),
        };
        // The pagination key has to be selected, but it is usually a column of
        // the table as well, in which case selecting it twice would cost a
        // whole extra value fetch per row. Measured over 200k rows of 11
        // columns, per-value fetching is about half the read's total time, so a
        // twelfth column is not free.
        let key_field = self.primary_key_column().and_then(|pk| {
            schema
                .fields()
                .iter()
                .position(|field| *field.name() == *pk)
        });
        let mut selected = String::new();
        for field in schema.fields() {
            if !selected.is_empty() {
                selected.push(',');
            }
            selected.push_str(&quote(field.name())?);
        }
        // When the key is not one of the fields it is selected ahead of them, so
        // the reader can paginate, under an alias the aggregate can name.
        let (row_columns, aggregate_arguments) = match key_field {
            Some(_) => (selected.clone(), selected),
            None => (
                format!("{key} AS \"{KEY_ALIAS}\",{selected}"),
                format!("\"{KEY_ALIAS}\",{selected}"),
            ),
        };
        let table = quote(self.table_name())?;
        let rows_sql =
            format!("SELECT {row_columns} FROM {table} WHERE {key} >= ?1 ORDER BY {key} LIMIT ?2");
        let sql = rows_sql.clone();
        let geometry_index = self.geometry_column().and_then(|geom| {
            schema
                .fields()
                .iter()
                .position(|field| *field.name() == geom.column_name)
        });
        let conn = self.gpkg().connection();
        let datetime = self.conversion_options().datetime;
        let names: Vec<String> = schema
            .fields()
            .iter()
            .map(|field| field.name().clone())
            .collect();

        // The aggregate path needs one function argument per selected column.
        // A table wider than SQLite's function-argument limit cannot use it, so
        // it falls back to the direct loop rather than failing. GDAL splits the
        // call across several aggregates instead; the fallback costs less to
        // maintain and the case is rare.
        let arg_count =
            i32::try_from(names.len() + usize::from(key_field.is_none())).unwrap_or(i32::MAX);
        let aggregate = if arg_count <= conn.limit(Limit::SQLITE_LIMIT_FUNCTION_ARG)? {
            Some(AggregateState::register(
                conn,
                arg_count,
                BatchFiller {
                    names: names.clone(),
                    types: schema
                        .fields()
                        .iter()
                        .map(|field| field.data_type().clone())
                        .collect(),
                    key_argument: key_field.unwrap_or(0),
                    field_offset: usize::from(key_field.is_none()),
                    geometry_index,
                    datetime,
                    capacity: options.batch_size.clamp(1, DEFAULT_BATCH_SIZE),
                    output: Arc::new(Mutex::new(None)),
                    failure: Arc::new(Mutex::new(None)),
                },
            )?)
        } else {
            None
        };

        // The batch has to be bounded by the inner query. An aggregate collapses
        // its input to a single result row, so a LIMIT beside it would bound the
        // aggregate's own output at one row and scan the whole table underneath.
        // GDAL avoids this by slicing with `BETWEEN` on a dense key instead;
        // wrapping the paginated query keeps this path working on any key.
        let aggregate_sql = aggregate
            .as_ref()
            .map(|state| {
                format!(
                    "SELECT {}({aggregate_arguments}) FROM ({rows_sql})",
                    state.name
                )
            })
            .unwrap_or_default();

        Ok(ArrowBatches {
            conn,
            schema,
            sql,
            aggregate_sql,
            key_field,
            geometry_index,
            names,
            datetime,
            batch_size: options.batch_size.max(1),
            next_key: i64::MIN,
            exhausted: false,
            aggregate,
        })
    }
}

/// A registered aggregate function and the slot its finaliser leaves the
/// finished builders in.
struct AggregateState {
    name: String,
    arg_count: i32,
    output: Arc<Mutex<Option<FilledBatch>>>,
    failure: Arc<Mutex<Option<Error>>>,
}

impl AggregateState {
    /// Register the function under a name unique to this reader.
    ///
    /// Unique because two readers can share a connection, and a shared name
    /// would have the second overwrite the first's function and the first's
    /// drop remove the second's.
    fn register(conn: &Connection, arg_count: i32, filler: BatchFiller) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            "geopackage_fill_arrow_{}",
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let output = Arc::clone(&filler.output);
        let failure = Arc::clone(&filler.failure);
        conn.create_aggregate_function(
            name.as_str(),
            arg_count,
            FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
            filler,
        )?;
        Ok(Self {
            name,
            arg_count,
            output,
            failure,
        })
    }
}

/// The aggregate that fills one batch, registered as a SQL function.
///
/// This is the technique GDAL's GeoPackage driver uses, and the reason for it is
/// measured rather than assumed: fetching every value of every row costs a tenth
/// as much through an aggregate as through the row loop, because the loop stays
/// inside SQLite instead of returning into this crate once per row (see
/// `roadmap/benchmarks/2026-07-25-gdal-arrow-comparison.md`).
///
/// The builders live in the accumulator rather than here, so appending a row
/// needs no synchronisation at all; [`Aggregate::finalize`] moves them into
/// `output` once per batch, which is the only time the lock is taken.
struct BatchFiller {
    names: Vec<String>,
    types: Vec<DataType>,
    /// Which argument carries the pagination key.
    key_argument: usize,
    /// Where the schema fields start among the arguments: 1 when the key had to
    /// be selected separately, 0 when it is one of the fields.
    field_offset: usize,
    geometry_index: Option<usize>,
    datetime: DateTimeParsing,
    capacity: usize,
    output: Arc<Mutex<Option<FilledBatch>>>,
    /// The first append failure, kept so a typed error survives instead of
    /// becoming a bare SQL error.
    ///
    /// Beside the accumulator rather than inside it, because the accumulator
    /// must be `UnwindSafe` and [`Error`] is not: it boxes a `dyn Error` whose
    /// interior mutability the compiler cannot rule out. Failures are rare, so
    /// taking a lock for one costs nothing that matters.
    failure: Arc<Mutex<Option<Error>>>,
}

/// One batch under construction, the aggregate's accumulator.
struct FilledBatch {
    builders: Vec<ColumnBuilder>,
    rows: usize,
    last_key: Option<i64>,
}

impl Aggregate<FilledBatch, i64> for BatchFiller {
    fn init(&self, _: &mut Context<'_>) -> rusqlite::Result<FilledBatch> {
        let mut builders = Vec::with_capacity(self.types.len());
        for (index, data_type) in self.types.iter().enumerate() {
            let is_geometry = Some(index) == self.geometry_index;
            builders.push(
                ColumnBuilder::new(data_type, is_geometry, self.capacity)
                    .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?,
            );
        }
        Ok(FilledBatch {
            builders,
            rows: 0,
            last_key: None,
        })
    }

    fn step(&self, ctx: &mut Context<'_>, acc: &mut FilledBatch) -> rusqlite::Result<()> {
        if let ValueRef::Integer(key) = ctx.get_raw(self.key_argument) {
            acc.last_key = Some(key);
        }
        for (index, builder) in acc.builders.iter_mut().enumerate() {
            let name = self.names.get(index).map_or("", String::as_str);
            let value = ctx.get_raw(index + self.field_offset);
            if let Err(error) = builder.append(name, value, self.datetime) {
                if let Ok(mut slot) = self.failure.lock() {
                    *slot = Some(error);
                }
                // Stop the scan. The finaliser still runs, so the typed error
                // above is what the caller sees rather than this one.
                return Err(rusqlite::Error::UserFunctionError(
                    "geopackage: columnar read failed".into(),
                ));
            }
        }
        acc.rows += 1;
        Ok(())
    }

    fn finalize(&self, _: &mut Context<'_>, acc: Option<FilledBatch>) -> rusqlite::Result<i64> {
        let rows = acc.as_ref().map_or(0, |batch| batch.rows);
        if let Some(batch) = acc
            && let Ok(mut slot) = self.output.lock()
        {
            *slot = Some(batch);
        }
        Ok(i64::try_from(rows).unwrap_or(i64::MAX))
    }
}

/// A stream of Arrow [`RecordBatch`]es over one layer, from
/// [`Layer::read_arrow`].
///
/// Implements [`RecordBatchReader`], so it can be handed to anything in the
/// Arrow ecosystem that consumes one.
pub struct ArrowBatches<'a> {
    conn: &'a Connection,
    schema: SchemaRef,
    /// The direct-loop query, selecting the columns as ordinary result columns.
    sql: String,
    /// The aggregate-path query, wrapping the same columns in the registered
    /// function. Built on first use.
    aggregate_sql: String,
    /// Index of the field that is also the pagination key, when it is one of
    /// them. `None` means the key is selected as an extra leading column.
    key_field: Option<usize>,
    /// Index of the geometry field, whose values need the GPB header stripped.
    geometry_index: Option<usize>,
    names: Vec<String>,
    datetime: DateTimeParsing,
    batch_size: usize,
    /// Rows with a key at or above this are still to be read.
    next_key: i64,
    exhausted: bool,
    /// The aggregate function, when this reader uses it. `None` falls back to
    /// the direct loop.
    aggregate: Option<AggregateState>,
}

impl Drop for ArrowBatches<'_> {
    fn drop(&mut self) {
        if let Some(state) = &self.aggregate {
            // Best effort: the reader is going away either way, and a failure
            // here would only leave an unused function registered on the
            // connection under a name nothing else uses.
            drop(
                self.conn
                    .remove_function(state.name.as_str(), state.arg_count),
            );
        }
    }
}

impl ArrowBatches<'_> {
    /// Read one batch, or `None` once the layer is exhausted.
    fn next_batch(&mut self) -> Result<Option<RecordBatch>> {
        if self.aggregate.is_some() {
            return self.next_batch_aggregate();
        }
        self.next_batch_direct()
    }

    /// The aggregate path: one function call per row, inside SQLite's own loop.
    fn next_batch_aggregate(&mut self) -> Result<Option<RecordBatch>> {
        let queried = self.conn.query_row(
            &self.aggregate_sql,
            rusqlite::params![
                self.next_key,
                i64::try_from(self.batch_size).unwrap_or(i64::MAX)
            ],
            |row| row.get::<_, i64>(0),
        );
        // A failed append stops the scan and stores the real reason; the error
        // the query returns is only the signal that it stopped.
        if let Some(state) = &self.aggregate
            && let Ok(mut slot) = state.failure.lock()
            && let Some(error) = slot.take()
        {
            self.exhausted = true;
            return Err(error);
        }
        let rows_read = queried?;

        let filled = self
            .aggregate
            .as_ref()
            .and_then(|state| state.output.lock().ok().and_then(|mut slot| slot.take()));
        let Some(filled) = filled else {
            self.exhausted = true;
            return Ok(None);
        };

        let rows_read = usize::try_from(rows_read).unwrap_or(0);
        if rows_read == 0 {
            self.exhausted = true;
            return Ok(None);
        }
        self.advance(filled.last_key, rows_read);

        let arrays: Vec<ArrayRef> = filled
            .builders
            .into_iter()
            .map(ColumnBuilder::finish)
            .collect();
        Ok(Some(RecordBatch::try_new(
            Arc::clone(&self.schema),
            arrays,
        )?))
    }

    /// Record where the next batch starts, and whether there can be one.
    fn advance(&mut self, last_key: Option<i64>, rows_read: usize) {
        match last_key.and_then(|key| key.checked_add(1)) {
            Some(next) => self.next_key = next,
            // The key space is exhausted at i64::MAX; there can be no next row.
            None => self.exhausted = true,
        }
        if rows_read < self.batch_size {
            self.exhausted = true;
        }
    }

    /// The direct path: step the rows and fetch each value. Kept as the
    /// fallback for a table too wide for the aggregate's argument list.
    fn next_batch_direct(&mut self) -> Result<Option<RecordBatch>> {
        // Pre-size the arrays so a batch does not spend its time growing them.
        // Capped rather than taken from `batch_size` directly, so an enormous
        // batch size does not reserve enormous buffers for a small table.
        let capacity = self.batch_size.min(DEFAULT_BATCH_SIZE);
        let mut builders: Vec<ColumnBuilder> = self
            .schema
            .fields()
            .iter()
            .enumerate()
            .map(|(index, field)| {
                ColumnBuilder::new(
                    field.data_type(),
                    Some(index) == self.geometry_index,
                    capacity,
                )
            })
            .collect::<Result<_>>()?;

        let mut rows_read = 0usize;
        let mut last_key = None;
        {
            let mut stmt = self.conn.prepare_cached(&self.sql)?;
            let mut rows = stmt.query(rusqlite::params![
                self.next_key,
                i64::try_from(self.batch_size).unwrap_or(i64::MAX)
            ])?;
            // Where the fields start: at 0 when the key is one of them, at 1
            // when it had to be selected separately.
            let offset = usize::from(self.key_field.is_none());
            while let Some(row) = rows.next()? {
                last_key = Some(row.get::<_, i64>(self.key_field.unwrap_or(0))?);
                for (index, builder) in builders.iter_mut().enumerate() {
                    let name = self.names.get(index).map_or("", String::as_str);
                    builder.append(name, row.get_ref(index + offset)?, self.datetime)?;
                }
                rows_read += 1;
            }
        }

        if rows_read == 0 {
            self.exhausted = true;
            return Ok(None);
        }
        self.advance(last_key, rows_read);

        let arrays: Vec<ArrayRef> = builders.into_iter().map(ColumnBuilder::finish).collect();
        Ok(Some(RecordBatch::try_new(
            Arc::clone(&self.schema),
            arrays,
        )?))
    }
}

impl Iterator for ArrowBatches<'_> {
    type Item = std::result::Result<RecordBatch, ArrowError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.exhausted {
            return None;
        }
        match self.next_batch() {
            Ok(Some(batch)) => Some(Ok(batch)),
            Ok(None) => None,
            Err(error) => {
                // A failed batch ends the stream: retrying would re-run the
                // same query against the same state.
                self.exhausted = true;
                Some(Err(ArrowError::ExternalError(Box::new(error))))
            }
        }
    }
}

impl RecordBatchReader for ArrowBatches<'_> {
    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }
}

/// One Arrow array under construction, with the append logic for the SQLite
/// storage classes that can legitimately reach it.
///
/// The variants mirror the types [`Layer::arrow_schema`] can produce. Geometry
/// is its own variant because its bytes need the GPB header removed first.
enum ColumnBuilder {
    Boolean(BooleanBuilder),
    Int64(Int64Builder),
    Float64(Float64Builder),
    Utf8(StringBuilder),
    Binary(BinaryBuilder),
    Date32(Date32Builder),
    Timestamp(TimestampMicrosecondBuilder),
    Geometry(BinaryBuilder),
}

impl ColumnBuilder {
    /// The builder for one field of the schema, sized for `capacity` rows.
    ///
    /// The byte estimates for the variable-width types only decide the first
    /// allocation; a longer value grows the buffer as usual.
    fn new(data_type: &DataType, is_geometry: bool, capacity: usize) -> Result<Self> {
        /// Assumed bytes per WKB geometry, enough for a point or a short line.
        const GEOMETRY_BYTES: usize = 64;
        /// Assumed bytes per text or blob value.
        const VALUE_BYTES: usize = 16;

        if is_geometry {
            return Ok(Self::Geometry(BinaryBuilder::with_capacity(
                capacity,
                capacity * GEOMETRY_BYTES,
            )));
        }
        Ok(match data_type {
            DataType::Boolean => Self::Boolean(BooleanBuilder::with_capacity(capacity)),
            DataType::Int64 => Self::Int64(Int64Builder::with_capacity(capacity)),
            DataType::Float64 => Self::Float64(Float64Builder::with_capacity(capacity)),
            DataType::Utf8 => Self::Utf8(StringBuilder::with_capacity(
                capacity,
                capacity * VALUE_BYTES,
            )),
            DataType::Binary => Self::Binary(BinaryBuilder::with_capacity(
                capacity,
                capacity * VALUE_BYTES,
            )),
            DataType::Date32 => Self::Date32(Date32Builder::with_capacity(capacity)),
            DataType::Timestamp(_, _) => {
                Self::Timestamp(TimestampMicrosecondBuilder::with_capacity(capacity))
            }
            // `arrow_schema` produces nothing else, so this is unreachable in
            // practice and is an error rather than a panic if that changes.
            other => {
                return Err(Error::UnsupportedArrowType {
                    data_type: other.to_string(),
                });
            }
        })
    }

    /// Append one stored value. `column` names the field, for diagnostics.
    fn append(
        &mut self,
        column: &str,
        value: ValueRef<'_>,
        datetime: DateTimeParsing,
    ) -> Result<()> {
        if let ValueRef::Null = value {
            self.append_null();
            return Ok(());
        }
        match (self, value) {
            // A BOOLEAN column can hold any integer, since SQLite gives the
            // declared type no affinity. Non-zero is `true`, as on the scalar
            // path's default (see `StorageStrictness`).
            (Self::Boolean(builder), ValueRef::Integer(int)) => builder.append_value(int != 0),
            (Self::Int64(builder), ValueRef::Integer(int)) => builder.append_value(int),
            (Self::Float64(builder), ValueRef::Real(real)) => builder.append_value(real),
            // An integer in a real column widens losslessly, as on the scalar
            // path.
            (Self::Float64(builder), ValueRef::Integer(int)) => builder.append_value(int as f64),
            (Self::Utf8(builder), ValueRef::Text(bytes)) => builder.append_value(text(bytes)?),
            (Self::Binary(builder), ValueRef::Blob(bytes)) => builder.append_value(bytes),
            (Self::Date32(builder), ValueRef::Text(bytes)) => {
                let text = text(bytes)?;
                let date = Date::parse(text).map_err(|source| Error::InvalidDateTimeValue {
                    column: column.to_owned(),
                    text: text.to_owned(),
                    source,
                })?;
                builder.append_value(days_since_epoch(date));
            }
            (Self::Timestamp(builder), ValueRef::Text(bytes)) => {
                let text = text(bytes)?;
                let parsed = match datetime {
                    DateTimeParsing::Strict => DateTime::parse_strict(text),
                    DateTimeParsing::Lenient => DateTime::parse_lenient(text),
                };
                let stamp = parsed.map_err(|source| Error::InvalidDateTimeValue {
                    column: column.to_owned(),
                    text: text.to_owned(),
                    source,
                })?;
                builder.append_value(micros_since_epoch(stamp));
            }
            // The GPB body is already ISO WKB, so the geometry costs a header
            // read and a copy, with no parsing of the geometry itself.
            (Self::Geometry(builder), ValueRef::Blob(blob)) => {
                let (_, offset) = gpb::parse_header(blob).map_err(geopackage_core::Error::from)?;
                builder.append_value(blob.get(offset..).unwrap_or_default());
            }
            (builder, other) => {
                return Err(Error::ArrowValueMismatch {
                    column: column.to_owned(),
                    expected: builder.type_name(),
                    found: storage_class(other),
                });
            }
        }
        Ok(())
    }

    /// Append a NULL to whichever array this is.
    fn append_null(&mut self) {
        match self {
            Self::Boolean(builder) => builder.append_null(),
            Self::Int64(builder) => builder.append_null(),
            Self::Float64(builder) => builder.append_null(),
            Self::Utf8(builder) => builder.append_null(),
            Self::Binary(builder) | Self::Geometry(builder) => builder.append_null(),
            Self::Date32(builder) => builder.append_null(),
            Self::Timestamp(builder) => builder.append_null(),
        }
    }

    /// The Arrow type name, for diagnostics.
    fn type_name(&self) -> &'static str {
        match self {
            Self::Boolean(_) => "Boolean",
            Self::Int64(_) => "Int64",
            Self::Float64(_) => "Float64",
            Self::Utf8(_) => "Utf8",
            Self::Binary(_) => "Binary",
            Self::Date32(_) => "Date32",
            Self::Timestamp(_) => "Timestamp",
            Self::Geometry(_) => "Binary (geoarrow.wkb)",
        }
    }

    /// Finish the array.
    fn finish(mut self) -> ArrayRef {
        match &mut self {
            Self::Boolean(builder) => Arc::new(builder.finish()),
            Self::Int64(builder) => Arc::new(builder.finish()),
            Self::Float64(builder) => Arc::new(builder.finish()),
            Self::Utf8(builder) => Arc::new(builder.finish()),
            Self::Binary(builder) | Self::Geometry(builder) => Arc::new(builder.finish()),
            Self::Date32(builder) => Arc::new(builder.finish()),
            Self::Timestamp(builder) => Arc::new(builder.finish().with_timezone(std::sync::Arc::<
                str,
            >::from(
                "UTC"
            ))),
        }
    }
}

/// Decode SQLite TEXT bytes as UTF-8.
fn text(bytes: &[u8]) -> Result<&str> {
    Ok(std::str::from_utf8(bytes).map_err(rusqlite::Error::from)?)
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

/// Days from the Unix epoch to a civil date, which is what `Date32` holds.
///
/// Howard Hinnant's `days_from_civil`, which is exact over the proleptic
/// Gregorian calendar and needs no lookup tables. Dates before 1970 give a
/// negative count, which `Date32` represents.
///
/// This and [`micros_since_epoch`] are an interim arrangement: calendar
/// arithmetic is not something this crate should be maintaining, and issue #24
/// tracks deferring it to `jiff` along with the rest of the datetime handling.
fn days_since_epoch(date: Date) -> i32 {
    let year = i64::from(date.year());
    let month = i64::from(date.month());
    let day = i64::from(date.day());

    // January and February are counted as months 13 and 14 of the previous
    // year, which puts the leap day at the end of the year and removes the
    // special case from the arithmetic below.
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    // 146097 days per 400-year era; 719468 shifts the epoch from 0000-03-01 to
    // 1970-01-01.
    let days = era * 146_097 + day_of_era - 719_468;
    i32::try_from(days).unwrap_or(i32::MAX)
}

/// Microseconds from the Unix epoch to an instant, which is what the
/// `Timestamp(Microsecond, "UTC")` columns hold.
///
/// A datetime carrying a UTC offset is normalised to UTC. Text with no zone
/// designator at all reaches here only under lenient parsing, and is read as
/// UTC, which is what the spec says a `DATETIME` column holds.
fn micros_since_epoch(stamp: DateTime) -> i64 {
    const MICROS_PER_SECOND: i64 = 1_000_000;
    let days = i64::from(days_since_epoch(stamp.date));
    let seconds_of_day =
        i64::from(stamp.hour) * 3600 + i64::from(stamp.minute) * 60 + i64::from(stamp.second);
    let offset_seconds = i64::from(stamp.offset_minutes.unwrap_or(0)) * 60;
    let seconds = days * 86_400 + seconds_of_day - offset_seconds;
    seconds * MICROS_PER_SECOND + i64::from(stamp.nanosecond / 1_000)
}
