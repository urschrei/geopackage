//! Tiled gridded coverages: the [`Coverage`] handle, the [`CoverageBuilder`]
//! that creates one, and the two ancillary tables that describe a coverage.
//!
//! A coverage is a tile pyramid whose payloads are measurements, such as
//! elevations, depths or temperatures, rather than images. The spec gives it a
//! separate `gpkg_contents.data_type` (`2d-gridded-coverage`, Requirement 5), a
//! row that states what the samples mean ([`CoverageAncillary`],
//! Requirement 1), and a row for each tile with the scale, offset and
//! statistics of that tile ([`TileAncillary`], Requirements 2 and 10). This
//! module contains the parts that use the database. The payload profile is in
//! [`geopackage_core::coverage`].
//!
//! # Samples
//!
//! By default this module reads and writes metadata and bytes only. A
//! coverage's samples are stored as TIFF or PNG, and the default build decodes
//! neither format. [`Coverage::get_tile`] returns the stored payload, and
//! [`Coverage::check_payload`] checks that the coverage permits the payload.
//! [`Coverage::value`] converts a sample you have decoded into a value.
//!
//! The `coverage-samples` feature adds the decode: [`Coverage::samples`] and
//! [`Coverage::decode`] return a tile's [`TileSamples`],
//! [`Coverage::value_at`] and [`CoverageSampler`] look up values at a
//! position, [`Coverage::statistics`] and
//! [`CoverageWriter::put_with_statistics`] calculate the per-tile statistics,
//! and [`Coverage::check_samples`] checks [Requirement 21].
//!
//! # Why a coverage is not a [`TilePyramid`]
//!
//! Each tile of a coverage needs an ancillary row, and a reader needs a value
//! with a scale, an offset and a null, not bytes for an image decoder.
//! [`GeoPackage::tiles`] therefore rejects a coverage and a TIFF payload, with
//! no special case.
//!
//! ```no_run
//! use geopackage::GeoPackage;
//! use geopackage::core::tiles::TileCoord;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let gpkg = GeoPackage::open_read_only("dem.gpkg")?;
//! let coverage = gpkg.coverage("elevation")?;
//!
//! println!("{:?} samples, null = {:?}",
//!     coverage.datatype(), coverage.ancillary().data_null);
//!
//! if let Some(payload) = coverage.get_tile(TileCoord::new(0, 0, 0))? {
//!     // Check the payload against the coverage (Requirements 13 to 20).
//!     let checked = coverage.check_payload(&payload)?;
//!     println!("{} {:?}", checked.mime_type(), checked.sample_type());
//! }
//! # Ok(()) }
//! ```
//!
//! [Requirement 21]: https://urschrei.github.io/geopackage/explanation/coverages.html#requirement-21

use geopackage_core::coverage::{
    COVERAGE_ANCILLARY_TABLE, COVERAGE_DATA_TYPE, CoverageDatatype, CoveragePayload,
    TILE_ANCILLARY_TABLE, coverage_payload,
};
use geopackage_core::ident::quote;
use geopackage_core::tiles::{TileCoord, TileMatrix, TileMatrixSet};
use rusqlite::OptionalExtension;

use crate::tiles::{TileCursor, TilePyramid};
use crate::{BoundingBox, Error, GeoPackage, Result, table_exists};

mod ancillary;
mod create;
#[cfg(feature = "coverage-samples")]
mod samples;
mod write;

#[cfg(feature = "coverage-samples")]
pub use geopackage_core::coverage::{Interpolation, SampleStatistics, TileSamples};
#[cfg(feature = "coverage-samples")]
pub use samples::CoverageSampler;

pub use create::CoverageBuilder;
pub use geopackage_core::coverage::{CoverageAncillary, TileAncillary};
pub use write::CoverageWriter;

/// An open tiled gridded coverage.
///
/// The handle gives the grid (the same [`TileMatrixSet`] and [`TileMatrix`]
/// rows as a tile pyramid), the ancillary metadata, the stored payloads, and a
/// check of a payload against the coverage. [`Coverage::writer`] writes tiles.
pub struct Coverage<'a> {
    /// Private, because only the tile code is shared, not the handle. Nothing
    /// here exposes the write path of [`TilePyramid`].
    pyramid: TilePyramid<'a>,
    ancillary: CoverageAncillary,
}

