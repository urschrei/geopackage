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
//! non-geometry values are the crate's [`Value`] enum.
//!
//! An owned transaction (rather than a caller-passed transaction object) keeps
//! the escape-hatch `rusqlite::Transaction` out of the public surface and lets
//! the writer maintain the running bounding-box fold and `last_change` at one
//! commit point. The raw connection ([`crate::GeoPackage::connection`]) remains
//! available for callers who want to drive their own transaction (D9).
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
//! # Envelopes and Z/M
//!
//! Every written geometry gets a GPB envelope (XY, or XYZ when it carries Z),
//! per design decision D6; encoding is delegated to
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
//! [`Layer::write_all`] additionally takes the D8 bulk-build path when it writes
//! a large batch into an indexed layer whose index is currently empty (a fresh
//! bulk load): it drops the triggers, inserts the rows without per-row index
//! maintenance, rebuilds the index in one bulk shadow-table copy (see
//! [`crate::bulk`]), and reinstalls the triggers. The threshold and forcing are
//! controlled by [`BulkIndexOptions`] via [`Layer::write_all_with`].
//!
//! # Atomicity of the bulk path
//!
//! The bulk `write_all` is a single transaction: dropping the triggers, every
//! row insert, the `gpkg_contents` flush, the index rebuild, and reinstalling
//! the triggers all commit together. A crash or an error at any point rolls the
//! whole thing back to the state before the call, so the rows can never be
//! committed against an index that was not rebuilt.
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
use geopackage_core::ident::quote;
use geopackage_core::triggers;
use geopackage_core::types::ZmFlag;
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, Transaction, params_from_iter};

use crate::bulk::{self, BulkIndexOptions};
use crate::index::drop_all_rtree_triggers;
use crate::value::value_to_sql;
use crate::{Error, Layer, Result, Value};

/// A new row for [`Layer::write_all`]: an optional explicit feature id, an
/// optional geometry, and the non-geometry values in column order.
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
    /// The non-geometry column values, in the layer's value-column order.
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
/// into the writer's transaction using rusqlite's per-connection statement
/// cache (so repeated calls reuse the compiled statement); [`Self::commit`]
/// flushes catalogue metadata and commits. Dropping a writer without committing
/// rolls its transaction back. The `gpkg_contents` bounding box is grown by a
/// running fold over written geometry envelopes and `last_change` is refreshed
/// on commit.
pub struct FeatureWriter<'conn> {
    tx: Transaction<'conn>,
    table_name: String,
    quoted_table: String,
    /// The primary-key expression: the quoted pk column, or `rowid`.
    pk_expr: String,
    /// Quoted non-geometry column names, in value order.
    value_columns: Vec<String>,
    geometry: Option<GeomTarget>,
    bbox: BboxFold,
    /// Any insert, or any update/delete that changed a row (drives
    /// `last_change`).
    dirty: bool,
    /// A geometry was written (drives the bounding-box flush).
    bbox_dirty: bool,
}

impl<'a> Layer<'a> {
    /// Begin a write transaction over this layer, returning a [`FeatureWriter`].
    ///
    /// The writer owns the transaction: stage rows with its `insert`/`update`/
    /// `delete` methods, then call [`FeatureWriter::commit`]. Dropping the
    /// writer without committing rolls the transaction back.
    pub fn writer(&self) -> Result<FeatureWriter<'a>> {
        let conn: &Connection = self.gpkg().connection();
        let tx = conn.unchecked_transaction()?;
        let existing = read_contents_bbox(&tx, self.table_name())?;

        let pk_name = self.primary_key_column();
        let pk_expr = match pk_name {
            Some(pk) => quote(pk)?,
            None => "rowid".to_owned(),
        };
        // The read path's value columns include the primary key; the write path
        // treats the primary key as a separate `fid` and the geometry through
        // its own column, so exclude both here.
        let value_columns = self
            .value_columns()
            .iter()
            .filter(|c| Some(c.name.as_str()) != pk_name)
            .map(|c| quote(&c.name))
            .collect::<std::result::Result<Vec<_>, _>>()?;
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
        bbox.seed(existing);

