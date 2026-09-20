//! Tiled gridded coverages: the [`Coverage`] handle, the [`CoverageBuilder`]
//! that creates one, and the two ancillary tables that describe a coverage.
//!
//! A coverage is a tile pyramid with payloads that are measurements, not
//! images: elevations, depths, temperatures. The spec gives it a separate
//! `gpkg_contents.data_type` (`2d-gridded-coverage`, Requirement 5), a row that
//! states what the samples mean ([`CoverageAncillary`], Requirement 1), and a
//! row for each tile with the scale, offset and statistics of that tile
//! ([`TileAncillary`], Requirements 2 and 10). This module contains the parts
//! that use the database. The payload profile is in
//! [`geopackage_core::coverage`].
//!
//! # Samples
//!
//! This module reads and writes metadata and bytes only. The samples of a
//! coverage are in a TIFF or a PNG, and this workspace does not decode either
//! format. [`Coverage::get_tile`] returns the stored payload, and
//! [`Coverage::check_payload`] checks that the coverage permits the payload.
//! [`Coverage::value`] converts a sample that the caller decoded into a value.
//!
//! # Why a coverage is not a [`TilePyramid`](crate::TilePyramid)
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

use geopackage_core::coverage::{
    COVERAGE_ANCILLARY_TABLE, COVERAGE_DATA_TYPE, COVERAGE_EXTENSION_DEFINITION,
    COVERAGE_EXTENSION_NAME, COVERAGE_EXTENSION_SCOPE, CoverageDatatype, CoveragePayload,
    TILE_ANCILLARY_TABLE, coverage_payload,
};
use geopackage_core::ddl;
use geopackage_core::ident::quote;
use geopackage_core::tiles::{self, TileCoord, TileFormat, TileMatrix, TileMatrixSet, TilePayload};
use rusqlite::{CachedStatement, Connection, OptionalExtension};

use crate::tiles::{PyramidSpec, TileCursor, TilePyramid};
use crate::transaction::WriteTransaction;
use crate::{BoundingBox, Error, GeoPackage, Result, table_exists};

/// A `gpkg_2d_gridded_coverage_ancillary` row, which states what the samples of
/// a coverage mean.
///
/// There is one row for each coverage (Requirements 1 and 7). The fields are
/// the columns of the spec, including the optional columns. Requirement 1 lets
/// an implementation add columns, and tells clients to ignore columns that they
/// do not recognise. This struct reads a fixed set of columns, so it ignores
/// other columns.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct CoverageAncillary {
    /// The `gpkg_tile_matrix_set` and `gpkg_contents` row that this row
    /// describes (Requirements 7 and 8).
    pub tile_matrix_set_name: String,
    /// `integer` or `float` (Requirement 9), as stored.
    ///
    /// The field is text, because a read does not reject a file.
    /// [`Coverage::datatype`] returns the parsed form, or `None` for a value
    /// that the requirement does not permit.
    pub datatype: String,
    /// The scale for all samples in the coverage. Requirement 11 sets it to
    /// 1.0 for a `float` coverage.
    pub scale: f64,
    /// The offset for all samples in the coverage, applied after the scale.
    pub offset: f64,
    /// The smallest difference between two values that the coverage
    /// distinguishes.
    pub precision: Option<f64>,
    /// The sample value that means "no data", to which scale and offset do
    /// **not** apply.
    pub data_null: Option<f64>,
    /// The position of the value in a cell: `grid-value-is-center`,
    /// `grid-value-is-area`, or a corner.
    pub grid_cell_encoding: Option<String>,
    /// The unit of measure of the values.
    pub uom: Option<String>,
    /// The name of the quantity: `Height` by default.
    pub field_name: Option<String>,
    /// The definition of the quantity that the field measures.
    pub quantity_definition: Option<String>,
}

/// A `gpkg_2d_gridded_tile_ancillary` row: the scale, offset and statistics of
/// one tile.
///
/// Requirement 10 specifies one row for each tile. The table definition makes
/// the four statistics optional, and a writer that does not decode samples
/// cannot calculate them. `None` therefore means "not recorded", not "no
/// value".
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct TileAncillary {
    /// The scale for the tile, applied before the scale of the coverage.
    /// Requirement 11 sets it to 1.0 for a `float` coverage.
    pub scale: f64,
    /// The offset for the tile, applied after the scale of the tile.
    pub offset: f64,
    /// The smallest sample in the tile, if recorded.
    pub min: Option<f64>,
    /// The largest sample in the tile, if recorded.
    pub max: Option<f64>,
    /// The mean of the samples in the tile, if recorded.
    pub mean: Option<f64>,
    /// The standard deviation of the samples in the tile, if recorded.
    pub std_dev: Option<f64>,
}