impl<'a> Coverage<'a> {
    /// Returns the physical SQLite table name backing this coverage.
    pub fn table_name(&self) -> &str {
        self.pyramid.table_name()
    }

    /// Returns the [`GeoPackage`] this coverage belongs to.
    pub fn gpkg(&self) -> &'a GeoPackage {
        self.pyramid.gpkg()
    }

    /// Returns the coverage's extent and spatial reference system.
    pub fn matrix_set(&self) -> &TileMatrixSet {
        self.pyramid.matrix_set()
    }

    /// Returns the zoom levels, ascending.
    pub fn matrices(&self) -> &[TileMatrix] {
        self.pyramid.matrices()
    }

    /// Returns the zoom levels this coverage declares.
    pub fn zoom_levels(&self) -> Vec<i64> {
        self.pyramid.zoom_levels()
    }

    /// Returns one zoom level's tile matrix.
    pub fn matrix(&self, zoom_level: i64) -> Option<&TileMatrix> {
        self.pyramid.matrix(zoom_level)
    }

    /// Returns the `gpkg_2d_gridded_coverage_ancillary` row.
    pub fn ancillary(&self) -> &CoverageAncillary {
        &self.ancillary
    }

    /// Returns the parsed `datatype` of the coverage.
    ///
    /// Returns `None` for a value that Requirement 9 does not permit. The
    /// `CHECK` constraint of the table definition prevents such a value, but a
    /// file written without the constraint can contain one.
    pub fn datatype(&self) -> Option<CoverageDatatype> {
        self.ancillary.datatype()
    }

    /// Checks a payload against this coverage: the encoding profile
    /// (Requirements 15 to 20, or 13 for PNG) and the `datatype` of the
    /// coverage (Requirements 13 and 14).
    ///
    /// Skips the `datatype` check if [`Coverage::datatype`] is `None`, because a
    /// value that the spec does not define constrains nothing.
    ///
    /// Does not check [Requirement 21] (every pixel valid, no NaN, no Inf).
    /// That requirement is about samples, which this check does not decode.
    /// With the `coverage-samples` feature, [`Coverage::check_samples`] checks
    /// it.
    ///
    /// # Errors
    ///
    /// [`Error::Tile`] with a
    /// [`TileError::CoverageProfileViolation`](geopackage_core::TileError::CoverageProfileViolation)
    /// for the requirement, or an unreadable-payload error for bytes that are
    /// neither a TIFF nor a PNG.
    ///
    /// [Requirement 21]: https://urschrei.github.io/geopackage/explanation/coverages.html#requirement-21
    pub fn check_payload(&self, bytes: &[u8]) -> Result<CoveragePayload> {
        let payload = coverage_payload(bytes)?;
        if let Some(datatype) = self.datatype() {
            datatype.check_payload(&payload)?;
        }
        Ok(payload)
    }

    /// Converts one stored sample to a value, and applies both scale and offset
    /// pairs in the order that the extension defines.
    ///
    /// The spec gives the calculation as pseudo-code: multiply the stored value
    /// by the `scale` of the tile and add the `offset` of the tile, then do the
    /// same with the pair of the coverage. Without the `coverage-samples`
    /// feature you decode the sample, because the default build does not read
    /// pixels. The arithmetic is part of the extension, so this crate supplies
    /// it either way.
    ///
    /// Do not use this function for `data_null`, which the requirement
    /// excludes. Use [`Coverage::is_null`].
    pub fn value(&self, tile: &TileAncillary, stored: f64) -> f64 {
        self.ancillary.value(tile, stored)
    }

    /// Returns `true` if a stored sample is the "no data" value of the
    /// coverage.
    ///
    /// The comparison is exact, and uses the stored sample, not the value from
    /// [`Coverage::value`]: "The scale and offset do not apply to the
    /// `data_null` value". A tolerance would also match valid samples near the
    /// sentinel.
    pub fn is_null(&self, stored: f64) -> bool {
        self.ancillary.is_null(stored)
    }

    /// Returns one tile's payload, or `None` if the coverage has no tile
    /// there.
    ///
    /// # Errors
    ///
    /// [`Error`] if the row cannot be read.
    pub fn get_tile(&self, coord: TileCoord) -> Result<Option<Vec<u8>>> {
        self.pyramid.get_tile(coord)
    }

    /// Reads the payload of one tile into an existing buffer. Returns `true` if
    /// there was a tile to read.
    ///
    /// # Errors
    ///
    /// [`Error`] if the row cannot be read.
    pub fn get_tile_into(&self, coord: TileCoord, buffer: &mut Vec<u8>) -> Result<bool> {
        self.pyramid.get_tile_into(coord, buffer)
    }

    /// Returns whether a tile exists at this address, without reading it.
    ///
    /// # Errors
    ///
    /// [`Error`] if the row cannot be read.
    pub fn has_tile(&self, coord: TileCoord) -> Result<bool> {
        self.pyramid.has_tile(coord)
    }

    /// Returns the number of tiles across every zoom level.
    ///
    /// # Errors
    ///
    /// [`Error`] if the count cannot be read.
    pub fn tile_count(&self) -> Result<i64> {
        self.pyramid.tile_count()
    }

    /// Returns the number of tiles at one zoom level.
    ///
    /// # Errors
    ///
    /// [`Error`] if the count cannot be read.
    pub fn tile_count_at(&self, zoom_level: i64) -> Result<i64> {
        self.pyramid.tile_count_at(zoom_level)
    }

    /// Opens a cursor over every tile, in matrix order.
    ///
    /// # Errors
    ///
    /// [`Error`] if the statement cannot be prepared.
    pub fn cursor(&self) -> Result<TileCursor<'_>> {
        self.pyramid.cursor()
    }

    /// Opens a cursor over one zoom level.
    ///
    /// # Errors
    ///
    /// [`Error`] if the statement cannot be prepared.
    pub fn cursor_at(&self, zoom_level: i64) -> Result<TileCursor<'_>> {
        self.pyramid.cursor_at(zoom_level)
    }

    /// Opens a cursor over the tiles of one zoom level that a bounding box
    /// touches.
    ///
    /// # Errors
    ///
    /// [`Error`] if the zoom level is not declared, or the statement cannot be
    /// prepared.
    pub fn cursor_in(&self, zoom_level: i64, bbox: BoundingBox) -> Result<TileCursor<'_>> {
        self.pyramid.cursor_in(zoom_level, bbox)
    }

    /// Returns the `gpkg_2d_gridded_tile_ancillary` row of a tile, or `None` if
    /// the tile has no row.
    ///
    /// Requirement 10 specifies a row for each tile, so `None` for a tile that
    /// exists is a defect in the file. The function reports it and does not
    /// reject the file, as with all reads in this crate.
    ///
    /// # Errors
    ///
    /// [`Error`] if the row cannot be read.
    pub fn tile_ancillary(&self, coord: TileCoord) -> Result<Option<TileAncillary>> {
        if !table_exists(self.gpkg().connection(), TILE_ANCILLARY_TABLE)? {
            return Ok(None);
        }
        let sql = format!(
            "SELECT a.scale, a.offset, a.min, a.max, a.mean, a.std_dev \
             FROM {TILE_ANCILLARY_TABLE} a JOIN {table} t ON t.id = a.tpudt_id \
             WHERE a.tpudt_name = ?1 COLLATE NOCASE \
             AND t.zoom_level = ?2 AND t.tile_column = ?3 AND t.tile_row = ?4",
            table = quote(self.table_name())?
        );
        Ok(self
            .gpkg()
            .connection()
            .query_row(
                &sql,
                rusqlite::params![self.table_name(), coord.zoom_level, coord.column, coord.row],
                |row| {
                    let mut ancillary = TileAncillary::new(row.get(0)?, row.get(1)?);
                    ancillary.min = row.get(2)?;
                    ancillary.max = row.get(3)?;
                    ancillary.mean = row.get(4)?;
                    ancillary.std_dev = row.get(5)?;
                    Ok(ancillary)
                },
            )
            .optional()?)
    }

    /// Checks the tile matrix rows of the coverage against the consistency
    /// rules of the spec, as [`TilePyramid::validate`] does for a pyramid.
    ///
    /// The grid rules are those of the core spec (Requirements 45 to 53). This
    /// function does not check the rules of the extension.
    ///
    /// # Errors
    ///
    /// [`Error::Tile`] for the first rule that the coverage breaks.
    pub fn validate(&self) -> Result<()> {
        self.pyramid.validate()
    }
}

