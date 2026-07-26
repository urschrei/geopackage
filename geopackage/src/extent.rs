//! The layer extent recorded in `gpkg_contents`: [`Layer::extent`] and
//! [`Layer::recompute_extent`].
//!
//! # The extent is not to be trusted
//!
//! The four `gpkg_contents` bounds are the least reliable values in a
//! GeoPackage, and that is by the standard's design rather than by neglect. The
//! spec text is prose rather than a numbered requirement, identical in every
//! version from 1.2.0 to 1.4.0: the bounding box "provides an informative
//! bounding box of the content", applications "may use this bounding box as the
//! extents of a default view", and "there are no requirements that this bounding
//! box be exact or represent the minimum bounding box of the content". The
//! columns are nullable and no requirement governs their values. The OGC SWG
//! considered adding a test that features fall inside the recorded extent and
//! [declined in 2018](https://github.com/opengeospatial/geopackage/issues/443),
//! on the grounds that maintaining it on insert and delete is too expensive.
//!
//! So a file that arrives from elsewhere may carry an extent that is stale,
//! inflated, or simply wrong, and it is still conformant. Nothing in this crate
//! ever uses the stored extent to answer a query: [`Layer::features_in`] and
//! [`Layer::cursor_in`] go through the RTree or a full scan with an exact `f64`
//! re-filter, never a short-circuit against the recorded bounds. That rule is
//! worth keeping. An optimisation that skips a scan because the query box misses
//! the recorded extent would be correct against files this crate wrote and wrong
//! against files it did not.
//!
//! # Why a wrong extent is worse than an absent one
//!
//! The columns are nullable, and readers treat the two cases very differently.
//! GDAL validates only that the four values are present and that `min <= max`
//! per axis; a well-ordered extent that is wrong is returned verbatim and never
//! recomputed, even when the caller passes `bForce`. A NULL extent, by contrast,
//! makes GDAL compute the real one. QGIS behaves the same way, and since 3.34
//! its "Update Extents" action is a no-op for a local GeoPackage, so a user who
//! receives a file with a wrong extent has no way to repair it short of a full
//! edit session.
//!
//! The rule this crate follows is therefore: **never record an extent that
//! cannot be vouched for**. NULL is spec-legal, honest, and self-repairing at
//! the reader. A wrong box is believed forever. Concretely:
//!
//! - [`crate::FeatureWriter`] grows the recorded box to cover what it writes and
//!   never shrinks it, so the result is exact or an over-estimate, which the
//!   spec expressly permits.
//! - A writer that cannot vouch for its starting point leaves the extent alone
//!   rather than replacing it with a box covering only the rows it wrote. That
//!   is the case where the stored extent is absent, NULL or inverted while the
//!   table already holds rows.
//! - [`Layer::recompute_extent`] writes NULL, rather than anything invented,
//!   for a layer with no geometries to measure.
//!
//! # Deviations from GDAL
//!
//! Two, both deliberate.
//!
//! GDAL persists a recomputed extent as a side effect of reading it, when the
//! dataset is open for update. [`Layer::extent`] computes and returns without
//! writing, and [`Layer::recompute_extent`] is the operation that persists.
//! Reading a file should not modify it, which is the same rule
//! [`Layer::repair_spatial_index`] follows.
//!
//! GDAL prefers the RTree when it has to compute an extent, which is faster but
//! yields a box rounded outward to `f32`. This crate always measures the
//! geometries themselves. GDAL computes routinely, so the shortcut earns its
//! imprecision; here a computation happens only when the stored value is
//! unusable, and a value that is about to be written into a file and believed
//! indefinitely is worth measuring exactly. It also removes any dependence on
//! the index being current.

use geopackage_core::ident::quote;
use rusqlite::OptionalExtension;

use crate::{BoundingBox, Error, Layer, Result};

