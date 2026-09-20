//! Tiled gridded coverages: the [`Coverage`] handle over an existing one, and
//! the two ancillary tables that describe it.
//!
//! A coverage is a tile pyramid whose payloads are measurements rather than
//! pictures: elevations, depths, temperatures. The spec gives it its own
//! `gpkg_contents.data_type` (`2d-gridded-coverage`, Requirement 5), its own
//! per-coverage row saying what the samples mean
//! ([`CoverageAncillary`], Requirement 1), and a row per tile carrying that
//! tile's scale, offset and statistics ([`TileAncillary`], Requirements 2 and
//! 10). This module is the part of that which needs a database; the payload
//! profile and the arithmetic live in [`geopackage_core::coverage`].
//!
//! # What this reads, and what it cannot
//!
//! Everything here is metadata and bytes. A coverage's samples are inside a
//! TIFF or a PNG, and this workspace decodes neither, so
//! [`Coverage::get_tile`] hands back the stored payload and
//! [`Coverage::check_payload`] says whether that payload is one the coverage
//! may hold. Turning a sample into a value is arithmetic this module will do
//! for you ([`Coverage::value`]) on a number you decoded elsewhere.
//!
//! # Why it is not a [`TilePyramid`](crate::TilePyramid)
//!
//! Because its tiles are not interchangeable with a basemap's: every one of
//! them needs an ancillary row, and what a reader wants back is a value with a
//! scale, an offset and a null rather than bytes to hand to an image decoder.
//! [`GeoPackage::tiles`] therefore still refuses a coverage, and still refuses
//! a TIFF payload, and neither has to learn a special case.
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
//!     // What the payload declares, checked against what the coverage says
//!     // it should be (Requirements 13 to 20).
//!     let checked = coverage.check_payload(&payload)?;
//!     println!("{} {:?}", checked.mime_type(), checked.sample_type());
//! }
//! # Ok(()) }
//! ```

use geopackage_core::coverage::{
    COVERAGE_ANCILLARY_TABLE, COVERAGE_DATA_TYPE, CoverageDatatype, CoveragePayload,
    TILE_ANCILLARY_TABLE, coverage_payload,
};
use geopackage_core::ident::quote;
use geopackage_core::tiles::{TileCoord, TileMatrix, TileMatrixSet};
use rusqlite::OptionalExtension;

use crate::tiles::{TileCursor, TilePyramid};
use crate::{BoundingBox, Error, GeoPackage, Result, table_exists};

/// A `gpkg_2d_gridded_coverage_ancillary` row: what a coverage's samples mean.
///
/// One row per coverage (Requirements 1 and 7). The columns are the spec's,
/// including the ones it leaves optional; Requirement 1 also allows an
/// implementation to add columns of its own and asks clients to ignore the
/// ones they do not recognise, which is what reading a fixed set of columns
/// does.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct CoverageAncillary {
    /// The `gpkg_tile_matrix_set` and `gpkg_contents` row this describes
    /// (Requirements 7 and 8).
    pub tile_matrix_set_name: String,
    /// `integer` or `float` (Requirement 9), as stored.
    ///
    /// Kept as text rather than parsed, because reading never refuses a file:
    /// [`Coverage::datatype`] is the parsed form, and it is `None` for a value
    /// the requirement does not allow.
    pub datatype: String,
    /// Coverage-wide scale applied to every sample (Requirement 11 pins it to
    /// 1.0 for a `float` coverage).
    pub scale: f64,
    /// Coverage-wide offset applied to every sample, after the scale.
    pub offset: f64,
    /// The smallest difference between values the coverage distinguishes.
    pub precision: Option<f64>,
    /// The sample value that means "no data", to which scale and offset do
    /// **not** apply.
    pub data_null: Option<f64>,
    /// Where in a cell the value sits: `grid-value-is-center`,
    /// `grid-value-is-area`, or a corner.
    pub grid_cell_encoding: Option<String>,
    /// Unit of measure of the values.
    pub uom: Option<String>,
    /// What the values are, by name: `Height` by default.
    pub field_name: Option<String>,
    /// The quantity the field measures.
    pub quantity_definition: Option<String>,
}

/// A `gpkg_2d_gridded_tile_ancillary` row: one tile's scale, offset and
/// statistics.
///
/// Requirement 10 asks for one of these per tile. The four statistics are
/// optional in the table definition, and a writer that cannot decode samples
/// cannot compute them, so `None` here means "not recorded" rather than "no
/// such value".
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct TileAncillary {
    /// Per-tile scale, applied before the coverage's own (Requirement 11 pins
    /// it to 1.0 for a `float` coverage).
    pub scale: f64,
    /// Per-tile offset, applied after the per-tile scale.
    pub offset: f64,
    /// Smallest sample in the tile, if recorded.
    pub min: Option<f64>,
    /// Largest sample in the tile, if recorded.
    pub max: Option<f64>,
    /// Mean of the tile's samples, if recorded.
    pub mean: Option<f64>,
    /// Standard deviation of the tile's samples, if recorded.
    pub std_dev: Option<f64>,
}

