//! The feature/attribute write path: [`FeatureWriter`] and the batched
//! [`Layer::write_all`] helper.
//!
//! # Transaction shape
//!
//! [`Layer::writer`] returns a [`FeatureWriter`] that **owns its transaction**
//! (opened with rusqlite's `unchecked_transaction`, so it works on the shared
//! `&Connection` the read path already uses). Writes stage into that
//! transaction; [`FeatureWriter::commit`] flushes the `gpkg_contents`
//! `last_change` and bounding box, then commits. Dropping a writer without
//! committing rolls the transaction back. rusqlite types never appear in the
//! public API: geometry is `impl geo_traits::GeometryTrait<T = f64>` and
//! non-geometry values are the crate's own value types. The per-row entry
//! points take borrowed [`crate::ValueRef`]s, so a row read from one layer
//! binds into another without its text and blob cells being copied;
//! [`NewFeature`] holds owned [`Value`]s, because [`Layer::write_all`] consumes
//! an iterator whose rows have to outlive any single call.
//!
//! An owned transaction (rather than a caller-passed transaction object) keeps
//! the escape-hatch `rusqlite::Transaction` out of the public surface and lets
//! the writer maintain the running bounding-box fold and `last_change` at one
//! commit point. The raw connection ([`crate::GeoPackage::connection`]) remains
//! available for callers who want to drive their own transaction.
//!
//! # Updating a layer while a cursor over it is stepping
//!
//! A writer and a [`crate::FeatureCursor`] share the connection, so a scan can
//! drive its own updates: read a row, recompute a column, write it back. That
//! is what [`FeatureWriter::update_columns`] is for.
//!
//! SQLite does not define what such a scan sees. Its isolation documentation is
//! explicit that a `SELECT` on one connection has no isolation from writes on
//! that same connection, and that an application "can UPDATE the current row or
//! any prior row, though doing so might cause that row to reappear in a
//! subsequent `sqlite3_step()`". The safety it does promise is only that the
//! file will not be harmed; the result set is not promised to be stable,
//! complete, or free of repeats.
//!
//! The scan is stable in practice when all three of these hold:
//!
//! - the cursor is a plain table scan ([`crate::Layer::cursor`]) rather than one
//!   driven by an index;
//! - the columns written are not ones the scan's index reads;
//! - the primary key is not written, since moving a row's id moves it within a
//!   rowid scan.
//!
//! The case to avoid is writing a geometry during a
//! [`crate::Layer::cursor_in`] scan. That cursor is driven by a join against
//! the RTree, and writing a geometry moves the row inside that index through
//! the triggers, which is the shape that makes a scan return rows it has
//! already returned. Recomputing geometries is better done in two passes:
//! collect the feature ids, finish the scan, then write.
//!
//! None of this is specific to this crate, and none of it is a bug that can be
//! fixed here: it is SQLite's stated contract for one connection reading and
//! writing at once.
//!
//! # Bounding box and `last_change`
//!
//! The writer seeds a bounding-box fold from the existing `gpkg_contents` row
//! and unions each written geometry's XY envelope into it (a cheap running
//! fold, never a rescan). Deletes do not shrink the box: an over-estimate is
//! spec-legal, and shrinking would need a rescan. On commit, a non-empty fold
//! is written back and `last_change` is refreshed to the strict 1.4 datetime
//! form via SQLite's `strftime` (matching the normative column default).
//!
//! The fold is written back only when the writer can vouch for it. Seeded from
//! a usable recorded box, growing it keeps it a valid cover. Starting from no
//! usable box over an empty table, the geometries written are the whole content
//! and the fold is exact. But starting from no usable box over a table that
//! already holds rows, the fold covers only what this writer wrote, and
//! recording it would replace an honest "unknown" with a box that excludes
//! every pre-existing row. Readers believe a well-ordered extent indefinitely,
//! so that case leaves the extent alone; [`Layer::recompute_extent`] is how it
//! gets fixed. See [`crate::extent`] for the reasoning in full.
//!
//! # Envelopes and Z/M
//!
//! Every written geometry gets a GPB envelope (XY, or XYZ when it carries Z),
//! so a reader, and the rtree triggers that ask for four bounds a row, never
//! have to decode the WKB body to get them; encoding is delegated to
//! [`geopackage_core::geometry::encode_gpb`]. A geometry's `z`/`m` presence is
//! validated against the column's [`ZmFlag`] before encoding, so a violation
//! is a typed [`Error::ZmViolation`] rather than a malformed row.
//!
//! # Spatial indexes
//!
//! Individual `insert`/`update`/`delete` calls, and the per-batch
//! [`Layer::write_all`] path, go through ordinary SQL, so a table that already
//! carries the rtree triggers has its index maintained by those triggers (the
//! `ST_*` functions are registered on every connection).
//!
//! [`Layer::write_all`] additionally takes the bulk path when it writes a large
//! batch into an indexed layer: it drops the triggers, inserts the rows without
//! per-row index maintenance, brings the index up to date in one operation, and
//! reinstalls the triggers. How the index is brought up to date depends on the
//! size of the write against the size of the index, and is chosen once the rows
//! are written and both counts are known: a write large enough to be worth it
//! rebuilds the index outright (see [`crate::bulk`]), and a smaller one adds the
//! new entries to the existing index instead. The threshold at which the whole
//! path engages, and forcing it either way, are controlled by
//! [`BulkIndexOptions`] via [`Layer::write_all_with`].
//!
//! # Atomicity of the bulk path
//!
//! The bulk `write_all` is a single transaction: dropping the triggers, every
//! row insert, the `gpkg_contents` flush, the index work at the end (rebuild or
//! append), and reinstalling the triggers all commit together. A crash or an
//! error at any point rolls the whole thing back to the state before the call,
//! so the rows can never be committed against an index that was not brought up
//! to date with them.
//!
//! This was not always so. The rebuild used to run in its own transaction
//! because it built the index in an `ATTACH`ed scratch database and `ATTACH`
//! requires autocommit, which left a window where a crash committed the rows but
//! not the index. Building the tree directly ([`crate::packed`]) removed the
//! `ATTACH` and with it the window. [`Layer::spatial_index_status`] and
//! [`Layer::repair_spatial_index`] still exist and still recover a
//! [`crate::SpatialIndexStatus::Stale`] index, since a file can arrive from
//! anywhere, but this path no longer produces one.

