//! Arrow schema derivation: GeoPackage column types to Arrow field types.
//!
//! This is the type mapping the columnar read and write paths share. It is the
//! "impedance mismatch" the GDAL developers report in their write-up of the
//! same exercise, so each choice below is stated with its reason rather than
//! left to be inferred from the code.
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
use crate::{BoundingBox, Error, Layer, Result};

/// Default number of rows per [`RecordBatch`].
///
/// The same default GDAL's driver uses (`MAX_FEATURES_IN_BATCH`). Large enough
/// that per-batch overhead disappears, small enough that a batch of wide rows
/// stays a sensible allocation.
pub const DEFAULT_BATCH_SIZE: usize = 65_536;

/// The default ceiling on the geometry bytes one [`RecordBatch`] may carry.
///
/// The geometry column is Arrow `Binary`, whose offsets are `i32`, so a single
/// batch cannot address more than 2 GB of WKB. This is a hard limit of the
/// type rather than a tuning choice: a batch that crossed it could not be
/// represented at all. A read that would cross it emits a short batch and
/// carries on, so the ceiling costs nothing except on layers whose geometries
/// are large enough to reach it.
///
/// This is the hard ceiling, and no setting can raise a batch past it. The
/// default a read actually uses is [`default_max_batch_bytes`], which follows
/// GDAL in taking `min(INT32_MAX, RAM / 4)`; a caller can set its own with
/// [`ArrowReadOptions::with_max_batch_bytes`].
///
/// The alternative was Arrow `LargeBinary`, whose `i64` offsets have no such
/// ceiling. It was declined because it would hand every consumer 64-bit
/// offsets to solve a problem only very large geometries have, and because
/// matching GDAL keeps our batches interchangeable with the encoding the
/// ecosystem already reads. The `geoarrow.wkb` encoding permits either.
pub const DEFAULT_MAX_BATCH_BYTES: usize = i32::MAX as usize;

