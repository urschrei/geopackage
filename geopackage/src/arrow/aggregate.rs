use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use arrow_schema::DataType;
use rusqlite::Connection;
use rusqlite::functions::{Aggregate, Context, FunctionFlags};
use rusqlite::types::ValueRef;

use crate::value::DateTimeParsing;
use crate::{Error, Result};

use super::builder::ColumnBuilder;

/// A registered aggregate function and the slot its finaliser leaves the
/// finished builders in.
pub(crate) struct AggregateState {
    pub(crate) name: String,
    pub(crate) arg_count: i32,
    pub(crate) output: Arc<Mutex<Option<FilledBatch>>>,
    pub(crate) failure: Arc<Mutex<Option<Error>>>,
}

impl AggregateState {
    /// Registers the function under a name unique to this reader.
    ///
    /// The name is unique because two readers can share a connection. With a
    /// shared name, the second reader would overwrite the first's function, and
    /// dropping the first would remove the second's.
    pub(crate) fn register(conn: &Connection, arg_count: i32, filler: BatchFiller) -> Result<Self> {
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
/// Filling a batch inside a SQL aggregate keeps the fetch loop inside SQLite,
/// instead of returning to this crate once per row. Measured over every value
/// of every row, this costs a tenth as much as the row loop. GDAL's GeoPackage
/// driver uses the same technique.
///
/// The builders live in the accumulator, not here, so appending a row needs no
/// synchronisation. [`Aggregate::finalize`] moves them into `output` once per
/// batch, which is the only time the lock is taken.
pub(crate) struct BatchFiller {
    pub(crate) names: Vec<String>,
    pub(crate) types: Vec<DataType>,
    /// Which argument is the pagination key.
    pub(crate) key_argument: usize,
    /// Where the schema fields start among the arguments: 1 when the key had to
    /// be selected separately, 0 when it is one of the fields.
    pub(crate) field_offset: usize,
    /// Set when the key is one of the fields and its column is `Int64`. The
    /// finaliser then takes the batch's last key from that column's builder,
    /// and the step function does not read the key a second time.
    pub(crate) key_in_fields: bool,
    pub(crate) geometry_index: Option<usize>,
    pub(crate) datetime: DateTimeParsing,
    /// Rows to pre-size the builders for.
    pub(crate) capacity: usize,
    /// Rows in a full batch. The aggregate stops the scan when it has them.
    pub(crate) limit: usize,
    /// Ceiling on the geometry bytes one batch may contain.
    pub(crate) max_bytes: usize,
    /// For each field, the value bytes per row of the last batch, rounded up,
    /// or 0 before the first batch and for fixed-width types. [`Aggregate::init`]
    /// sizes the next batch's buffers from it, so that a buffer does not grow
    /// and copy itself while the batch is filled.
    pub(crate) row_bytes: Vec<AtomicUsize>,
    pub(crate) output: Arc<Mutex<Option<FilledBatch>>>,
    /// The first append failure, kept so a typed error survives instead of
    /// becoming a bare SQL error.
    ///
    /// It is kept beside the accumulator, not inside it, because the
    /// accumulator must be `UnwindSafe`. [`Error`] is not, because it boxes a
    /// `dyn Error` whose interior mutability the compiler can't rule out.
    /// Failures are rare, so the cost of taking a lock for one doesn't matter.
    pub(crate) failure: Arc<Mutex<Option<Error>>>,
}

/// One batch under construction, the aggregate's accumulator.
pub(crate) struct FilledBatch {
    pub(crate) builders: Vec<ColumnBuilder>,
    pub(crate) rows: usize,
    pub(crate) last_key: Option<i64>,
    /// Geometry bytes appended so far, against the batch's byte ceiling.
    pub(crate) bytes: usize,
    /// Set once the ceiling is reached. Later rows are left for the next batch
    /// instead of being appended, and `last_key` stops advancing, so the
    /// pagination cursor resumes at the first row that didn't fit.
    pub(crate) truncated: bool,
    /// Set when the aggregate stopped the scan, because the batch was full or
    /// at its byte ceiling, so that rows may remain after it.
    pub(crate) stopped: bool,
}

impl BatchFiller {
    /// Appends one row to `builders`, one builder per field. `geometry` is the
    /// geometry field's index and value, already read for the byte ceiling,
    /// so that it is not read a second time.
    fn append_row(
        &self,
        ctx: &Context<'_>,
        builders: &mut [ColumnBuilder],
        geometry: Option<(usize, ValueRef<'_>)>,
    ) -> Result<()> {
        let split = geometry.and_then(|(index, value)| {
            let (before, rest) = builders.split_at_mut_checked(index)?;
            let (builder, after) = rest.split_first_mut()?;
            Some((before, (index, builder, value), after))
        });
        let Some((before, (index, builder, value), after)) = split else {
            return self.append_fields(ctx, builders, 0);
        };
        self.append_fields(ctx, before, 0)?;
        builder.append(&self.names, index, value, self.datetime)?;
        self.append_fields(ctx, after, index + 1)
    }

    /// Appends the arguments of fields `first..` to `builders`, which hold
    /// those fields in order.
    fn append_fields(
        &self,
        ctx: &Context<'_>,
        builders: &mut [ColumnBuilder],
        first: usize,
    ) -> Result<()> {
        for (offset, builder) in builders.iter_mut().enumerate() {
            let index = first + offset;
            let value = ctx.get_raw(index + self.field_offset);
            builder.append(&self.names, index, value, self.datetime)?;
        }
        Ok(())
    }
}

/// Marks the batch as stopped and returns the error that ends SQLite's scan.
///
/// The query has no `LIMIT`, because a `LIMIT` on the rows needs a subquery,
/// and SQLite copies every text and blob value out of a subquery before the
/// aggregate reads it. An error from the step function stops the scan at once,
/// and SQLite still runs the finaliser, which passes the batch on.
fn stop(acc: &mut FilledBatch) -> rusqlite::Error {
    acc.stopped = true;
    rusqlite::Error::UserFunctionError("geopackage: batch full".into())
}

impl Aggregate<FilledBatch, i64> for BatchFiller {
    fn init(&self, _: &mut Context<'_>) -> rusqlite::Result<FilledBatch> {
        let mut builders = Vec::with_capacity(self.types.len());
        for (index, data_type) in self.types.iter().enumerate() {
            let is_geometry = Some(index) == self.geometry_index;
            let value_bytes = self
                .row_bytes
                .get(index)
                .map(|row_bytes| row_bytes.load(Ordering::Relaxed))
                .filter(|row_bytes| *row_bytes > 0)
                .map(|row_bytes| {
                    // An eighth more than the last batch used, so that a batch
                    // with slightly longer values does not grow the buffer.
                    let bytes = row_bytes.saturating_mul(self.capacity);
                    let bytes = bytes.saturating_add(bytes / 8);
                    if is_geometry {
                        bytes.min(self.max_bytes)
                    } else {
                        bytes
                    }
                });
            builders.push(
                ColumnBuilder::new(data_type, is_geometry, self.capacity, value_bytes)
                    .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?,
            );
        }
        Ok(FilledBatch {
            builders,
            rows: 0,
            last_key: None,
            bytes: 0,
            truncated: false,
            stopped: false,
        })
    }

    #[hotpath::measure(label = "arrow::BatchFiller::step")]
    fn step(&self, ctx: &mut Context<'_>, acc: &mut FilledBatch) -> rusqlite::Result<()> {
        // Read once, for the byte ceiling and for its builder.
        let geometry = self
            .geometry_index
            .map(|index| (index, ctx.get_raw(index + self.field_offset)));
        let geometry_bytes = match geometry {
            // The GPB header is stripped before the body is appended, so this
            // over-counts by a header per row. Erring high is the right
            // direction for a ceiling that must not be crossed.
            Some((_, ValueRef::Blob(blob))) => blob.len(),
            _ => 0,
        };
        // The row count guard means a batch always contains at least one row,
        // so a geometry larger than the whole budget still makes progress
        // instead of stalling the read.
        if acc.rows > 0 && acc.bytes.saturating_add(geometry_bytes) > self.max_bytes {
            acc.truncated = true;
            return Err(stop(acc));
        }
        if !self.key_in_fields
            && let ValueRef::Integer(key) = ctx.get_raw(self.key_argument)
        {
            acc.last_key = Some(key);
        }
        if let Err(error) = self.append_row(ctx, &mut acc.builders, geometry) {
            if let Ok(mut slot) = self.failure.lock() {
                *slot = Some(error);
            }
            // Stop the scan. The finaliser still runs, so the typed error
            // above is what the caller sees rather than this one.
            return Err(rusqlite::Error::UserFunctionError(
                "geopackage: columnar read failed".into(),
            ));
        }
        acc.rows += 1;
        acc.bytes = acc.bytes.saturating_add(geometry_bytes);
        if acc.rows >= self.limit {
            return Err(stop(acc));
        }
        Ok(())
    }

    fn finalize(&self, _: &mut Context<'_>, mut acc: Option<FilledBatch>) -> rusqlite::Result<i64> {
        let rows = acc.as_ref().map_or(0, |batch| batch.rows);
        if let Some(batch) = &mut acc
            && batch.rows > 0
        {
            if self.key_in_fields {
                batch.last_key = batch
                    .builders
                    .get(self.key_argument)
                    .and_then(ColumnBuilder::last_integer);
            }
            for (builder, row_bytes) in batch.builders.iter().zip(&self.row_bytes) {
                if let Some(bytes) = builder.value_bytes() {
                    row_bytes.store(bytes.div_ceil(batch.rows), Ordering::Relaxed);
                }
            }
        }
        if let Some(batch) = acc
            && let Ok(mut slot) = self.output.lock()
        {
            *slot = Some(batch);
        }
        Ok(i64::try_from(rows).unwrap_or(i64::MAX))
    }
}