use geo_traits::{Dimensions, GeometryTrait};
use geopackage_core::geometry::encode_gpb;
#[cfg(feature = "arrow")]
use geopackage_core::geometry::encode_gpb_from_wkb;
use geopackage_core::ident::quote;
use geopackage_core::schema::{ColumnConstraint, ConstraintKind};
use geopackage_core::triggers;
use geopackage_core::types::ZmFlag;
use rusqlite::types::{ToSqlOutput, Value as SqlValue, ValueRef};
use rusqlite::{CachedStatement, Connection, Params, Transaction, params_from_iter};

use crate::bulk::{self, BulkIndexOptions};

/// How large a `write_all` must be, relative to the rows already in a spatial
/// index, before the bulk path rebuilds that index instead of adding its
/// entries to it.
///
/// A write of at least `existing / MERGE_REBUILD_RATIO` new entries takes the
/// rebuild. See [`rebuild_beats_append`] for the measurements behind the value.
const MERGE_REBUILD_RATIO: usize = 10;
use crate::index::drop_all_rtree_triggers;
use crate::value::{value_ref_to_bind, value_to_bind};
use crate::{Error, Layer, Result, Value, ValueRef as CellRef};

/// A new row for [`Layer::write_all`]: an optional explicit feature id, an
/// optional geometry, and the value-column values in column order.
///
/// Construct with [`NewFeature::new`] (a geometry) or
/// [`NewFeature::attributes`] (none); set an explicit id with
/// [`NewFeature::with_fid`].
#[derive(Debug, Clone)]
pub struct NewFeature<G> {
    /// Explicit feature id, or `None` to let SQLite assign one.
    pub fid: Option<i64>,
    /// The geometry, or `None` for a NULL geometry / attribute row.
    pub geometry: Option<G>,
    /// The value-column values, in the layer's value-column order: every
    /// column except the geometry and the primary key.
    pub values: Vec<Value>,
}

impl<G> NewFeature<G> {
    /// A feature with a geometry and its values (auto-assigned id).
    pub fn new(geometry: G, values: Vec<Value>) -> Self {
        Self {
            fid: None,
            geometry: Some(geometry),
            values,
        }
    }

    /// A row with no geometry (a NULL geometry, or an attribute row).
    pub fn attributes(values: Vec<Value>) -> Self {
        Self {
            fid: None,
            geometry: None,
            values,
        }
    }

    /// Set an explicit feature id.
    #[must_use]
    pub fn with_fid(mut self, fid: i64) -> Self {
        self.fid = Some(fid);
        self
    }
}

/// A row that [`Layer::write_all`] and its bulk counterpart can write.
///
/// Implemented by [`NewFeature`], whose geometry is an object to be encoded, and
/// by the columnar write path, whose geometry is already ISO WKB and only needs
/// a header. Both paths share the batching, the bulk-index decision and the
/// transaction handling; they differ only in how one row reaches the database,
/// which is what this trait names.
pub(crate) trait WritableRow {
    /// Write this row through `writer`, returning its assigned feature id and
    /// the XY envelope of its geometry, or `None` when it has no indexable
    /// geometry.
    fn write(self, writer: &mut FeatureWriter<'_>) -> Result<(i64, Option<[f64; 4]>)>;
}

impl<G: GeometryTrait<T = f64>> WritableRow for NewFeature<G> {
    fn write(self, writer: &mut FeatureWriter<'_>) -> Result<(i64, Option<[f64; 4]>)> {
        match &self.geometry {
            Some(geometry) => writer.insert_returning_envelope(self.fid, geometry, &self.values),
            None => writer
                .insert_row_owned(self.fid, &self.values)
                .map(|fid| (fid, None)),
        }
    }
}

/// The geometry column a [`FeatureWriter`] targets.
#[derive(Debug)]
struct GeomTarget {
    name: String,
    quoted_name: String,
    srs_id: i32,
    z: ZmFlag,
    m: ZmFlag,
}

/// A running union of written XY envelopes, seeded from the existing
/// `gpkg_contents` bounding box. Bounds are stored as
/// `[min_x, max_x, min_y, max_y]` (the shape [`encode_gpb`] returns).
#[derive(Debug, Clone, Copy)]
struct BboxFold {
    min_x: f64,
    max_x: f64,
    min_y: f64,
    max_y: f64,
    seen: bool,
}

impl BboxFold {
    fn new() -> Self {
        Self {
            min_x: f64::INFINITY,
            max_x: f64::NEG_INFINITY,
            min_y: f64::INFINITY,
            max_y: f64::NEG_INFINITY,
            seen: false,
        }
    }

    fn seed(&mut self, existing: Option<[f64; 4]>) {
        if let Some([min_x, max_x, min_y, max_y]) = existing {
            self.min_x = min_x;
            self.max_x = max_x;
            self.min_y = min_y;
            self.max_y = max_y;
            self.seen = true;
        }
    }

    fn add(&mut self, [min_x, max_x, min_y, max_y]: [f64; 4]) {
        self.min_x = self.min_x.min(min_x);
        self.max_x = self.max_x.max(max_x);
        self.min_y = self.min_y.min(min_y);
        self.max_y = self.max_y.max(max_y);
        self.seen = true;
    }