impl TileAncillary {
    /// Returns the default row: scale 1.0, offset 0.0, and no statistics.
    ///
    /// Requirement 11 specifies these values for a `float` coverage. For an
    /// `integer` coverage, they mean that the samples are the values.
    pub fn defaults() -> Self {
        Self {
            scale: 1.0,
            offset: 0.0,
            min: None,
            max: None,
            mean: None,
            std_dev: None,
        }
    }

    /// Returns a row with its own scale and offset. Only an `integer` coverage
    /// permits a pair other than the defaults (Requirement 11).
    pub fn new(scale: f64, offset: f64) -> Self {
        Self {
            scale,
            offset,
            ..Self::defaults()
        }
    }

    /// Sets the statistics of the samples in the tile.
    ///
    /// This workspace does not decode samples, so the caller must supply the
    /// statistics.
    #[must_use]
    pub fn with_statistics(mut self, min: f64, max: f64, mean: f64, std_dev: f64) -> Self {
        self.min = Some(min);
        self.max = Some(max);
        self.mean = Some(mean);
        self.std_dev = Some(std_dev);
        self
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

/// An open tiled gridded coverage.
///
/// The handle gives the grid (the same [`TileMatrixSet`] and [`TileMatrix`]
/// rows as a tile pyramid), the ancillary metadata, the stored payloads, and a
/// check of a payload against the coverage. [`Coverage::writer`] writes tiles.
pub struct Coverage<'a> {
    /// Private: the tile code is shared, but the handle is separate. Nothing
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
        CoverageDatatype::parse(&self.ancillary.datatype)
    }

    /// Checks a payload against this coverage: the encoding profile
    /// (Requirements 15 to 20, or 13 for PNG) and the `datatype` of the
    /// coverage (Requirements 13 and 14).
    ///
    /// Skips the `datatype` check if [`Coverage::datatype`] is `None`, because a
    /// value that the spec does not define constrains nothing.
    ///
    /// Does not check Requirement 21 (every pixel valid, no NaN, no Inf). That
    /// requirement is about samples, and this workspace does not check it.
    ///
    /// # Errors
    ///
    /// [`Error::Tile`] with a
    /// [`TileError::CoverageProfileViolation`](geopackage_core::TileError::CoverageProfileViolation)
    /// for the requirement, or an unreadable-payload error for bytes that are
    /// neither a TIFF nor a PNG.
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
    /// same with the pair of the coverage. The caller decodes the sample,
    /// because this workspace does not read pixels. The arithmetic is part of
    /// the extension, so this crate supplies it.
    ///
    /// Do not use this function for `data_null`, which the requirement
    /// excludes. Use [`Coverage::is_null`].
    pub fn value(&self, tile: &TileAncillary, stored: f64) -> f64 {
        (stored * tile.scale + tile.offset) * self.ancillary.scale + self.ancillary.offset
    }

