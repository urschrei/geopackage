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
use arrow_array::cast::AsArray;
use arrow_array::{Array, ArrayRef, RecordBatch, RecordBatchReader};
use arrow_schema::{ArrowError, DataType, Field, Schema, SchemaRef, TimeUnit};
use geopackage_core::datetime::{Date, DateTime};
use geopackage_core::gpb;
use geopackage_core::ident::quote;
use geopackage_core::types::{ColumnType, GeometryType};
use rusqlite::Connection;
use rusqlite::functions::{Aggregate, Context, FunctionFlags};
use rusqlite::limits::Limit;
use rusqlite::types::ValueRef;

use crate::schema::{Column, GeometryColumn};
use crate::value::DateTimeParsing;
use crate::{Error, Layer, Result, Value};

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

/// Ceiling on the automatically chosen thread count.
///
/// GDAL's driver uses the same `min(4, cpus)` for the same path. Beyond a few
/// readers the work is bounded by how fast SQLite can pull pages rather than by
/// cores.
const DEFAULT_MAX_THREADS: usize = 4;

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
    /// How many threads may read at once. `0` chooses
    /// `min(4, available parallelism)`, matching GDAL's default for the same
    /// path; `1` reads on the calling thread.
    ///
    /// More than one thread is only possible under the conditions in
    /// [`Layer::read_arrow`]; when they do not hold, the read is single-threaded
    /// whatever this says.
    pub threads: usize,
}

impl Default for ArrowReadOptions {
    fn default() -> Self {
        Self {
            batch_size: DEFAULT_BATCH_SIZE,
            threads: 0,
        }
    }
}

impl ArrowReadOptions {
    /// Options with an explicit batch size. A size of `0` is raised to `1`.
    pub fn with_batch_size(batch_size: usize) -> Self {
        Self {
            batch_size: batch_size.max(1),
            ..Self::default()
        }
    }

    /// Set the thread count. `0` chooses a default, `1` reads on the calling
    /// thread.
    #[must_use]
    pub fn with_threads(mut self, threads: usize) -> Self {
        self.threads = threads;
        self
    }

    /// The thread count to actually use, resolving `0` to the default.
    fn resolved_threads(self) -> usize {
        if self.threads > 0 {
            return self.threads;
        }
        std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1)
            .min(DEFAULT_MAX_THREADS)
    }
}

