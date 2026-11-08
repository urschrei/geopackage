use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

use arrow_schema::DataType;
use geopackage_core::ident::quote;
use rusqlite::OptionalExtension;
use rusqlite::limits::Limit;

use crate::{BoundingBox, Error, Layer, Result};

use super::aggregate::AggregateState;
use super::aggregate::BatchFiller;
use super::options::{ArrowReadOptions, DEFAULT_BATCH_SIZE, DEFAULT_MAX_BATCH_BYTES};
use super::parallel::{ParallelBatches, WorkerPlan, database_path, key_span};
use super::sequential::{Segment, SequentialBatches, SpatialFilter};
use super::{ArrowBatches, BatchSource};

/// Alias for the pagination key when it is not itself one of the columns, so
/// the aggregate can name it as an argument.
const KEY_ALIAS: &str = "__gpkg_key";

impl Layer<'_> {
    /// Reads this layer as a stream of Arrow [`RecordBatch`](arrow_array::RecordBatch)es.
    ///
    /// By default it reads on several threads where it can, which is the
    /// common case. See [Threading](#threading) below for the conditions, and
    /// for how to use a single thread instead.
    ///
    /// Attribute columns follow the mapping in the
    /// [module documentation](super). The geometry column is WKB with the
    /// `geoarrow.wkb` extension name.
    ///
    /// This doesn't go through [`crate::Feature`] or [`crate::Value`]. Arrow
    /// arrays are built directly from the statement's column values. That is
    /// the reason for this path: GDAL measured its generic implementation,
    /// which does go through a per-row feature object, as *slower* than the
    /// row API it wraps.
    ///
    /// # Threading
    ///
    /// `options.threads` defaults to `min(4, available parallelism)`. Set it to
    /// `1` to read only on the calling thread.
    ///
    /// Threads are used only when all of the following are true. Otherwise the
    /// read is single-threaded instead of failing.
    ///
    /// - **The database is a file.** Workers read through their own
    ///   connections, and a `:memory:` database is private to the connection
    ///   that created it.
    /// - **The primary key spans at least two batches**: its largest and
    ///   smallest values differ by at least `2 * batch_size - 1`. Below that,
    ///   opening connections and starting threads costs more than it saves.
    /// - **The layer reads every column.** A layer from
    ///   [`Self::with_columns`] or [`Self::without_geometry`] reads on one
    ///   thread, because the workers rebuild the layer from its table name and
    ///   would read every column.
    ///
    /// Workers open their connections **read-only**. Several connections can
    /// therefore read one table safely without agreeing on a snapshot, because
    /// there is no writer to race.
    ///
    /// Batches arrive in primary-key order regardless of thread count. On
    /// threads, each batch holds the rows of `batch_size` consecutive key
    /// values, so a batch has fewer rows where the key has gaps.
    ///
    /// Dropping the reader before it is drained stops the workers. The drop
    /// waits for each worker to finish its current batch, so it can block for
    /// as long as one batch takes to read.
    ///
    /// # Consistency
    ///
    /// Each batch is a separate query, paginated on the primary key, so a
    /// concurrent writer can change the table between batches. Wrap the read in
    /// your own transaction on [`crate::GeoPackage::connection`] if you need a
    /// stable snapshot across the whole layer. Running each batch as its own
    /// query lets batches be fetched by key range, which the threaded path
    /// needs.
    ///
    /// # Errors
    ///
    /// [`Error`] if the schema cannot be introspected or the query cannot be
    /// prepared, or if a worker connection cannot be opened. Per-batch failures
    /// surface through the iterator.
    #[hotpath::measure(label = "Layer::read_arrow")]
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

    /// Reads the layer's rows intersecting `bbox` as Arrow record batches.
    ///
    /// This is the columnar equivalent of [`Layer::features_in`], and returns
    /// the same rows, in primary-key order. [`Layer::features_in`] returns
    /// them in the order its query produces, which on an indexed layer is the
    /// index's. It is single-threaded. The parallel
    /// path gives key *windows* to workers and assumes that a window's key
    /// span implies its row count. A spatial filter breaks that assumption,
    /// because matching rows are scattered through the key space. Whether a
    /// threaded filtered read would be faster is a separate question, to be
    /// answered by measuring it against this one.
    ///
    /// The exception is a box that selects every row: one containing the
    /// bounds of the layer's spatial index, widened by one `f32` step, on a
    /// layer where every row is indexed. That read is [`Layer::read_arrow`],
    /// with its threads and its aggregate path, since the filter would remove
    /// nothing. A map canvas zoomed out to the whole layer asks for this.
    ///
    /// Like [`Layer::features_in`], it uses the RTree index if the layer has
    /// one, and otherwise falls back to a full scan with the same exact
    /// filter. Either way, each candidate's geometry is re-tested against its
    /// true `f64` envelope. The index stores `f32` envelopes and is queried
    /// with widened bounds, so its candidates are a superset.
    ///
    /// The index is scanned once, when the read is opened, and the candidate
    /// set is then fixed. A row inserted while batches are still being read is
    /// not returned, even if it intersects the box. A row deleted in that time
    /// stops being returned. This gives one consistent answer instead of a
    /// mixture. It also makes the read fast, because pages walk candidate key
    /// ranges instead of re-querying the index.
    ///
    /// A batch can have fewer rows than the batch size even when more rows
    /// remain, because filtering removes candidates after the query has
    /// limited them. The byte ceiling already has the same effect, so if you
    /// read until `None`, this makes no difference.
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
        let indexed = self.has_spatial_index()?;
        if indexed && self.box_selects_every_row(&bbox)? {
            return self.read_arrow(options);
        }
        self.read_arrow_filtered(options, Some((bbox, indexed)), None)
    }

    /// Reads the rows matching a caller-supplied `WHERE` clause as Arrow
    /// record batches.
    ///
    /// This is the columnar equivalent of [`Layer::select`], with the same
    /// contract. `where_clause` is appended, in parentheses, to the query. It
    /// is **raw SQL, trusted from the caller**, and this crate doesn't parse
    /// or sanitise it. Its placeholders are `?1` to `?N`, and `params` bind in
    /// slice order, exactly as [`Layer::select`] binds them. The pagination
    /// this read adds around the clause is numbered after `N`, so the two
    /// never collide.
    ///
    /// To read a single row, use `fid = ?1` with the key as its parameter.
    ///
    /// The filter runs inside SQLite, so unlike [`Layer::read_arrow_in`] there
    /// is no client-side re-test, and only the byte ceiling cuts a batch
    /// short. Like every filtered read, it is single-threaded. The parallel
    /// path gives key windows to workers and assumes that a window's key span
    /// implies its row count, which a filter breaks.
    ///
    /// # Errors
    ///
    /// As [`Layer::read_arrow`]. A clause that SQLite can't prepare is
    /// reported through the iterator's first item, because each batch is its
    /// own query.
    pub fn read_arrow_where(
        &self,
        where_clause: &str,
        params: &[crate::ValueRef<'_>],
        options: ArrowReadOptions,
    ) -> Result<ArrowBatches<'_>> {
        self.read_arrow_filtered(options, None, Some((where_clause, params)))
    }

    /// Reads the rows intersecting `bbox` **and** matching a caller-supplied
    /// `WHERE` clause, as Arrow record batches.
    ///
    /// The result is the intersection of the rows from
    /// [`Layer::features_in`] and [`Layer::select`], in primary-key order. The
    /// bounding box uses the RTree index as [`Layer::read_arrow_in`] does,
    /// including the exact re-test. The clause has the same contract as in
    /// [`Layer::read_arrow_where`], including its `?1` to `?N` placeholder
    /// numbering. A box that selects every row, as described there, makes
    /// this read [`Layer::read_arrow_where`].
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
        let indexed = self.has_spatial_index()?;
        if indexed && self.box_selects_every_row(&bbox)? {
            return self.read_arrow_where(where_clause, params, options);
        }
        self.read_arrow_filtered(options, Some((bbox, indexed)), Some((where_clause, params)))
    }

    /// Whether `bbox` selects every row of the layer, so that a filtered read
    /// can be the unfiltered one, which can use the aggregate path and threads.
    ///
    /// Two facts make it so. The RTree's root bounds, widened by one `f32`
    /// step, lie inside the box, and they contain every indexed envelope,
    /// since an entry is the envelope rounded to `f32`, outwards except in the
    /// subnormal range, where it can be inward by less than a step; so every
    /// indexed row passes the exact re-test. And the index has an entry for
    /// every row, so no row is left out because it has no geometry or an
    /// empty one.
    ///
    /// The root bounds are one node read, about 4 µs, which is all a smaller
    /// box pays. The two counts run only when the bounds already lie inside
    /// the box. Each walks its table's pages: about 3 ms together for 200,000
    /// points, against a full read several times that.
    ///
    /// The caller has established that the layer has a usable index
    /// ([`Layer::has_spatial_index`]). It trusts that index's entries, as
    /// every indexed read does; an index whose entries have drifted from the
    /// table is what [`Layer::audit_spatial_index`] finds.
    fn box_selects_every_row(&self, bbox: &BoundingBox) -> Result<bool> {
        let Some(geom) = self.geometry_column() else {
            return Ok(false);
        };
        let conn = self.gpkg().connection();
        let rtree =
            geopackage_core::triggers::rtree_table_name(self.table_name(), &geom.column_name);
        let root: Option<Vec<u8>> = conn
            .prepare_cached(&format!(
                "SELECT data FROM {} WHERE nodeno = 1",
                quote(&format!("{rtree}_node"))?
            ))?
            .query_row([], |row| row.get(0))
            .optional()?;
        let Some([min_x, max_x, min_y, max_y]) =
            root.as_deref().and_then(crate::packed::node_bounds)
        else {
            return Ok(false);
        };
        // A stored bound can be inward of the true one by less than one `f32`
        // step: the RTree module stores a subnormal value as the nearest
        // `f32`. So the box must hold the bounds widened by one step.
        // NaN anywhere (a NaN box, or a corrupt node) fails every comparison,
        // so it falls back to the ordinary read rather than selecting all.
        let inside = bbox.min_x <= f64::from(min_x.next_down())
            && bbox.max_x >= f64::from(max_x.next_up())
            && bbox.min_y <= f64::from(min_y.next_down())
            && bbox.max_y >= f64::from(max_y.next_up());
        if !inside {
            return Ok(false);
        }
        let count = |table: &str| -> Result<i64> {
            Ok(conn.query_row(
                &format!("SELECT count(*) FROM {}", quote(table)?),
                [],
                |row| row.get(0),
            )?)
        };
        Ok(count(&format!("{rtree}_rowid"))? == count(self.table_name())?)
    }

    /// Returns the single-threaded reader. The threaded reader falls back to
    /// it, and its workers are built from it.
    fn read_arrow_sequential(&self, options: ArrowReadOptions) -> Result<ArrowBatches<'_>> {
        self.read_arrow_filtered(options, None, None)
    }

    /// Returns the single-threaded reader, optionally filtered to a bounding
    /// box, with whether the layer has a usable spatial index, and a
    /// caller-supplied `WHERE` clause.
    #[hotpath::measure(label = "arrow::read_arrow_filtered")]
    fn read_arrow_filtered(
        &self,
        options: ArrowReadOptions,
        filter: Option<(BoundingBox, bool)>,
        sql_filter: Option<(&str, &[crate::ValueRef<'_>])>,
    ) -> Result<ArrowBatches<'_>> {
        let indexed = filter.is_some_and(|(_, indexed)| indexed);
        let filter = filter.map(|(bbox, _)| bbox.for_query());
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
        // the reader can paginate.
        let (row_columns, aggregate_arguments) = match key_field {
            Some(_) => (selected.clone(), selected),
            None => (
                format!("{key} AS \"{KEY_ALIAS}\",{selected}"),
                format!("{key},{selected}"),
            ),
        };
        let table = quote(self.table_name())?;
        // The caller's `WHERE` clause keeps [`Layer::select`]'s contract: its
        // placeholders are `?1` to `?N` and its params bind in slice order. The
        // key, limit and upper key bound are numbered explicitly after `N`, so
        // with no clause the text degenerates to the unfiltered one (`?1` key,
        // `?2` limit, `?3` upper bound) and every path binds the same way:
        // user params first, then key, limit and upper bound.
        let base = sql_filter.map_or(0, |(_, params)| params.len());
        let (key_slot, limit_slot, upper_slot) = (base + 1, base + 2, base + 3);
        let user_params: Vec<rusqlite::types::Value> = sql_filter
            .map(|(_, params)| params.iter().map(crate::value::value_ref_to_sql).collect())
            .unwrap_or_default();
        let user_and = match sql_filter {
            Some((clause, _)) => format!("({clause}) AND "),
            None => String::new(),
        };
        // The spatial predicate, when there is one and the layer has an index
        // to answer it with. The RTree is scanned exactly once, here, and its
        // sorted candidate ids are planned into segments of at most one page
        // of ids each (`Segment::plan`): a dense segment is read as a key
        // range, a sparse one by its explicit ids. Two earlier shapes cost
        // more. An `IN (SELECT ... FROM rtree)` subquery is re-evaluated on
        // every page, re-scanning the index once per batch. Walking runs of
        // consecutive keys costs a query per run, and spatially scattered
        // rows make nearly every candidate its own run: a box selecting 1% of
        // 200,000 scattered points issued about 2,000 queries. A fixed
        // candidate set also gives the read one consistent answer: rows
        // inserted after this point are not returned, exactly as a snapshot
        // would.
        let segments = match filter {
            Some(bbox) if indexed => {
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
                Some(Segment::plan(&ids, options.batch_size.max(1)))
            }
            // No index, or no filter: a full scan. When a filter is set the
            // exact re-test still runs over every row, which is what
            // `features_in` falls back to as well.
            _ => None,
        };
        // The upper key bound is the current segment's last key on a segmented
        // read, a parallel worker's window end, and otherwise `i64::MAX`.
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
        // A box with no index to answer it is tested in SQL as well, so that
        // SQLite does not read the other columns of a row outside it. Its four
        // values follow the upper bound.
        let scan_filter = match (filter, &segments, self.geometry_column()) {
            (Some(_), None, Some(geom)) => format!(
                " AND ST_EnvIntersects({}, ?{}, ?{}, ?{}, ?{})",
                quote(&geom.column_name)?,
                base + 4,
                base + 5,
                base + 6,
                base + 7
            ),
            _ => String::new(),
        };
        let rows_sql = format!(
            "SELECT {row_columns} FROM {table} \
             WHERE {user_and}{key} >= ?{key_slot} AND {key} <= ?{upper_slot}{scan_filter} \
             ORDER BY {key} LIMIT ?{limit_slot}"
        );
        let sql = rows_sql.clone();
        // A sparse segment's query: the same, with its candidate ids bound as
        // one `rarray` parameter after the upper bound. A segmented read never
        // has the unindexed scan's box parameters, so the slot is free.
        let ids_sql = format!(
            "SELECT {row_columns} FROM {table} \
             WHERE {user_and}{key} >= ?{key_slot} AND {key} <= ?{upper_slot} \
             AND {key} IN rarray(?{ids_slot}) \
             ORDER BY {key} LIMIT ?{limit_slot}",
            ids_slot = base + 4
        );
        let conn = self.gpkg().connection();
        let conversion = self.conversion_options();
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
        //
        // The aggregate also relies on SQLite scanning the key range in
        // ascending order, which `PRAGMA reverse_unordered_selects` reverses.
        let reversed: bool =
            conn.query_row("PRAGMA reverse_unordered_selects", [], |row| row.get(0))?;
        let aggregate = if filter.is_none()
            && sql_filter.is_none()
            && !reversed
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
                    key_in_fields: key_field
                        .and_then(|index| schema.fields().get(index))
                        .is_some_and(|field| *field.data_type() == DataType::Int64),
                    geometry_index,
                    conversion,
                    capacity: options.batch_size.clamp(1, DEFAULT_BATCH_SIZE),
                    limit: options.batch_size.max(1),
                    max_bytes: options.max_batch_bytes.clamp(1, DEFAULT_MAX_BATCH_BYTES),
                    row_bytes: schema
                        .fields()
                        .iter()
                        .map(|_| AtomicUsize::new(0))
                        .collect(),
                    output: Arc::new(Mutex::new(None)),
                    failure: Arc::new(Mutex::new(None)),
                },
            )?)
        } else {
            None
        };

        // No `LIMIT` and no `ORDER BY`: the aggregate stops the scan itself when
        // the batch is full (see `aggregate::stop`), and a range on the integer
        // primary key is scanned in key order. A `LIMIT` would need a subquery,
        // and SQLite copies every text and blob value out of one.
        let aggregate_sql = aggregate
            .as_ref()
            .map(|state| {
                format!(
                    "SELECT {}({aggregate_arguments}) FROM {table} \
                     WHERE {key} >= ?1 AND {key} <= ?2",
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
                conversion,
                batch_size: options.batch_size.max(1),
                max_batch_bytes: options.max_batch_bytes.clamp(1, DEFAULT_MAX_BATCH_BYTES),
                upper_key: i64::MAX,
                // A segmented read starts at its first segment; an empty
                // candidate set has nothing to read at all, and issues no
                // query to find that out.
                next_key: match &segments {
                    Some(segments) => segments.first().map_or(i64::MIN, |s| s.lo),
                    None => i64::MIN,
                },
                exhausted: segments.as_ref().is_some_and(Vec::is_empty),
                segments,
                ids_sql,
                segment: 0,
                user_params,
                hidden_geometry,
                filter: filter.map(|bbox| Box::new(SpatialFilter { bbox })),
                aggregate,
            })),
        })
    }

    /// Returns the worker pool for this read, or `None` if the conditions in
    /// [`Self::read_arrow`] don't hold.
    fn parallel_source(&self, options: ArrowReadOptions) -> Result<Option<ParallelBatches>> {
        let Some(path) = database_path(self.gpkg().connection())? else {
            return Ok(None);
        };
        let Some(key) = self.primary_key_column() else {
            return Ok(None);
        };
        let Some((first, last)) = key_span(self.gpkg().connection(), self.table_name(), key)?
        else {
            return Ok(None);
        };
        let batch_size = options.batch_size.max(1);
        // Below two windows there is nothing to overlap, and opening
        // connections and starting threads would cost more than it saves.
        let keys = last.abs_diff(first).saturating_add(1);
        if keys < (batch_size as u64).saturating_mul(2) {
            return Ok(None);
        }
        let mmap_size: i64 = self
            .gpkg()
            .connection()
            .query_row("PRAGMA mmap_size", [], |row| row.get(0))?;
        Ok(Some(ParallelBatches::spawn(&WorkerPlan {
            path,
            table: self.table_name().to_owned(),
            key: key.to_owned(),
            conversion: self.conversion_options(),
            first,
            last,
            batch_size,
            max_batch_bytes: options.max_batch_bytes.clamp(1, DEFAULT_MAX_BATCH_BYTES),
            threads: options.resolved_threads(),
            mmap_size: u64::try_from(mmap_size).unwrap_or(0),
        })))
    }
}

