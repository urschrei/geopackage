//! How GeoPackage column types map to Arrow field types.
//!
//! The columnar read and write paths share this mapping. SQLite's declared
//! types don't line up with Arrow's, so several entries below are judgement
//! calls rather than translations. Each comes with its reason, rather than
//! leaving you to infer it from the code.
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
//! **Integer widths collapse to `Int64`.** SQLite doesn't enforce a declared
//! integer width. A `TINYINT` column stores whatever integer it was given, and
//! reading values that exceed the width is routine. Every width therefore maps
//! to `Int64`, which can't truncate. [`crate::Value::Integer`] collapses the
//! same widths for the same reason.
//!
//! **`DATETIME` becomes microsecond UTC.** GeoPackage stores a datetime as a
//! UTC ISO 8601 string with millisecond precision.
//! [`geopackage_core::datetime::DateTime`] also parses finer input, so
//! microseconds cover everything the strict form can express, with room for the
//! lenient form. Values are converted, not reinterpreted: the text is parsed
//! and the instant is written, so a consumer sees a timestamp rather than a
//! string.
//!
//! **Other declared types are mapped by SQLite's affinity rules.** Files
//! written by other tools often declare types such as `VARCHAR(20)`, and
//! [`geopackage_core::types::ColumnType::parse`] returns `None` for them.
//! Rejecting those columns would make the Arrow path useless on real files.
//! Instead, the declared type goes through the affinity rules in
//! [SQLite section 3.1](https://www.sqlite.org/datatype3.html#determination_of_column_affinity),
//! which are the rules SQLite itself uses to decide what such a column
//! contains.
//!
//! # Nullability
//!
//! A field is non-nullable only when the column is `NOT NULL`. The primary key
//! is non-nullable regardless, since SQLite's `INTEGER PRIMARY KEY` cannot be
//! NULL.

use std::sync::Arc;

use arrow_array::{RecordBatch, RecordBatchReader};
use arrow_schema::{ArrowError, SchemaRef};

use parallel::ParallelBatches;
use sequential::SequentialBatches;

mod aggregate;
mod builder;
mod options;
mod parallel;
mod read;
mod schema;
mod sequential;
mod write;

pub use options::ArrowReadOptions;
pub use options::DEFAULT_BATCH_SIZE;
pub use options::DEFAULT_MAX_BATCH_BYTES;
pub use options::default_max_batch_bytes;

/// A stream of Arrow [`RecordBatch`]es over one layer, from
/// [`crate::Layer::read_arrow`].
///
/// Implements [`RecordBatchReader`], so you can pass it to anything in the
/// Arrow ecosystem that accepts one. Batches arrive in primary-key order
/// whether or not the read uses threads.
pub struct ArrowBatches<'a> {
    pub(crate) schema: SchemaRef,
    pub(crate) source: BatchSource<'a>,
}

pub(crate) enum BatchSource<'a> {
    /// Boxed because the sequential reader holds its query text, schema
    /// handles and segment list, several times the size of the parallel
    /// variant, and an `ArrowBatches` should stay cheap to move.
    Sequential(Box<SequentialBatches<'a>>),
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
