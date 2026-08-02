use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Unique because two readers can share a connection, and a shared name
    /// would have the second overwrite the first's function and the first's
    /// drop remove the second's.
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
/// This is the technique GDAL's GeoPackage driver uses, and the reason for it is
/// measured rather than assumed: fetching every value of every row costs a tenth
/// as much through an aggregate as through the row loop, because the loop stays
/// inside SQLite instead of returning into this crate once per row (see
/// `roadmap/benchmarks/2026-07-25-gdal-arrow-comparison.md`).
///
/// The builders live in the accumulator rather than here, so appending a row
/// needs no synchronisation at all; [`Aggregate::finalize`] moves them into
/// `output` once per batch, which is the only time the lock is taken.
pub(crate) struct BatchFiller {
    pub(crate) names: Vec<String>,
    pub(crate) types: Vec<DataType>,
    /// Which argument is the pagination key.
    pub(crate) key_argument: usize,
    /// Where the schema fields start among the arguments: 1 when the key had to
    /// be selected separately, 0 when it is one of the fields.
    pub(crate) field_offset: usize,
    pub(crate) geometry_index: Option<usize>,
    pub(crate) datetime: DateTimeParsing,
    pub(crate) capacity: usize,
    /// Ceiling on the geometry bytes one batch may contain.
    pub(crate) max_bytes: usize,
    pub(crate) output: Arc<Mutex<Option<FilledBatch>>>,
    /// The first append failure, kept so a typed error survives instead of
    /// becoming a bare SQL error.
    ///
    /// Beside the accumulator rather than inside it, because the accumulator
    /// must be `UnwindSafe` and [`Error`] is not: it boxes a `dyn Error` whose
    /// interior mutability the compiler cannot rule out. Failures are rare, so
    /// taking a lock for one costs nothing that matters.
    pub(crate) failure: Arc<Mutex<Option<Error>>>,
}

/// One batch under construction, the aggregate's accumulator.
pub(crate) struct FilledBatch {
    pub(crate) builders: Vec<ColumnBuilder>,
    pub(crate) rows: usize,
    pub(crate) last_key: Option<i64>,
    /// Geometry bytes appended so far, against the batch's byte ceiling.
    pub(crate) bytes: usize,
    /// Set once the ceiling is reached. Rows after it are left for the next
    /// batch rather than appended, and `last_key` stops advancing, so the
    /// pagination cursor resumes at the first row that did not fit.
    pub(crate) truncated: bool,
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
        // The row count guard means a batch always contains at least one row,
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