impl Layer<'_> {
    /// The layer's extent: the recorded `gpkg_contents` bounds when they are
    /// usable, and otherwise the true extent measured from the geometries.
    ///
    /// `None` when the layer holds no geometry to measure. The recorded bounds
    /// are usable when all four are present and neither axis is inverted, which
    /// is the same test GDAL applies; a recorded box that passes it is returned
    /// as it stands, so this inherits whatever inexactness the file carries. Use
    /// [`Self::recompute_extent`] for an answer that does not.
    ///
    /// This never writes to the file, so a computed extent is not persisted and
    /// a later call measures again.
    ///
    /// # Errors
    ///
    /// [`Error::NoGeometryColumn`] if the layer has no geometry column.
    pub fn extent(&self) -> Result<Option<BoundingBox>> {
        // Checked before the recorded value is consulted, so that a layer with
        // no geometry column answers the same way whether or not its
        // `gpkg_contents` row happens to carry bounds.
        if self.geometry_column().is_none() {
            return Err(Error::NoGeometryColumn {
                table_name: self.table_name().to_owned(),
            });
        }
        match self.stored_extent()? {
            Some(stored) => Ok(Some(stored)),
            None => self.measure_extent(self.gpkg().connection()),
        }
    }

    /// Measure the layer's true extent and record it in `gpkg_contents`,
    /// returning what was written.
    ///
    /// The counterpart of GDAL's `RECOMPUTE EXTENT ON <layer>`. A layer with no
    /// geometry to measure has its bounds set to NULL, which is what lets a
    /// reader compute the extent itself rather than believe an invented one.
    ///
    /// This is the only operation here that writes, and it is never automatic:
    /// an extent that is merely suspect is the file's business, not this
    /// crate's.
    ///
    /// The measurement and the write share one transaction. Two statements in
    /// autocommit would leave a window for another connection to commit a row
    /// between them, and the box recorded would then exclude it: a value the
    /// crate cannot vouch for, which is the one thing this module forbids.
    /// Holding the read open closes the window under a rollback journal, and
    /// under WAL turns it into a `SQLITE_BUSY_SNAPSHOT` error rather than a
    /// silent write of a stale measurement.
    ///
    /// # Errors
    ///
    /// [`Error::NoGeometryColumn`] if the layer has no geometry column.
    pub fn recompute_extent(&self) -> Result<Option<BoundingBox>> {
        let conn = self.gpkg().connection().unchecked_transaction()?;
        let measured = self.measure_extent(&conn)?;
        match measured {
            Some(bbox) => conn.execute(
                "UPDATE gpkg_contents SET min_x = ?1, min_y = ?2, max_x = ?3, max_y = ?4 \
                 WHERE table_name = ?5",
                rusqlite::params![
                    bbox.min_x,
                    bbox.min_y,
                    bbox.max_x,
                    bbox.max_y,
                    self.table_name()
                ],
            )?,
            None => conn.execute(
                "UPDATE gpkg_contents \
                 SET min_x = NULL, min_y = NULL, max_x = NULL, max_y = NULL \
                 WHERE table_name = ?1",
                [self.table_name()],
            )?,
        };
        conn.commit()?;
        Ok(measured)
    }

    /// The recorded extent, if it is usable: all four bounds present, and
    /// neither axis inverted.
    ///
    /// An inverted box is treated as absent rather than repaired. It carries no
    /// information about where the data is, and GDAL reads it the same way. It
    /// is also left in the file rather than normalised to NULL: a reader meets
    /// an inverted box and a NULL one with the same recompute, so rewriting one
    /// into the other would buy nothing and would mean a write nobody asked
    /// for. [`Layer::recompute_extent`] is what replaces it.
    pub(crate) fn stored_extent(&self) -> Result<Option<BoundingBox>> {
        let bounds: Option<[Option<f64>; 4]> = self
            .gpkg()
            .connection()
            .query_row(
                "SELECT min_x, min_y, max_x, max_y FROM gpkg_contents WHERE table_name = ?1",
                [self.table_name()],
                |row| Ok([row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?]),
            )
            .optional()?;
        // No `gpkg_contents` row at all is no recorded extent, as four NULLs are.
        let Some([Some(min_x), Some(min_y), Some(max_x), Some(max_y)]) = bounds else {
            return Ok(None);
        };
        if min_x > max_x || min_y > max_y {
            return Ok(None);
        }
        Ok(Some(BoundingBox::new(min_x, min_y, max_x, max_y)))
    }

    /// Measure the true extent from the geometries themselves, through the
    /// registered `ST_*` functions.
    ///
    /// NULL and empty geometries contribute nothing: `ST_MinX` and its siblings
    /// return NULL for them, and the aggregates skip NULLs, exactly as the RTree
    /// triggers skip the same rows. A layer whose geometries are all NULL or
    /// empty therefore measures as `None` rather than as a degenerate box.
    ///
    /// Reads through the connection it is given rather than the layer's, so a
    /// caller that is going to record the result can measure inside the same
    /// transaction it writes in.
    fn measure_extent(&self, conn: &rusqlite::Connection) -> Result<Option<BoundingBox>> {
        let geometry = self
            .geometry_column()
            .ok_or_else(|| Error::NoGeometryColumn {
                table_name: self.table_name().to_owned(),
            })?;
        let column = quote(&geometry.column_name)?;
        let sql = format!(
            "SELECT min(ST_MinX({column})), min(ST_MinY({column})), \
             max(ST_MaxX({column})), max(ST_MaxY({column})) FROM {}",
            quote(self.table_name())?
        );
        let bounds: [Option<f64>; 4] = conn.query_row(&sql, [], |row| {
            Ok([row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?])
        })?;
        let [Some(min_x), Some(min_y), Some(max_x), Some(max_y)] = bounds else {
            return Ok(None);
        };
        Ok(Some(BoundingBox::new(min_x, min_y, max_x, max_y)))
    }

    /// Whether the table holds at least one row.
    ///
    /// The writer needs this to tell "the extent is unknown because nothing has
    /// been written yet", where a fold over what it writes is the whole truth,
    /// from "the extent is unknown but rows already exist", where it is only a
    /// lower bound.
    pub(crate) fn has_rows(&self) -> Result<bool> {
        let sql = format!(
            "SELECT EXISTS(SELECT 1 FROM {} LIMIT 1)",
            quote(self.table_name())?
        );
        Ok(self.gpkg().connection().query_row(&sql, [], |row| {
            row.get::<_, i64>(0).map(|exists| exists != 0)
        })?)
    }
}
