//! Writing tiles to a coverage.

use geopackage_core::coverage::{
    CoverageDatatype, CoveragePayload, TILE_ANCILLARY_TABLE, coverage_payload,
};
use geopackage_core::ident::quote;
use geopackage_core::tiles::{TileCoord, TileMatrix};
use rusqlite::{CachedStatement, Connection};

use crate::transaction::{WriteTransaction, in_savepoint};
use crate::{Error, Result};

use super::Coverage;
#[cfg(feature = "coverage-samples")]
use super::CoverageAncillary;
use super::TileAncillary;
use super::ancillary::check_float_defaults;
use super::ancillary::check_not_nan;

impl<'a> Coverage<'a> {
    /// Writes one tile and its ancillary row, in a separate transaction.
    ///
    /// The ancillary row has the default scale and offset, and no statistics.
    /// Use [`CoverageWriter::put_with_ancillary`] to supply them.
    ///
    /// # Errors
    ///
    /// As [`CoverageWriter::put`].
    pub fn put_tile(&self, coord: TileCoord, data: &[u8]) -> Result<()> {
        let mut writer = self.writer()?;
        writer.put(coord, data)?;
        writer.commit()
    }

    /// Deletes one tile and its ancillary row. Returns `true` if there was a
    /// tile to delete.
    ///
    /// # Errors
    ///
    /// [`Error`] if the rows cannot be deleted.
    pub fn delete_tile(&self, coord: TileCoord) -> Result<bool> {
        let mut writer = self.writer()?;
        let deleted = writer.delete(coord)?;
        writer.commit()?;
        Ok(deleted)
    }

    /// Opens a [`CoverageWriter`]: one transaction, prepared statements, and
    /// per-tile `put` and `delete` operations that keep the two tables
    /// consistent.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedExtension`] if an extension this crate cannot
    /// identify covers the coverage ([`GeoPackage::blocking_extension`](crate::GeoPackage::blocking_extension)), and
    /// [`Error`] if the transaction cannot be opened or the statements
    /// prepared.
    pub fn writer(&self) -> Result<CoverageWriter<'a>> {
        CoverageWriter::new(self)
    }
}

/// A transaction over one coverage, with per-tile `put` and `delete`
/// operations that maintain the ancillary rows of Requirement 10.
///
/// You get one from [`Coverage::writer`]. The writer writes a tile and its
/// ancillary row together, and deletes them together. The normative table
/// definition has no `ON DELETE CASCADE`, so the database does not delete the
/// row. A tile without its row loses the scale and offset that give its samples
/// a meaning.
pub struct CoverageWriter<'conn> {
    tx: WriteTransaction<'conn>,
    conn: &'conn Connection,
    table_name: String,
    datatype: Option<CoverageDatatype>,
    /// The coverage row, for the statistics of
    /// [`CoverageWriter::put_with_statistics`], which are in values and so
    /// need the scale, offset and `data_null` of the coverage.
    #[cfg(feature = "coverage-samples")]
    pub(crate) ancillary: CoverageAncillary,
    /// See [`crate::OpenOptions::max_decoded_samples`].
    #[cfg(feature = "coverage-samples")]
    pub(crate) max_decoded_samples: usize,
    /// The zoom levels of the coverage, ascending. Copied as
    /// [`crate::TileWriter`] copies the zoom levels of a pyramid, for the same
    /// reason.
    matrices: Vec<TileMatrix>,
    put_stmt: CachedStatement<'conn>,
    tile_id_stmt: CachedStatement<'conn>,
    put_ancillary_stmt: CachedStatement<'conn>,
    delete_stmt: CachedStatement<'conn>,
    delete_ancillary_stmt: CachedStatement<'conn>,
    dirty: bool,
}

