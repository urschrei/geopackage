use std::sync::{Arc, Mutex};

use geopackage_core::ident::quote;
use rusqlite::limits::Limit;

use crate::{BoundingBox, Error, Layer, Result};

use super::aggregate::AggregateState;
use super::aggregate::BatchFiller;
use super::options::{ArrowReadOptions, DEFAULT_BATCH_SIZE, DEFAULT_MAX_BATCH_BYTES};
use super::parallel::{ParallelBatches, database_path, dense_key_span};
use super::sequential::{SequentialBatches, SpatialFilter};
use super::{ArrowBatches, BatchSource};

/// Alias for the pagination key when it is not itself one of the columns, so
/// the aggregate can name it as an argument.
const KEY_ALIAS: &str = "__gpkg_key";

/// Largest key gap folded into one candidate segment.
///
/// Sorted rtree candidates are walked as key ranges rather than fetched id by
/// id, so a gap inside a segment over-fetches the rows in it, each dropped by
/// the exact re-test. Folding small gaps is the cheaper side of that trade:
/// continuing a range scan over a few dozen rows costs less than ending the
/// page and starting a fresh index probe at the next island, and clustered
/// data, where filtered reads matter, produces mostly such gaps.
const SEGMENT_GAP: i64 = 64;

/// Sorted candidate ids folded into closed key ranges, gaps up to
/// [`SEGMENT_GAP`] included.
fn segment_runs(ids: &[i64]) -> Vec<(i64, i64)> {
    let mut runs = Vec::new();
    let mut ids = ids.iter().copied();
    let Some(first) = ids.next() else {
        return runs;
    };
    let (mut lo, mut hi) = (first, first);
    for id in ids {
        if id.saturating_sub(hi) <= SEGMENT_GAP {
            hi = id;
        } else {
            runs.push((lo, hi));
            (lo, hi) = (id, id);
        }
    }
    runs.push((lo, hi));
    runs
}

impl Layer<'_> {
    /// Read this layer as a stream of Arrow [`RecordBatch`](arrow_array::RecordBatch)es.
    ///
    /// Reads on several threads where it can, which is the common case and the
    /// default; see [Threading](#threading) below for the conditions and for how
    /// to ask for a single thread instead.
    ///
    /// Attribute columns follow the mapping in the [module documentation](super);
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
        // A projected layer declines the parallel path: its workers rebuild
        // the layer from the table name over their own connections, which
        // would read every column. Declining is the idiom the other
        // conditions below use.
        if options.resolved_threads() < 2 || self.is_projected() {
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
    /// The index is scanned once, when the read is opened, and the candidate
    /// set is fixed from then on: a row inserted while batches are still
    /// being pulled is not returned, even if it intersects the box. A row
    /// deleted in that window simply stops arriving. This is one consistent
    /// answer rather than a mixture, and it is also what makes the read fast:
    /// pages walk candidate key ranges instead of re-querying the index.
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
        // to answer it with. The RTree is scanned exactly once, here, and its
        // candidate ids become key segments the pages walk with an ordinary
        // range bound; an earlier shape re-evaluated an `IN (SELECT ... FROM
        // rtree)` subquery on every page, which re-scanned the index once per
        // batch and measured as most of the filtered read's overhead
        // (benchmarks/2026-08-02-threaded-filtered-read.md). A fixed candidate
        // set also gives the read one consistent answer: rows inserted after
        // this point are not returned, exactly as a snapshot would.
        let (spatial_sql, segments) = match filter {
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
                let mut stmt = self.gpkg().connection().prepare_cached(&format!(
                    "SELECT id FROM {rtree} \
                     WHERE minx <= ?1 AND maxx >= ?2 AND miny <= ?3 AND maxy >= ?4"
                ))?;
                let mut ids: Vec<i64> = stmt
                    .query_map(
                        [
                            crate::layer::widen_up(bbox.max_x),
                            crate::layer::widen_down(bbox.min_x),
                            crate::layer::widen_up(bbox.max_y),
                            crate::layer::widen_down(bbox.min_y),
                        ],
                        |row| row.get(0),
                    )?
                    .collect::<rusqlite::Result<_>>()?;
                ids.sort_unstable();
                (
                    format!(" AND {key} <= ?{}", base + 3),
                    Some(segment_runs(&ids)),
                )
            }
            // No index, or no filter: a full scan. When a filter is set the
            // exact re-test still runs over every row, which is what
            // `features_in` falls back to as well.
            _ => (String::new(), None),
        };
        let geometry_index = self.geometry_column().and_then(|geom| {
            schema
                .fields()
                .iter()
                .position(|field| *field.name() == geom.column_name)
        });
        // A bounding-box filter needs each candidate's geometry for the exact
        // re-test, and a projection may have left it out of the schema. It is
        // then selected as a hidden trailing column: read for the test,
        // appended to no builder, absent from the batches. The row path does
        // the same, wordlessly, by reading the geometry and not carrying it
        // into the feature.
        let mut row_columns = row_columns;
        let hidden_geometry = match (filter, geometry_index, self.geometry_column()) {
            (Some(_), None, Some(geom)) => {
                row_columns.push(',');
                row_columns.push_str(&quote(&geom.column_name)?);
                Some(schema.fields().len() + usize::from(key_field.is_none()))
            }
            _ => None,
        };
        let rows_sql = format!(
            "SELECT {row_columns} FROM {table} WHERE {user_and}{key} >= ?{key_slot}{spatial_sql} \
             ORDER BY {key} LIMIT ?{limit_slot}"
        );
        let sql = rows_sql.clone();
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
            source: BatchSource::Sequential(Box::new(SequentialBatches {
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
                // A segmented read starts at its first segment; an empty
                // candidate set has nothing to read at all, and issues no
                // query to find that out.
                next_key: match &segments {
                    Some(segments) => segments.first().map_or(i64::MIN, |(lo, _)| *lo),
                    None => i64::MIN,
                },
                exhausted: segments.as_ref().is_some_and(Vec::is_empty),
                segments,
                segment: 0,
                user_params,
                hidden_geometry,
                filter: filter.map(|bbox| Box::new(SpatialFilter { bbox })),
                aggregate,
            })),
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