        Ok(FeatureWriter {
            tx,
            table_name: self.table_name().to_owned(),
            quoted_table: quote(self.table_name())?,
            pk_expr,
            value_columns,
            geometry,
            bbox,
            dirty: false,
            bbox_dirty: false,
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
    /// When the layer carries a spatial index whose contents are currently
    /// empty and `features` advertises at least
    /// [`DEFAULT_BULK_THRESHOLD`](bulk::DEFAULT_BULK_THRESHOLD) rows (via its
    /// [`Iterator::size_hint`]), the write takes the D8 bulk-build path instead;
    /// [`Self::write_all_with`] tunes or forces that choice.
    pub fn write_all<G, I>(&self, features: I, batch_size: usize) -> Result<Vec<i64>>
    where
        G: GeometryTrait<T = f64>,
        I: IntoIterator<Item = NewFeature<G>>,
    {
        self.write_all_with(features, batch_size, BulkIndexOptions::default())
    }

    /// [`Self::write_all`] with an explicit [`BulkIndexOptions`] controlling the
    /// bulk-vs-triggered index-build choice (design decision D8).
    ///
    /// The bulk path is taken only when the layer has a spatial index, that
    /// index is currently empty (bulk building a fresh index; appends to a
    /// populated index always use the triggered path), and the incoming
    /// iterator's `size_hint` lower bound reaches `options.bulk_threshold`.
    /// [`BulkIndexOptions::always_bulk`] drops the size condition (still
    /// requiring an empty index); [`BulkIndexOptions::never_bulk`] disables it.
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
        self.write_all_impl(features, batch_size, options, bulk::no_tamper)
    }

    /// The `write_all_with` core, taking the bulk build's test seam so a test can
    /// force the index build to fail after the rows have been staged.
    pub(crate) fn write_all_impl<G, I>(
        &self,
        features: I,
        batch_size: usize,
        options: BulkIndexOptions,
        tamper: bulk::ScratchTamper,
    ) -> Result<Vec<i64>>
    where
        G: GeometryTrait<T = f64>,
        I: IntoIterator<Item = NewFeature<G>>,
    {
        let iter = features.into_iter();
        if self.bulk_write_applicable(iter.size_hint().0, options)? {
            self.write_all_bulk(iter, options, tamper)
        } else {
            self.write_all_batched(iter, batch_size)
        }
    }

    /// The per-batch triggered write path: one committed transaction per
    /// `batch_size` rows (`0` = a single transaction for the whole iterator).
    fn write_all_batched<G, I>(&self, features: I, batch_size: usize) -> Result<Vec<i64>>
    where
        G: GeometryTrait<T = f64>,
        I: IntoIterator<Item = NewFeature<G>>,
    {
        let mut fids = Vec::new();
        let mut iter = features.into_iter();
        let mut batch = self.writer()?;
        let mut in_batch = 0usize;
        let mut wrote_any = false;
        for feature in iter.by_ref() {
            let fid = match &feature.geometry {
                Some(geometry) => batch.insert(feature.fid, geometry, &feature.values)?,
                None => batch.insert_row(feature.fid, &feature.values)?,
            };
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

    /// Whether a `write_all` should take the D8 bulk-build path: the layer has a
    /// geometry column and a single-column primary key, a recognised spatial
    /// index whose contents are empty, and `size_hint_lower` reaches the
    /// threshold.
    fn bulk_write_applicable(
        &self,
        size_hint_lower: usize,
        options: BulkIndexOptions,
    ) -> Result<bool> {
        if size_hint_lower < options.bulk_threshold {
            return Ok(false);
        }
        let Some(geom) = self.geometry_column() else {
            return Ok(false);
        };
        if self.primary_key_column().is_none() || !self.has_spatial_index()? {
            return Ok(false);
        }
        let rtree = triggers::rtree_table_name(self.table_name(), &geom.column_name);
        let count: i64 = self.gpkg().connection().query_row(
            &format!("SELECT count(*) FROM {}", quote(&rtree)?),
            [],
            |r| r.get(0),
        )?;
        Ok(count == 0)
    }

    /// The D8 bulk write path: drop the rtree triggers, insert every row in one
    /// transaction (no per-row index maintenance, but `gpkg_contents` bbox and
    /// `last_change` are still maintained by the writer commit), then rebuild the
    /// index in bulk and reinstall the triggers.
    ///
    /// On any failure after the triggers are dropped, the index is restored to a
    /// consistent, trigger-maintained state before the error is returned.
    fn write_all_bulk<G, I>(
        &self,
        features: I,
        options: BulkIndexOptions,
        tamper: bulk::ScratchTamper,
    ) -> Result<Vec<i64>>
    where
        G: GeometryTrait<T = f64>,
        I: IntoIterator<Item = NewFeature<G>>,
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
        let conn = self.gpkg().connection();

        // Envelopes computed while encoding can be reused as the RTree entry set
        // only if this write accounts for every indexable row in the table. An
        // empty table before the write is the cheap, sufficient proof; otherwise
        // `fill_index` re-derives the set with its own `ST_*` scan.
        let table_was_empty = bulk::table_row_count(conn, table)? == 0;

        (|| -> Result<Vec<i64>> {
            let mut fids = Vec::new();
            let mut entries = Vec::new();
            let mut writer = self.writer()?;
            // Inside the writer's transaction, so a failure or a crash rolls the
            // trigger drop back along with everything else.
            drop_all_rtree_triggers(writer.connection(), table, column)?;
            for feature in features {
                let fid = match &feature.geometry {
                    Some(geometry) => {
                        let (fid, envelope) = writer.insert_returning_envelope(
                            feature.fid,
                            geometry,
                            &feature.values,
                        )?;
                        if let Some(envelope) = envelope {
                            entries.push((fid, envelope));
                        }
                        fid
                    }
                    None => writer.insert_row(feature.fid, &feature.values)?,
                };
                fids.push(fid);
            }
            // Flush the catalogue metadata but keep the transaction open, so the
            // rows and the rebuilt index commit together.
            let tx = writer.flush()?;
            let precomputed = table_was_empty.then_some(entries);
            bulk::fill_index_in_transaction(
                &tx,
                table,
                column,
                pk,
                &rtree,
                options,
                precomputed,
                tamper,
                |conn| {
                    for sql in triggers::create_triggers_sql(table, column, pk)? {
                        conn.execute_batch(&sql)?;
                    }
                    Ok(())
                },
            )?;
            tx.commit()?;
            Ok(fids)
        })()
    }
}

impl<'conn> FeatureWriter<'conn> {
    /// Insert a feature with a geometry, returning its feature id.
    ///
    /// `fid` is `None` to let SQLite assign the id (returned), or `Some(id)` for
    /// an explicit id. `values` must have one entry per non-geometry column, in
    /// the layer's value-column order.
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
        values: &[Value],
    ) -> Result<i64> {
        self.insert_returning_envelope(fid, geometry, values)
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
        self.check_values(values)?;
        let (blob, xy) = self.encode_geometry(geometry)?;
        let sql = self.insert_sql(fid.is_some(), true);
        let mut binds: Vec<SqlValue> = Vec::with_capacity(values.len() + 2);
        if let Some(id) = fid {
            binds.push(SqlValue::Integer(id));
        }
        binds.extend(values.iter().map(value_to_sql));
        binds.push(SqlValue::Blob(blob));
        let assigned = self.exec_insert(&sql, &binds, fid)?;
        if let Some(envelope) = xy {
            self.bbox.add(envelope);
            self.bbox_dirty = true;
        }
        self.dirty = true;
        Ok((assigned, xy))
    }

    /// Insert a row with no geometry (a NULL geometry on a feature table, or an
    /// attribute row), returning its feature id.
    ///
    /// # Errors
    ///
    /// [`Error::ValueCountMismatch`] if `values` has the wrong length.
    pub fn insert_row(&mut self, fid: Option<i64>, values: &[Value]) -> Result<i64> {
        self.check_values(values)?;
        let sql = self.insert_sql(fid.is_some(), false);
        let mut binds: Vec<SqlValue> = Vec::with_capacity(values.len() + 1);
        if let Some(id) = fid {
            binds.push(SqlValue::Integer(id));
        }
        binds.extend(values.iter().map(value_to_sql));
        let assigned = self.exec_insert(&sql, &binds, fid)?;
        self.dirty = true;
        Ok(assigned)
    }

    /// Update the feature `fid`, setting its geometry and values. Returns
    /// whether a row matched.
    ///
    /// # Errors
    ///
    /// As [`Self::insert`].
    pub fn update<G: GeometryTrait<T = f64>>(
        &mut self,
        fid: i64,
        geometry: &G,
        values: &[Value],
    ) -> Result<bool> {
        self.check_values(values)?;
        let (blob, xy) = self.encode_geometry(geometry)?;
        let sql = self.update_sql(true);
        let mut binds: Vec<SqlValue> = Vec::with_capacity(values.len() + 2);
        binds.extend(values.iter().map(value_to_sql));
        binds.push(SqlValue::Blob(blob));
        binds.push(SqlValue::Integer(fid));
        let matched = self.exec_update(&sql, &binds)?;
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
    pub fn update_row(&mut self, fid: i64, values: &[Value]) -> Result<bool> {
        self.check_values(values)?;
        let sql = self.update_sql(false);
        let mut binds: Vec<SqlValue> = Vec::with_capacity(values.len() + 1);
        binds.extend(values.iter().map(value_to_sql));
        binds.push(SqlValue::Integer(fid));
        let matched = self.exec_update(&sql, &binds)?;
        if matched {
            self.dirty = true;
        }
        Ok(matched)
    }

    /// Delete the feature `fid`. Returns whether a row matched.
    ///
    /// The bounding box is not shrunk (that would need a rescan; an
    /// over-estimate is spec-legal).
    pub fn delete(&mut self, fid: i64) -> Result<bool> {
        let sql = format!(
            "DELETE FROM {} WHERE {} = ?1",
            self.quoted_table, self.pk_expr
        );
        let matched = {
            let mut stmt = self.tx.prepare_cached(&sql)?;
            stmt.execute([fid])? > 0
        };
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
        if bbox_dirty && let Some([min_x, max_x, min_y, max_y]) = bbox.bounds() {
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

    fn check_values(&self, values: &[Value]) -> Result<()> {
        if values.len() == self.value_columns.len() {
            return Ok(());
        }
        Err(Error::ValueCountMismatch {
            table_name: self.table_name.clone(),
            expected: self.value_columns.len(),
            found: values.len(),
        })
    }

    /// Build the `INSERT` statement for the given fid/geometry presence.
    fn insert_sql(&self, with_fid: bool, with_geometry: bool) -> String {
        let mut columns: Vec<&str> = Vec::with_capacity(self.value_columns.len() + 2);
        if with_fid {
            columns.push(&self.pk_expr);
        }
        for column in &self.value_columns {
            columns.push(column);
        }
        if with_geometry && let Some(geom) = &self.geometry {
            columns.push(&geom.quoted_name);
        }
        if columns.is_empty() {
            return format!("INSERT INTO {} DEFAULT VALUES", self.quoted_table);
        }
        let placeholders = (1..=columns.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "INSERT INTO {} ({}) VALUES ({placeholders})",
            self.quoted_table,
            columns.join(", ")
        )
    }

    /// Build the `UPDATE ... WHERE <pk> = ?` statement.
    fn update_sql(&self, with_geometry: bool) -> String {
        let mut assignments: Vec<String> = Vec::with_capacity(self.value_columns.len() + 1);
        let mut index = 1;
        for column in &self.value_columns {
            assignments.push(format!("{column} = ?{index}"));
            index += 1;
        }
        if with_geometry && let Some(geom) = &self.geometry {
            assignments.push(format!("{} = ?{index}", geom.quoted_name));
            index += 1;
        }
        if assignments.is_empty() {
            // Nothing to change (an attribute table with only a primary key):
            // a self-assignment keeps the statement valid and rows-affected
            // meaningful.
            assignments.push(format!("{pk} = {pk}", pk = self.pk_expr));
        }
        format!(
            "UPDATE {} SET {} WHERE {} = ?{index}",
            self.quoted_table,
            assignments.join(", "),
            self.pk_expr
        )
    }

    fn exec_insert(&self, sql: &str, binds: &[SqlValue], fid: Option<i64>) -> Result<i64> {
        let mut stmt = self.tx.prepare_cached(sql)?;
        stmt.execute(params_from_iter(binds.iter()))?;
        Ok(fid.unwrap_or_else(|| self.tx.last_insert_rowid()))
    }

    fn exec_update(&self, sql: &str, binds: &[SqlValue]) -> Result<bool> {
        let mut stmt = self.tx.prepare_cached(sql)?;
        Ok(stmt.execute(params_from_iter(binds.iter()))? > 0)
    }
}

/// Read the existing `gpkg_contents` bounding box for `table` as
/// `[min_x, max_x, min_y, max_y]`, or `None` when the row or any bound is
/// absent.
fn read_contents_bbox(conn: &Connection, table: &str) -> Result<Option<[f64; 4]>> {
    let row = conn
        .query_row(
            "SELECT min_x, min_y, max_x, max_y FROM gpkg_contents WHERE table_name = ?1",
            [table],
            |r| {
                Ok((
                    r.get::<_, Option<f64>>(0)?,
                    r.get::<_, Option<f64>>(1)?,
                    r.get::<_, Option<f64>>(2)?,
                    r.get::<_, Option<f64>>(3)?,
                ))
            },
        )
        .optional()?;
    Ok(match row {
        Some((Some(min_x), Some(min_y), Some(max_x), Some(max_y))) => {
            Some([min_x, max_x, min_y, max_y])
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GeoPackage, GeometrySpec, TableSchemaBuilder};
    use geo_types::Point;
    use geopackage_core::types::GeometryType;

    /// A tamper that fails the index build outright, standing in for a crash or
    /// an I/O error between staging the rows and rebuilding the index.
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
                    .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
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
        assert!(result.is_err(), "the tampered build should have failed");

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
}