impl<'conn> CoverageWriter<'conn> {
    fn new(coverage: &Coverage<'conn>) -> Result<Self> {
        coverage.gpkg().check_writable(coverage.table_name())?;
        let conn = coverage.gpkg().connection();
        let table = quote(coverage.table_name())?;
        let address = "zoom_level = ?1 AND tile_column = ?2 AND tile_row = ?3";
        let tx = WriteTransaction::begin(conn)?;
        Ok(Self {
            tx,
            conn,
            table_name: coverage.table_name().to_owned(),
            datatype: coverage.datatype(),
            #[cfg(feature = "coverage-samples")]
            ancillary: coverage.ancillary().clone(),
            #[cfg(feature = "coverage-samples")]
            max_decoded_samples: coverage.gpkg().max_decoded_samples(),
            matrices: coverage.matrices().to_vec(),
            put_stmt: conn.prepare_cached(&format!(
                "INSERT INTO {table} (zoom_level, tile_column, tile_row, tile_data) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (zoom_level, tile_column, tile_row) \
                 DO UPDATE SET tile_data = excluded.tile_data"
            ))?,
            // The id of the tile, read back with a SELECT and not returned by
            // the insert. `RETURNING` needs SQLite 3.35, and this crate links
            // the system SQLite by default (D1), so `RETURNING` would add a
            // runtime requirement that nothing else in the workspace has.
            // `last_insert_rowid` does not change when `ON CONFLICT DO UPDATE`
            // updates a row, so it is not an alternative.
            tile_id_stmt: conn
                .prepare_cached(&format!("SELECT id FROM {table} WHERE {address}"))?,
            put_ancillary_stmt: conn.prepare_cached(&format!(
                "INSERT INTO {TILE_ANCILLARY_TABLE} \
                 (tpudt_name, tpudt_id, scale, offset, min, max, mean, std_dev) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT (tpudt_name, tpudt_id) DO UPDATE SET \
                 scale = excluded.scale, offset = excluded.offset, min = excluded.min, \
                 max = excluded.max, mean = excluded.mean, std_dev = excluded.std_dev"
            ))?,
            delete_stmt: conn.prepare_cached(&format!("DELETE FROM {table} WHERE {address}"))?,
            delete_ancillary_stmt: conn.prepare_cached(&format!(
                "DELETE FROM {TILE_ANCILLARY_TABLE} WHERE tpudt_name = ?1 COLLATE NOCASE \
                 AND tpudt_id IN (SELECT id FROM {table} WHERE {address2})",
                address2 = "zoom_level = ?2 AND tile_column = ?3 AND tile_row = ?4"
            ))?,
            dirty: false,
        })
    }

    /// Writes one tile with a default ancillary row: scale 1.0, offset 0.0,
    /// and no statistics.
    ///
    /// # Errors
    ///
    /// As [`CoverageWriter::put_with_ancillary`].
    pub fn put(&mut self, coord: TileCoord, data: &[u8]) -> Result<()> {
        self.put_with_ancillary(coord, data, &TileAncillary::defaults())
    }

    /// Writes one tile with the ancillary row you supply.
    ///
    /// If you decoded the samples, supply the statistics here. The table
    /// definition makes the four statistics optional, and this function does
    /// not decode samples, so it cannot calculate them.
    /// [`CoverageWriter::put_with_statistics`], with the `coverage-samples`
    /// feature, does.
    ///
    /// # Checks
    ///
    /// The payload is checked against the encoding profile (Requirements 15 to
    /// 20, or 13 for PNG), the `datatype` of the coverage (Requirements 13 and
    /// 14), and the pixel size of its zoom level. The writer rejects Deflate,
    /// although a read accepts it, because Requirement 15 specifies baseline
    /// TIFF and Deflate is not baseline.
    ///
    /// **This function does not check [Requirement 21]** ("all pixels … set
    /// with a valid component value … NaN and Inf SHALL NOT be used"). That
    /// requirement is about samples, and this function does not decode
    /// samples. If you write a payload from another source, you are responsible
    /// for it, or, with the `coverage-samples` feature,
    /// [`CoverageWriter::put_with_statistics`] checks it.
    ///
    /// # Errors
    ///
    /// - [`Error::UnknownZoomLevel`] if the coverage declares no such level.
    /// - [`Error::Tile`] if the address is outside the grid, or the payload
    ///   breaks the profile, contradicts the `datatype`, or is the wrong size.
    /// - [`Error::UnwritableCoveragePayload`] for a payload that a read accepts
    ///   but a write does not.
    /// - [`Error::FloatCoverageScaled`] if the ancillary row has a scale or
    ///   offset other than the defaults on a `float` coverage (Requirement 11).
    /// - [`Error::NanAncillaryValue`] if a value in the ancillary row is NaN.
    ///
    /// [Requirement 21]: https://urschrei.github.io/geopackage/explanation/coverages.html#requirement-21
    pub fn put_with_ancillary(
        &mut self,
        coord: TileCoord,
        data: &[u8],
        ancillary: &TileAncillary,
    ) -> Result<()> {
        let matrix = self.matrix_at(coord)?;
        let payload = coverage_payload(data)?;
        if let Some(datatype) = self.datatype {
            datatype.check_payload(&payload)?;
        }
        if let CoveragePayload::Tiff(tiff) = payload
            && !tiff.compression.is_baseline()
        {
            return Err(Error::UnwritableCoveragePayload {
                table_name: self.table_name.clone(),
                reason: format!(
                    "{:?} compression is a TIFF extension, and Requirement 15 specifies baseline TIFF",
                    tiff.compression
                ),
            });
        }
        matrix.check_payload(&payload.tile_payload())?;
        check_float_defaults(
            self.datatype.unwrap_or(CoverageDatatype::Integer),
            ancillary.scale,
            ancillary.offset,
            &self.table_name,
        )?;
        check_not_nan(
            &self.table_name,
            &[
                (
                    "gpkg_2d_gridded_tile_ancillary.scale",
                    Some(ancillary.scale),
                ),
                (
                    "gpkg_2d_gridded_tile_ancillary.offset",
                    Some(ancillary.offset),
                ),
                ("gpkg_2d_gridded_tile_ancillary.min", ancillary.min),
                ("gpkg_2d_gridded_tile_ancillary.max", ancillary.max),
                ("gpkg_2d_gridded_tile_ancillary.mean", ancillary.mean),
                ("gpkg_2d_gridded_tile_ancillary.std_dev", ancillary.std_dev),
            ],
        )?;

        // The tile row and its ancillary row are written together or not at
        // all, so a failed put leaves no tile without its row.
        in_savepoint(self.conn, "coverage_put", || -> Result<()> {
            self.put_stmt.execute(rusqlite::params![
                coord.zoom_level,
                coord.column,
                coord.row,
                data
            ])?;
            // Requirement 10: the ancillary row of the tile, with the id of the
            // tile as its key.
            let tile_id: i64 = self.tile_id_stmt.query_row(
                rusqlite::params![coord.zoom_level, coord.column, coord.row],
                |row| row.get(0),
            )?;
            self.put_ancillary_stmt.execute(rusqlite::params![
                self.table_name,
                tile_id,
                ancillary.scale,
                ancillary.offset,
                ancillary.min,
                ancillary.max,
                ancillary.mean,
                ancillary.std_dev,
            ])?;
            Ok(())
        })?;
        self.dirty = true;
        Ok(())
    }