/// The byte ceiling a read uses when the caller sets none: GDAL's
/// `min(INT32_MAX, RAM / 4)`.
///
/// The memory term only binds below about 8 GB of RAM, since a quarter of
/// anything larger already exceeds [`DEFAULT_MAX_BATCH_BYTES`]. It is there so
/// a small machine does not spend a quarter of itself on a single batch.
///
/// The system's memory is read once and cached. It is queried to choose a
/// default, not to track a machine whose memory changes while we run.
#[must_use]
pub fn default_max_batch_bytes() -> usize {
    static RESOLVED: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *RESOLVED.get_or_init(|| {
        let mut system = sysinfo::System::new();
        system.refresh_memory();
        let quarter = usize::try_from(system.total_memory() / 4).unwrap_or(usize::MAX);
        // An unavailable reading comes back as 0, which would put one row in
        // every batch. Treat it as no information and keep the fixed ceiling.
        if quarter == 0 {
            DEFAULT_MAX_BATCH_BYTES
        } else {
            quarter.min(DEFAULT_MAX_BATCH_BYTES)
        }
    })
}

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
/// PROJJSON is what GeoArrow prefers, because an authority code leaves the
/// reader to resolve it against a registry it may not have, so that is what
/// this emits wherever the EPSG registry has a definition. A `srs_id` the
/// registry does not know falls back to an `EPSG:<code>` authority string,
/// which GeoArrow permits. `srs_id` 0 and -1 are the spec's undefined values
/// and carry no CRS at all.
fn crs_metadata(srs_id: i32) -> String {
    if srs_id <= 0 {
        return "{}".to_owned();
    }
    // GeoArrow recommends PROJJSON and says an authority code "should only be
    // used as a last resort", because it leaves the reader to resolve the code
    // against a registry it may not have. Emit the full definition when we can
    // and keep the code as the fallback for anything outside the EPSG registry
    // (a user-defined srs_id, say).
    epsg_utils::epsg_to_projjson(srs_id).map_or_else(
        |_| format!(r#"{{"crs":"EPSG:{srs_id}","crs_type":"authority_code"}}"#),
        |projjson| format!(r#"{{"crs":{projjson},"crs_type":"projjson"}}"#),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_ceiling_never_exceeds_the_offset_limit() {
        let resolved = default_max_batch_bytes();
        assert!(resolved > 0, "a zero ceiling would put one row in a batch");
        assert!(
            resolved <= DEFAULT_MAX_BATCH_BYTES,
            "i32 offsets cannot address {resolved} bytes"
        );
        // Cached, so a second call cannot disagree with the first.
        assert_eq!(resolved, default_max_batch_bytes());
    }

    #[test]
    fn a_caller_cannot_raise_the_ceiling_past_the_offset_limit() {
        let options = ArrowReadOptions::default().with_max_batch_bytes(usize::MAX);
        assert_eq!(options.max_batch_bytes, DEFAULT_MAX_BATCH_BYTES);
    }

    #[test]
    fn epsg_code_reads_the_crs_id_not_a_nested_one() {
        // EPSG:4326's PROJJSON nests the ellipsoidal coordinate system's own
        // identifier, 6422, ahead of the CRS's. A scan for the first code
        // would return that.
        let metadata = crs_metadata(4326);
        assert!(
            metadata.contains(r#""code":6422"#),
            "the trap this guards against has moved, update the test: {metadata}"
        );
        assert_eq!(epsg_code(&metadata), Some(4326));
    }

    #[test]
    fn epsg_code_reads_the_authority_code_form() {
        // What we emit for a code the registry does not know, and a form other
        // producers use.
        assert_eq!(
            epsg_code(r#"{"crs":"EPSG:27700","crs_type":"authority_code"}"#),
            Some(27700)
        );
    }

    #[test]
    fn epsg_code_declines_what_it_cannot_identify() {
        assert_eq!(epsg_code("{}"), None);
        assert_eq!(epsg_code("not json"), None);
        // A CRS defined by some other authority is not an EPSG code.
        assert_eq!(
            epsg_code(r#"{"crs":{"id":{"authority":"ESRI","code":104305}}}"#),
            None
        );
    }

    #[test]
    fn a_layer_srs_round_trips_through_the_metadata() {
        for code in [4326, 27700, 32630, 4979] {
            assert_eq!(epsg_code(&crs_metadata(code)), Some(code), "code {code}");
        }
    }

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
}

/// Options for the columnar read path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ArrowReadOptions {
    /// Rows per [`RecordBatch`]. Defaults to [`DEFAULT_BATCH_SIZE`].
    pub batch_size: usize,
    /// Ceiling on the geometry bytes one [`RecordBatch`] may carry. Defaults
    /// to [`DEFAULT_MAX_BATCH_BYTES`]. A batch that would cross it is emitted
    /// short, and the rows that did not fit begin the next one.
    pub max_batch_bytes: usize,
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
            max_batch_bytes: default_max_batch_bytes(),
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

    /// Set the ceiling on geometry bytes per batch. See
    /// [`DEFAULT_MAX_BATCH_BYTES`] for what it is for and why it cannot simply
    /// be raised past `i32::MAX`.
    ///
    /// Values above `i32::MAX` are clamped to it, since Arrow `Binary` cannot
    /// address more than that within one batch whatever the caller asks for.
    #[must_use]
    pub fn with_max_batch_bytes(mut self, max_batch_bytes: usize) -> Self {
        self.max_batch_bytes = max_batch_bytes.clamp(1, DEFAULT_MAX_BATCH_BYTES);
        self
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
    /// row API it wraps.
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
    /// stable snapshot across the whole layer. This shape is what lets batches
    /// be fetched by key range, which the threaded path needs.
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

    /// Read the layer's rows intersecting `bbox` as Arrow record batches.
    ///
    /// The columnar counterpart of [`Layer::features_in`], returning the same
    /// rows in the same order. Single-threaded: the parallel path assigns key
    /// *windows* to workers on the assumption that a window's key span implies
    /// its row count, and a spatial filter voids that, since matching rows
    /// scatter through the key space. Whether a threaded filtered read pays is
    /// a separate question, to be answered by measurement against this.
    ///
    /// Uses the RTree index when the layer has one, and falls back to a full
    /// scan carrying the same exact filter when it does not, exactly as
    /// [`Layer::features_in`] does. Either way the geometry of every candidate
    /// is re-tested against its true `f64` envelope, because the index stores
    /// `f32` envelopes and is queried with widened bounds, so its candidates
    /// are a superset.
    ///
    /// A batch can therefore come back with fewer rows than the batch size
    /// while rows remain: filtering removes candidates after the query has
    /// bounded them. That is already true of the byte ceiling, so a caller
    /// reading until `None` is unaffected.
    ///
    /// # Errors
    ///
    /// [`Error::NoGeometryColumn`] if the layer has none, as
    /// [`Layer::features_in`]; otherwise as [`Layer::read_arrow`].
    pub fn read_arrow_in(
        &self,
        bbox: BoundingBox,
        options: ArrowReadOptions,
    ) -> Result<ArrowBatches<'_>> {
        if self.geometry_column().is_none() {
            return Err(Error::NoGeometryColumn {
                table_name: self.table_name().to_owned(),
            });
        }
        self.read_arrow_filtered(options, Some(bbox), None)
    }

    /// Read the rows matching a caller-supplied `WHERE` clause as Arrow record
    /// batches.
    ///
    /// The columnar counterpart of [`Layer::select`], with the same contract:
    /// `where_clause` is appended (parenthesised) to the query and is **raw
    /// SQL, trusted from the caller**; this crate does not parse or sanitise
    /// it. Its placeholders are `?1` to `?N` and `params` bind in slice order,
    /// exactly as [`Layer::select`] binds them; the pagination this read adds
    /// around the clause is numbered after `N` and never collides with it.
    ///
    /// This is also the columnar read for a single row: `fid = ?1` with the
    /// key as its parameter.
    ///
    /// The filter runs inside SQLite, so unlike [`Layer::read_arrow_in`] there
    /// is no client-side re-test and a batch under-fills only at the byte
    /// ceiling. Single-threaded, like every filtered read: the parallel path
    /// assigns key windows to workers on the assumption that a window's key
    /// span implies its row count, and a filter voids that.
    ///
    /// # Errors
    ///
    /// As [`Layer::read_arrow`]; a clause SQLite cannot prepare surfaces
    /// through the iterator's first item, since each batch is its own query.
    pub fn read_arrow_where(
        &self,
        where_clause: &str,
        params: &[crate::ValueRef<'_>],
        options: ArrowReadOptions,
    ) -> Result<ArrowBatches<'_>> {
        self.read_arrow_filtered(options, None, Some((where_clause, params)))
    }

    /// Read the rows intersecting `bbox` **and** matching a caller-supplied
    /// `WHERE` clause, as Arrow record batches.
    ///
    /// The two filters compose: the rows are
    /// [`Layer::features_in`]'s intersected with [`Layer::select`]'s, in
    /// primary-key order. The bounding box uses the RTree index on
    /// [`Layer::read_arrow_in`]'s terms, including the exact re-test; the
    /// clause carries [`Layer::read_arrow_where`]'s contract, including its
    /// `?1` to `?N` placeholder numbering.
    ///
    /// # Errors
    ///
    /// [`Error::NoGeometryColumn`] if the layer has none; otherwise as
    /// [`Layer::read_arrow_where`].
    pub fn read_arrow_in_where(
        &self,
        bbox: BoundingBox,
        where_clause: &str,
        params: &[crate::ValueRef<'_>],
        options: ArrowReadOptions,
    ) -> Result<ArrowBatches<'_>> {
        if self.geometry_column().is_none() {
            return Err(Error::NoGeometryColumn {
                table_name: self.table_name().to_owned(),
            });
        }
        self.read_arrow_filtered(options, Some(bbox), Some((where_clause, params)))
    }

    /// The single-threaded reader, which the threaded one falls back to and its
    /// workers are built from.
    fn read_arrow_sequential(&self, options: ArrowReadOptions) -> Result<ArrowBatches<'_>> {
        self.read_arrow_filtered(options, None, None)
    }

    /// The single-threaded reader, optionally filtered to a bounding box and a
    /// caller-supplied `WHERE` clause.
    fn read_arrow_filtered(
        &self,
        options: ArrowReadOptions,
        filter: Option<BoundingBox>,
        sql_filter: Option<(&str, &[crate::ValueRef<'_>])>,
    ) -> Result<ArrowBatches<'_>> {
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
        // The caller's `WHERE` clause keeps [`Layer::select`]'s contract: its
        // placeholders are `?1` to `?N` and its params bind in slice order. The
        // key, limit and rtree bounds are numbered explicitly after `N`, so
        // with no clause the text degenerates to the unfiltered one (`?1` key,
        // `?2` limit, `?3` to `?6` bounds) and every path binds the same way:
        // user params first, then key, limit and bounds.
        let base = sql_filter.map_or(0, |(_, params)| params.len());
        let (key_slot, limit_slot) = (base + 1, base + 2);
        let user_params: Vec<rusqlite::types::Value> = sql_filter
            .map(|(_, params)| params.iter().map(crate::value::value_ref_to_sql).collect())
            .unwrap_or_default();
        let user_and = match sql_filter {
            Some((clause, _)) => format!("({clause}) AND "),
            None => String::new(),
        };
        // The spatial predicate, when there is one and the layer has an index
        // to answer it with. A subquery against the RTree rather than a join,
        // so the select list stays unqualified and the surrounding query is
        // unchanged; SQLite drives the same index lookup either way.
        let (spatial_sql, filter_params) = match filter {
            Some(bbox) if self.has_spatial_index()? => {
                let geom = self
                    .geometry_column()
                    .ok_or_else(|| Error::NoGeometryColumn {
                        table_name: self.table_name().to_owned(),
                    })?;
                let rtree = quote(&geopackage_core::triggers::rtree_table_name(
                    self.table_name(),
                    &geom.column_name,
                ))?;
                let id = match self.primary_key_column() {
                    Some(pk) => quote(pk)?,
                    None => "rowid".to_owned(),
                };
                (
                    format!(
                        " AND {id} IN (SELECT id FROM {rtree} \
                          WHERE minx <= ?{} AND maxx >= ?{} AND miny <= ?{} AND maxy >= ?{})",
                        base + 3,
                        base + 4,
                        base + 5,
                        base + 6
                    ),
                    vec![
                        rusqlite::types::Value::Real(crate::layer::widen_up(bbox.max_x)),
                        rusqlite::types::Value::Real(crate::layer::widen_down(bbox.min_x)),
                        rusqlite::types::Value::Real(crate::layer::widen_up(bbox.max_y)),
                        rusqlite::types::Value::Real(crate::layer::widen_down(bbox.min_y)),
                    ],
                )
            }
            // No index, or no filter: a full scan. When a filter is set the
            // exact re-test still runs over every row, which is what
            // `features_in` falls back to as well.
            _ => (String::new(), Vec::new()),
        };
        let rows_sql = format!(
            "SELECT {row_columns} FROM {table} WHERE {user_and}{key} >= ?{key_slot}{spatial_sql} \
             ORDER BY {key} LIMIT ?{limit_slot}"
        );
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
        // A filtered read declines the aggregate and takes the direct loop. For
        // a bounding box the reason is structural: the aggregate builds the
        // columns inside SQLite's own scan, where the exact re-test against
        // each geometry's true envelope has nowhere to run; the direct loop can
        // drop a candidate between reading it and appending it. A `WHERE`
        // clause is exact SQL and could keep the aggregate; it declines anyway
        // until a measurement shows the aggregate paying on filtered reads,
        // so both filters take one path. Declining rather than failing is what
        // the threaded path does for each of its own conditions.
        let aggregate = if filter.is_none()
            && sql_filter.is_none()
            && arg_count <= conn.limit(Limit::SQLITE_LIMIT_FUNCTION_ARG)?
        {
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
                    max_bytes: options.max_batch_bytes.clamp(1, DEFAULT_MAX_BATCH_BYTES),
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
                max_batch_bytes: options.max_batch_bytes.clamp(1, DEFAULT_MAX_BATCH_BYTES),
                last_batch_rows: 0,
                next_key: i64::MIN,
                exhausted: false,
                user_params,
                filter: filter.map(|bbox| {
                    Box::new(SpatialFilter {
                        bbox,
                        params: filter_params,
                    })
                }),
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
            options.max_batch_bytes.clamp(1, DEFAULT_MAX_BATCH_BYTES),
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
    receivers: Vec<std::sync::mpsc::Receiver<std::result::Result<WorkerMessage, ArrowError>>>,
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
        max_batch_bytes: usize,
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
                    &path,
                    &table,
                    conversion,
                    first,
                    last,
                    batch_size,
                    max_batch_bytes,
                    threads,
                    worker,
                    &tx,
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
        loop {
            match self.receivers.get(self.turn)?.recv() {
                Ok(Ok(WorkerMessage::Batch(batch))) => return Some(Ok(batch)),
                // This worker's window is done, so the next window in key
                // order belongs to the next worker.
                Ok(Ok(WorkerMessage::WindowEnd)) => {
                    self.turn = (self.turn + 1) % self.receivers.len().max(1);
                }
                Ok(Err(error)) => {
                    self.done = true;
                    return Some(Err(error));
                }
                // The worker whose turn it is has finished, and workers are
                // assigned windows in rotation, so every later worker has
                // finished too.
                Err(_) => {
                    self.done = true;
                    return None;
                }
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
    max_batch_bytes: usize,
    threads: usize,
    worker: usize,
    tx: &std::sync::mpsc::SyncSender<std::result::Result<WorkerMessage, ArrowError>>,
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
    let options = ArrowReadOptions::with_batch_size(batch_size)
        .with_threads(1)
        .with_max_batch_bytes(max_batch_bytes);
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
        // One window of `batch_size` rows, which is usually one batch. The
        // byte ceiling can split it into several, and every one of them
        // belongs to this worker: skipping to the next window on a short batch
        // would drop the rows that did not fit.
        let mut remaining = batch_size;
        let mut at = key;
        while remaining > 0 {
            match source.read_batch_at(at, remaining) {
                Ok(Some(batch)) => {
                    let rows = source.last_batch_rows;
                    if tx.send(Ok(WorkerMessage::Batch(batch))).is_err() {
                        return; // the consumer stopped
                    }
                    if rows == 0 {
                        break;
                    }
                    remaining -= rows.min(remaining);
                    at = source.next_key;
                }
                // The layer ends inside this window, so it ends for this
                // worker too: every later window starts beyond it.
                Ok(None) => return,
                Err(error) => return send_error(error),
            }
        }
        if tx.send(Ok(WorkerMessage::WindowEnd)).is_err() {
            return; // the consumer stopped
        }
        match key.checked_add(stride) {
            Some(next) => key = next,
            None => return,
        }
    }
}

/// What a worker sends for each window it reads.
///
/// Batches arrive in key order because each worker owns a fixed, repeating
/// slice of the key space and the consumer takes their windows in rotation.
/// The byte ceiling can split one window into several batches, so a worker
/// marks where its window ends rather than the consumer assuming one batch per
/// turn.
enum WorkerMessage {
    Batch(RecordBatch),
    WindowEnd,
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
    /// Ceiling on the geometry bytes one batch may carry.
    max_bytes: usize,
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
    /// Geometry bytes appended so far, against the batch's byte ceiling.
    bytes: usize,
    /// Set once the ceiling is reached. Rows after it are left for the next
    /// batch rather than appended, and `last_key` stops advancing, so the
    /// pagination cursor resumes at the first row that did not fit.
    truncated: bool,
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
            bytes: 0,
            truncated: false,
        })
    }

    fn step(&self, ctx: &mut Context<'_>, acc: &mut FilledBatch) -> rusqlite::Result<()> {
        // SQLite has already been asked for this row, so the cheapest correct
        // response once the ceiling is reached is to drop it and let the next
        // batch fetch it again. The waste is bounded by one batch, and it only
        // arises on layers whose geometries are large enough to hit the limit.
        if acc.truncated {
            return Ok(());
        }
        let geometry_bytes = self.geometry_index.map_or(0, |index| {
            match ctx.get_raw(index + self.field_offset) {
                // The GPB header is stripped before the body is appended, so
                // this over-counts by a header per row. Erring high is the
                // right direction for a ceiling that must not be crossed.
                ValueRef::Blob(blob) => blob.len(),
                _ => 0,
            }
        });
        // The row count guard means a batch always carries at least one row,
        // so a geometry larger than the whole budget still makes progress
        // instead of stalling the read.
        if acc.rows > 0 && acc.bytes.saturating_add(geometry_bytes) > self.max_bytes {
            acc.truncated = true;
            return Ok(());
        }
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
        acc.bytes = acc.bytes.saturating_add(geometry_bytes);
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
    /// Ceiling on the geometry bytes one batch may carry.
    max_batch_bytes: usize,
    /// Rows in the batch just produced. The parallel path reads it to tell a
    /// batch cut short by the byte ceiling from one that filled its window.
    last_batch_rows: usize,
    batch_size: usize,
    /// Rows with a key at or above this are still to be read.
    next_key: i64,
    exhausted: bool,
    /// The caller's `WHERE` parameters, bound first (`?1` to `?N`) on every
    /// page. Empty for a reader with no `WHERE` clause.
    user_params: Vec<rusqlite::types::Value>,
    /// The spatial filter, set only by `read_arrow_in`. Boxed so an unfiltered
    /// reader, which is every reader the threaded path builds, carries one
    /// pointer rather than the whole of it.
    filter: Option<Box<SpatialFilter>>,
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
    /// Read up to `limit` rows starting at `key`, ignoring where the reader
    /// had got to.
    ///
    /// Used by the parallel path, whose workers each read whole batches at
    /// offsets assigned to them rather than walking the layer in sequence.
    /// `limit` is what remains of the worker's window: the byte ceiling can
    /// cut a batch short, and the rest of that window still has to be read.
    fn read_batch_at(&mut self, key: i64, limit: usize) -> Result<Option<RecordBatch>> {
        self.next_key = key;
        self.exhausted = false;
        let full = self.batch_size;
        self.batch_size = limit.max(1);
        let batch = self.next_batch();
        self.batch_size = full;
        batch
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
        // The aggregate counts every row it was given, including any it left
        // for the next batch, so the appended count is what the builders hold.
        let rows_appended = filled.rows;
        self.last_batch_rows = rows_appended;
        self.advance(filled.last_key, rows_appended, filled.truncated);

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
    fn advance(&mut self, last_key: Option<i64>, rows_read: usize, truncated: bool) {
        match last_key.and_then(|key| key.checked_add(1)) {
            Some(next) => self.next_key = next,
            // The key space is exhausted at i64::MAX; there can be no next row.
            None => self.exhausted = true,
        }
        // A short batch normally means the layer ran out. It does not when the
        // byte ceiling cut the batch short: there are rows left, and treating
        // this as the end would silently drop them.
        if rows_read < self.batch_size && !truncated {
            self.exhausted = true;
        }
    }

    /// The direct path: step the rows and fetch each value. Kept as the
    /// fallback for a table too wide for the aggregate's argument list.
    fn next_batch_direct(&mut self) -> Result<Option<RecordBatch>> {
        // Looped rather than recursive: under a selective filter many pages in
        // a row can come back with every candidate dropped, and one stack frame
        // per empty page would be a stack overflow waiting for a large enough
        // layer.
        loop {
            match self.next_batch_page()? {
                Page::Batch(batch) => return Ok(Some(batch)),
                Page::Exhausted => return Ok(None),
                Page::AllFiltered => {}
            }
        }
    }

    /// One page of candidates: a batch, nothing left, or a page whose rows were
    /// all filtered out and which the caller should follow with another.
    fn next_batch_page(&mut self) -> Result<Page> {
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
        let mut bytes = 0usize;
        let mut truncated = false;
        // Candidates the query returned, as against rows that survived the
        // filter. Pagination and exhaustion run off this one: a page whose rows
        // were all filtered out is not the end of the layer, and treating it as
        // one would silently drop everything after it.
        let mut candidates = 0usize;
        {
            let mut stmt = self.conn.prepare_cached(&self.sql)?;
            // Binding is positional, so the order here is the placeholder
            // numbering: the caller's params take `?1` to `?N`, then the key,
            // the limit and the widened bounds follow, as the query text
            // numbered them.
            let mut params: Vec<rusqlite::types::Value> = self.user_params.clone();
            params.push(rusqlite::types::Value::Integer(self.next_key));
            params.push(rusqlite::types::Value::Integer(
                i64::try_from(self.batch_size).unwrap_or(i64::MAX),
            ));
            if let Some(filter) = &self.filter {
                params.extend(filter.params.iter().cloned());
            }
            let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
            // Where the fields start: at 0 when the key is one of them, at 1
            // when it had to be selected separately.
            let offset = usize::from(self.key_field.is_none());
            while let Some(row) = rows.next()? {
                let geometry_bytes = match self.geometry_index {
                    Some(index) => match row.get_ref(index + offset)? {
                        ValueRef::Blob(blob) => blob.len(),
                        _ => 0,
                    },
                    None => 0,
                };
                // Same ceiling as the aggregate path, for the same reason: the
                // geometry column's i32 offsets cannot address more.
                if rows_read > 0 && bytes.saturating_add(geometry_bytes) > self.max_batch_bytes {
                    truncated = true;
                    break;
                }
                // Counted, and its key remembered, before the filter: a
                // candidate that is filtered out has still been read, and the
                // next page must start after it.
                candidates += 1;
                last_key = Some(row.get::<_, i64>(self.key_field.unwrap_or(0))?);

                // The exact re-test, against the true f64 envelope. Done before
                // any value is converted, so a row outside the box costs
                // nothing beyond reading its geometry, and conversion errors
                // surface only for rows that are actually returned. Same rule
                // and same helper as the row path.
                if let Some(filter) = &self.filter {
                    let geometry_index = self.geometry_index.map(|index| index + offset);
                    if !crate::layer::row_in_box(row, geometry_index, &filter.bbox)? {
                        continue;
                    }
                }

                for (index, builder) in builders.iter_mut().enumerate() {
                    builder.append(
                        &self.names,
                        index,
                        row.get_ref(index + offset)?,
                        self.datetime,
                    )?;
                }
                rows_read += 1;
                bytes = bytes.saturating_add(geometry_bytes);
            }
        }

        if candidates == 0 {
            self.exhausted = true;
            return Ok(Page::Exhausted);
        }
        if rows_read == 0 {
            // Every candidate on this page was filtered out. Advance past them
            // and let the caller try the next page rather than reporting the
            // layer exhausted.
            self.advance(last_key, candidates, truncated);
            return Ok(if self.exhausted {
                Page::Exhausted
            } else {
                Page::AllFiltered
            });
        }
        self.last_batch_rows = rows_read;
        self.advance(last_key, candidates, truncated);

        let arrays: Vec<ArrayRef> = builders.into_iter().map(ColumnBuilder::finish).collect();
        Ok(Page::Batch(RecordBatch::try_new(
            Arc::clone(&self.schema),
            arrays,
        )?))
    }
}

/// The exact re-filter a `read_arrow_in` carries, with the rtree bounds its
/// query binds.
///
/// The index stores `f32` envelopes and is queried with outward-widened bounds,
/// so its candidates are a superset: every one has to be re-tested against its
/// true `f64` envelope, or a filtered columnar read would return rows
/// `features_in` does not.
struct SpatialFilter {
    bbox: BoundingBox,
    /// The four widened bounds, bound as `?3` to `?6`. Empty when the layer has
    /// no index, in which case the read is a full scan and the exact test does
    /// all the work.
    params: Vec<rusqlite::types::Value>,
}

/// What one page of candidates produced.
enum Page {
    /// A batch with at least one row in it.
    Batch(RecordBatch),
    /// Nothing left to read.
    Exhausted,
    /// Candidates were read and every one was filtered out; there may be more.
    AllFiltered,
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
                builder.append_value(date.days_since_epoch());
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
                let micros =
                    stamp
                        .micros_since_epoch()
                        .map_err(|source| Error::InvalidDateTimeValue {
                            column: column_name(names, index),
                            text: text.to_owned(),
                            source,
                        })?;
                builder.append_value(micros);
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

/// Where each of a layer's columns sits in a batch, shared by all its rows.
struct RowLayout {
    fid: Option<usize>,
    geometry: Option<usize>,
    /// Batch column index for each of the layer's value columns, in order.
    /// `None` for a column the batch does not carry, which is written as NULL.
    values: Vec<Option<usize>>,
}

/// One row of a [`RecordBatch`], as a view rather than a copy.
///
/// Holding the batch and an index, rather than owned values, is what lets the
/// write path bind strings and blobs straight out of the Arrow arrays. Both
/// handles are `Arc`, so a row costs two reference-count bumps where owning its
/// values cost an allocation per text cell plus a copy of the geometry.
struct ArrowRow {
    batch: Arc<RecordBatch>,
    layout: Arc<RowLayout>,
    row: usize,
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
        let fid = match self
            .layout
            .fid
            .and_then(|index| self.batch.columns().get(index))
        {
            Some(column) => read_i64(column, self.row)?,
            None => None,
        };

        let mut values = Vec::with_capacity(self.layout.values.len());
        for (position, index) in self.layout.values.iter().enumerate() {
            let bound = match index.and_then(|index| self.batch.columns().get(index)) {
                Some(column) => bind_value(column, self.row, position, &self.batch)?,
                None => rusqlite::types::ToSqlOutput::Borrowed(rusqlite::types::ValueRef::Null),
            };
            values.push(bound);
        }

        let geometry = self
            .layout
            .geometry
            .and_then(|index| self.batch.columns().get(index));
        match geometry {
            Some(column) if !column.is_null(self.row) => {
                let wkb = binary_at(column, self.row)?;
                writer.insert_wkb_bound(fid, wkb, &values)
            }
            _ => writer.insert_row_bound(fid, &values).map(|fid| (fid, None)),
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
        // The layer's value columns already exclude both the geometry and the
        // primary key, which this path binds through its own arguments.
        let value_columns: Vec<String> = self
            .value_columns()
            .iter()
            .map(|column| column.name.clone())
            .collect();
        let primary_key = self.primary_key_column().map(str::to_owned);

        // One batch is taken apart at a time and the rows are handed on lazily,
        // so peak memory is a batch rather than the whole input, and the write
        // path sees the unsized source it buffers to size up (issue #17).
        // Collecting here would undo both.
        let rows = batches.into_iter().flat_map(move |batch| {
            let taken = batch.map_err(Error::Arrow).and_then(|batch| {
                let layout = layout_of(
                    &batch,
                    primary_key.as_deref(),
                    geometry_column.as_deref(),
                    &value_columns,
                )?;
                Ok((Arc::new(batch), Arc::new(layout)))
            });
            // Split into the two arms rather than collecting either, so a
            // batch's rows are still built one at a time. The arms are chained
            // instead of matched so both are the same iterator type: exactly one
            // of them ever yields.
            //
            // A failure travels as a row of its own, so it reaches the write
            // path and rolls the transaction back rather than needing a second
            // channel out of the iterator.
            let (batch, error) = match taken {
                Ok(batch) => (Some(batch), None),
                Err(error) => (None, Some(error)),
            };
            batch
                .into_iter()
                .flat_map(|(batch, layout)| {
                    (0..batch.num_rows()).map(move |row| {
                        ArrowRowResult::Row(ArrowRow {
                            batch: Arc::clone(&batch),
                            layout: Arc::clone(&layout),
                            row,
                        })
                    })
                })
                .chain(error.into_iter().map(ArrowRowResult::Failed))
        });
        self.write_all_impl(rows, batch_size, options, crate::bulk::no_fault)
    }
}

/// Work out where each of the layer's columns sits in this batch.
///
/// The layout is computed once per batch and shared, so a row carries two `Arc`
/// handles rather than a copy of anything.
fn layout_of(
    batch: &RecordBatch,
    primary_key: Option<&str>,
    geometry: Option<&str>,
    value_columns: &[String],
) -> Result<RowLayout> {
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

    let index_of = |name: &str| schema.fields().iter().position(|f| f.name() == name);
    Ok(RowLayout {
        fid: primary_key.and_then(index_of),
        geometry: geometry.and_then(index_of),
        values: value_columns.iter().map(|name| index_of(name)).collect(),
    })
}

/// The bytes of a binary cell, borrowed from the array.
fn binary_at(column: &ArrayRef, row: usize) -> Result<&[u8]> {
    if let Some(binary) = column.as_binary_opt::<i32>() {
        return Ok(binary.value(row));
    }
    if let Some(binary) = column.as_binary_opt::<i64>() {
        return Ok(binary.value(row));
    }
    Err(Error::ArrowValueMismatch {
        column: String::new(),
        expected: "Binary or LargeBinary",
        found: "another Arrow type",
    })
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

/// Bind one attribute cell, borrowing from the array wherever that is possible.
///
/// The inverse of the read path's mapping, and deliberately narrower: it accepts
/// what [`Layer::arrow_schema`] produces, so a round trip works, plus the
/// narrower integer and float widths another producer is likely to emit.
///
/// Strings and blobs are bound as slices into the Arrow buffers, which is the
/// point of this function: they are already contiguous there, so copying them
/// into a `Value` first would allocate once per cell for nothing. `DATE` and
/// `DATETIME` are the exception, because a GeoPackage stores them as text and
/// the text has to be produced.
fn bind_value<'a>(
    column: &'a ArrayRef,
    row: usize,
    position: usize,
    batch: &RecordBatch,
) -> Result<rusqlite::types::ToSqlOutput<'a>> {
    use arrow_array::types::{
        Date32Type, Float32Type, Float64Type, Int8Type, Int16Type, Int32Type, Int64Type,
        TimestampMicrosecondType, TimestampMillisecondType,
    };
    use rusqlite::types::{ToSqlOutput, Value as SqlV, ValueRef};

    let borrowed = |value: ValueRef<'a>| Ok(ToSqlOutput::Borrowed(value));
    let owned = |value: SqlV| Ok(ToSqlOutput::Owned(value));

    if column.is_null(row) {
        return borrowed(ValueRef::Null);
    }
    // Only built when something is wrong, so the happy path does not pay for
    // naming the column.
    let name = || {
        batch
            .schema()
            .fields()
            .get(position)
            .map(|field| field.name().clone())
            .unwrap_or_default()
    };
    let mismatch = |expected: &'static str| Error::ArrowValueMismatch {
        column: name(),
        expected,
        found: "an array of another type",
    };
    let out_of_range = |source| Error::InvalidDateTimeValue {
        column: name(),
        text: "an Arrow date or timestamp outside the representable range".to_owned(),
        source,
    };

    match column.data_type() {
        DataType::Boolean => owned(SqlV::Integer(i64::from(
            column
                .as_boolean_opt()
                .ok_or_else(|| mismatch("Boolean"))?
                .value(row),
        ))),
        DataType::Int8 => owned(SqlV::Integer(i64::from(
            column
                .as_primitive_opt::<Int8Type>()
                .ok_or_else(|| mismatch("Int8"))?
                .value(row),
        ))),
        DataType::Int16 => owned(SqlV::Integer(i64::from(
            column
                .as_primitive_opt::<Int16Type>()
                .ok_or_else(|| mismatch("Int16"))?
                .value(row),
        ))),
        DataType::Int32 => owned(SqlV::Integer(i64::from(
            column
                .as_primitive_opt::<Int32Type>()
                .ok_or_else(|| mismatch("Int32"))?
                .value(row),
        ))),
        DataType::Int64 => borrowed(ValueRef::Integer(
            column
                .as_primitive_opt::<Int64Type>()
                .ok_or_else(|| mismatch("Int64"))?
                .value(row),
        )),
        DataType::Float32 => owned(SqlV::Real(f64::from(
            column
                .as_primitive_opt::<Float32Type>()
                .ok_or_else(|| mismatch("Float32"))?
                .value(row),
        ))),
        DataType::Float64 => borrowed(ValueRef::Real(
            column
                .as_primitive_opt::<Float64Type>()
                .ok_or_else(|| mismatch("Float64"))?
                .value(row),
        )),
        DataType::Utf8 => borrowed(ValueRef::Text(
            column
                .as_string_opt::<i32>()
                .ok_or_else(|| mismatch("Utf8"))?
                .value(row)
                .as_bytes(),
        )),
        DataType::LargeUtf8 => borrowed(ValueRef::Text(
            column
                .as_string_opt::<i64>()
                .ok_or_else(|| mismatch("LargeUtf8"))?
                .value(row)
                .as_bytes(),
        )),
        DataType::Binary => borrowed(ValueRef::Blob(
            column
                .as_binary_opt::<i32>()
                .ok_or_else(|| mismatch("Binary"))?
                .value(row),
        )),
        DataType::LargeBinary => borrowed(ValueRef::Blob(
            column
                .as_binary_opt::<i64>()
                .ok_or_else(|| mismatch("LargeBinary"))?
                .value(row),
        )),
        DataType::Date32 => owned(SqlV::Text(
            Date::from_days_since_epoch(
                column
                    .as_primitive_opt::<Date32Type>()
                    .ok_or_else(|| mismatch("Date32"))?
                    .value(row),
            )
            .map_err(out_of_range)?
            .to_string(),
        )),
        DataType::Timestamp(TimeUnit::Microsecond, _) => owned(SqlV::Text(
            DateTime::from_micros_since_epoch(
                column
                    .as_primitive_opt::<TimestampMicrosecondType>()
                    .ok_or_else(|| mismatch("Timestamp"))?
                    .value(row),
            )
            .map_err(out_of_range)?
            .to_string(),
        )),
        DataType::Timestamp(TimeUnit::Millisecond, _) => owned(SqlV::Text(
            DateTime::from_micros_since_epoch(
                column
                    .as_primitive_opt::<TimestampMillisecondType>()
                    .ok_or_else(|| mismatch("Timestamp"))?
                    .value(row)
                    .saturating_mul(1_000),
            )
            .map_err(out_of_range)?
            .to_string(),
        )),
        other => Err(Error::UnsupportedArrowType {
            data_type: other.to_string(),
        }),
    }
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
/// Handles both forms [`crs_metadata`] emits, and the same two forms from any
/// other producer: a PROJJSON object, or an `EPSG:<code>` authority string.
///
/// PROJJSON needs a real parse rather than a scan for the first `"code"`. A
/// CRS object nests identifiers for its coordinate system, datum and
/// ellipsoid, all of which have EPSG codes of their own: in EPSG:4326 the
/// first one to appear is 6422, the ellipsoidal coordinate system. Only the
/// top-level `id` identifies the CRS itself. Anything else yields `None` and
/// the caller sets the SRS themselves.
fn epsg_code(metadata: &str) -> Option<i32> {
    let value: serde_json::Value = serde_json::from_str(metadata).ok()?;
    let crs = value.get("crs")?;
    if let Some(id) = crs.get("id")
        && id
            .get("authority")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|a| a.eq_ignore_ascii_case("EPSG"))
    {
        return id.get("code")?.as_i64()?.try_into().ok();
    }
    let code = crs.as_str()?.strip_prefix("EPSG:")?;
    code.parse().ok()
}
