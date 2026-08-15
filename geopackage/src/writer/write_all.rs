use geo_traits::GeometryTrait;
use geopackage_core::ident::quote;
use geopackage_core::triggers;
use geopackage_core::types::{GeometryTypeSet, ZmFlag};
use rusqlite::Connection;

use crate::bulk::{self, BulkIndexOptions};
use crate::index::drop_all_rtree_triggers;
use crate::transaction::WriteTransaction;
use crate::{Error, Layer, Result};

use super::constraints::ColumnConstraints;
use super::feature_writer::{
    FeatureWriter, Shape, ValueColumn, build_insert_sql, build_partial_update_sql, build_update_sql,
};
use super::row::{NewFeature, WritableRow};

/// How large a `write_all` must be, relative to the rows already in a spatial
/// index, before the bulk path rebuilds that index instead of adding its
/// entries to it.
///
/// A write of at least `existing / MERGE_REBUILD_RATIO` new entries takes the
/// rebuild. See [`rebuild_beats_append`] for the measurements behind the value.
const MERGE_REBUILD_RATIO: usize = 10;

/// The geometry column a [`FeatureWriter`] targets.
#[derive(Debug)]
pub(crate) struct GeomTarget {
    pub(crate) name: String,
    pub(crate) quoted_name: String,
    pub(crate) srs_id: i32,
    pub(crate) z: ZmFlag,
    pub(crate) m: ZmFlag,
}

/// A running union of written XY envelopes, seeded from the existing
/// `gpkg_contents` bounding box. Bounds are stored as
/// `[min_x, max_x, min_y, max_y]` (the shape [`encode_gpb`] returns).
#[derive(Debug, Clone, Copy)]
pub(crate) struct BboxFold {
    pub(crate) min_x: f64,
    pub(crate) max_x: f64,
    pub(crate) min_y: f64,
    pub(crate) max_y: f64,
    pub(crate) seen: bool,
}

impl BboxFold {
    pub(crate) fn new() -> Self {
        Self {
            min_x: f64::INFINITY,
            max_x: f64::NEG_INFINITY,
            min_y: f64::INFINITY,
            max_y: f64::NEG_INFINITY,
            seen: false,
        }
    }

    pub(crate) fn seed(&mut self, existing: Option<[f64; 4]>) {
        if let Some([min_x, max_x, min_y, max_y]) = existing {
            self.min_x = min_x;
            self.max_x = max_x;
            self.min_y = min_y;
            self.max_y = max_y;
            self.seen = true;
        }
    }

    pub(crate) fn add(&mut self, [min_x, max_x, min_y, max_y]: [f64; 4]) {
        self.min_x = self.min_x.min(min_x);
        self.max_x = self.max_x.max(max_x);
        self.min_y = self.min_y.min(min_y);
        self.max_y = self.max_y.max(max_y);
        self.seen = true;
    }

    pub(crate) fn bounds(&self) -> Option<[f64; 4]> {
        self.seen
            .then_some([self.min_x, self.max_x, self.min_y, self.max_y])
    }
}

impl<'a> Layer<'a> {
    /// Begins a write transaction over this layer, returning a
    /// [`FeatureWriter`].
    ///
    /// The writer owns the transaction: stage rows with its `insert`/`update`/
    /// `delete` methods, then call [`FeatureWriter::commit`]. Dropping the
    /// writer without committing rolls the transaction back.
    ///
    /// Unless a transaction is already open on the connection, in which case
    /// the writer joins that one and both of those sentences belong to whoever
    /// began it. [`FeatureWriter::commit`] says what changes.
    pub fn writer(&self) -> Result<FeatureWriter<'a>> {
        self.gpkg().check_writable(self.table_name())?;
        let conn: &Connection = self.gpkg().connection();
        let tx = WriteTransaction::begin(conn)?;
        let existing = self.stored_extent()?;
        // An unusable recorded box over a table that already contains rows
        // makes the fold a lower bound: it can be grown and used, but not
        // recorded.
        let bbox_covers_layer = existing.is_some() || !self.has_rows()?;

