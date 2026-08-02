use geo_traits::{Dimensions, GeometryTrait};
use geopackage_core::extensions::{Extension, GEOM_TYPE_EXTENSION_DEFINITION};
use geopackage_core::geometry::encode_gpb;
use geopackage_core::geometry::encode_gpb_from_wkb;
use geopackage_core::types::{GeometryTypeSet, ZmFlag};
use rusqlite::types::{ToSqlOutput, Value as SqlValue, ValueRef};
use rusqlite::{CachedStatement, Connection, Params, params_from_iter};

use crate::extensions;
use crate::transaction::WriteTransaction;
use crate::value::{value_ref_to_bind, value_to_bind};
use crate::{Error, Result, Value, ValueRef as CellRef};

use super::constraints::{AsCheckable, Checkable, ColumnConstraints};
use super::write_all::{BboxFold, GeomTarget};

/// A prepared-statement writer over one layer, owning a transaction.
///
/// Obtain one with [`crate::Layer::writer`]. Each `insert`/`update`/`delete` stages
/// into the writer's transaction through a statement the writer keeps;
/// [`Self::commit`] flushes catalogue metadata and commits. Dropping a writer
/// without committing rolls its transaction back. The `gpkg_contents` bounding
/// box is grown by a running fold over written geometry envelopes and
/// `last_change` is refreshed on commit.
///
/// Both of those last two sentences change when the writer was opened inside a
/// transaction the caller had already begun: see [`Self::commit`].
pub struct FeatureWriter<'conn> {
    /// Opened by [`crate::Layer::writer`], or the caller's, inherited. Only the commit
    /// distinguishes them; every statement is issued against `conn`, which is
    /// the same connection either way.
    pub(crate) tx: WriteTransaction<'conn>,
    /// The connection the transaction runs on, for preparing the statement a
    /// partial update needs, whose shape is not known until it is called.
    pub(crate) conn: &'conn Connection,
    pub(crate) table_name: String,
    pub(crate) quoted_table: String,
    /// The primary-key expression: the quoted pk column, or `rowid`.
    pub(crate) pk_expr: String,
    /// The value columns, in value order: neither the geometry nor the primary
    /// key is among them.
    pub(crate) value_columns: Vec<ValueColumn>,
    pub(crate) geometry: Option<GeomTarget>,
    pub(crate) bbox: BboxFold,
    /// The four possible `INSERT` statements, by whether the row has an
    /// explicit feature id and whether it has a geometry.
    ///
    /// A layer's shape is fixed for a writer's lifetime, so every statement it
    /// can issue is composed and prepared once here rather than per row.
    /// Composing an `INSERT` costs a `Vec` of column names, a `String` per
    /// placeholder and two joins, which is around seventeen allocations for a
    /// fifteen-column table; looking one up in the connection's statement cache
    /// costs one more. Prepared this way, a row costs neither.
    ///
    /// The statements come from the connection rather than from `tx`, so they
    /// borrow what the transaction borrows instead of borrowing the transaction
    /// itself. They still run inside it: a SQLite transaction belongs to the
    /// connection, not to the statements prepared against it.
    pub(crate) insert_stmts: [CachedStatement<'conn>; 4],
    /// The two possible `UPDATE` statements, by whether the row has a
    /// geometry. A writer's update sets every value column, so there are only
    /// these two shapes.
    pub(crate) update_stmts: [CachedStatement<'conn>; 2],
    /// The `DELETE`, likewise fixed for the writer's lifetime.
    pub(crate) delete_stmt: CachedStatement<'conn>,
    /// The `gpkg_schema` constraints to check written values against, resolved
    /// once here. Empty unless the file was opened asking for them.
    pub(crate) constraints: ColumnConstraints<'conn>,
    /// The value columns named by the most recent [`Self::update_columns`]
    /// call, in the order given, and the statement prepared for them.
    ///
    /// The caller chooses a partial update's shape, so it cannot be
    /// prepared up front like the others. Keeping the last one makes a loop
    /// that recomputes the same columns for every row prepare once; a caller
    /// alternating between two sets of columns re-prepares on each change.
    /// The starting state names no columns, which is a statement in its own
    /// right (it assigns the primary key to itself), so the slot is never
    /// empty.
    pub(crate) partial_columns: Vec<String>,
    pub(crate) partial_stmt: CachedStatement<'conn>,
    /// Any insert, or any update/delete that changed a row (drives
    /// `last_change`).
    pub(crate) dirty: bool,
    /// A geometry was written (drives the bounding-box flush).
    pub(crate) bbox_dirty: bool,
    /// Whether the fold covers the whole layer, and so may be recorded. False
    /// when the writer started with no usable recorded box over a table that
    /// already contained rows, which makes the fold a lower bound rather than
    /// the extent.
    pub(crate) bbox_covers_layer: bool,
    /// The non-linear geometry types written through this writer, which the
    /// flush registers as `gpkg_geom_<TYPE>` rows.
    ///
    /// Only the WKB entry points can add to this: a `GeometryTrait` has no
    /// non-linear representation to offer. Accumulated rather than registered
    /// per row so that a write of a million curves issues one registration
    /// rather than a million lookups, and registered at the flush so that a
    /// writer dropped without committing registers nothing, matching the rows
    /// it also did not write.
    pub(crate) geometry_types: GeometryTypeSet,
}

impl<'conn> FeatureWriter<'conn> {
    /// Inserts a feature with a geometry, returning its feature id.
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

    /// Inserts a feature whose geometry is already ISO WKB, returning its
    /// feature id.
    ///
    /// [`Self::insert`] takes a geometry object, which the `geo-traits`
    /// interface can only describe for the linear types. The non-linear types
    /// (`CIRCULARSTRING`, `COMPOUNDCURVE`, `CURVEPOLYGON`, `MULTICURVE`,
    /// `MULTISURFACE`) have no such representation, so WKB bytes are how one is
    /// written. The bytes are read for the envelope the header and the index
    /// both need, and to reject a body that is not ISO WKB.
    ///
    /// The bytes are copied into the blob rather than re-serialised, so a body
    /// read from another GeoPackage passes through unchanged.
    ///
    /// # Errors
    ///
    /// As [`Self::insert`], plus [`Error::Core`] if the bytes are not a
    /// geometry that can be read: malformed, EWKB rather than ISO WKB, or one
    /// of the abstract supertypes, which have no encoding.
    pub fn insert_wkb(
        &mut self,
        fid: Option<i64>,
        wkb: &[u8],
        values: &[CellRef<'_>],
    ) -> Result<i64> {
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

        let assigned = self.exec_insert(
            fid.is_some(),
            true,
            params_from_iter(
                fid.map(|id| ToSqlOutput::Borrowed(ValueRef::Integer(id)))
                    .into_iter()
                    .chain(values.iter().copied().map(value_ref_to_bind))
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
        // After the row is in, so a rejected insert does not register a type
        // the table does not hold.
        self.geometry_types.extend(encoded.extension_types);
        self.dirty = true;
        Ok(assigned)
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

    /// Inserts a feature whose geometry is already ISO WKB, with its
    /// non-geometry values already prepared as bindings.
    ///
    /// The counterpart of [`Self::insert_returning_envelope`] for the columnar
    /// path, and the reason it takes bindings rather than [`Value`]s: an Arrow
    /// batch already stores every string and blob contiguously, so a binding that
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
        // As in `insert_wkb`: recorded once the row is in.
        self.geometry_types.extend(encoded.extension_types);
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

    /// Inserts a row with no geometry (a NULL geometry on a feature table, or
    /// an attribute row), returning its feature id.
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

    /// [`Self::insert_row`] for a caller with owned values.
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

    /// Updates the feature `fid`, setting its geometry and values. Returns
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

    /// [`Self::update`] with the geometry as WKB rather than as a
    /// [`GeometryTrait`], the counterpart of [`Self::insert_wkb`].
    ///
    /// The bytes are wrapped in a GPB header and stored as they arrive, so a
    /// geometry this crate cannot represent as a `geo-types` value, a curve
    /// above all, survives an update the way it survives an insert. That is
    /// also the right behaviour for moving geometry between files: no decode,
    /// no re-encode, and nothing lost in between.
    ///
    /// Returns whether a row matched.
    ///
    /// # Errors
    ///
    /// - [`Error::NoGeometryColumn`] if the layer has no geometry column (use
    ///   [`Self::update_row`]).
    /// - [`Error::ZmViolation`] if the geometry's `z`/`m` presence breaks the
    ///   column's constraint.
    /// - [`Error::ValueCountMismatch`] if `values` has the wrong length.
    /// - [`Error::Core`] if the bytes are not WKB this crate can read far
    ///   enough to header.
    pub fn update_wkb(&mut self, fid: i64, wkb: &[u8], values: &[CellRef<'_>]) -> Result<bool> {
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

        let matched = self.exec_update(
            true,
            params_from_iter(values.iter().copied().map(value_ref_to_bind).chain([
                ToSqlOutput::Owned(SqlValue::Blob(encoded.blob)),
                ToSqlOutput::Borrowed(ValueRef::Integer(fid)),
            ])),
        )?;
        if matched {
            // As everywhere else, the fold only grows: an update that shrinks a
            // geometry leaves the recorded box an over-estimate, which the spec
            // permits and which shrinking would need a rescan to correct.
            if let Some(envelope) = encoded.xy_envelope {
                self.bbox.add(envelope);
                self.bbox_dirty = true;
            }
            // Only when a row matched: an update that changed nothing put no
            // new type in the table.
            self.geometry_types.extend(encoded.extension_types);
            self.dirty = true;
        }
        Ok(matched)
    }

    /// Updates the feature `fid`'s non-geometry values, leaving the geometry
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

    /// Updates named value columns of the feature `fid`, leaving every other
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
    /// is sound for a plain [`crate::Layer::cursor`] writing non-indexed columns; see
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

    /// Returns `true` if `columns` names exactly what the kept partial
    /// statement was prepared for, in the same order. Comparing the names rather than
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

    /// Deletes the feature `fid`. Returns whether a row matched.
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

    /// Flushes `gpkg_contents` (`last_change`, and the bounding box when a
    /// geometry was written) and commits the transaction.
    ///
    /// # When the transaction was the caller's
    ///
    /// A writer opened while a transaction was already open on the connection
    /// joined that transaction rather than nesting inside it, because SQLite
    /// does not nest. This call then does everything above except the commit:
    /// the `gpkg_contents` flush is staged like every other statement, and
    /// success means the work is in the caller's transaction, not that it is
    /// durable. The caller issues the commit, or the rollback.
    ///
    /// It follows that dropping such a writer without calling this does not
    /// roll anything back, so an error part-way through a sequence of writes
    /// leaves what preceded it staged for the caller to discard.
    pub fn commit(self) -> Result<()> {
        self.flush()?.commit()?;
        Ok(())
    }

    /// Returns the connection underlying this writer's transaction, so a
    /// caller can run additional statements inside the same transaction.
    ///
    /// Borrowed for `'conn` rather than for the writer, because the connection
    /// outlives the writer and the bulk path needs it after
    /// [`Self::flush`] has consumed one.
    pub(crate) fn connection(&self) -> &'conn Connection {
        self.conn
    }

    /// Flushes the `gpkg_contents` metadata and returns the still-open
    /// transaction, leaving it to the caller to commit.
    ///
    /// The bulk `write_all` path uses this to keep the row inserts and the
    /// index rebuild in one transaction. Dropping the returned transaction
    /// without committing rolls the whole write back, exactly as dropping the
    /// writer would have, unless the transaction is the caller's.
    ///
    /// The two updates are issued against the connection rather than against
    /// the returned value, which is the same connection and is what an
    /// inherited transaction has no handle on.
    pub(crate) fn flush(self) -> Result<WriteTransaction<'conn>> {
        let Self {
            tx,
            conn,
            table_name,
            geometry,
            bbox,
            dirty,
            bbox_dirty,
            bbox_covers_layer,
            geometry_types,
            ..
        } = self;
        // Annex F.1 Requirement 67, for the types that were written rather than
        // the one the column declares. `create_layer` registers the declared
        // type, so what is new here is a container's members: a MULTICURVE
        // layer holding CIRCULARSTRINGs needs a row for those too, which is
        // what GDAL writes and what a reader checks the file against.
        if let Some(geom) = geometry.as_ref() {
            for ty in geometry_types.iter() {
                extensions::register_if_absent(
                    conn,
                    Some(&table_name),
                    Some(&geom.name),
                    &Extension::GeometryType(ty).name(),
                    GEOM_TYPE_EXTENSION_DEFINITION,
                    "read-write",
                )?;
            }
        }
        if dirty {
            conn.execute(
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
            conn.execute(
                "UPDATE gpkg_contents \
                 SET min_x = ?1, min_y = ?2, max_x = ?3, max_y = ?4 \
                 WHERE table_name = ?5",
                rusqlite::params![min_x, min_y, max_x, max_y, table_name],
            )?;
        }
        Ok(tx)
    }

    /// Validates the geometry's `z`/`m` against the column and encodes it to a
    /// GPB blob, returning the blob and its XY envelope (for the bbox fold).
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

    /// Enforces a `z`/`m` presence constraint for a written geometry.
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
            verb: if present { "has" } else { "lacks" },
        })
    }

    /// Rejects a value list whose length does not match the layer's value
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

    /// Checks a whole row's values against the layer's `gpkg_schema`
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

    /// Builds the error for a value the constraint at `index` rejected. Off
    /// the hot path, so it re-reads what it needs rather than being threaded
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

    /// Returns the prepared `INSERT` for this combination of explicit id and
    /// geometry.
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

    /// Returns the prepared `UPDATE`, with or without the geometry
    /// assignment.
    fn update_stmt(&mut self, with_geometry: bool) -> &mut CachedStatement<'conn> {
        let [plain, with_geom] = &mut self.update_stmts;
        if with_geometry { with_geom } else { plain }
    }

    /// Runs one insert.
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
        Ok(fid.unwrap_or_else(|| self.conn.last_insert_rowid()))
    }

    fn exec_update<P: Params>(&mut self, with_geometry: bool, binds: P) -> Result<bool> {
        Ok(self.update_stmt(with_geometry).execute(binds)? > 0)
    }
}