    /// Deletes one tile and its ancillary row. Returns `true` if there was a
    /// tile to delete.
    ///
    /// Deletes the ancillary row first, because the delete finds that row
    /// through the tile.
    ///
    /// # Errors
    ///
    /// [`Error`] if either delete fails.
    pub fn delete(&mut self, coord: TileCoord) -> Result<bool> {
        self.delete_ancillary_stmt.execute(rusqlite::params![
            self.table_name,
            coord.zoom_level,
            coord.column,
            coord.row
        ])?;
        let deleted = self.delete_stmt.execute(rusqlite::params![
            coord.zoom_level,
            coord.column,
            coord.row
        ])?;
        self.dirty |= deleted > 0;
        Ok(deleted > 0)
    }

    /// Updates `gpkg_contents.last_change` and commits.
    ///
    /// Behaves as [`crate::TileWriter::commit`] does, also when the caller owns
    /// the transaction.
    ///
    /// # Errors
    ///
    /// [`Error`] if the commit fails.
    pub fn commit(self) -> Result<()> {
        let Self {
            tx,
            conn,
            table_name,
            dirty,
            put_stmt,
            tile_id_stmt,
            put_ancillary_stmt,
            delete_stmt,
            delete_ancillary_stmt,
            ..
        } = self;
        drop(put_stmt);
        drop(tile_id_stmt);
        drop(put_ancillary_stmt);
        drop(delete_stmt);
        drop(delete_ancillary_stmt);
        if dirty {
            conn.execute(
                "UPDATE gpkg_contents \
                 SET last_change = strftime('%Y-%m-%dT%H:%M:%fZ','now') \
                 WHERE table_name = ?1",
                [&table_name],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Returns the zoom level of `coord`, after checking that the address is
    /// inside its grid.
    pub(crate) fn matrix_at(&self, coord: TileCoord) -> Result<&TileMatrix> {
        let matrix = self
            .matrices
            .binary_search_by_key(&coord.zoom_level, |matrix| matrix.zoom_level)
            .ok()
            .and_then(|index| self.matrices.get(index))
            .ok_or_else(|| Error::UnknownZoomLevel {
                table_name: self.table_name.clone(),
                zoom_level: coord.zoom_level,
            })?;
        matrix.check_contains(coord.column, coord.row)?;
        Ok(matrix)
    }
}

impl std::fmt::Debug for CoverageWriter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoverageWriter")
            .field("table_name", &self.table_name)
            .field("datatype", &self.datatype)
            .finish_non_exhaustive()
    }
}