    /// Returns `true` if a stored sample is the "no data" value of the
    /// coverage.
    ///
    /// The comparison is exact, and uses the stored sample, not the value from
    /// [`Coverage::value`]: "The scale and offset do not apply to the
    /// `data_null` value". A tolerance would also match valid samples near the
    /// sentinel.
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

/// A declarative builder for a tiled gridded coverage.
///
/// Declares the grid, as [`crate::TilePyramidBuilder`] does, and what the
/// samples mean: the `datatype` (Requirement 9), the scale and offset that
/// convert a stored sample to a value, and the sentinel for "no data".
///
/// ```
/// use geopackage::core::coverage::CoverageDatatype;
/// use geopackage::core::tiles::{TileMatrixSet, ZoomLadder};
/// use geopackage::{CoverageBuilder, GeoPackage};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # let dir = tempfile::tempdir()?;
/// let gpkg = GeoPackage::create(dir.path().join("dem.gpkg"))?;
/// gpkg.add_epsg_srs(3857)?;
///
/// let matrix_set = TileMatrixSet::web_mercator_quad();
/// let matrices = matrix_set.ladder(ZoomLadder::new(0, 2))?;
/// let coverage = gpkg.create_coverage(
///     &CoverageBuilder::new("elevation", matrix_set, CoverageDatatype::Float)
///         .matrices(matrices)
///         .data_null(-9999.0)
///         .uom("m"),
/// )?;
///
/// assert_eq!(coverage.datatype(), Some(CoverageDatatype::Float));
/// assert_eq!(coverage.ancillary().data_null, Some(-9999.0));
/// # Ok(()) }
/// ```
#[derive(Debug, Clone)]
pub struct CoverageBuilder {
    table_name: String,
    matrix_set: TileMatrixSet,
    matrices: Vec<TileMatrix>,
    identifier: Option<String>,
    description: Option<String>,
    datatype: CoverageDatatype,
    scale: f64,
    offset: f64,
    precision: Option<f64>,
    data_null: Option<f64>,
    grid_cell_encoding: Option<String>,
    uom: Option<String>,
    field_name: Option<String>,
    quantity_definition: Option<String>,
    allow_zoom_other: bool,
}

impl CoverageBuilder {
    /// Starts a coverage over `matrix_set`, with samples of type `datatype`.
    ///
    /// The scale and offset are 1.0 and 0.0 by default. Requirement 11
    /// specifies these values for a `float` coverage. An `integer` coverage
    /// uses them until [`CoverageBuilder::scale`] sets other values.
    pub fn new(
        table_name: impl Into<String>,
        matrix_set: TileMatrixSet,
        datatype: CoverageDatatype,
    ) -> Self {
        Self {
            table_name: table_name.into(),
            matrix_set,
            matrices: Vec::new(),
            identifier: None,
            description: None,
            datatype,
            scale: 1.0,
            offset: 0.0,
            precision: None,
            data_null: None,
            grid_cell_encoding: None,
            uom: None,
            field_name: None,
            quantity_definition: None,
            allow_zoom_other: false,
        }
    }

    /// Adds one zoom level.
    #[must_use]
    pub fn matrix(mut self, matrix: TileMatrix) -> Self {
        self.matrices.push(matrix);
        self
    }

    /// Adds zoom levels, as [`TileMatrixSet::ladder`] produces them.
    #[must_use]
    pub fn matrices(mut self, matrices: impl IntoIterator<Item = TileMatrix>) -> Self {
        self.matrices.extend(matrices);
        self
    }

    /// Sets the `gpkg_contents.identifier`, which defaults to the table name.
    #[must_use]
    pub fn identifier(mut self, identifier: impl Into<String>) -> Self {
        self.identifier = Some(identifier.into());
        self
    }

    /// Sets the `gpkg_contents.description`.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Sets the scale and offset for the coverage.
    ///
    /// For a `float` coverage, [`GeoPackage::create_coverage`] rejects values
    /// other than the defaults (Requirement 11).
    #[must_use]
    pub fn scale(mut self, scale: f64, offset: f64) -> Self {
        self.scale = scale;
        self.offset = offset;
        self
    }

    /// Sets the smallest difference between two values that the coverage
    /// distinguishes.
    #[must_use]
    pub fn precision(mut self, precision: f64) -> Self {
        self.precision = Some(precision);
        self
    }

    /// Sets the stored sample that means "no data".
    #[must_use]
    pub fn data_null(mut self, data_null: f64) -> Self {
        self.data_null = Some(data_null);
        self
    }

    /// Sets the position of the value in a cell, for example
    /// `grid-value-is-center`.
    #[must_use]
    pub fn grid_cell_encoding(mut self, encoding: impl Into<String>) -> Self {
        self.grid_cell_encoding = Some(encoding.into());
        self
    }

    /// Sets the unit of measure.
    #[must_use]
    pub fn uom(mut self, uom: impl Into<String>) -> Self {
        self.uom = Some(uom.into());
        self
    }

    /// Sets the field name, which the table definition defaults to `Height`.
    #[must_use]
    pub fn field_name(mut self, field_name: impl Into<String>) -> Self {
        self.field_name = Some(field_name.into());
        self
    }

    /// Sets the definition of the quantity that the field measures.
    #[must_use]
    pub fn quantity_definition(mut self, definition: impl Into<String>) -> Self {
        self.quantity_definition = Some(definition.into());
        self
    }

