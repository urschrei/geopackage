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
//! So a file that arrives from elsewhere may record an extent that is stale,
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
//! cannot be guaranteed to cover the layer**. NULL is spec-legal, accurate,
//! and self-repairing at the reader. A wrong box is believed forever.
//! Concretely:
//!
//! - [`crate::FeatureWriter`] grows the recorded box to cover what it writes and
//!   never shrinks it, so the result is exact or an over-estimate, which the
//!   spec expressly permits.
//! - A writer that cannot rely on its starting point leaves the extent alone
//!   rather than replacing it with a box covering only the rows it wrote. That
//!   is the case where the stored extent is absent, NULL or inverted while the
//!   table already contains rows.
//! - [`Layer::extent`] and [`Layer::recompute_extent`] write NULL, rather than
//!   anything invented, for a layer with no geometries to measure.
//! - [`Layer::extent`] records what it measured only when it could measure the
//!   whole layer inside one transaction. Losing that race to another writer
//!   means the measurement describes a layer that changed underneath it, so the
//!   file keeps what it had.
//!
//! # Reading an extent can write one
//!
//! [`Layer::extent`] records what it measures, so **reading the extent of a
//! file whose recorded extent is unusable modifies that file**: its content and
//! its modification time both change. The point is that a file improves by
//! being read: the next reader, here or anywhere else, finds a usable box and
//! no longer has to scan. GDAL behaves the same way: its `GetExtent` persists
//! through `SaveExtent` on any dataset open for update.
//!
//! The cost is that a pipeline checksumming its inputs will see them change. If
//! that matters, open read-only, which suppresses the write entirely, or read
//! [`crate::GeoPackage::contents`] for the recorded values without measuring
//! anything.
//!
//! Note the asymmetry this leaves with [`Layer::repair_spatial_index`], which
//! still never writes unless asked. The index is repaired only on request
//! because a rebuild is expensive and its absence is not silently wrong; an
//! unusable extent is cheap to fix and *is* silently wrong for every reader
//! until someone fixes it.
//!
//! # Deviation from GDAL
//!
//! One. GDAL prefers the RTree when it has to compute an extent, which is
//! faster but yields a box rounded outward to `f32` and takes the index's word
//! for what the layer contains. This crate always measures the geometries. A
//! value about to be written into a file and believed indefinitely is worth
//! measuring exactly, and measuring removes any dependence on the index being
//! current.

use geopackage_core::ident::quote;
use rusqlite::OptionalExtension;

use crate::transaction::WriteTransaction;
use crate::{BoundingBox, Error, Layer, Result};