#[cfg(test)]
mod tests {
    use crate::{BoundingBox, GeoPackage, GeometrySpec, TableSchemaBuilder};
    use geo_types::Point;
    use geopackage_core::types::GeometryType;

    /// An indexed point layer with a point at `(i, 2i)` for each `i` in
    /// `0..count`.
    fn layer(count: i32) -> (tempfile::TempDir, GeoPackage) {
        let dir = tempfile::tempdir().unwrap();
        let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
        let layer = gpkg
            .create_layer(
                &TableSchemaBuilder::new("pts")
                    .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
            )
            .unwrap();
        let mut writer = layer.writer().unwrap();
        for i in 0..count {
            writer
                .insert(None, &Point::new(f64::from(i), f64::from(2 * i)), &[])
                .unwrap();
        }
        writer.commit().unwrap();
        (dir, gpkg)
    }

    /// Whether a filtered read of `bbox` would be the unfiltered read.
    fn selects_all(gpkg: &GeoPackage, bbox: BoundingBox) -> bool {
        let layer = gpkg.layer("pts").unwrap();
        layer.has_spatial_index().unwrap() && layer.box_selects_every_row(&bbox).unwrap()
    }

    #[test]
    fn a_box_selects_every_row_when_it_holds_the_index_bounds() {
        let (_dir, gpkg) = layer(100);
        let out = |v: f32, up: bool| f64::from(if up { v.next_up() } else { v.next_down() });
        let (min_x, min_y, max_x, max_y) = (
            out(0.0, false),
            out(0.0, false),
            out(99.0, true),
            out(198.0, true),
        );
        assert!(selects_all(
            &gpkg,
            BoundingBox::new(min_x, min_y, max_x, max_y)
        ));
        assert!(selects_all(&gpkg, BoundingBox::new(-1.0, -1.0, 1e6, 1e6)));
        // Exactly the index's bounds is not enough: a stored bound may be
        // inward of the true one by less than a step.
        assert!(!selects_all(&gpkg, BoundingBox::new(0.0, 0.0, 99.0, 198.0)));
        // Short on each side in turn.
        for bbox in [
            BoundingBox::new(0.0, min_y, max_x, max_y),
            BoundingBox::new(min_x, 0.0, max_x, max_y),
            BoundingBox::new(min_x, min_y, 99.0, max_y),
            BoundingBox::new(min_x, min_y, max_x, 198.0),
        ] {
            assert!(!selects_all(&gpkg, bbox), "{bbox:?}");
        }
    }