impl GeoPackage {
    /// Opens an existing tiled gridded coverage by `gpkg_contents` name.
    ///
    /// # Errors
    ///
    /// - [`Error::NoSuchLayer`] if `name` is not in `gpkg_contents`.
    /// - [`Error::WrongDataType`] if it is registered as something other than
    ///   `2d-gridded-coverage`. A tile pyramid is [`GeoPackage::tiles`].
    /// - [`Error::NoTileMatrixSet`] if its `gpkg_tile_matrix_set` row is
    ///   missing, which leaves its tiles unlocatable.
    /// - [`Error::NoCoverageAncillary`] if its
    ///   `gpkg_2d_gridded_coverage_ancillary` row is missing, which leaves its
    ///   samples uninterpretable.
    pub fn coverage(&self, name: &str) -> Result<Coverage<'_>> {
        let pyramid = self.open_pyramid(name, COVERAGE_DATA_TYPE)?;
        let ancillary = read_coverage_ancillary(self, pyramid.table_name())?.ok_or_else(|| {
            Error::NoCoverageAncillary {
                table_name: pyramid.table_name().to_owned(),
            }
        })?;
        Ok(Coverage { pyramid, ancillary })
    }

    /// Returns every tiled gridded coverage in the file, by `gpkg_contents`
    /// name.
    ///
    /// Coverages only: a basemap is [`GeoPackage::tile_pyramids`], and neither
    /// call returns the other's tables.
    ///
    /// # Errors
    ///
    /// [`Error`] if the catalogue cannot be read, or if a coverage it lists
    /// cannot be opened.
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

/// An open tiled gridded coverage.
///
/// Read-only: creating and writing one is not implemented yet, so a payload
/// reaches this crate only if another implementation put it there. What can be
/// asked of it is its grid (the same [`TileMatrixSet`] and [`TileMatrix`] rows
/// a pyramid has), its ancillary metadata, its stored payloads, and whether
/// those payloads are ones it may hold.
pub struct Coverage<'a> {
    /// Private, and deliberately: the tile machinery is shared, the handle is
    /// not. Nothing here exposes [`TilePyramid`]'s write path.
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

    /// Returns the coverage's `datatype`, parsed.
    ///
    /// `None` for a value Requirement 9 does not allow, which the table
    /// definition's `CHECK` constraint should have prevented and a file
    /// written without it may still contain.
    pub fn datatype(&self) -> Option<CoverageDatatype> {
        CoverageDatatype::parse(&self.ancillary.datatype)
    }

    /// Checks a payload against this coverage: the encoding profile
    /// (Requirements 15 to 20, or 13 for PNG) and the `datatype` the coverage
    /// declares (Requirements 13 and 14).
    ///
    /// The `datatype` half is skipped when [`Coverage::datatype`] is `None`,
    /// since a value the spec does not define constrains nothing.
    ///
    /// Requirement 21 — every pixel valid, no NaN, no Inf — is a statement
    /// about samples and is not checked here or anywhere in this workspace.
    ///
    /// # Errors
    ///
    /// [`Error::Tile`] carrying a
    /// [`TileError::CoverageProfileViolation`](geopackage_core::TileError::CoverageProfileViolation)
    /// naming the requirement, or an unreadable-payload error for bytes that
    /// are neither a TIFF nor a PNG.
    pub fn check_payload(&self, bytes: &[u8]) -> Result<CoveragePayload> {
        let payload = coverage_payload(bytes)?;
        if let Some(datatype) = self.datatype() {
            datatype.check_payload(&payload)?;
        }
        Ok(payload)
    }

    /// Converts one stored sample to a value, applying both scale and offset
    /// pairs in the order the extension defines.
    ///
    /// The spec gives it as pseudo-code: the stored value is multiplied by the
    /// tile's `scale` and offset by the tile's `offset`, and that result by the
    /// coverage's. Decoding the sample is the caller's business — this
    /// workspace reads no pixels — but the arithmetic afterwards is the
    /// extension's, so it lives here rather than in every caller.
    ///
    /// `data_null` is not passed through this: the requirement excludes it
    /// explicitly, which is what [`Coverage::is_null`] is for.
    pub fn value(&self, tile: &TileAncillary, stored: f64) -> f64 {
        (stored * tile.scale + tile.offset) * self.ancillary.scale + self.ancillary.offset
    }

    /// Whether a stored sample is the coverage's "no data" value.
    ///
    /// Compared exactly, and against the stored sample rather than the value
    /// [`Coverage::value`] would produce: "The scale and offset do not apply to
    /// the `data_null` value". A tolerance here would swallow real samples
    /// near the sentinel, which is why there is none.
    pub fn is_null(&self, stored: f64) -> bool {
        self.ancillary.data_null == Some(stored)
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

    /// Reads one tile's payload into an existing buffer, returning whether
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

    /// Returns a tile's `gpkg_2d_gridded_tile_ancillary` row, or `None` when
    /// the tile has none.
    ///
    /// Requirement 10 says every tile has one, so `None` for a tile that
    /// exists is a defect in the file rather than an ordinary answer; it is
    /// reported rather than refused, as reading is throughout this crate.
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
                    Ok(TileAncillary {
                        scale: row.get(0)?,
                        offset: row.get(1)?,
                        min: row.get(2)?,
                        max: row.get(3)?,
                        mean: row.get(4)?,
                        std_dev: row.get(5)?,
                    })
                },
            )
            .optional()?)
    }

    /// Checks the coverage's tile matrix rows against the spec's consistency
    /// rules, as [`TilePyramid::validate`] does for a pyramid.
    ///
    /// The grid rules are the core spec's (Requirements 45 to 53) and apply
    /// unchanged here. The extension's own rules are not checked by this call.
    ///
    /// # Errors
    ///
    /// [`Error::Tile`] naming the first rule the pyramid breaks.
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

/// Reads a coverage's `gpkg_2d_gridded_coverage_ancillary` row.
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
                Ok(CoverageAncillary {
                    tile_matrix_set_name: row.get(0)?,
                    datatype: row.get(1)?,
                    scale: row.get(2)?,
                    offset: row.get(3)?,
                    precision: row.get(4)?,
                    data_null: row.get(5)?,
                    grid_cell_encoding: row.get(6)?,
                    uom: row.get(7)?,
                    field_name: row.get(8)?,
                    quantity_definition: row.get(9)?,
                })
            },
        )
        .optional()?)
}
