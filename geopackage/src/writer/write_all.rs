use geo_traits::GeometryTrait;
use geopackage_core::ident::quote;
use geopackage_core::triggers;
use rusqlite::Connection;

use crate::bulk::{self, BulkIndexOptions};
use crate::index::drop_all_rtree_triggers;
use crate::{Error, Layer, Result};

use super::row::{NewFeature, WritableRow};

/// How large a `write_all` must be, relative to the rows already in a spatial
/// index, before the bulk path rebuilds that index instead of adding its
/// entries to it.
///
/// A write of at least `existing / MERGE_REBUILD_RATIO` new entries takes the
/// rebuild. See [`rebuild_beats_append`] for the measurements behind the value.
const MERGE_REBUILD_RATIO: usize = 10;

impl<'a> Layer<'a> {
    /// Writes every item of `features` in batches, each batch its own
    /// committed transaction.
    ///
    /// `batch_size` bounds how many rows share a transaction, and `0` writes
    /// them all in a single transaction. Returns the assigned feature ids in
    /// order. Batches commit independently. An error part-way leaves
    /// already-committed batches in place, so pass `0` when you need
    /// all-or-nothing.
    ///
    /// If a transaction is already open on the connection, `batch_size` bounds
    /// nothing. The batch commits are staged rather than durable, so every row
    /// belongs to the caller's transaction. An error part-way leaves all of the
    /// rows staged, rather than some of them committed. This follows from
    /// opening the transaction, and matches the all-or-nothing behaviour of
    /// `batch_size = 0`.
    ///
    /// Rows with `geometry: Some(_)` go through [`FeatureWriter::insert`](crate::FeatureWriter::insert), and
    /// rows with `None` through [`FeatureWriter::insert_row`](crate::FeatureWriter::insert_row).
    ///
    /// When the layer has a spatial index and the write is at least
    /// [`DEFAULT_BULK_THRESHOLD`](bulk::DEFAULT_BULK_THRESHOLD) rows, it takes
    /// the bulk path instead. That path brings the index up to date at the end,
    /// rather than row by row through the triggers. [`Self::write_all_with`]
    /// describes how, and lets you tune or force that choice.
    pub fn write_all<G, I>(&self, features: I, batch_size: usize) -> Result<Vec<i64>>
    where
        G: GeometryTrait<T = f64>,
        I: IntoIterator<Item = NewFeature<G>>,
    {
        self.write_all_with(features, batch_size, BulkIndexOptions::default())
    }

    /// Like [`Self::write_all`], with an explicit [`BulkIndexOptions`] that
    /// chooses between a bulk and a triggered index build.
    ///
    /// The write takes the bulk path when the layer has a spatial index and the
    /// write reaches `options.bulk_threshold` rows. The size check uses the
    /// iterator's `size_hint` where possible, and otherwise buffers up to
    /// `bulk_threshold` rows, so an iterator that does not know its own length
    /// can still take the bulk path. [`BulkIndexOptions::always_bulk`] drops
    /// the size condition, and [`BulkIndexOptions::never_bulk`] disables the
    /// bulk path.
    ///
    /// The bulk path rebuilds the index when the write adds at least a tenth
    /// as many entries as the index already holds, and otherwise adds the new
    /// entries to it.
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
    /// This is the only decision that must be made before any row is written.
    /// The bulk path drops the RTree triggers first, and for a handful of rows
    /// the schema churn would cost more than the bulk path saves. Whether the
    /// index is then rebuilt or appended to is decided after the write, from
    /// the exact counts (see [`rebuild_beats_append`]).
    ///
    /// [`Iterator::size_hint`] answers the size condition whenever the hint
    /// settles it, which covers every `Vec`-like source at no cost. An iterator
    /// that does not know its own length, as is true of most iterators not
    /// backed by a collection, reports a lower bound of `0`. Going by the hint
    /// alone, it would never reach the threshold, however many rows it went on
    /// to yield. For such an iterator the rows themselves are the only
    /// evidence, so they are buffered until the threshold is reached, which
    /// settles the decision, or until the iterator ends first, which gives an
    /// exact count below the threshold.
    ///
    /// Buffering is therefore bounded by `options.bulk_threshold` rows, never
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

    /// The per-batch triggered write path, with one committed transaction per
    /// `batch_size` rows. `0` means a single transaction for the whole
    /// iterator.
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

    /// The bulk write path. It drops the RTree triggers, inserts every row in
    /// one transaction, then brings the index up to date and reinstalls the
    /// triggers. There is no per-row index maintenance, but the writer commit
    /// still maintains the `gpkg_contents` bbox and `last_change`.
    ///
    /// On any failure, dropping the writer rolls its transaction back, so the
    /// triggers and the index are as they were before the call. Inside a
    /// transaction the caller opened, nothing is rolled back, and the partial
    /// work stays staged for the caller to discard.
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
            // scan of the table.
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
/// The decision is made after the rows are written, not before, so both counts
/// are exact. Deciding up front would mean guessing the size of the write from
/// [`Iterator::size_hint`], which only ever gives a lower bound, and an
/// iterator that gives none could not be placed on either side of the ratio.
///
/// An empty index is the easy case. There is nothing to preserve, so the
/// rebuild is a clear win. A populated index is a trade-off, because a rebuild
/// pays for the rows already in it. Measured at 1M and 100k existing rows, a
/// rebuild costs roughly 1.5 us per row of the *total* table, while an append
/// costs roughly 18 to 40 us per *new* row, rising with table size as the index
/// deepens. Rebuilding therefore wins once the new entries are somewhere between
/// 5% and 10% of the existing ones. [`MERGE_REBUILD_RATIO`] takes the
/// conservative end. At 100k existing rows, a 10k-row append measured 187 ms
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
/// This does the work the `_insert` trigger would have done if it were still
/// installed: the same `INSERT OR REPLACE`, with the same values, in the same
/// row order. The resulting index is the one a triggered write would have
/// produced. The envelopes were computed while encoding the geometries, and a
/// row whose geometry is NULL or empty contributed none, which exactly matches
/// the trigger's `NEW.geom NOT NULL AND NOT ST_IsEmpty(NEW.geom)` condition.
///
/// Nothing gates the result. The bulk build is gated because it writes a tree by
/// hand into an on-disk format SQLite does not document as an interface. These
/// inserts go through the RTree module itself, and need no more checking than
/// the triggers do.
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
    /// behind: no rows, no half-built index, and no dropped trigger set.
    ///
    /// The bulk path used to lack this atomicity. The rebuild ran in its own
    /// transaction, because building the index in an `ATTACH`ed scratch
    /// database required autocommit. The rows were therefore already committed
    /// by the time it ran, and a failure here left them against a stale index.
    /// The assertions below all fail against that arrangement.
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

    /// Tests the same atomicity on the other branch. A bulk `write_all` that
    /// adds its entries to a populated index, and then fails, must also leave
    /// nothing behind.
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