impl std::fmt::Debug for Coverage<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Coverage")
            .field("table_name", &self.table_name())
            .field("datatype", &self.ancillary.datatype)
            .field("zoom_levels", &self.zoom_levels())
            .finish_non_exhaustive()
    }
}

impl GeoPackage {
    /// Opens an existing tiled gridded coverage by `gpkg_contents` name.
    ///
    /// # Errors
    ///
    /// - [`Error::NoSuchLayer`] if `name` is not in `gpkg_contents`.
    /// - [`Error::WrongDataType`] if its `data_type` is not
    ///   `2d-gridded-coverage`. Use [`GeoPackage::tiles`] for a tile pyramid.
    /// - [`Error::NoTileMatrixSet`] if its `gpkg_tile_matrix_set` row is
    ///   missing. Without the row, the tiles have no location.
    /// - [`Error::NoCoverageAncillary`] if its
    ///   `gpkg_2d_gridded_coverage_ancillary` row is missing. Without the row,
    ///   the samples have no meaning.
    pub fn coverage(&self, name: &str) -> Result<Coverage<'_>> {
        let pyramid = self.open_pyramid(name, COVERAGE_DATA_TYPE)?;
        let ancillary = read_coverage_ancillary(self, pyramid.table_name())?.ok_or_else(|| {
            Error::NoCoverageAncillary {
                table_name: pyramid.table_name().to_owned(),
            }
        })?;
        Ok(Coverage { pyramid, ancillary })
    }

    /// Returns all tiled gridded coverages in the file, in order of
    /// `gpkg_contents` name.
    ///
    /// Returns coverages only. [`GeoPackage::tile_pyramids`] returns tile
    /// pyramids, and neither function returns the tables of the other.
    ///
    /// # Errors
    ///
    /// [`Error`] if the catalogue cannot be read, or if a coverage in the
    /// catalogue cannot be opened.
    pub fn coverages(&self) -> Result<Vec<Coverage<'_>>> {
        if !table_exists(self.connection(), COVERAGE_ANCILLARY_TABLE)? {
            return Ok(Vec::new());
        }
        let names: Vec<String> = {
            let mut stmt = self.connection().prepare(
                "SELECT table_name FROM gpkg_contents WHERE data_type = ?1 ORDER BY table_name",
            )?;
            stmt.query_map([COVERAGE_DATA_TYPE], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        names.iter().map(|name| self.coverage(name)).collect()
    }
}