    fn bounds(&self) -> Option<[f64; 4]> {
        self.seen
            .then_some([self.min_x, self.max_x, self.min_y, self.max_y])
    }
}

/// A prepared-statement writer over one layer, owning a transaction.
///
/// Obtain one with [`Layer::writer`]. Each `insert`/`update`/`delete` stages
/// into the writer's transaction through a statement the writer holds;
/// [`Self::commit`] flushes catalogue metadata and commits. Dropping a writer
/// without committing rolls its transaction back. The `gpkg_contents` bounding
/// box is grown by a running fold over written geometry envelopes and
/// `last_change` is refreshed on commit.
pub struct FeatureWriter<'conn> {
    tx: Transaction<'conn>,
    /// The connection the transaction runs on, for preparing the statement a
    /// partial update needs, whose shape is not known until it is called.
    conn: &'conn Connection,
    table_name: String,
    quoted_table: String,
    /// The primary-key expression: the quoted pk column, or `rowid`.
    pk_expr: String,
    /// The value columns, in value order: neither the geometry nor the primary
    /// key is among them.
    value_columns: Vec<ValueColumn>,
    geometry: Option<GeomTarget>,
    bbox: BboxFold,
    /// The four possible `INSERT` statements, by whether the row carries an
    /// explicit feature id and whether it carries a geometry.
    ///
    /// A layer's shape is fixed for a writer's lifetime, so every statement it
    /// can issue is composed and prepared once here rather than per row.
    /// Composing an `INSERT` costs a `Vec` of column names, a `String` per
    /// placeholder and two joins, which is around seventeen allocations for a
    /// fifteen-column table; looking one up in the connection's statement cache
    /// costs one more. Held this way, a row costs neither.
    ///
    /// The statements come from the connection rather than from `tx`, so they
    /// borrow what the transaction borrows instead of borrowing the transaction
    /// itself. They still run inside it: a SQLite transaction belongs to the
    /// connection, not to the statements prepared against it.
    insert_stmts: [CachedStatement<'conn>; 4],
    /// The two possible `UPDATE` statements, by whether the row carries a
    /// geometry. A writer's update sets every value column, so there are only
    /// these two shapes.
    update_stmts: [CachedStatement<'conn>; 2],
    /// The `DELETE`, likewise fixed for the writer's lifetime.
    delete_stmt: CachedStatement<'conn>,
    /// The `gpkg_schema` constraints to check written values against, resolved
    /// once here. Empty unless the file was opened asking for them.
    constraints: ColumnConstraints<'conn>,
    /// The value columns named by the most recent [`Self::update_columns`]
    /// call, in the order given, and the statement prepared for them.
    ///
    /// A partial update's shape is the caller's to choose, so it cannot be
    /// prepared up front like the others. Holding the last one makes a loop
    /// that recomputes the same columns for every row prepare once; a caller
    /// alternating between two sets of columns re-prepares on each change.
    /// The starting state names no columns, which is a statement in its own
    /// right (it assigns the primary key to itself), so the slot is never
    /// empty.
    partial_columns: Vec<String>,
    partial_stmt: CachedStatement<'conn>,
    /// Any insert, or any update/delete that changed a row (drives
    /// `last_change`).
    dirty: bool,
    /// A geometry was written (drives the bounding-box flush).
    bbox_dirty: bool,
    /// Whether the fold covers the whole layer, and so may be recorded. False
    /// when the writer started with no usable recorded box over a table that
    /// already held rows, which makes the fold a lower bound rather than the
    /// extent.
    bbox_covers_layer: bool,
}

impl<'a> Layer<'a> {
    /// Begin a write transaction over this layer, returning a [`FeatureWriter`].
    ///
    /// The writer owns the transaction: stage rows with its `insert`/`update`/
    /// `delete` methods, then call [`FeatureWriter::commit`]. Dropping the
    /// writer without committing rolls the transaction back.
    pub fn writer(&self) -> Result<FeatureWriter<'a>> {
        self.gpkg().check_writable(self.table_name())?;
        let conn: &Connection = self.gpkg().connection();
        let tx = conn.unchecked_transaction()?;
        let existing = self.stored_extent()?;
        // An unusable recorded box over a table that already holds rows makes
        // the fold a lower bound: it can be grown and used, but not recorded.
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
        })
    }

    /// Write every item of `features` in batches, each batch its own committed
    /// transaction.
    ///
    /// `batch_size` bounds how many rows share a transaction (`0` writes them
    /// all in a single transaction). Returns the assigned feature ids in order.
    /// Batches commit independently: an error part-way leaves already-committed
    /// batches in place, so pass `0` when you need all-or-nothing.
    ///
    /// Rows with `geometry: Some(_)` go through [`FeatureWriter::insert`]; rows
    /// with `None` through [`FeatureWriter::insert_row`].
    ///
    /// When the layer carries a spatial index and the write is at least
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

    /// Whether this write is large enough to take the bulk path, along with any
    /// rows that had to be pulled from `features` to decide.
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
            // rows and the index commit together.
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
                    &tx,
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
                append_entries(&tx, &rtree, &entries)?;
                // The rebuild branch hands this to `fill_index`, which calls it
                // at the equivalent point. Calling it here as well is what lets
                // a test fail this branch too, once its index work is done.
                fault(&tx, &rtree)?;
                reinstall(&tx)?;
            }
            tx.commit()?;
            Ok(fids)
        })()
    }
}

/// Whether a bulk `write_all` that produced `new_entries` index entries against
/// an index already holding `indexed` of them should rebuild that index rather
/// than append the new entries to it.
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
fn rebuild_beats_append(new_entries: usize, indexed: usize) -> bool {
    if indexed == 0 {
        return true;
    }
    new_entries >= indexed / MERGE_REBUILD_RATIO
}