    /// Allows zoom levels that do not step by factors of two, and registers
    /// `gpkg_zoom_other` (Annex F.6).
    #[must_use]
    pub fn allow_zoom_other(mut self, allow: bool) -> Self {
        self.allow_zoom_other = allow;
        self
    }

    /// Returns the table name of the coverage that this builder creates.
    pub fn table_name(&self) -> &str {
        &self.table_name
    }
}

impl GeoPackage {
    /// Creates a tiled gridded coverage from a [`CoverageBuilder`].
    ///
    /// Writes the same rows as a tile pyramid, with the `data_type`
    /// `2d-gridded-coverage` (Requirement 5). Also writes the two ancillary
    /// tables (Requirements 1 and 2), the ancillary row of the coverage
    /// (Requirement 7) and the three `gpkg_extensions` rows (Requirement 6), in
    /// one transaction.
    ///
    /// Adds the EPSG:4979 row that Requirement 3 specifies, if the file does not
    /// have it. A geographic 3D CRS has no WKT1 form, so this also adds the
    /// `gpkg_crs_wkt` extension column.
    ///
    /// # Per-tile statistics
    ///
    /// The file contains per-tile statistics only if the caller supplies them.
    /// `min`, `max`, `mean` and `std_dev` come from the samples, this workspace
    /// does not decode samples, and the table definition makes all four
    /// optional. See [`CoverageWriter::put_with_ancillary`].
    ///
    /// # Errors
    ///
    /// Those of [`GeoPackage::create_tile_pyramid`], and
    /// [`Error::FloatCoverageScaled`] if a `float` coverage has a scale or
    /// offset other than the defaults (Requirement 11).
    pub fn create_coverage(&self, builder: &CoverageBuilder) -> Result<Coverage<'_>> {
        let spec = PyramidSpec {
            table_name: &builder.table_name,
            matrix_set: &builder.matrix_set,
            matrices: &builder.matrices,
            identifier: builder.identifier.as_deref(),
            description: builder.description.as_deref(),
            data_type: COVERAGE_DATA_TYPE,
            allow_zoom_other: builder.allow_zoom_other,
        };
        let zoom_other = self.check_new_pyramid(&spec)?;
        check_float_defaults(
            builder.datatype,
            builder.scale,
            builder.offset,
            &builder.table_name,
        )?;

        let conn = self.connection();
        let tx = WriteTransaction::begin(conn)?;
        self.write_pyramid(&spec, zoom_other)?;
        // Requirement 3: a file that complies with this extension contains the
        // EPSG:4979 row, whether or not a coverage uses it. The requirement is
        // about the file, so the creation of a coverage adds the row. 4979 is
        // geographic 3D and has no WKT1 form, so the registration also adds
        // the `gpkg_crs_wkt` extension column. GDAL writes the same row and
        // column.
        self.add_epsg_srs(COVERAGE_VERTICAL_SRS_ID)?;
        for (exists, sql) in [
            (
                table_exists(conn, COVERAGE_ANCILLARY_TABLE)?,
                ddl::CREATE_GPKG_2D_GRIDDED_COVERAGE_ANCILLARY,
            ),
            (
                table_exists(conn, TILE_ANCILLARY_TABLE)?,
                ddl::CREATE_GPKG_2D_GRIDDED_TILE_ANCILLARY,
            ),
        ] {
            if !exists {
                conn.execute_batch(sql)?;
            }
        }
        conn.execute(
            &format!(
                "INSERT INTO {COVERAGE_ANCILLARY_TABLE} \
                 (tile_matrix_set_name, datatype, scale, offset, precision, data_null, \
                  grid_cell_encoding, uom, field_name, quantity_definition) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
            ),
            rusqlite::params![
                builder.table_name,
                builder.datatype.as_str(),
                builder.scale,
                builder.offset,
                builder.precision,
                builder.data_null,
                builder.grid_cell_encoding,
                builder.uom,
                builder.field_name,
                builder.quantity_definition,
            ],
        )?;
        // Requirement 6: one row for each ancillary table, and one for the
        // payload column of the coverage.
        for (table, column) in [
            (COVERAGE_ANCILLARY_TABLE, None),
            (TILE_ANCILLARY_TABLE, None),
            (builder.table_name.as_str(), Some(tiles::TILE_DATA_COLUMN)),
        ] {
            crate::extensions::register_if_absent(
                conn,
                Some(table),
                column,
                COVERAGE_EXTENSION_NAME,
                COVERAGE_EXTENSION_DEFINITION,
                COVERAGE_EXTENSION_SCOPE,
            )?;
        }
        tx.commit()?;
        self.coverage(&builder.table_name)
    }
}

