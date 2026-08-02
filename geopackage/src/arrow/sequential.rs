use std::sync::Arc;

use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{ArrowError, SchemaRef};
use rusqlite::Connection;
use rusqlite::types::ValueRef;

use crate::value::DateTimeParsing;
use crate::{BoundingBox, Result};

use super::aggregate::AggregateState;
use super::builder::ColumnBuilder;
use super::options::DEFAULT_BATCH_SIZE;

/// One layer's batches read on the calling thread.
pub(crate) struct SequentialBatches<'a> {
    pub(crate) conn: &'a Connection,
    pub(crate) schema: SchemaRef,
    /// The direct-loop query, selecting the columns as ordinary result columns.
    pub(crate) sql: String,
    /// The aggregate-path query, wrapping the same columns in the registered
    /// function. Built on first use.
    pub(crate) aggregate_sql: String,
    /// Index of the field that is also the pagination key, when it is one of
    /// them. `None` means the key is selected as an extra leading column.
    pub(crate) key_field: Option<usize>,
    /// Index of the geometry field, whose values need the GPB header stripped.
    pub(crate) geometry_index: Option<usize>,
    pub(crate) names: Vec<String>,
    pub(crate) datetime: DateTimeParsing,
    /// Ceiling on the geometry bytes one batch may contain.
    pub(crate) max_batch_bytes: usize,
    /// Rows in the batch just produced. The parallel path reads it to tell a
    /// batch cut short by the byte ceiling from one that filled its window.
    pub(crate) last_batch_rows: usize,
    pub(crate) batch_size: usize,
    /// Rows with a key at or above this are still to be read.
    pub(crate) next_key: i64,
    pub(crate) exhausted: bool,
    /// The candidate key ranges from the one-time rtree scan, walked in
    /// order. `None` for an unfiltered read or the indexless full scan.
    pub(crate) segments: Option<Vec<(i64, i64)>>,
    /// Index into [`Self::segments`] of the range being walked.
    pub(crate) segment: usize,
    /// The caller's `WHERE` parameters, bound first (`?1` to `?N`) on every
    /// page. Empty for a reader with no `WHERE` clause.
    pub(crate) user_params: Vec<rusqlite::types::Value>,
    /// Absolute row index of the geometry selected only for the bbox re-test,
    /// set when a projection keeps the geometry out of the schema but a
    /// filter still needs it. Never a field, so never appended to a builder.
    pub(crate) hidden_geometry: Option<usize>,
    /// The spatial filter, set only by `read_arrow_in`. Boxed so an unfiltered
    /// reader, which is every reader the threaded path builds, stores one
    /// pointer rather than the whole of it.
    pub(crate) filter: Option<Box<SpatialFilter>>,
    /// The aggregate function, when this reader uses it. `None` falls back to
    /// the direct loop.
    pub(crate) aggregate: Option<AggregateState>,
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
    /// Reads up to `limit` rows starting at `key`, ignoring where the reader
    /// had got to.
    ///
    /// Used by the parallel path, whose workers each read whole batches at
    /// offsets assigned to them rather than walking the layer in sequence.
    /// `limit` is what remains of the worker's window: the byte ceiling can
    /// cut a batch short, and the rest of that window still has to be read.
    pub(crate) fn read_batch_at(&mut self, key: i64, limit: usize) -> Result<Option<RecordBatch>> {
        self.next_key = key;
        self.exhausted = false;
        let full = self.batch_size;
        self.batch_size = limit.max(1);
        let batch = self.next_batch();
        self.batch_size = full;
        batch
    }

    /// Reads one batch, or `None` once the layer is exhausted.
    pub(crate) fn next_batch(&mut self) -> Result<Option<RecordBatch>> {
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
        // A short batch normally means the current range ran out: the whole
        // layer for an unsegmented read, the current segment for a segmented
        // one. It does not when the byte ceiling cut the batch short: there
        // are rows left, and treating this as the end would silently drop
        // them.
        if rows_read < self.batch_size && !truncated {
            // The key still advances first: a parallel worker resumes its
            // window from `next_key` even when the page ended short, and a
            // segmented read overwrites it with the next segment's start.
            if let Some(next) = last_key.and_then(|key| key.checked_add(1)) {
                self.next_key = next;
            }
            self.next_segment();
            return;
        }
        match last_key.and_then(|key| key.checked_add(1)) {
            Some(next) => self.next_key = next,
            // The key space is exhausted at i64::MAX; there can be no next
            // row in this segment, nor a later segment to hold one.
            None => self.exhausted = true,
        }
    }

    /// Move to the next candidate segment, or the end of the read.
    ///
    /// For an unsegmented read there is no next range, so this is where a
    /// finished scan becomes `exhausted`.
    fn next_segment(&mut self) {
        let Some(segments) = &self.segments else {
            self.exhausted = true;
            return;
        };
        self.segment += 1;
        match segments.get(self.segment) {
            Some((lo, _)) => self.next_key = *lo,
            None => self.exhausted = true,
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
            // the limit, and the current segment's upper bound where the read
            // is segmented, as the query text numbered them.
            let mut params: Vec<rusqlite::types::Value> = self.user_params.clone();
            params.push(rusqlite::types::Value::Integer(self.next_key));
            params.push(rusqlite::types::Value::Integer(
                i64::try_from(self.batch_size).unwrap_or(i64::MAX),
            ));
            if let Some(segments) = &self.segments {
                // `next_batch_page` is only reached while a segment remains,
                // since exhausting the last one sets `exhausted`; an i64::MAX
                // bound on a missing segment would read past the candidates,
                // and the exact re-test would still drop every extra row.
                let (_, hi) = segments.get(self.segment).copied().unwrap_or((0, i64::MAX));
                params.push(rusqlite::types::Value::Integer(hi));
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
                // and same helper as the row path. The geometry is a schema
                // field or, on a projected read, the hidden trailing column
                // selected for exactly this test.
                if let Some(filter) = &self.filter {
                    let geometry_index = self
                        .geometry_index
                        .map(|index| index + offset)
                        .or(self.hidden_geometry);
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
            // An empty page ends the current segment, not necessarily the
            // read: a segmented read moves to its next range and tries again.
            self.next_segment();
            return Ok(if self.exhausted {
                Page::Exhausted
            } else {
                Page::AllFiltered
            });
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

/// The exact re-filter a `read_arrow_in` applies.
///
/// The index stores `f32` envelopes and is queried with outward-widened
/// bounds, and a candidate segment additionally folds small key gaps, so the
/// fetched rows are a superset twice over: every one has to be re-tested
/// against its true `f64` envelope, or a filtered columnar read would return
/// rows `features_in` does not.
pub(crate) struct SpatialFilter {
    pub(crate) bbox: BoundingBox,
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