impl Layer<'_> {
    /// Returns the layer's extent: the recorded `gpkg_contents` bounds when
    /// they are usable, and otherwise the true extent measured from the
    /// geometries.
    ///
    /// `None` when the layer contains no geometry to measure. The recorded
    /// bounds are usable when all four are present and neither axis is
    /// inverted, which is as much as any reader can check, since no
    /// requirement governs the values; a recorded box that passes it is
    /// returned as it stands, so this inherits whatever inexactness the file
    /// records. Use [`Self::recompute_extent`] for an answer that does not.
    ///
    /// # This can modify the file
    ///
    /// When the recorded bounds are unusable and the connection can write, the
    /// measurement is recorded before it is returned, so **this method changes
    /// the file's contents and modification time**. The point is that the file
    /// stops being wrong, for every later reader rather than only this one.
    ///
    /// The recorded extent deserves this caution because the format gives it
    /// none: the spec calls the bounds informative, sets no requirement on
    /// their values, and a reader can only check that they are present and
    /// well ordered. A well-ordered wrong box is therefore believed
    /// indefinitely (GDAL returns one verbatim and never recomputes it, even
    /// under `bForce`; QGIS behaves the same way), while NULL bounds make a
    /// reader compute the real extent. That asymmetry is why this crate
    /// records NULL rather than anything invented, and never records a box it
    /// cannot guarantee covers the layer.
    ///
    /// Nothing is written when the recorded bounds are already usable, which is
    /// the ordinary case, nor on a read-only connection, nor when there is
    /// nothing to measure and the bounds are already NULL. To read the recorded
    /// values without measuring or writing anything, use
    /// [`crate::GeoPackage::contents`].
    ///
    /// A layer with no geometry to measure has its bounds set to NULL, which is
    /// what makes a reader compute the extent itself rather than believe an
    /// invented one. GDAL does the same through `UpdateContentsToNullExtent`.
    ///
    /// # Errors
    ///
    /// - [`Error::NoGeometryColumn`] if the layer has no geometry column.
    /// - [`Error::ExtentPersist`] if the measurement could not be recorded for a
    ///   reason other than another connection holding a lock: an unwritable
    ///   directory, a full disk, an I/O error. It includes the measured
    ///   extent, so the answer is not lost with the failure.
    ///
    /// Lock contention is deliberately **not** an error. A concurrent writer
    /// means the measurement describes a layer that is being changed underneath
    /// it, so it is not a value this crate can guarantee; the file is left as
    /// it was and the measurement is returned. Waiting for the lock is already
    /// covered by [`crate::OpenOptions::busy_timeout`], and in the two cases
    /// that arise here most often, a read-to-write upgrade under a rollback
    /// journal and a stale snapshot under WAL, SQLite reports the conflict
    /// immediately whatever that timeout says.
    pub fn extent(&self) -> Result<Option<BoundingBox>> {
        // Checked before the recorded value is consulted, so that a layer with
        // no geometry column answers the same way whether or not its
        // `gpkg_contents` row happens to carry bounds.
        if self.geometry_column().is_none() {
            return Err(Error::NoGeometryColumn {
                table_name: self.table_name().to_owned(),
            });
        }
        if let Some(stored) = self.stored_extent()? {
            return Ok(Some(stored));
        }
        self.measure_and_record()
    }

    /// Measures the extent and records it, returning the measurement whether
    /// or not it could be recorded.
    fn measure_and_record(&self) -> Result<Option<BoundingBox>> {
        let conn = self.gpkg().connection();
        // A read-only connection has nothing to record and nothing has gone
        // wrong, so this is not a failure to report. One call covers both a
        // read-only open and a file the process cannot write, since SQLite
        // downgrades the latter on open.
        if conn.is_readonly(rusqlite::MAIN_DB)? {
            return self.measure_extent(conn);
        }
        // Measuring and recording share a transaction, so the box recorded
        // cannot exclude a row committed between the two. When the caller
        // already has one open, that one is used: SQLite cannot nest, and
        // inheriting theirs gives the same atomicity. This branch is where that
        // reasoning was first written down; `WriteTransaction` is it made
        // reusable, and every write path in the crate now works this way.
        let tx = WriteTransaction::begin(conn)?;
        let measured = self.measure_extent(conn)?;
        let recorded = self
            .record_extent(conn, measured)
            .and_then(|()| tx.commit());
        self.finish_recording(measured, recorded)
    }

    /// Writes a measured extent into `gpkg_contents`.
    ///
    /// Nothing to measure records NULL, but only where the bounds are not NULL
    /// already: an empty layer is read far more often than it changes, and a
    /// write that changes no value would still change the file's modification
    /// time.
    fn record_extent(
        &self,
        conn: &rusqlite::Connection,
        measured: Option<BoundingBox>,
    ) -> rusqlite::Result<()> {
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
            ),
            None => conn.execute(
                "UPDATE gpkg_contents \
                 SET min_x = NULL, min_y = NULL, max_x = NULL, max_y = NULL \
                 WHERE table_name = ?1 AND (min_x IS NOT NULL OR min_y IS NOT NULL \
                 OR max_x IS NOT NULL OR max_y IS NOT NULL)",
                [self.table_name()],
            ),
        }
        .map(|_| ())
    }

    /// Decides what a failed recording means.
    ///
    /// Another connection's lock is not a failure to report: it says a writer
    /// is active, which is exactly the condition under which the measurement
    /// cannot be guaranteed, so the file keeps what it had and the
    /// measurement is returned. Anything else means the store is unwritable or
    /// broken, which the caller should hear about, and the measurement is
    /// included with the error so that hearing about it costs nothing.
    fn finish_recording(
        &self,
        measured: Option<BoundingBox>,
        recorded: rusqlite::Result<()>,
    ) -> Result<Option<BoundingBox>> {
        match recorded {
            Ok(()) => Ok(measured),
            Err(source) if is_lock_contention(&source) => Ok(measured),
            Err(source) => Err(Error::ExtentPersist {
                table_name: self.table_name().to_owned(),
                extent: measured,
                source: Box::new(source),
            }),
        }
    }

    /// Measures the layer's true extent and records it in `gpkg_contents`,
    /// returning what was written.
    ///
    /// The counterpart of GDAL's `RECOMPUTE EXTENT ON <layer>`. A layer with no
    /// geometry to measure has its bounds set to NULL, which is what lets a
    /// reader compute the extent itself rather than believe an invented one.
    ///
    /// Where [`Self::extent`] records only what it had to measure, and so
    /// leaves a well-ordered box alone however wrong it is, this measures
    /// unconditionally. It is the way to correct an extent that is merely
    /// suspect, which nothing else here will touch, because nothing
    /// distinguishes a wrong well-ordered box from a right one without
    /// measuring.
    ///
    /// The measurement and the write share one transaction. Two statements in
    /// autocommit would leave a window for another connection to commit a row
    /// between them, and the box recorded would then exclude it: a value the
    /// crate cannot guarantee, which is the one thing this module forbids.
    /// Holding the read open closes the window under a rollback journal, and
    /// under WAL turns it into a `SQLITE_BUSY_SNAPSHOT` error rather than a
    /// silent write of a stale measurement.
    ///
    /// # Errors
    ///
    /// [`Error::NoGeometryColumn`] if the layer has no geometry column.
    pub fn recompute_extent(&self) -> Result<Option<BoundingBox>> {
        let conn = self.gpkg().connection();
        let tx = WriteTransaction::begin(conn)?;
        let measured = self.measure_extent(conn)?;
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
        tx.commit()?;
        Ok(measured)
    }

    /// Returns the recorded extent, if it is usable: all four bounds present,
    /// and neither axis inverted.
    ///
    /// An inverted box is treated as absent rather than repaired. It says
    /// nothing about where the data is, and GDAL reads it the same way.
    ///
    /// This reports only what the file says; it never writes. Its callers
    /// decide what to do about an unusable answer: [`Layer::extent`] measures
    /// and records, and [`crate::FeatureWriter`] does not record a fold it
    /// cannot guarantee.
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

    /// Measures the true extent from the geometries themselves, through the
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

    /// Returns `true` if the table contains at least one row.
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

/// Returns `true` if an error is another connection holding a lock.
///
/// `DatabaseBusy` is the plain code behind both `SQLITE_BUSY` and
/// `SQLITE_BUSY_SNAPSHOT`, which is the whole of what contention looks like on
/// a connection that does not use shared cache.
fn is_lock_contention(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy)
    )
}