impl Layer<'_> {
    /// Read this layer as a stream of Arrow [`RecordBatch`]es.
    ///
    /// Reads on several threads where it can, which is the common case and the
    /// default; see [Threading](#threading) below for the conditions and for how
    /// to ask for a single thread instead.
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
    /// # Threading
    ///
    /// `options.threads` defaults to `min(4, available parallelism)`. Set it to
    /// `1` for a read that touches no thread but the caller's.
    ///
    /// Threads are used only when all of the following hold, and the read is
    /// otherwise single-threaded rather than failing:
    ///
    /// - **The database is a file.** Workers read through their own
    ///   connections, and a `:memory:` database is private to the connection
    ///   that created it.
    /// - **The primary key is dense**, with no gaps between its smallest and
    ///   largest value. Workers are handed key ranges before any row is read, so
    ///   a range has to imply a known row count. GDAL's driver requires the
    ///   same, in the stricter form of a key starting at 1.
    /// - **There is more than one batch of rows.** Below that, opening
    ///   connections and starting threads costs more than it saves.
    ///
    /// Workers open their connections **read-only**, which is what makes it safe
    /// for several connections to read one table without agreeing on a snapshot:
    /// there is no writer to race.
    ///
    /// Batches arrive in primary-key order regardless of thread count.
    ///
    /// Dropping the reader before it is drained stops the workers, but waits for
    /// each to finish the batch it is on, so the drop can block for as long as
    /// one batch takes to read.
    ///
    /// # Consistency
    ///
    /// Each batch is a separate query, paginated on the primary key, so a
    /// concurrent writer can change the table between batches. Wrap the read in
    /// your own transaction on [`crate::GeoPackage::connection`] if you need a
    /// stable snapshot across the whole layer (design decision D9). This shape
    /// is what lets batches be fetched by key range, which the threaded path
    /// needs.
    ///
    /// # Errors
    ///
    /// [`Error`] if the schema cannot be introspected or the query cannot be
    /// prepared, or if a worker connection cannot be opened. Per-batch failures
    /// surface through the iterator.
    pub fn read_arrow(&self, options: ArrowReadOptions) -> Result<ArrowBatches<'_>> {
        let sequential = self.read_arrow_sequential(options)?;
        if options.resolved_threads() < 2 {
            return Ok(sequential);
        }
        let Some(parallel) = self.parallel_source(options)? else {
            return Ok(sequential);
        };
        Ok(ArrowBatches {
            schema: sequential.schema,
            source: BatchSource::Parallel(parallel),
        })
    }

    /// The single-threaded reader, which the threaded one falls back to and its
    /// workers are built from.
    fn read_arrow_sequential(&self, options: ArrowReadOptions) -> Result<ArrowBatches<'_>> {
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
            schema: Arc::clone(&schema),
            source: BatchSource::Sequential(SequentialBatches {
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
            }),
        })
    }

    /// The worker pool for this read, or `None` when the conditions in
    /// [`Self::read_arrow`] do not hold.
    fn parallel_source(&self, options: ArrowReadOptions) -> Result<Option<ParallelBatches>> {
        let Some(path) = database_path(self.gpkg().connection())? else {
            return Ok(None);
        };
        let Some(key) = self.primary_key_column() else {
            return Ok(None);
        };
        let Some(span) = dense_key_span(self.gpkg().connection(), self.table_name(), key)? else {
            return Ok(None);
        };
        let batch_size = options.batch_size.max(1);
        // Below two batches there is nothing to overlap, and opening connections
        // and starting threads would cost more than it saves.
        let rows = span.1.saturating_sub(span.0).saturating_add(1);
        if rows < i64::try_from(batch_size.saturating_mul(2)).unwrap_or(i64::MAX) {
            return Ok(None);
        }
        Ok(Some(ParallelBatches::spawn(
            path,
            self.table_name().to_owned(),
            self.conversion_options(),
            span,
            batch_size,
            options.resolved_threads(),
        )))
    }
}

/// The file backing a connection's `main` database, or `None` for one with no
/// file: a `:memory:` or temporary database, which no other connection can open.
fn database_path(conn: &Connection) -> Result<Option<std::path::PathBuf>> {
    let file: String = conn.query_row(
        "SELECT file FROM pragma_database_list WHERE name = 'main'",
        [],
        |row| row.get(0),
    )?;
    if file.is_empty() {
        return Ok(None);
    }
    Ok(Some(std::path::PathBuf::from(file)))
}