        let pk_name = self.primary_key_column();
        let pk_expr = match pk_name {
            Some(pk) => quote(pk)?,
            None => "rowid".to_owned(),
        };
        // The same set the read path yields: the layer's value columns exclude
        // both the geometry and the primary key, which are written through
        // their own arguments.
        let value_columns = self
            .value_columns()
            .iter()
            .map(|c| {
                Ok(ValueColumn {
                    quoted: quote(&c.name)?,
                    name: c.name.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let geometry = match self.geometry_column() {
            Some(g) => Some(GeomTarget {
                name: g.column_name.clone(),
                quoted_name: quote(&g.column_name)?,
                srs_id: g.srs_id,
                z: g.z,
                m: g.m,
            }),
            None => None,
        };
        let mut bbox = BboxFold::new();
        bbox.seed(existing.map(|b| [b.min_x, b.max_x, b.min_y, b.max_y]));
        let quoted_table = quote(self.table_name())?;

        // Compose every statement the writer can issue, then prepare them all,
        // before anything moves into the writer.
        let shape = Shape {
            table_name: self.table_name(),
            quoted_table: &quoted_table,
            pk_expr: &pk_expr,
            value_columns: &value_columns,
            geometry: geometry.as_ref(),
        };
        let insert_stmts = [
            conn.prepare_cached(&build_insert_sql(&shape, false, false))?,
            conn.prepare_cached(&build_insert_sql(&shape, true, false))?,
            conn.prepare_cached(&build_insert_sql(&shape, false, true))?,
            conn.prepare_cached(&build_insert_sql(&shape, true, true))?,
        ];
        let update_stmts = [
            conn.prepare_cached(&build_update_sql(&shape, false))?,
            conn.prepare_cached(&build_update_sql(&shape, true))?,
        ];
        let delete_stmt =
            conn.prepare_cached(&format!("DELETE FROM {quoted_table} WHERE {pk_expr} = ?1"))?;
        let partial_stmt = conn.prepare_cached(&build_partial_update_sql(&shape, &[])?)?;
        let constraints = ColumnConstraints::read(self, conn, &value_columns)?;

        Ok(FeatureWriter {
            tx,
            conn,
            table_name: self.table_name().to_owned(),
            quoted_table,
            pk_expr,
            value_columns,
            geometry,
            bbox,
            insert_stmts,
            update_stmts,
            delete_stmt,
            partial_columns: Vec::new(),
            partial_stmt,
            dirty: false,
            bbox_dirty: false,
            bbox_covers_layer,
            constraints,
            geometry_types: GeometryTypeSet::new(),
        })
    }

    /// Writes every item of `features` in batches, each batch its own
    /// committed transaction.
    ///
    /// `batch_size` bounds how many rows share a transaction (`0` writes them
    /// all in a single transaction). Returns the assigned feature ids in order.
    /// Batches commit independently: an error part-way leaves already-committed
    /// batches in place, so pass `0` when you need all-or-nothing.
    ///
    /// `batch_size` bounds nothing when a transaction is already open on the
    /// connection: the batch commits are staged rather than durable, so every
    /// row belongs to the caller's transaction and an error part-way leaves all
    /// of them staged rather than some of them committed. This follows from
    /// opening the transaction, and matches the all-or-nothing behaviour of
    /// `batch_size = 0`.
    ///
    /// Rows with `geometry: Some(_)` go through [`FeatureWriter::insert`]; rows
    /// with `None` through [`FeatureWriter::insert_row`].
    ///
    /// When the layer has a spatial index and the write is at least
    /// [`DEFAULT_BULK_THRESHOLD`](bulk::DEFAULT_BULK_THRESHOLD) rows, it takes
    /// the bulk path instead, maintaining the index in one operation at the end
    /// rather than row by row through the triggers;
    /// [`Self::write_all_with`] tunes or forces that choice.
    pub fn write_all<G, I>(&self, features: I, batch_size: usize) -> Result<Vec<i64>>
    where
        G: GeometryTrait<T = f64>,
        I: IntoIterator<Item = NewFeature<G>>,
    {
        self.write_all_with(features, batch_size, BulkIndexOptions::default())
    }

    /// [`Self::write_all`] with an explicit [`BulkIndexOptions`] controlling the
    /// bulk-vs-triggered index-build choice.
    ///
    /// The bulk path is taken when the layer has a spatial index, the write
    /// reaches `options.bulk_threshold` rows, and it is large enough relative to
    /// the rows already indexed to be worth rebuilding the index rather than
    /// appending to it. The size condition is settled from the iterator's
    /// `size_hint` where that is possible, and by buffering up to
    /// `bulk_threshold` rows where it is not, so an iterator that does not know
    /// its own length still reaches the path.
    /// [`BulkIndexOptions::always_bulk`] drops the size condition;
    /// [`BulkIndexOptions::never_bulk`] disables the path.
    pub fn write_all_with<G, I>(
        &self,
        features: I,
        batch_size: usize,
        options: BulkIndexOptions,
    ) -> Result<Vec<i64>>
    where
        G: GeometryTrait<T = f64>,
        I: IntoIterator<Item = NewFeature<G>>,
    {
        self.write_all_impl(features, batch_size, options, bulk::no_fault)
    }

    /// The `write_all_with` core, taking a [`bulk::TestFault`] so that a test
    /// can force the index build to fail after the rows have been staged.
    #[hotpath::measure(label = "write_all_impl")]
    pub(crate) fn write_all_impl<R, I>(
        &self,
        features: I,
        batch_size: usize,
        options: BulkIndexOptions,
        fault: bulk::TestFault,
    ) -> Result<Vec<i64>>
    where
        R: WritableRow,
        I: IntoIterator<Item = R>,
    {
        // The chokepoint for every bulk write, the Arrow path included, which
        // is why the check sits here rather than in each public entry point.
        self.gpkg().check_writable(self.table_name())?;
        let mut iter = features.into_iter();
        let (bulk, buffered) = self.bulk_write_engages(&mut iter, options)?;
        // Rows pulled to reach the decision are put back in front of the rest,
        // so either path sees the same sequence it would have seen.
        let features = buffered.into_iter().chain(iter);
        if bulk {
            self.write_all_bulk(features, options, fault)
        } else {
            self.write_all_batched(features, batch_size)
        }
    }

    /// Returns whether this write is large enough to take the bulk path,
    /// along with any rows that had to be pulled from `features` to decide.
    ///
    /// This is the one decision that has to be made before a row is written,
    /// because the bulk path drops the RTree triggers first and dropping them
    /// for a handful of rows would cost more in schema churn than it saves.
    /// Whether the index is then rebuilt or appended to is settled after the
    /// write, from the exact counts (see [`rebuild_beats_append`]).
    ///
    /// The size condition is answered from [`Iterator::size_hint`] whenever the
    /// hint settles it, which covers every `Vec`-like source at no cost. An
    /// iterator that does not know its own length, which is most iterators that
    /// are not backed by a collection, reports a lower bound of `0` and would
    /// otherwise never reach the threshold however many rows it went on to
    /// yield. For those the rows themselves are the only evidence available, so
    /// they are buffered until either the threshold is reached, which is all the
    /// proof the decision needs, or the iterator ends first, which gives an exact
    /// count that is known to be below it.
    ///
    /// Buffering is therefore bounded by `options.bulk_threshold` rows and never
    /// by the length of the input. Raising the threshold raises that bound for
    /// unsized iterators.
    fn bulk_write_engages<R, I>(
        &self,
        features: &mut I,
        options: BulkIndexOptions,
    ) -> Result<(bool, Vec<R>)>
    where
        I: Iterator<Item = R>,
    {
        let threshold = options.bulk_threshold;
        // `never_bulk`: the caller has ruled the bulk path out, so there is
        // nothing to decide and nothing to buffer deciding it.
        if threshold == usize::MAX {
            return Ok((false, Vec::new()));
        }
        let (lower, upper) = features.size_hint();
        // An upper bound below the threshold settles it without touching the
        // database, which is the common case for a small write from a `Vec`.
        if upper.is_some_and(|upper| upper < threshold) {
            return Ok((false, Vec::new()));
        }
        // A lower bound that already clears the threshold settles it the other
        // way, again without pulling a row. `has_spatial_index` already requires
        // a geometry column and a single-column primary key, which are the other
        // two things the bulk path needs, so it is the whole availability test.
        if lower >= threshold {
            return Ok((self.has_spatial_index()?, Vec::new()));
        }
        // Neither bound settles it, so buffer up to the threshold. Testing the
        // layer first means an unindexed layer, which can never take the bulk
        // path, does not buffer rows only to discard the decision.
        if !self.has_spatial_index()? {
            return Ok((false, Vec::new()));
        }
        let mut buffered = Vec::new();
        while buffered.len() < threshold {
            let Some(feature) = features.next() else {
                return Ok((false, buffered));
            };
            buffered.push(feature);
        }
        Ok((true, buffered))
    }

    /// The per-batch triggered write path: one committed transaction per
    /// `batch_size` rows (`0` = a single transaction for the whole iterator).
    #[hotpath::measure(label = "write_all_batched")]
    fn write_all_batched<R, I>(&self, features: I, batch_size: usize) -> Result<Vec<i64>>
    where
        R: WritableRow,
        I: IntoIterator<Item = R>,
    {
        let mut fids = Vec::new();
        let mut iter = features.into_iter();
        let mut batch = self.writer()?;
        let mut in_batch = 0usize;
        let mut wrote_any = false;
        for feature in iter.by_ref() {
            let (fid, _) = feature.write(&mut batch)?;
            fids.push(fid);
            wrote_any = true;
            in_batch += 1;
            if batch_size != 0 && in_batch >= batch_size {
                batch.commit()?;
                batch = self.writer()?;
                in_batch = 0;
                wrote_any = false;
            }
        }
        if wrote_any || in_batch > 0 {
            batch.commit()?;
        } else {
            // Nothing written into the final (fresh) writer: roll it back.
            drop(batch);
        }
        Ok(fids)
    }

    /// The bulk write path: drop the rtree triggers, insert every row in one
    /// transaction (no per-row index maintenance, but `gpkg_contents` bbox and
    /// `last_change` are still maintained by the writer commit), then bring the
    /// index up to date and reinstall the triggers.
    ///
    /// On any failure after the triggers are dropped, the index is restored to a
    /// consistent, trigger-maintained state before the error is returned.
    #[hotpath::measure(label = "write_all_bulk")]
    fn write_all_bulk<R, I>(
        &self,
        features: I,
        options: BulkIndexOptions,
        fault: bulk::TestFault,
    ) -> Result<Vec<i64>>
    where
        R: WritableRow,
        I: IntoIterator<Item = R>,
    {
        let geom = self
            .geometry_column()
            .ok_or_else(|| Error::NoGeometryColumn {
                table_name: self.table_name().to_owned(),
            })?;
        let pk = self
            .primary_key_column()
            .ok_or_else(|| Error::NoPrimaryKey {
                table_name: self.table_name().to_owned(),
            })?;
        let table = self.table_name();
        let column = &geom.column_name;
        let rtree = triggers::rtree_table_name(table, column);

        (|| -> Result<Vec<i64>> {
            let mut fids = Vec::new();
            let mut entries = Vec::new();
            let mut writer = self.writer()?;
            // Inside the writer's transaction, so a failure or a crash rolls the
            // trigger drop back along with everything else.
            drop_all_rtree_triggers(writer.connection(), table, column)?;

            // Both counts describe the table as it was before this write, and
            // both are read here rather than earlier because dropping the
            // triggers is this transaction's first write statement and so the
            // point at which SQLite hands it the write lock. Read before that,
            // there is a window in which another connection commits a row
            // between the count and the lock, and `table_was_empty` in
            // particular cannot survive that: the entry set would then be
            // missing that row, and the gate cannot notice, because it checks
            // the built index against that same set.
            let conn = writer.connection();
            // Envelopes computed while encoding can be reused as the RTree entry
            // set only if this write accounts for every indexable row in the
            // table. An empty table before the write is the cheap, sufficient
            // proof; otherwise `fill_index` re-derives the set with its own
            // `ST_*` scan.
            let table_was_empty = bulk::table_row_count(conn, table)? == 0;
            let indexed = rtree_entry_count(conn, &rtree)?;

            for feature in features {
                let (fid, envelope) = feature.write(&mut writer)?;
                if let Some(envelope) = envelope {
                    entries.push((fid, envelope));
                }
                fids.push(fid);
            }
            // Flush the catalogue metadata but keep the transaction open, so the
            // rows and the index commit together. The index work below runs
            // against `conn`, which is the connection that transaction belongs
            // to, so it is inside it whether the transaction is ours or the
            // caller's.
            let tx = writer.flush()?;

            // Reinstalling the trigger set is the last thing either branch does,
            // and it happens inside the same transaction as the drop, so a
            // failure anywhere rolls both back together.
            let reinstall = |conn: &Connection| -> Result<()> {
                for sql in triggers::create_triggers_sql(table, column, pk)? {
                    conn.execute_batch(&sql)?;
                }
                Ok(())
            };

            if rebuild_beats_append(entries.len(), indexed) {
                let precomputed = table_was_empty.then_some(entries);
                bulk::fill_index_in_transaction(
                    conn,
                    table,
                    column,
                    pk,
                    &rtree,
                    options,
                    precomputed,
                    fault,
                    reinstall,
                )?;
            } else {
                append_entries(conn, &rtree, &entries)?;
                // The rebuild branch hands this to `fill_index`, which calls it
                // at the equivalent point. Calling it here as well is what lets
                // a test fail this branch too, once its index work is done.
                fault(conn, &rtree)?;
                reinstall(conn)?;
            }
            tx.commit()?;
            Ok(fids)
        })()
    }
}

/// Returns `true` if a bulk `write_all` that produced `new_entries` index
/// entries against an index already containing `indexed` of them should
/// rebuild that index rather than append the new entries to it.
///
/// This is decided after the rows are written rather than before, so both counts
/// are exact. Deciding it up front meant guessing the size of the write from
/// [`Iterator::size_hint`], which can only ever supply a lower bound, and an
/// iterator that supplies none at all could not be placed on either side of the
/// ratio.
///
/// An empty index is the clear case: there is nothing to preserve, so the
/// rebuild is a straight win. A populated index is a trade, because a rebuild
/// pays for the rows already in it. Measured at 1M and 100k existing rows, a
/// rebuild costs roughly 1.5 us per row of the *total* table while an append
/// costs roughly 18 to 40 us per *new* row, rising with table size as the index
/// deepens. Rebuilding therefore wins once the new entries are somewhere between
/// 5% and 10% of the existing ones. [`MERGE_REBUILD_RATIO`] takes the
/// conservative end: at 100k existing rows a 10k-row append measured 187 ms
/// against 149 ms rebuilt, and at 1M existing a 100k-row append measured 2938 ms
/// against 1783 ms.
pub(crate) fn rebuild_beats_append(new_entries: usize, indexed: usize) -> bool {
    if indexed == 0 {
        return true;
    }
    new_entries >= indexed / MERGE_REBUILD_RATIO
}

/// Adds one RTree entry per newly written row, leaving the existing index in
/// place.
///
/// This is the work the `_insert` trigger would have done had it still been
/// installed: the same `INSERT OR REPLACE`, the same values, in the same row
/// order, so the index this leaves behind is the one a triggered write would
/// have produced. The envelopes were computed while encoding the geometries, and
/// a row whose geometry is NULL or empty contributed none, which is exactly the
/// trigger's `NEW.geom NOT NULL AND NOT ST_IsEmpty(NEW.geom)` condition.
///
/// Nothing gates the result. The bulk build is gated because it writes a tree by
/// hand into an on-disk format SQLite does not document as an interface; these
/// inserts go through the RTree module itself and need no more checking than the
/// triggers do.
pub(crate) fn append_entries(
    conn: &Connection,
    rtree: &str,
    entries: &[(i64, [f64; 4])],
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let sql = format!(
        "INSERT OR REPLACE INTO {} VALUES (?1, ?2, ?3, ?4, ?5)",
        quote(rtree)?
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    for &(fid, [min_x, max_x, min_y, max_y]) in entries {
        stmt.execute(rusqlite::params![fid, min_x, max_x, min_y, max_y])?;
    }
    Ok(())
}

/// The number of entries currently in the RTree `rtree`.
pub(crate) fn rtree_entry_count(conn: &Connection, rtree: &str) -> Result<usize> {
    let count: i64 = conn.query_row(
        &format!("SELECT count(*) FROM {}", quote(rtree)?),
        [],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(count).unwrap_or(usize::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GeoPackage, GeometrySpec, TableSchemaBuilder};
    use geo_types::Point;
    use geopackage_core::types::GeometryType;

    /// A [`bulk::TestFault`] that fails the index build outright, standing in
    /// for a crash or an I/O error between staging the rows and rebuilding the
    /// index.
    fn fail_the_build(_: &Connection, _: &str) -> Result<()> {
        Err(Error::NoSpatialIndex {
            table_name: "pts".to_owned(),
            column_name: "geom".to_owned(),
        })
    }

    /// An indexed layer over an empty table, which is the state that makes
    /// `write_all` take the bulk path.
    fn indexed_empty_layer() -> (tempfile::TempDir, GeoPackage) {
        let dir = tempfile::tempdir().unwrap();
        let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
        let layer = gpkg
            .create_layer(
                &TableSchemaBuilder::new("pts")
                    .geometry(GeometrySpec::new(GeometryType::Point, 4326))
                    // Created here, so the test controls when it exists.
                    .spatial_index(false),
            )
            .unwrap();
        layer.create_spatial_index().unwrap();
        (dir, gpkg)
    }

    /// A bulk `write_all` that fails during the index build must leave nothing
    /// behind: not the rows, not a half-built index, not a dropped trigger set.
    ///
    /// This is the atomicity the bulk path used to lack. The rebuild ran in its
    /// own transaction, because building the index in an `ATTACH`ed scratch
    /// database required autocommit, so the rows were already committed by the
    /// time it ran and a failure here left them against a stale index. The
    /// assertions below all fail against that arrangement.
    #[test]
    fn failed_bulk_write_rolls_back_rows_and_index() {
        let (_dir, gpkg) = indexed_empty_layer();
        let layer = gpkg.layer("pts").unwrap();

        let features: Vec<NewFeature<Point<f64>>> = (1..=50)
            .map(|i| {
                let f = f64::from(i);
                NewFeature::new(Point::new(f, -f), Vec::new()).with_fid(i64::from(i))
            })
            .collect();

        let result =
            layer.write_all_impl(features, 0, BulkIndexOptions::always_bulk(), fail_the_build);
        assert!(result.is_err(), "the build should have failed here");

        // No rows: the inserts rolled back with the failed build.
        let rows: i64 = gpkg
            .connection()
            .query_row("SELECT count(*) FROM pts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "rows survived a failed bulk build");

        // The triggers are dropped inside the same transaction, so the rollback
        // restores them and the index is usable without a repair.
        assert_eq!(
            layer.spatial_index_status().unwrap(),
            crate::SpatialIndexStatus::Current,
            "index left desynchronised by a failed bulk build"
        );

        // And the layer still works: a later write is indexed as normal.
        let mut writer = layer.writer().unwrap();
        writer.insert(Some(1), &Point::new(5.0, 5.0), &[]).unwrap();
        writer.commit().unwrap();
        let indexed: i64 = gpkg
            .connection()
            .query_row("SELECT count(*) FROM rtree_pts_geom", [], |r| r.get(0))
            .unwrap();
        assert_eq!(indexed, 1, "triggers did not survive the rollback");
    }

    /// The same atomicity on the other branch: a bulk `write_all` that adds its
    /// entries to a populated index, and then fails, must also leave nothing
    /// behind.
    ///
    /// `failed_bulk_write_rolls_back_rows_and_index` cannot cover this. It writes
    /// into an empty index, which always rebuilds, so the branch that appends
    /// had no failure test of its own.
    #[test]
    fn failed_append_write_rolls_back_rows_and_index() {
        let (_dir, gpkg) = indexed_empty_layer();
        let layer = gpkg.layer("pts").unwrap();

        // Populate the index, so that a small write into it appends rather than
        // rebuilding.
        {
            let mut writer = layer.writer().unwrap();
            for i in 1..=100 {
                let f = f64::from(i);
                writer
                    .insert(Some(i64::from(i)), &Point::new(f, -f), &[])
                    .unwrap();
            }
            writer.commit().unwrap();
        }

        // 5 new entries against 100 indexed is under the rebuild ratio.
        let features: Vec<NewFeature<Point<f64>>> = (101..=105)
            .map(|i| {
                let f = f64::from(i);
                NewFeature::new(Point::new(f, -f), Vec::new()).with_fid(i64::from(i))
            })
            .collect();
        let result = layer.write_all_impl(
            features,
            0,
            BulkIndexOptions::with_threshold(1),
            fail_the_build,
        );
        assert!(result.is_err(), "the append should have failed here");

        let conn = gpkg.connection();
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM pts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 100, "rows survived a failed append");
        let indexed: i64 = conn
            .query_row("SELECT count(*) FROM rtree_pts_geom", [], |r| r.get(0))
            .unwrap();
        assert_eq!(indexed, 100, "index entries survived a failed append");
        assert_eq!(
            layer.spatial_index_status().unwrap(),
            crate::SpatialIndexStatus::Current,
            "index left desynchronised by a failed append"
        );
    }
}