/// Reads the `gpkg_2d_gridded_coverage_ancillary` row of a coverage.
fn read_coverage_ancillary(
    gpkg: &GeoPackage,
    table_name: &str,
) -> Result<Option<CoverageAncillary>> {
    if !table_exists(gpkg.connection(), COVERAGE_ANCILLARY_TABLE)? {
        return Ok(None);
    }
    Ok(gpkg
        .connection()
        .query_row(
            &format!(
                "SELECT tile_matrix_set_name, datatype, scale, offset, precision, data_null, \
                 grid_cell_encoding, uom, field_name, quantity_definition \
                 FROM {COVERAGE_ANCILLARY_TABLE} WHERE tile_matrix_set_name = ?1 COLLATE NOCASE"
            ),
            [table_name],
            |row| {
                let mut ancillary =
                    CoverageAncillary::new(row.get::<_, String>(0)?, row.get::<_, String>(1)?);
                ancillary.scale = row.get(2)?;
                ancillary.offset = row.get(3)?;
                ancillary.precision = row.get(4)?;
                ancillary.data_null = row.get(5)?;
                ancillary.grid_cell_encoding = row.get(6)?;
                ancillary.uom = row.get(7)?;
                ancillary.field_name = row.get(8)?;
                ancillary.quantity_definition = row.get(9)?;
                Ok(ancillary)
            },
        )
        .optional()?)
}