    #[test]
    fn a_subnormal_coordinate_stored_inward_is_not_selected_by_a_tight_box() {
        // The RTree module stores 3.5e-46 as 0.0, the nearest `f32`, so the
        // root bounds are the zero box; a zero box must still filter the row
        // out, because the point lies outside it.
        let (_dir, gpkg) = layer(0);
        let mut writer = gpkg.layer("pts").unwrap().writer().unwrap();
        writer.insert(None, &Point::new(0.0, 3.5e-46), &[]).unwrap();
        writer.commit().unwrap();
        assert!(!selects_all(&gpkg, BoundingBox::new(0.0, 0.0, 0.0, 0.0)));
        let read = gpkg
            .layer("pts")
            .unwrap()
            .read_arrow_in(BoundingBox::new(0.0, 0.0, 0.0, 0.0), Default::default())
            .unwrap()
            .map(|batch| batch.unwrap().num_rows())
            .sum::<usize>();
        assert_eq!(read, 0);
    }

    #[test]
    fn a_row_missing_from_the_index_means_the_box_must_filter() {
        let (_dir, gpkg) = layer(10);
        gpkg.connection()
            .execute("INSERT INTO pts (geom) VALUES (NULL)", [])
            .unwrap();
        assert!(!selects_all(&gpkg, BoundingBox::new(-1e6, -1e6, 1e6, 1e6)));
    }

    #[test]
    fn no_rows_or_no_index_never_select_every_row() {
        let everything = BoundingBox::new(-1e6, -1e6, 1e6, 1e6);
        let (_dir, gpkg) = layer(0);
        assert!(!selects_all(&gpkg, everything));
        let (_dir, gpkg) = layer(10);
        gpkg.layer("pts").unwrap().drop_spatial_index().unwrap();
        assert!(!selects_all(&gpkg, everything));
    }
}