/// The EPSG code that Requirement 3 specifies for every coverage file: WGS 84
/// 3D, the geographic 3D CRS that vertical values refer to.
const COVERAGE_VERTICAL_SRS_ID: i32 = 4979;

/// Requirement 11: "When the datatype of the corresponding
/// `gpkg_2d_gridded_coverage_ancillary` row is _float_, the `scale` and
/// `offset` values _SHALL_ be set to the defaults."
#[expect(
    clippy::float_cmp,
    reason = "the requirement says \"set to the defaults\", which is exactly 1.0 and 0.0; a tolerance would accept values it does not"
)]
fn check_float_defaults(
    datatype: CoverageDatatype,
    scale: f64,
    offset: f64,
    table_name: &str,
) -> Result<()> {
    if datatype == CoverageDatatype::Float && (scale != 1.0 || offset != 0.0) {
        return Err(Error::FloatCoverageScaled {
            table_name: table_name.to_owned(),
            scale,
            offset,
        });
    }
    Ok(())
}

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
    /// [`Error`] if the transaction cannot be opened or the statements
    /// prepared.
    pub fn writer(&self) -> Result<CoverageWriter<'a>> {
        CoverageWriter::new(self)
    }
}

/// A transaction over one coverage, with per-tile `put` and `delete`
/// operations that maintain the ancillary rows of Requirement 10.
///
/// Obtained from [`Coverage::writer`]. The writer writes a tile and its
/// ancillary row together, and deletes them together. The normative table
/// definition has no `ON DELETE CASCADE`, so the database does not delete the
/// row. A tile without its row loses the scale and offset that give its samples
/// a meaning.
pub struct CoverageWriter<'conn> {
    tx: WriteTransaction<'conn>,
    conn: &'conn Connection,
    table_name: String,
    datatype: Option<CoverageDatatype>,
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
        let conn = coverage.gpkg().connection();
        let table = quote(coverage.table_name())?;
        let address = "zoom_level = ?1 AND tile_column = ?2 AND tile_row = ?3";
        let tx = WriteTransaction::begin(conn)?;
        Ok(Self {
            tx,
            conn,
            table_name: coverage.table_name().to_owned(),
            datatype: coverage.datatype(),
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

    /// Writes one tile with the ancillary row the caller supplies.
    ///
    /// A caller that decoded the samples supplies the statistics here. The
    /// table definition makes the four statistics optional, and this workspace
    /// does not decode samples, so it cannot calculate them.
    ///
    /// # Checks
    ///
    /// The payload is checked against the encoding profile (Requirements 15 to
    /// 20, or 13 for PNG), the `datatype` of the coverage (Requirements 13 and
    /// 14), and the pixel size of its zoom level. The writer rejects Deflate,
    /// which a read accepts: Requirement 15 specifies baseline TIFF, and
    /// Deflate is not baseline.
    ///
    /// **This function does not check Requirement 21** ("all pixels … set with
    /// a valid component value … NaN and Inf SHALL NOT be used"). That
    /// requirement is about samples, and this crate does not decode samples. A
    /// caller that writes a payload from another source is responsible for it.
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
    pub fn put_with_ancillary(
        &mut self,
        coord: TileCoord,
        data: &[u8],
        ancillary: &TileAncillary,
    ) -> Result<()> {
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
        matrix.check_payload(&TilePayload {
            format: match payload {
                CoveragePayload::Tiff(_) => TileFormat::Tiff,
                CoveragePayload::Png(_) => TileFormat::Png,
                _ => TileFormat::Other,
            },
            width: payload.width(),
            height: payload.height(),
        })?;
        check_float_defaults(
            self.datatype.unwrap_or(CoverageDatatype::Integer),
            ancillary.scale,
            ancillary.offset,
            &self.table_name,
        )?;

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
}

impl std::fmt::Debug for CoverageWriter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoverageWriter")
            .field("table_name", &self.table_name)
            .field("datatype", &self.datatype)
            .finish_non_exhaustive()
    }
}