/// The `(first, last)` primary key of `table` when the key has no gaps, or
/// `None` when it has gaps or the table is empty.
///
/// Density is what lets a worker be handed a key range before any row is read
/// and know how many rows it covers. Testing `max - min + 1 == count` is a
/// slightly wider rule than GDAL's `min == 1 && max == count`, and costs the
/// same single scan.
fn dense_key_span(conn: &Connection, table: &str, key: &str) -> Result<Option<(i64, i64)>> {
    let sql = format!(
        "SELECT min({key}), max({key}), count(*) FROM {table}",
        key = quote(key)?,
        table = quote(table)?
    );
    let (min, max, count): (Option<i64>, Option<i64>, i64) =
        conn.query_row(&sql, [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
    let (Some(min), Some(max)) = (min, max) else {
        return Ok(None);
    };
    let span = max.checked_sub(min).and_then(|d| d.checked_add(1));
    if span != Some(count) {
        return Ok(None);
    }
    Ok(Some((min, max)))
}

/// One layer's batches read by a pool of worker threads.
///
/// Worker `w` of `n` reads batches `w`, `w + n`, `w + 2n`, and so on, and the
/// consumer takes from the workers in the same rotation. Batches therefore
/// arrive in key order without any reordering buffer, and each worker's channel
/// holds one batch, so the memory in flight is bounded by the thread count.
struct ParallelBatches {
    /// One receiver per worker, drained in rotation.
    receivers: Vec<std::sync::mpsc::Receiver<std::result::Result<RecordBatch, ArrowError>>>,
    workers: Vec<std::thread::JoinHandle<()>>,
    /// Which worker to take from next.
    turn: usize,
    done: bool,
}

impl ParallelBatches {
    fn spawn(
        path: std::path::PathBuf,
        table: String,
        conversion: crate::ConversionOptions,
        (first, last): (i64, i64),
        batch_size: usize,
        threads: usize,
    ) -> Self {
        let mut receivers = Vec::with_capacity(threads);
        let mut workers = Vec::with_capacity(threads);
        for worker in 0..threads {
            // A capacity of one keeps a worker at most one batch ahead of the
            // consumer, which is what bounds the memory in flight.
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            let path = path.clone();
            let table = table.clone();
            let handle = std::thread::spawn(move || {
                run_worker(
                    &path, &table, conversion, first, last, batch_size, threads, worker, &tx,
                );
            });
            receivers.push(rx);
            workers.push(handle);
        }
        Self {
            receivers,
            workers,
            turn: 0,
            done: false,
        }
    }
}

impl Iterator for ParallelBatches {
    type Item = std::result::Result<RecordBatch, ArrowError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let received = self.receivers.get(self.turn)?.recv();
        self.turn = (self.turn + 1) % self.receivers.len().max(1);
        match received {
            Ok(batch) => {
                if batch.is_err() {
                    self.done = true;
                }
                Some(batch)
            }
            // The worker whose turn it is has finished, and workers are assigned
            // batches in rotation, so every later worker has finished too.
            Err(_) => {
                self.done = true;
                None
            }
        }
    }
}

impl Drop for ParallelBatches {
    fn drop(&mut self) {
        // Dropping the receivers makes each worker's next send fail, which is
        // how a consumer that stops early tells the pool to stop.
        self.receivers.clear();
        for worker in self.workers.drain(..) {
            drop(worker.join());
        }
    }
}

/// One worker: open a read-only connection of its own and read the batches
/// assigned to it, in order, until the layer runs out or the consumer goes away.
#[expect(
    clippy::too_many_arguments,
    reason = "a worker's whole context, passed once at spawn; a struct would be used by this call site alone"
)]
fn run_worker(
    path: &std::path::Path,
    table: &str,
    conversion: crate::ConversionOptions,
    first: i64,
    last: i64,
    batch_size: usize,
    threads: usize,
    worker: usize,
    tx: &std::sync::mpsc::SyncSender<std::result::Result<RecordBatch, ArrowError>>,
) {
    let send_error = |error: Error| {
        drop(tx.send(Err(ArrowError::ExternalError(Box::new(error)))));
    };
    let gpkg = match crate::GeoPackage::open_read_only(path) {
        Ok(gpkg) => gpkg,
        Err(error) => return send_error(error),
    };
    let layer = match gpkg.layer(table) {
        Ok(layer) => layer.with_conversion_options(conversion),
        Err(error) => return send_error(error),
    };
    // Threads of its own would recurse; this reader is the thread.
    let options = ArrowReadOptions::with_batch_size(batch_size).with_threads(1);
    let mut batches = match layer.read_arrow(options) {
        Ok(batches) => batches,
        Err(error) => return send_error(error),
    };
    let BatchSource::Sequential(source) = &mut batches.source else {
        return;
    };

    let stride = match i64::try_from(batch_size.saturating_mul(threads)) {
        Ok(stride) if stride > 0 => stride,
        _ => return,
    };
    let start = match i64::try_from(batch_size.saturating_mul(worker)) {
        Ok(offset) => match first.checked_add(offset) {
            Some(start) => start,
            None => return,
        },
        Err(_) => return,
    };

    let mut key = start;
    while key <= last {
        match source.read_batch_at(key) {
            Ok(Some(batch)) => {
                if tx.send(Ok(batch)).is_err() {
                    return; // the consumer stopped
                }
            }
            Ok(None) => return,
            Err(error) => return send_error(error),
        }
        match key.checked_add(stride) {
            Some(next) => key = next,
            None => return,
        }
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
            let value = ctx.get_raw(index + self.field_offset);
            if let Err(error) = builder.append(&self.names, index, value, self.datetime) {
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

/// One layer's batches read on the calling thread.
struct SequentialBatches<'a> {
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

impl Drop for SequentialBatches<'_> {
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

impl SequentialBatches<'_> {
    /// Read the batch starting at `key`, ignoring where the reader had got to.
    ///
    /// Used by the parallel path, whose workers each read whole batches at
    /// offsets assigned to them rather than walking the layer in sequence.
    fn read_batch_at(&mut self, key: i64) -> Result<Option<RecordBatch>> {
        self.next_key = key;
        self.exhausted = false;
        self.next_batch()
    }

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
                    builder.append(
                        &self.names,
                        index,
                        row.get_ref(index + offset)?,
                        self.datetime,
                    )?;
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

impl Iterator for SequentialBatches<'_> {
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

/// A stream of Arrow [`RecordBatch`]es over one layer, from
/// [`Layer::read_arrow`].
///
/// Implements [`RecordBatchReader`], so it can be handed to anything in the
/// Arrow ecosystem that consumes one. Batches arrive in primary-key order
/// whether the read is threaded or not.
pub struct ArrowBatches<'a> {
    schema: SchemaRef,
    source: BatchSource<'a>,
}

enum BatchSource<'a> {
    Sequential(SequentialBatches<'a>),
    Parallel(ParallelBatches),
}

impl Iterator for ArrowBatches<'_> {
    type Item = std::result::Result<RecordBatch, ArrowError>;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.source {
            BatchSource::Sequential(batches) => batches.next(),
            BatchSource::Parallel(batches) => batches.next(),
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

    /// Append one stored value.
    ///
    /// `names` and `index` locate the column for diagnostics, rather than a
    /// resolved `&str`, so the lookup happens only on the failure paths. It is
    /// otherwise a bounds-checked index and a deref per value, and there are
    /// tens of millions of values in a large read.
    fn append(
        &mut self,
        names: &[String],
        index: usize,
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
                    column: column_name(names, index),
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
                    column: column_name(names, index),
                    text: text.to_owned(),
                    source,
                })?;
                builder.append_value(micros_since_epoch(stamp));
            }
            // The GPB body is already ISO WKB, so the geometry costs a header
            // read and a copy, with no parsing of the geometry itself.
            (Self::Geometry(builder), ValueRef::Blob(blob)) => {
                // `body_offset` rather than `parse_header`: the offset follows
                // from the envelope indicator, and parsing the header would
                // decode the envelope's doubles once per row only to discard
                // them.
                let offset = gpb::body_offset(blob).map_err(geopackage_core::Error::from)?;
                builder.append_value(blob.get(offset..).unwrap_or_default());
            }
            (builder, other) => {
                return Err(Error::ArrowValueMismatch {
                    column: column_name(names, index),
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

/// The name of column `index`, for an error message.
fn column_name(names: &[String], index: usize) -> String {
    names.get(index).cloned().unwrap_or_default()
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

/// One row taken from a [`RecordBatch`], ready for the write path.
///
/// Geometry stays as the WKB bytes the batch holds, so the write path can put a
/// header in front of them rather than parsing a geometry and writing it back
/// out. See [`crate::writer::WritableRow`].
struct ArrowRow {
    fid: Option<i64>,
    wkb: Option<Vec<u8>>,
    values: Vec<Value>,
}

/// A row, or the failure that stopped one being made.
///
/// Batches are taken apart lazily, so an error has to travel with the rows
/// rather than out of the side of the iterator. Writing one of these fails the
/// write, which rolls the transaction back exactly as any other write error
/// does.
enum ArrowRowResult {
    Row(ArrowRow),
    Failed(Error),
}

impl crate::writer::WritableRow for ArrowRowResult {
    fn write(self, writer: &mut crate::FeatureWriter<'_>) -> Result<(i64, Option<[f64; 4]>)> {
        match self {
            Self::Row(row) => row.write(writer),
            Self::Failed(error) => Err(error),
        }
    }
}

impl crate::writer::WritableRow for ArrowRow {
    fn write(self, writer: &mut crate::FeatureWriter<'_>) -> Result<(i64, Option<[f64; 4]>)> {
        match &self.wkb {
            Some(wkb) => writer.insert_wkb(self.fid, wkb, &self.values),
            None => writer
                .insert_row(self.fid, &self.values)
                .map(|fid| (fid, None)),
        }
    }
}

impl Layer<'_> {
    /// Write Arrow [`RecordBatch`]es into this layer.
    ///
    /// The columnar counterpart of [`crate::Layer::write_all`], and it shares
    /// that path: batching, the bulk spatial-index decision and the single
    /// transaction all behave identically. What differs is that a geometry
    /// arrives as WKB and stays as WKB, gaining a GPB header rather than being
    /// parsed into a geometry object and serialised again.
    ///
    /// The reader's schema must name columns this layer has. Extra columns in
    /// the batch are an error rather than being ignored, since silently dropping
    /// data a caller asked to write is worse than refusing it. A column of the
    /// layer that the batch does not name is left to its default.
    ///
    /// A [`RecordBatchReader`] does not say how many rows it will produce, which
    /// is the case the bulk index path buffers for rather than trusting a size
    /// hint (issue #17).
    ///
    /// Returns the assigned feature ids, in the order the rows were written.
    ///
    /// # Errors
    ///
    /// [`Error::ArrowValueMismatch`] for a column whose Arrow type does not fit
    /// the layer's declared type, [`Error`] for the write itself, and any error
    /// the reader yields.
    pub fn write_arrow<R>(&self, batches: R, batch_size: usize) -> Result<Vec<i64>>
    where
        R: IntoIterator<Item = std::result::Result<RecordBatch, ArrowError>>,
    {
        self.write_arrow_with(batches, batch_size, crate::BulkIndexOptions::default())
    }

    /// [`Self::write_arrow`] with an explicit [`crate::BulkIndexOptions`].
    ///
    /// # Errors
    ///
    /// As [`Self::write_arrow`].
    pub fn write_arrow_with<R>(
        &self,
        batches: R,
        batch_size: usize,
        options: crate::BulkIndexOptions,
    ) -> Result<Vec<i64>>
    where
        R: IntoIterator<Item = std::result::Result<RecordBatch, ArrowError>>,
    {
        let geometry_column = self.geometry_column().map(|g| g.column_name.clone());
        let value_columns: Vec<String> = self
            .value_columns()
            .iter()
            .filter(|column| Some(column.name.as_str()) != self.primary_key_column())
            .map(|column| column.name.clone())
            .collect();
        let primary_key = self.primary_key_column().map(str::to_owned);

        // One batch is taken apart at a time and the rows are handed on lazily,
        // so peak memory is a batch rather than the whole input, and the write
        // path sees the unsized source it buffers to size up (issue #17).
        // Collecting here would undo both.
        let rows = batches.into_iter().flat_map(move |batch| {
            let taken = batch.map_err(Error::Arrow).and_then(|batch| {
                rows_of(
                    &batch,
                    primary_key.as_deref(),
                    geometry_column.as_deref(),
                    &value_columns,
                )
            });
            match taken {
                Ok(rows) => rows.into_iter().map(ArrowRowResult::Row).collect(),
                // A failure travels as a row of its own, so it reaches the write
                // path and rolls the transaction back rather than needing a
                // second channel out of the iterator.
                Err(error) => vec![ArrowRowResult::Failed(error)],
            }
        });
        self.write_all_impl(rows, batch_size, options, crate::bulk::no_fault)
    }
}

/// Take one batch apart into rows the write path can consume.
fn rows_of(
    batch: &RecordBatch,
    primary_key: Option<&str>,
    geometry: Option<&str>,
    value_columns: &[String],
) -> Result<Vec<ArrowRow>> {
    let schema = batch.schema();
    for field in schema.fields() {
        let known = Some(field.name().as_str()) == primary_key
            || Some(field.name().as_str()) == geometry
            || value_columns.iter().any(|name| name == field.name());
        if !known {
            return Err(Error::NoSuchColumn {
                table_name: String::new(),
                column_name: field.name().clone(),
            });
        }
    }

    let rows = batch.num_rows();
    let mut out = Vec::with_capacity(rows);
    for row in 0..rows {
        let fid = match primary_key.and_then(|name| batch.column_by_name(name)) {
            Some(column) => read_i64(column, row)?,
            None => None,
        };
        let wkb = match geometry.and_then(|name| batch.column_by_name(name)) {
            Some(column) => read_binary(column, row)?,
            None => None,
        };
        let mut values = Vec::with_capacity(value_columns.len());
        for name in value_columns {
            values.push(match batch.column_by_name(name) {
                Some(column) => read_value(column, row, name)?,
                None => Value::Null,
            });
        }
        out.push(ArrowRow { fid, wkb, values });
    }
    Ok(out)
}

/// Read one `Int64` cell, for the feature id.
fn read_i64(column: &ArrayRef, row: usize) -> Result<Option<i64>> {
    if column.is_null(row) {
        return Ok(None);
    }
    let values = column
        .as_primitive_opt::<arrow_array::types::Int64Type>()
        .ok_or_else(|| Error::ArrowValueMismatch {
            column: String::new(),
            expected: "Int64",
            found: "other",
        })?;
    Ok(Some(values.value(row)))
}

/// Read one binary cell, for the geometry.
fn read_binary(column: &ArrayRef, row: usize) -> Result<Option<Vec<u8>>> {
    if column.is_null(row) {
        return Ok(None);
    }
    if let Some(binary) = column.as_binary_opt::<i32>() {
        return Ok(Some(binary.value(row).to_vec()));
    }
    if let Some(binary) = column.as_binary_opt::<i64>() {
        return Ok(Some(binary.value(row).to_vec()));
    }
    Err(Error::ArrowValueMismatch {
        column: String::new(),
        expected: "Binary or LargeBinary",
        found: "other",
    })
}

/// Read one attribute cell as a [`Value`].
///
/// The inverse of the read path's mapping, and deliberately narrower: it accepts
/// the types [`Layer::arrow_schema`] produces, so a round trip works, plus the
/// narrower integer and float widths another producer is likely to emit.
fn read_value(column: &ArrayRef, row: usize, name: &str) -> Result<Value> {
    use arrow_array::types::{
        Date32Type, Float32Type, Float64Type, Int8Type, Int16Type, Int32Type, Int64Type,
        TimestampMicrosecondType, TimestampMillisecondType,
    };

    if column.is_null(row) {
        return Ok(Value::Null);
    }
    let mismatch = |expected: &'static str| Error::ArrowValueMismatch {
        column: name.to_owned(),
        expected,
        found: "an unsupported Arrow type",
    };
    Ok(match column.data_type() {
        DataType::Boolean => Value::Boolean(
            column
                .as_boolean_opt()
                .ok_or_else(|| mismatch("Boolean"))?
                .value(row),
        ),
        DataType::Int8 => Value::Integer(i64::from(
            column
                .as_primitive_opt::<Int8Type>()
                .ok_or_else(|| mismatch("Int8"))?
                .value(row),
        )),
        DataType::Int16 => Value::Integer(i64::from(
            column
                .as_primitive_opt::<Int16Type>()
                .ok_or_else(|| mismatch("Int16"))?
                .value(row),
        )),
        DataType::Int32 => Value::Integer(i64::from(
            column
                .as_primitive_opt::<Int32Type>()
                .ok_or_else(|| mismatch("Int32"))?
                .value(row),
        )),
        DataType::Int64 => Value::Integer(
            column
                .as_primitive_opt::<Int64Type>()
                .ok_or_else(|| mismatch("Int64"))?
                .value(row),
        ),
        DataType::Float32 => Value::Float(f64::from(
            column
                .as_primitive_opt::<Float32Type>()
                .ok_or_else(|| mismatch("Float32"))?
                .value(row),
        )),
        DataType::Float64 => Value::Float(
            column
                .as_primitive_opt::<Float64Type>()
                .ok_or_else(|| mismatch("Float64"))?
                .value(row),
        ),
        DataType::Utf8 => Value::Text(
            column
                .as_string_opt::<i32>()
                .ok_or_else(|| mismatch("Utf8"))?
                .value(row)
                .to_owned(),
        ),
        DataType::LargeUtf8 => Value::Text(
            column
                .as_string_opt::<i64>()
                .ok_or_else(|| mismatch("LargeUtf8"))?
                .value(row)
                .to_owned(),
        ),
        DataType::Binary => Value::Blob(
            column
                .as_binary_opt::<i32>()
                .ok_or_else(|| mismatch("Binary"))?
                .value(row)
                .to_vec(),
        ),
        DataType::LargeBinary => Value::Blob(
            column
                .as_binary_opt::<i64>()
                .ok_or_else(|| mismatch("LargeBinary"))?
                .value(row)
                .to_vec(),
        ),
        DataType::Date32 => Value::Date(date_from_days(
            column
                .as_primitive_opt::<Date32Type>()
                .ok_or_else(|| mismatch("Date32"))?
                .value(row),
        )?),
        DataType::Timestamp(TimeUnit::Microsecond, _) => Value::DateTime(datetime_from_micros(
            column
                .as_primitive_opt::<TimestampMicrosecondType>()
                .ok_or_else(|| mismatch("Timestamp"))?
                .value(row),
        )?),
        DataType::Timestamp(TimeUnit::Millisecond, _) => Value::DateTime(datetime_from_micros(
            column
                .as_primitive_opt::<TimestampMillisecondType>()
                .ok_or_else(|| mismatch("Timestamp"))?
                .value(row)
                .saturating_mul(1_000),
        )?),
        other => {
            return Err(Error::UnsupportedArrowType {
                data_type: other.to_string(),
            });
        }
    })
}

/// The civil date `days` after the Unix epoch: the inverse of
/// [`days_since_epoch`].
///
/// Howard Hinnant's `civil_from_days`, the counterpart of the `days_from_civil`
/// used on the read side. Like it, this is an interim arrangement pending issue
/// #24.
fn date_from_days(days: i32) -> Result<Date> {
    let days = i64::from(days) + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = if month <= 2 { year + 1 } else { year };

    let out_of_range = || Error::UnsupportedArrowType {
        data_type: format!("Date32 value {days} outside the representable range"),
    };
    Date::new(
        u16::try_from(year).ok().ok_or_else(out_of_range)?,
        u8::try_from(month).ok().ok_or_else(out_of_range)?,
        u8::try_from(day).ok().ok_or_else(out_of_range)?,
    )
    .map_err(|source| Error::InvalidDateTimeValue {
        column: String::new(),
        text: format!("Date32 {days}"),
        source,
    })
}

/// The UTC instant `micros` after the Unix epoch: the inverse of
/// [`micros_since_epoch`].
fn datetime_from_micros(micros: i64) -> Result<DateTime> {
    const MICROS_PER_SECOND: i64 = 1_000_000;
    const SECONDS_PER_DAY: i64 = 86_400;

    // Floor division, so instants before the epoch land on the right day rather
    // than rounding toward zero into the next one.
    let seconds = micros.div_euclid(MICROS_PER_SECOND);
    let sub_second = micros.rem_euclid(MICROS_PER_SECOND);
    let days = seconds.div_euclid(SECONDS_PER_DAY);
    let seconds_of_day = seconds.rem_euclid(SECONDS_PER_DAY);

    let days = i32::try_from(days)
        .ok()
        .ok_or_else(|| Error::UnsupportedArrowType {
            data_type: format!("timestamp {micros} outside the representable range"),
        })?;
    let date = date_from_days(days)?;
    Ok(DateTime {
        date,
        hour: u8::try_from(seconds_of_day / 3600).unwrap_or(0),
        minute: u8::try_from((seconds_of_day % 3600) / 60).unwrap_or(0),
        second: u8::try_from(seconds_of_day % 60).unwrap_or(0),
        nanosecond: u32::try_from(sub_second.saturating_mul(1_000)).unwrap_or(0),
        // Always UTC: that is what the column means, and what the read side
        // normalised to when it produced the timestamp.
        offset_minutes: Some(0),
    })
}

impl crate::TableSchemaBuilder {
    /// Derive a layer definition from an Arrow schema.
    ///
    /// The inverse of [`Layer::arrow_schema`], for creating a layer to receive
    /// [`Layer::write_arrow`]. Everything the builder normally takes can still
    /// be overridden afterwards.
    ///
    /// The mapping is the read mapping run backwards, with three things worth
    /// knowing:
    ///
    /// - **Integer widths are honoured here but not on the way out.** `Int8`
    ///   becomes `TINYINT`, `Int16` `SMALLINT`, `Int32` `MEDIUMINT`, `Int64`
    ///   `INTEGER`. Reading collapses all four to `Int64`, because SQLite does
    ///   not enforce a declared width, so a layer round-tripped through Arrow
    ///   comes back with every integer column widened to `INTEGER`. The data is
    ///   unchanged; the declared type is not.
    /// - **The geometry column is found by its `geoarrow.wkb` extension name**,
    ///   not by position or by name. Its declared type is `GEOMETRY`, which
    ///   accepts any geometry, because WKB does not say what it will contain.
    ///   The SRS comes from the field's CRS metadata when that is an
    ///   `EPSG:<code>` authority code, and is otherwise `0`, the spec's
    ///   undefined value.
    /// - **A field named as the primary key is skipped**, not made an attribute
    ///   column, since the builder creates the key itself. Call
    ///   [`crate::TableSchemaBuilder::primary_key`] before this if the key is
    ///   not named `fid`.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedArrowType`] for a field whose type has no GeoPackage
    /// equivalent.
    pub fn from_arrow_schema(self, schema: &Schema) -> Result<Self> {
        let mut builder = self;
        for field in schema.fields() {
            if *field.name() == builder.primary_key_name() {
                continue;
            }
            if field.metadata().get(EXTENSION_NAME_KEY).map(String::as_str) == Some(GEOARROW_WKB) {
                let srs_id = field
                    .metadata()
                    .get(EXTENSION_METADATA_KEY)
                    .and_then(|json| epsg_code(json))
                    .unwrap_or(0);
                builder = builder.geometry(
                    crate::GeometrySpec::new(GeometryType::Geometry, srs_id)
                        .column_name(field.name()),
                );
                continue;
            }
            let column_type = column_type_for(field.data_type())?;
            let mut column = crate::ColumnSpec::new(field.name(), column_type);
            if !field.is_nullable() {
                column = column.not_null();
            }
            builder = builder.column(column);
        }
        Ok(builder)
    }
}

/// The GeoPackage column type for an Arrow type, the inverse of the mapping in
/// the [module documentation](self).
fn column_type_for(data_type: &DataType) -> Result<ColumnType> {
    Ok(match data_type {
        DataType::Boolean => ColumnType::Boolean,
        DataType::Int8 | DataType::UInt8 => ColumnType::TinyInt,
        DataType::Int16 | DataType::UInt16 => ColumnType::SmallInt,
        DataType::Int32 | DataType::UInt32 => ColumnType::MediumInt,
        DataType::Int64 | DataType::UInt64 => ColumnType::Integer,
        DataType::Float32 => ColumnType::Float,
        DataType::Float64 => ColumnType::Double,
        DataType::Utf8 | DataType::LargeUtf8 => ColumnType::Text(None),
        DataType::Binary | DataType::LargeBinary => ColumnType::Blob(None),
        DataType::Date32 => ColumnType::Date,
        DataType::Timestamp(_, _) => ColumnType::DateTime,
        other => {
            return Err(Error::UnsupportedArrowType {
                data_type: other.to_string(),
            });
        }
    })
}

/// The numeric part of an `EPSG:<code>` authority code in GeoArrow CRS
/// metadata, if that is the form it takes.
///
/// Deliberately not a JSON parse. The only form this crate emits is the one
/// [`crs_metadata`] writes, and a full parse would mean a JSON dependency in
/// order to read back a string we produced. A CRS in any other form, PROJJSON
/// included, yields `None` and the caller sets the SRS themselves. Widening this
/// is part of issue #23.
fn epsg_code(metadata: &str) -> Option<i32> {
    let start = metadata.find("EPSG:")? + "EPSG:".len();
    let rest = metadata.get(start..)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}