/// Add one RTree entry per newly written row, leaving the existing index in
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
fn append_entries(conn: &Connection, rtree: &str, entries: &[(i64, [f64; 4])]) -> Result<()> {
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
fn rtree_entry_count(conn: &Connection, rtree: &str) -> Result<usize> {
    let count: i64 = conn.query_row(
        &format!("SELECT count(*) FROM {}", quote(rtree)?),
        [],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(count).unwrap_or(usize::MAX))
}

impl<'conn> FeatureWriter<'conn> {
    /// Insert a feature with a geometry, returning its feature id.
    ///
    /// `fid` is `None` to let SQLite assign the id (returned), or `Some(id)` for
    /// an explicit id. `values` must have one entry per value column, in
    /// the layer's value-column order.
    ///
    /// Values are borrowed, so a row read from one layer binds straight into
    /// another without its text and blob cells being copied, and a literal
    /// needs no allocation: `ValueRef::Text("a")` rather than
    /// `Value::Text("a".to_owned())`. An owned [`Value`] converts with
    /// `ValueRef::from`.
    ///
    /// # Errors
    ///
    /// - [`Error::NoGeometryColumn`] if the layer has no geometry column (use
    ///   [`Self::insert_row`]).
    /// - [`Error::ZmViolation`] if the geometry's `z`/`m` presence breaks the
    ///   column's constraint.
    /// - [`Error::ValueCountMismatch`] if `values` has the wrong length.
    pub fn insert<G: GeometryTrait<T = f64>>(
        &mut self,
        fid: Option<i64>,
        geometry: &G,
        values: &[CellRef<'_>],
    ) -> Result<i64> {
        self.check_constraints(values)?;
        self.insert_geometry_binds(
            fid,
            geometry,
            values.len(),
            values.iter().copied().map(value_ref_to_bind),
        )
        .map(|(assigned, _)| assigned)
    }

    /// [`Self::insert`], additionally returning the geometry's XY envelope
    /// (`[min_x, max_x, min_y, max_y]`, `None` for an empty geometry) so the
    /// bulk write path can accumulate RTree entries without a second `ST_*`
    /// scan of the table.
    pub(crate) fn insert_returning_envelope<G: GeometryTrait<T = f64>>(
        &mut self,
        fid: Option<i64>,
        geometry: &G,
        values: &[Value],
    ) -> Result<(i64, Option<[f64; 4]>)> {
        self.check_constraints(values)?;
        self.insert_geometry_binds(
            fid,
            geometry,
            values.len(),
            values.iter().map(value_to_bind),
        )
    }

    /// The shared body of the geometry inserts, taking its bindings as an
    /// iterator so an owned `&[Value]` and a borrowed `&[ValueRef]` both reach
    /// it without either being collected into a vector first.
    fn insert_geometry_binds<'v, G, I>(
        &mut self,
        fid: Option<i64>,
        geometry: &G,
        count: usize,
        binds: I,
    ) -> Result<(i64, Option<[f64; 4]>)>
    where
        G: GeometryTrait<T = f64>,
        I: Iterator<Item = ToSqlOutput<'v>>,
    {
        self.check_value_count(count)?;
        let (blob, xy) = self.encode_geometry(geometry)?;
        // Bound straight from the caller's values and the freshly encoded blob,
        // in one chained iterator: no vector of bindings per row, and no copy
        // of the row's text and blob cells.
        let assigned = self.exec_insert(
            fid.is_some(),
            true,
            params_from_iter(
                fid.map(|id| ToSqlOutput::Borrowed(ValueRef::Integer(id)))
                    .into_iter()
                    .chain(binds)
                    .chain(std::iter::once(ToSqlOutput::Owned(SqlValue::Blob(blob)))),
            ),
            fid,
        )?;
        if let Some(envelope) = xy {
            self.bbox.add(envelope);
            self.bbox_dirty = true;
        }
        self.dirty = true;
        Ok((assigned, xy))
    }

    /// Insert a feature whose geometry is already ISO WKB, with its non-geometry
    /// values already prepared as bindings.
    ///
    /// The counterpart of [`Self::insert_returning_envelope`] for the columnar
    /// path, and the reason it takes bindings rather than [`Value`]s: an Arrow
    /// batch already holds every string and blob contiguously, so a binding that
    /// borrows from it costs nothing, where building a `Value` would allocate
    /// per cell and copy. Only `DATE` and `DATETIME` have to be owned, because
    /// they are formatted rather than copied.
    ///
    /// The WKB is parsed once, for the envelope the header and the index both
    /// need, and to reject a body that is not ISO WKB.
    ///
    /// # Errors
    ///
    /// As [`Self::insert_returning_envelope`], plus [`Error::Core`] if the bytes
    /// are not a geometry the `wkb` reader accepts.
    #[cfg(feature = "arrow")]
    pub(crate) fn insert_wkb_bound(
        &mut self,
        fid: Option<i64>,
        wkb: &[u8],
        values: &[rusqlite::types::ToSqlOutput<'_>],
    ) -> Result<(i64, Option<[f64; 4]>)> {
        self.check_value_count(values.len())?;
        self.check_constraints(values)?;
        let geom = self
            .geometry
            .as_ref()
            .ok_or_else(|| Error::NoGeometryColumn {
                table_name: self.table_name.clone(),
            })?;
        let encoded = encode_gpb_from_wkb(wkb, geom.srs_id).map_err(|e| Error::Core(e.into()))?;
        let has_z = matches!(encoded.dimensions, Dimensions::Xyz | Dimensions::Xyzm);
        let has_m = matches!(encoded.dimensions, Dimensions::Xym | Dimensions::Xyzm);
        self.check_zm("z", geom.z, has_z, &geom.name)?;
        self.check_zm("m", geom.m, has_m, &geom.name)?;

        // The caller's bindings are bound by reference; only the fid and the
        // freshly built blob are owned, and the blob moves into its binding
        // rather than being borrowed from something that has to outlive the
        // statement.
        let assigned = self.exec_insert(
            fid.is_some(),
            true,
            params_from_iter(
                fid.map(|id| ToSqlOutput::Borrowed(ValueRef::Integer(id)))
                    .into_iter()
                    .chain(values.iter().map(borrow_bind))
                    .chain(std::iter::once(ToSqlOutput::Owned(SqlValue::Blob(
                        encoded.blob,
                    )))),
            ),
            fid,
        )?;
        if let Some(envelope) = encoded.xy_envelope {
            self.bbox.add(envelope);
            self.bbox_dirty = true;
        }
        self.dirty = true;
        Ok((assigned, encoded.xy_envelope))
    }

    /// [`Self::insert_wkb_bound`] for a row with no geometry.
    #[cfg(feature = "arrow")]
    pub(crate) fn insert_row_bound(
        &mut self,
        fid: Option<i64>,
        values: &[rusqlite::types::ToSqlOutput<'_>],
    ) -> Result<i64> {
        self.check_value_count(values.len())?;
        self.check_constraints(values)?;
        let assigned = self.exec_insert(
            fid.is_some(),
            false,
            params_from_iter(
                fid.map(|id| ToSqlOutput::Borrowed(ValueRef::Integer(id)))
                    .into_iter()
                    .chain(values.iter().map(borrow_bind)),
            ),
            fid,
        )?;
        self.dirty = true;
        Ok(assigned)
    }

    /// Insert a row with no geometry (a NULL geometry on a feature table, or an
    /// attribute row), returning its feature id.
    ///
    /// # Errors
    ///
    /// [`Error::ValueCountMismatch`] if `values` has the wrong length.
    pub fn insert_row(&mut self, fid: Option<i64>, values: &[CellRef<'_>]) -> Result<i64> {
        self.check_constraints(values)?;
        self.insert_row_binds(
            fid,
            values.len(),
            values.iter().copied().map(value_ref_to_bind),
        )
    }

    /// [`Self::insert_row`] for a caller holding owned values.
    pub(crate) fn insert_row_owned(&mut self, fid: Option<i64>, values: &[Value]) -> Result<i64> {
        self.check_constraints(values)?;
        self.insert_row_binds(fid, values.len(), values.iter().map(value_to_bind))
    }

    /// The shared body of the geometryless inserts. As
    /// [`Self::insert_geometry_binds`], the bindings arrive as an iterator so
    /// neither caller collects them first.
    fn insert_row_binds<'v, I>(&mut self, fid: Option<i64>, count: usize, binds: I) -> Result<i64>
    where
        I: Iterator<Item = ToSqlOutput<'v>>,
    {
        self.check_value_count(count)?;
        let assigned = self.exec_insert(
            fid.is_some(),
            false,
            params_from_iter(
                fid.map(|id| ToSqlOutput::Borrowed(ValueRef::Integer(id)))
                    .into_iter()
                    .chain(binds),
            ),
            fid,
        )?;
        self.dirty = true;
        Ok(assigned)
    }

    /// Update the feature `fid`, setting its geometry and values. Returns
    /// whether a row matched.
    ///
    /// Writing a geometry while a cursor over the same layer is stepping is the
    /// one case the module documentation says to avoid: on an indexed layer it
    /// moves the row within the index the scan may be reading.
    ///
    /// # Errors
    ///
    /// As [`Self::insert`].
    pub fn update<G: GeometryTrait<T = f64>>(
        &mut self,
        fid: i64,
        geometry: &G,
        values: &[CellRef<'_>],
    ) -> Result<bool> {
        self.check_value_count(values.len())?;
        self.check_constraints(values)?;
        let (blob, xy) = self.encode_geometry(geometry)?;
        let matched = self.exec_update(
            true,
            params_from_iter(values.iter().copied().map(value_ref_to_bind).chain([
                ToSqlOutput::Owned(SqlValue::Blob(blob)),
                ToSqlOutput::Borrowed(ValueRef::Integer(fid)),
            ])),
        )?;
        if matched {
            if let Some(envelope) = xy {
                self.bbox.add(envelope);
                self.bbox_dirty = true;
            }
            self.dirty = true;
        }
        Ok(matched)
    }

    /// Update the feature `fid`'s non-geometry values, leaving the geometry
    /// untouched. Returns whether a row matched.
    ///
    /// # Errors
    ///
    /// [`Error::ValueCountMismatch`] if `values` has the wrong length.
    pub fn update_row(&mut self, fid: i64, values: &[CellRef<'_>]) -> Result<bool> {
        self.check_value_count(values.len())?;
        self.check_constraints(values)?;
        let matched =
            self.exec_update(
                false,
                params_from_iter(values.iter().copied().map(value_ref_to_bind).chain(
                    std::iter::once(ToSqlOutput::Borrowed(ValueRef::Integer(fid))),
                )),
            )?;
        if matched {
            self.dirty = true;
        }
        Ok(matched)
    }

    /// Update named value columns of the feature `fid`, leaving every other
    /// column, and the geometry, untouched. Returns whether a row matched.
    ///
    /// [`Self::update_row`] restates the whole row, so a caller recomputing one
    /// column has to supply the values of every other; this takes only what
    /// changed. The statement is composed from the names given and held until a
    /// call names a different set, so a loop recomputing the same columns for
    /// every row prepares once and then costs no allocation a row.
    ///
    /// The geometry and the primary key are not value columns: write a geometry
    /// through [`Self::update`], which maintains the bounding-box fold that this
    /// cannot.
    ///
    /// An empty `columns` reports whether the row exists and changes nothing.
    ///
    /// The example below drives the update from a scan of the same layer, which
    /// is sound for a plain [`Layer::cursor`] writing non-indexed columns; see
    /// the module documentation for where that stops holding.
    ///
    /// ```no_run
    /// # fn main() -> geopackage::Result<()> {
    /// # let gpkg = geopackage::GeoPackage::open("roads.gpkg")?;
    /// # let layer = gpkg.layer("roads")?;
    /// let mut cursor = layer.cursor()?;
    /// let mut writer = layer.writer()?;
    /// for feature in cursor.features()? {
    ///     let feature = feature?;
    ///     let length = feature.value("length").and_then(|v| v.as_f64()).unwrap_or(0.0);
    ///     writer.update_column(feature.fid(), "length_km", geopackage::ValueRef::Float(length / 1000.0))?;
    /// }
    /// writer.commit()?;
    /// # Ok(()) }
    /// ```
    ///
    /// # Errors
    ///
    /// - [`Error::NoSuchColumn`] if a name is not one of the layer's value
    ///   columns.
    /// - [`Error::DuplicateUpdateColumn`] if a name is given twice. SQLite
    ///   accepts a repeated assignment and applies the last, which is more
    ///   likely to be a caller's mistake than an intention.
    pub fn update_columns(&mut self, fid: i64, columns: &[(&str, CellRef<'_>)]) -> Result<bool> {
        self.check_named_constraints(columns)?;
        if !self.partial_matches(columns) {
            let sql = build_partial_update_sql(&self.shape(), columns)?;
            self.partial_stmt = self.conn.prepare_cached(&sql)?;
            self.partial_columns.clear();
            self.partial_columns
                .extend(columns.iter().map(|(name, _)| (*name).to_owned()));
        }
        let matched = self.partial_stmt.execute(params_from_iter(
            columns
                .iter()
                .map(|(_, value)| value_ref_to_bind(*value))
                .chain(std::iter::once(ToSqlOutput::Borrowed(ValueRef::Integer(
                    fid,
                )))),
        ))? > 0;
        if matched {
            self.dirty = true;
        }
        Ok(matched)
    }

    /// [`Self::update_columns`] for a single column.
    pub fn update_column(&mut self, fid: i64, column: &str, value: CellRef<'_>) -> Result<bool> {
        self.update_columns(fid, &[(column, value)])
    }

    /// Whether `columns` names exactly what the held partial statement was
    /// prepared for, in the same order. Comparing the names rather than
    /// rebuilding the statement text is what keeps a repeated shape free.
    fn partial_matches(&self, columns: &[(&str, CellRef<'_>)]) -> bool {
        self.partial_columns.len() == columns.len()
            && self
                .partial_columns
                .iter()
                .zip(columns)
                .all(|(held, (name, _))| held.as_str() == *name)
    }

    /// The layer's shape, for composing a statement whose text is not fixed
    /// when the writer is built.
    fn shape(&self) -> Shape<'_> {
        Shape {
            table_name: &self.table_name,
            quoted_table: &self.quoted_table,
            pk_expr: &self.pk_expr,
            value_columns: &self.value_columns,
            geometry: self.geometry.as_ref(),
        }
    }

    /// Delete the feature `fid`. Returns whether a row matched.
    ///
    /// The bounding box is not shrunk (that would need a rescan; an
    /// over-estimate is spec-legal).
    pub fn delete(&mut self, fid: i64) -> Result<bool> {
        let matched = self.delete_stmt.execute([fid])? > 0;
        if matched {
            self.dirty = true;
        }
        Ok(matched)
    }

    /// Flush `gpkg_contents` (`last_change`, and the bounding box when a
    /// geometry was written) and commit the transaction.
    pub fn commit(self) -> Result<()> {
        self.flush()?.commit()?;
        Ok(())
    }

    /// The connection underlying this writer's transaction, so a caller holding
    /// the writer can run additional statements inside the same transaction.
    pub(crate) fn connection(&self) -> &Connection {
        &self.tx
    }

    /// Flush the `gpkg_contents` metadata and hand back the still-open
    /// transaction, leaving it to the caller to commit.
    ///
    /// The bulk `write_all` path uses this to keep the row inserts and the
    /// index rebuild in one transaction. Dropping the returned transaction
    /// without committing rolls the whole write back, exactly as dropping the
    /// writer would have.
    pub(crate) fn flush(self) -> Result<Transaction<'conn>> {
        let Self {
            tx,
            table_name,
            bbox,
            dirty,
            bbox_dirty,
            bbox_covers_layer,
            ..
        } = self;
        if dirty {
            tx.execute(
                "UPDATE gpkg_contents \
                 SET last_change = strftime('%Y-%m-%dT%H:%M:%fZ','now') \
                 WHERE table_name = ?1",
                [&table_name],
            )?;
        }
        if bbox_dirty
            && bbox_covers_layer
            && let Some([min_x, max_x, min_y, max_y]) = bbox.bounds()
        {
            tx.execute(
                "UPDATE gpkg_contents \
                 SET min_x = ?1, min_y = ?2, max_x = ?3, max_y = ?4 \
                 WHERE table_name = ?5",
                rusqlite::params![min_x, min_y, max_x, max_y, table_name],
            )?;
        }
        Ok(tx)
    }

    /// Validate the geometry's `z`/`m` against the column and encode it to a GPB
    /// blob, returning the blob and its XY envelope (for the bbox fold).
    fn encode_geometry<G: GeometryTrait<T = f64>>(
        &self,
        geometry: &G,
    ) -> Result<(Vec<u8>, Option<[f64; 4]>)> {
        let geom = self
            .geometry
            .as_ref()
            .ok_or_else(|| Error::NoGeometryColumn {
                table_name: self.table_name.clone(),
            })?;
        let dim = geometry.dim();
        let has_z = matches!(dim, Dimensions::Xyz | Dimensions::Xyzm);
        let has_m = matches!(dim, Dimensions::Xym | Dimensions::Xyzm);
        self.check_zm("z", geom.z, has_z, &geom.name)?;
        self.check_zm("m", geom.m, has_m, &geom.name)?;
        encode_gpb(geometry, geom.srs_id).map_err(|e| Error::Core(e.into()))
    }

    /// Enforce a `z`/`m` presence constraint for a written geometry.
    fn check_zm(
        &self,
        dimension: &'static str,
        constraint: ZmFlag,
        present: bool,
        column: &str,
    ) -> Result<()> {
        let ok = match constraint {
            ZmFlag::Prohibited => !present,
            ZmFlag::Mandatory => present,
            ZmFlag::Optional => true,
            // `ZmFlag` is `#[non_exhaustive]`; a future constraint we do not
            // understand should not block a write.
            _ => true,
        };
        if ok {
            return Ok(());
        }
        Err(Error::ZmViolation {
            table_name: self.table_name.clone(),
            column: column.to_owned(),
            dimension,
            constraint,
            verb: if present { "carries" } else { "lacks" },
        })
    }

    /// Reject a value list whose length does not match the layer's value
    /// columns.
    fn check_value_count(&self, found: usize) -> Result<()> {
        if found == self.value_columns.len() {
            return Ok(());
        }
        let _ = found;
        Err(Error::ValueCountMismatch {
            table_name: self.table_name.clone(),
            expected: self.value_columns.len(),
            found,
        })
    }

    /// Check a whole row's values against the layer's `gpkg_schema`
    /// constraints, in value-column order.
    ///
    /// Returns immediately for a file that did not ask for enforcement, or a
    /// layer no constraint covers, so the ordinary write path pays one branch.
    fn check_constraints<V: AsCheckable>(&mut self, values: &[V]) -> Result<()> {
        if self.constraints.is_empty() {
            return Ok(());
        }
        for (index, value) in values.iter().enumerate() {
            if !self.constraints.satisfied(index, value.as_checkable())? {
                return Err(self.violation(index, value.as_checkable()));
            }
        }
        Ok(())
    }

    /// As [`Self::check_constraints`], for a partial update naming its columns.
    fn check_named_constraints(&mut self, columns: &[(&str, CellRef<'_>)]) -> Result<()> {
        if self.constraints.is_empty() {
            return Ok(());
        }
        for (name, value) in columns {
            let Some(index) = self
                .value_columns
                .iter()
                .position(|column| column.name == *name)
            else {
                continue;
            };
            if !self.constraints.satisfied(index, value.as_checkable())? {
                return Err(self.violation(index, value.as_checkable()));
            }
        }
        Ok(())
    }

    /// Build the error for a value the constraint at `index` refused. Off the
    /// hot path, so it re-reads what it needs rather than being threaded
    /// through the check.
    fn violation(&self, index: usize, value: Checkable<'_>) -> Error {
        let (constraint_name, constraint) = match self.constraints.at(index) {
            Some(constraint) => (constraint.name.clone(), constraint.kind.to_string()),
            None => (String::new(), String::new()),
        };
        Error::ColumnConstraintViolation {
            table_name: self.table_name.clone(),
            column_name: self
                .value_columns
                .get(index)
                .map_or_else(String::new, |column| column.name.clone()),
            constraint_name,
            constraint,
            value: match value {
                Checkable::Null => "NULL".to_owned(),
                Checkable::Integer(number) => number.to_string(),
                Checkable::Real(number) => number.to_string(),
                Checkable::Text(text) => format!("{text:?}"),
                Checkable::Unchecked => "(unchecked)".to_owned(),
            },
        }
    }

    /// The prepared `INSERT` for this combination of explicit id and geometry.
    ///
    /// Selected by matching rather than by indexing, so the four cases are
    /// exhaustive and there is no absent-slot case to invent an answer for.
    fn insert_stmt(&mut self, with_fid: bool, with_geometry: bool) -> &mut CachedStatement<'conn> {
        let [plain, fid_only, geom_only, both] = &mut self.insert_stmts;
        match (with_fid, with_geometry) {
            (false, false) => plain,
            (true, false) => fid_only,
            (false, true) => geom_only,
            (true, true) => both,
        }
    }

    /// The prepared `UPDATE`, with or without the geometry assignment.
    fn update_stmt(&mut self, with_geometry: bool) -> &mut CachedStatement<'conn> {
        let [plain, with_geom] = &mut self.update_stmts;
        if with_geometry { with_geom } else { plain }
    }

    /// Run one insert.
    ///
    /// Takes the bindings as [`Params`] rather than a slice so callers can pass
    /// a chained iterator of borrowed bindings: a row is then written without
    /// collecting its bindings into a vector first, and without copying the
    /// text and blob cells out of the caller's values.
    fn exec_insert<P: Params>(
        &mut self,
        with_fid: bool,
        with_geometry: bool,
        binds: P,
        fid: Option<i64>,
    ) -> Result<i64> {
        self.insert_stmt(with_fid, with_geometry).execute(binds)?;
        Ok(fid.unwrap_or_else(|| self.tx.last_insert_rowid()))
    }

    fn exec_update<P: Params>(&mut self, with_geometry: bool, binds: P) -> Result<bool> {
        Ok(self.update_stmt(with_geometry).execute(binds)? > 0)
    }
}

/// The pieces of a layer's shape that its statement text is composed from,
/// borrowed while [`Layer::writer`] builds them and before they move into the
/// writer.
struct Shape<'s> {
    table_name: &'s str,
    quoted_table: &'s str,
    pk_expr: &'s str,
    value_columns: &'s [ValueColumn],
    geometry: Option<&'s GeomTarget>,
}

/// One value column of the layer being written: its name as declared, and the
/// same name quoted for use in a statement.
struct ValueColumn {
    name: String,
    quoted: String,
}

/// The `gpkg_schema` constraints a writer checks its values against, one entry
/// per value column and `None` where a column has none.
///
/// Empty, and so free per row, unless
/// [`OpenOptions::enforce_column_constraints`](crate::OpenOptions::enforce_column_constraints)
/// was set: the constraints are advisory in the format ("These restrictions
/// MAY be enforced by SQL triggers or by code in applications that update
/// GeoPackage data values"), so enforcing them is the caller's decision rather
/// than this crate's.
struct ColumnConstraints<'conn> {
    per_column: Vec<Option<ColumnConstraint>>,
    /// `SELECT ?1 GLOB ?2`, prepared once, for the glob form.
    ///
    /// The pattern language is SQLite's, defined by whatever the engine
    /// holding the file does, so the engine answers rather than a copy of its
    /// rules living here. That also gives numbers SQLite's own text coercion,
    /// which is what a trigger enforcing the same constraint would apply.
    /// `None` when no column carries a glob, which is the usual case.
    glob: Option<CachedStatement<'conn>>,
}

impl<'conn> ColumnConstraints<'conn> {
    /// Resolve each value column's constraint, once per writer.
    fn read(
        layer: &Layer<'_>,
        conn: &'conn Connection,
        value_columns: &[ValueColumn],
    ) -> Result<Self> {
        if !layer.gpkg().enforces_column_constraints() {
            return Ok(Self::none());
        }
        let described = layer.gpkg().data_columns(layer.table_name())?;
        let mut per_column = Vec::with_capacity(value_columns.len());
        for column in value_columns {
            let constraint_name = described
                .iter()
                .find(|described| described.column_name == column.name)
                .and_then(|described| described.constraint_name.as_deref());
            per_column.push(match constraint_name {
                Some(name) => layer.gpkg().column_constraint(name)?,
                None => None,
            });
        }
        let needs_glob = per_column
            .iter()
            .flatten()
            .any(|constraint| matches!(constraint.kind, ConstraintKind::Glob(_)));
        let glob = match needs_glob {
            true => Some(conn.prepare_cached("SELECT ?1 GLOB ?2")?),
            false => None,
        };
        Ok(Self { per_column, glob })
    }

    /// Nothing enforced.
    fn none() -> Self {
        Self {
            per_column: Vec::new(),
            glob: None,
        }
    }

    /// Whether anything at all is enforced, so the row paths can skip the walk.
    fn is_empty(&self) -> bool {
        self.per_column.iter().all(Option::is_none)
    }

    fn at(&self, index: usize) -> Option<&ColumnConstraint> {
        self.per_column.get(index).and_then(Option::as_ref)
    }

    /// Whether `value` satisfies the constraint on the column at `index`.
    ///
    /// The range and enum forms are decided here; the glob form is put to
    /// SQLite.
    fn satisfied(&mut self, index: usize, value: Checkable<'_>) -> Result<bool> {
        let Self { per_column, glob } = self;
        let Some(Some(constraint)) = per_column.get(index) else {
            return Ok(true);
        };
        match (&constraint.kind, value) {
            (_, Checkable::Null | Checkable::Unchecked) => Ok(true),
            (ConstraintKind::Range { .. }, Checkable::Text(_)) => Ok(false),
            (ConstraintKind::Range { .. }, Checkable::Integer(number)) => {
                // i64 to f64 loses precision above 2^53, and a bound that far
                // out is not one anybody wrote deliberately.
                Ok(in_range(&constraint.kind, number as f64))
            }
            (ConstraintKind::Range { .. }, Checkable::Real(number)) => {
                Ok(in_range(&constraint.kind, number))
            }
            (ConstraintKind::Enum(members), value) => Ok(match value {
                Checkable::Text(text) => members.iter().any(|member| member == text),
                // The members are text, and the spec's own sample enumerates
                // the numbers 1, 3, 5, 7 and 9, so a number is compared by its
                // decimal form.
                Checkable::Integer(number) => members.contains(&number.to_string()),
                Checkable::Real(number) => members.contains(&number.to_string()),
                Checkable::Null | Checkable::Unchecked => true,
            }),
            (ConstraintKind::Glob(pattern), value) => {
                let statement = glob
                    .as_mut()
                    .expect("a glob constraint means the statement was prepared");
                let matched: i64 = match value {
                    Checkable::Text(text) => {
                        statement.query_one(rusqlite::params![text, pattern], |r| r.get(0))?
                    }
                    Checkable::Integer(number) => {
                        statement.query_one(rusqlite::params![number, pattern], |r| r.get(0))?
                    }
                    Checkable::Real(number) => {
                        statement.query_one(rusqlite::params![number, pattern], |r| r.get(0))?
                    }
                    Checkable::Null | Checkable::Unchecked => 1,
                };
                Ok(matched != 0)
            }
        }
    }
}

/// Whether `value` falls inside a range constraint, honouring both bounds'
/// inclusivity. Anything but a range is outside it.
fn in_range(kind: &ConstraintKind, value: f64) -> bool {
    let ConstraintKind::Range {
        min,
        min_is_inclusive,
        max,
        max_is_inclusive,
    } = kind
    else {
        return false;
    };
    let above = if *min_is_inclusive {
        value >= *min
    } else {
        value > *min
    };
    let below = if *max_is_inclusive {
        value <= *max
    } else {
        value < *max
    };
    above && below
}

/// A value reduced to what a constraint can judge it by.
///
/// The three write paths hand values over in three forms; this is what they
/// have in common as far as a `range`, `enum` or `glob` is concerned.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Checkable<'a> {
    /// No value. A constraint says what a value may be, not that there has to
    /// be one, so NULL satisfies every constraint. A column that must hold
    /// something says so with `NOT NULL`.
    Null,
    /// An integer, compared numerically against a `range` and by its decimal
    /// form against an `enum` or `glob`: the spec's own sample enumerates the
    /// numbers 1, 3, 5, 7 and 9 as `enum` values.
    Integer(i64),
    /// A float, compared as [`Checkable::Integer`] is.
    Real(f64),
    /// Text, which no `range` admits and which an `enum` or `glob` compares
    /// directly.
    Text(&'a str),
    /// A blob, date or datetime, none of which any constraint form describes.
    /// Not checked.
    Unchecked,
}

/// The value forms a write path can hand over.
pub(crate) trait AsCheckable {
    /// This value, as a constraint sees it.
    fn as_checkable(&self) -> Checkable<'_>;
}

impl AsCheckable for Value {
    fn as_checkable(&self) -> Checkable<'_> {
        match self {
            Self::Null => Checkable::Null,
            Self::Integer(value) => Checkable::Integer(*value),
            Self::Boolean(value) => Checkable::Integer(i64::from(*value)),
            Self::Float(value) => Checkable::Real(*value),
            Self::Text(value) => Checkable::Text(value),
            Self::Blob(_) | Self::Date(_) | Self::DateTime(_) => Checkable::Unchecked,
        }
    }
}

impl AsCheckable for CellRef<'_> {
    fn as_checkable(&self) -> Checkable<'_> {
        match self {
            Self::Null => Checkable::Null,
            Self::Integer(value) => Checkable::Integer(*value),
            Self::Boolean(value) => Checkable::Integer(i64::from(*value)),
            Self::Float(value) => Checkable::Real(*value),
            Self::Text(value) => Checkable::Text(value),
            Self::Blob(_) | Self::Date(_) | Self::DateTime(_) => Checkable::Unchecked,
        }
    }
}

impl AsCheckable for ToSqlOutput<'_> {
    fn as_checkable(&self) -> Checkable<'_> {
        let value = match self {
            Self::Borrowed(value) => *value,
            Self::Owned(value) => value.into(),
            _ => return Checkable::Unchecked,
        };
        match value {
            rusqlite::types::ValueRef::Null => Checkable::Null,
            rusqlite::types::ValueRef::Integer(value) => Checkable::Integer(value),
            rusqlite::types::ValueRef::Real(value) => Checkable::Real(value),
            rusqlite::types::ValueRef::Text(bytes) => match std::str::from_utf8(bytes) {
                Ok(text) => Checkable::Text(text),
                Err(_) => Checkable::Unchecked,
            },
            rusqlite::types::ValueRef::Blob(_) => Checkable::Unchecked,
        }
    }
}

/// Compose one of the four `INSERT` statements. Called once per writer.
fn build_insert_sql(shape: &Shape<'_>, with_fid: bool, with_geometry: bool) -> String {
    let mut columns: Vec<&str> = Vec::with_capacity(shape.value_columns.len() + 2);
    if with_fid {
        columns.push(shape.pk_expr);
    }
    for column in shape.value_columns {
        columns.push(&column.quoted);
    }
    if with_geometry && let Some(geom) = shape.geometry {
        columns.push(&geom.quoted_name);
    }
    if columns.is_empty() {
        return format!("INSERT INTO {} DEFAULT VALUES", shape.quoted_table);
    }
    let placeholders = (1..=columns.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "INSERT INTO {} ({}) VALUES ({placeholders})",
        shape.quoted_table,
        columns.join(", ")
    )
}

/// Compose one of the two `UPDATE ... WHERE <pk> = ?` statements. Called once
/// per writer.
fn build_update_sql(shape: &Shape<'_>, with_geometry: bool) -> String {
    let mut assignments: Vec<String> = Vec::with_capacity(shape.value_columns.len() + 1);
    let mut index = 1;
    for column in shape.value_columns {
        assignments.push(format!("{} = ?{index}", column.quoted));
        index += 1;
    }
    if with_geometry && let Some(geom) = shape.geometry {
        assignments.push(format!("{} = ?{index}", geom.quoted_name));
        index += 1;
    }
    if assignments.is_empty() {
        // Nothing to change (an attribute table with only a primary key): a
        // self-assignment keeps the statement valid and rows-affected
        // meaningful.
        assignments.push(format!("{pk} = {pk}", pk = shape.pk_expr));
    }
    format!(
        "UPDATE {} SET {} WHERE {} = ?{index}",
        shape.quoted_table,
        assignments.join(", "),
        shape.pk_expr
    )
}

/// Compose the `UPDATE` for a named subset of the value columns, assigning them
/// in the order given. Called only when [`FeatureWriter::update_columns`] is
/// asked for a set of columns other than the one it last prepared.
fn build_partial_update_sql(shape: &Shape<'_>, columns: &[(&str, CellRef<'_>)]) -> Result<String> {
    let mut assignments: Vec<String> = Vec::with_capacity(columns.len());
    for (position, (name, _)) in columns.iter().enumerate() {
        if columns
            .iter()
            .take(position)
            .any(|(earlier, _)| earlier == name)
        {
            return Err(Error::DuplicateUpdateColumn {
                table_name: shape.table_name.to_owned(),
                column_name: (*name).to_owned(),
            });
        }
        let column = shape
            .value_columns
            .iter()
            .find(|candidate| candidate.name == *name)
            .ok_or_else(|| Error::NoSuchColumn {
                table_name: shape.table_name.to_owned(),
                column_name: (*name).to_owned(),
            })?;
        assignments.push(format!("{} = ?{}", column.quoted, position + 1));
    }
    // Before the self-assignment below, which takes no placeholder of its own.
    let fid_placeholder = assignments.len() + 1;
    if assignments.is_empty() {
        // As the whole-row form: nothing to change, but the statement stays
        // valid and rows-affected keeps its meaning.
        assignments.push(format!("{pk} = {pk}", pk = shape.pk_expr));
    }
    Ok(format!(
        "UPDATE {} SET {} WHERE {} = ?{fid_placeholder}",
        shape.quoted_table,
        assignments.join(", "),
        shape.pk_expr
    ))
}

/// Re-borrow a prepared binding so it can be chained with owned ones.
///
/// The columnar path hands over a slice of bindings it still owns. Cloning them
/// to build the statement's parameter list would copy every owned cell, which
/// is exactly the copy those bindings exist to avoid, so an owned binding is
/// re-borrowed rather than duplicated.
#[cfg(feature = "arrow")]
fn borrow_bind<'a>(bind: &'a ToSqlOutput<'_>) -> ToSqlOutput<'a> {
    match bind {
        ToSqlOutput::Borrowed(value) => ToSqlOutput::Borrowed(*value),
        ToSqlOutput::Owned(value) => ToSqlOutput::Borrowed(ValueRef::from(value)),
        // `ToSqlOutput` is non-exhaustive; nothing this crate builds reaches
        // here, and the remaining variants are all cheap to copy.
        other => other.clone(),
    }
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