/// The pieces of a layer's shape that its statement text is composed from,
/// borrowed while [`crate::Layer::writer`] builds them and before they move into the
/// writer.
pub(crate) struct Shape<'s> {
    pub(crate) table_name: &'s str,
    pub(crate) quoted_table: &'s str,
    pub(crate) pk_expr: &'s str,
    pub(crate) value_columns: &'s [ValueColumn],
    pub(crate) geometry: Option<&'s GeomTarget>,
}

/// One value column of the layer being written: its name as declared, and the
/// same name quoted for use in a statement.
pub(crate) struct ValueColumn {
    pub(crate) name: String,
    pub(crate) quoted: String,
}

/// Composes one of the four `INSERT` statements. Called once per writer.
pub(crate) fn build_insert_sql(shape: &Shape<'_>, with_fid: bool, with_geometry: bool) -> String {
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

/// Composes one of the two `UPDATE ... WHERE <pk> = ?` statements. Called
/// once per writer.
pub(crate) fn build_update_sql(shape: &Shape<'_>, with_geometry: bool) -> String {
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

/// Composes the `UPDATE` for a named subset of the value columns, assigning
/// them in the order given. Called only when [`FeatureWriter::update_columns`]
/// is called with a set of columns other than the one it last prepared.
pub(crate) fn build_partial_update_sql(
    shape: &Shape<'_>,
    columns: &[(&str, CellRef<'_>)],
) -> Result<String> {
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

/// Re-borrows a prepared binding so it can be chained with owned ones.
///
/// The columnar path hands over a slice of bindings it still owns. Cloning them
/// to build the statement's parameter list would copy every owned cell, which
/// is exactly the copy those bindings exist to avoid, so an owned binding is
/// re-borrowed rather than duplicated.
#[cfg(feature = "arrow")]
pub(crate) fn borrow_bind<'a>(bind: &'a ToSqlOutput<'_>) -> ToSqlOutput<'a> {
    match bind {
        ToSqlOutput::Borrowed(value) => ToSqlOutput::Borrowed(*value),
        ToSqlOutput::Owned(value) => ToSqlOutput::Borrowed(ValueRef::from(value)),
        // `ToSqlOutput` is non-exhaustive; nothing this crate builds reaches
        // here, and the remaining variants are all cheap to copy.
        other => other.clone(),
    }
}
