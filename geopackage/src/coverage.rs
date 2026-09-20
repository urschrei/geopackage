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

impl TileAncillary {
    /// The row a tile gets when nothing is said about it: scale 1.0, offset
    /// 0.0, no statistics.
    ///
    /// What Requirement 11 requires of a `float` coverage, and what an
    /// `integer` one means by "the samples are the values".
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

    /// A row carrying a scale and offset of its own, which only an `integer`
    /// coverage may do (Requirement 11).
    pub fn new(scale: f64, offset: f64) -> Self {
        Self {
            scale,
            offset,
            ..Self::defaults()
        }
    }

    /// Records the statistics of the tile's samples.
    ///
    /// For a caller that decoded them: this workspace does not, so these
    /// arrive from outside or not at all.
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

/// A declarative builder for a tiled gridded coverage.
///
/// Declares the grid, as [`crate::TilePyramidBuilder`] does, plus what the
/// samples mean: the `datatype` (Requirement 9), the scale and offset that
/// carry a stored sample back to a value, and the sentinel that means there is
/// no value there.
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
    /// Starts a coverage over `matrix_set`, whose samples are of `datatype`.
    ///
    /// The scale and offset default to 1.0 and 0.0, which is what
    /// Requirement 11 requires of a `float` coverage and what an `integer` one
    /// uses until [`CoverageBuilder::scale`] says otherwise.
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

    /// Sets the coverage-wide scale and offset.
    ///
    /// Refused at creation for a `float` coverage unless they are the defaults
    /// (Requirement 11).
    #[must_use]
    pub fn scale(mut self, scale: f64, offset: f64) -> Self {
        self.scale = scale;
        self.offset = offset;
        self
    }

    /// Sets the smallest difference between values the coverage distinguishes.
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

    /// Sets where in a cell the value sits, e.g. `grid-value-is-center`.
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

    /// Sets the quantity the field measures.
    #[must_use]
    pub fn quantity_definition(mut self, definition: impl Into<String>) -> Self {
        self.quantity_definition = Some(definition.into());
        self
    }

    /// Allows zoom levels that do not step by factors of two, registering
    /// `gpkg_zoom_other` (Annex F.6).
    #[must_use]
    pub fn allow_zoom_other(mut self, allow: bool) -> Self {
        self.allow_zoom_other = allow;
        self
    }

    /// Returns the table name this coverage will be created as.
    pub fn table_name(&self) -> &str {
        &self.table_name
    }
}

impl GeoPackage {
    /// Creates a tiled gridded coverage from a [`CoverageBuilder`].
    ///
    /// Emits everything a tile pyramid needs, with `data_type` of
    /// `2d-gridded-coverage` (Requirement 5), plus the two ancillary tables
    /// (Requirements 1 and 2), the coverage's own ancillary row (Requirement
    /// 7) and the three `gpkg_extensions` rows (Requirement 6), in one
    /// transaction.
    ///
    /// # What a file written here does not have
    ///
    /// Per-tile statistics, unless the caller supplies them: `min`, `max`,
    /// `mean` and `std_dev` are computed from samples, this workspace decodes
    /// none, and the table definition leaves all four optional. See
    /// [`CoverageWriter::put_with_ancillary`].
    ///
    /// The EPSG:4979 row Requirement 3 asks for is added if the file does not
    /// already have it, which also brings in the `gpkg_crs_wkt` extension
    /// column, since a geographic 3D CRS has no WKT1 form.
    ///
    /// # Errors
    ///
    /// Those of [`GeoPackage::create_tile_pyramid`], plus
    /// [`Error::FloatCoverageScaled`] when a `float` coverage asks for a scale
    /// or offset other than the defaults (Requirement 11).
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
        // Requirement 3: a file complying with this extension carries the
        // EPSG:4979 row, whether or not this coverage is the thing that uses
        // it. The requirement is about the file, so creating a coverage is
        // what puts it there, and GDAL writes the same row for the same
        // reason. 4979 is geographic 3D and has no WKT1 form, so registering
        // it also brings in the `gpkg_crs_wkt` extension column; that too is
        // what GDAL does.
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
        // Requirement 6: one row per ancillary table, and one for the payload
        // column of the coverage itself.
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

/// The EPSG code Requirement 3 asks every coverage file to carry: WGS 84 3D,
/// the geographic 3D CRS a vertical datum is expressed against.
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
    /// Writes one tile and its ancillary row, in its own transaction.
    ///
    /// The ancillary row carries the default scale and offset and no
    /// statistics; [`CoverageWriter::put_with_ancillary`] is the call that
    /// takes them.
    ///
    /// # Errors
    ///
    /// As [`CoverageWriter::put`].
    pub fn put_tile(&self, coord: TileCoord, data: &[u8]) -> Result<()> {
        let mut writer = self.writer()?;
        writer.put(coord, data)?;
        writer.commit()
    }

    /// Deletes one tile and its ancillary row, returning whether there was a
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
    /// per-tile `put`/`delete` that keep both tables in step.
    ///
    /// # Errors
    ///
    /// [`Error`] if the transaction cannot be opened or the statements
    /// prepared.
    pub fn writer(&self) -> Result<CoverageWriter<'a>> {
        CoverageWriter::new(self)
    }
}

/// A transaction over one coverage, with per-tile `put` and `delete` that
/// maintain the ancillary rows Requirement 10 asks for.
///
/// Obtained from [`Coverage::writer`]. The tile and its ancillary row are
/// written together and deleted together: the normative table definition has
/// no `ON DELETE CASCADE`, so nothing does that for us, and a tile whose row
/// is missing loses the scale and offset that make its samples mean anything.
pub struct CoverageWriter<'conn> {
    tx: WriteTransaction<'conn>,
    conn: &'conn Connection,
    table_name: String,
    datatype: Option<CoverageDatatype>,
    /// The coverage's zoom levels, ascending, copied in as [`crate::TileWriter`]
    /// copies a pyramid's and for the same reason.
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
            // The tile's id, read back rather than returned by the insert:
            // `RETURNING` needs SQLite 3.35, and this crate links the system
            // SQLite by default (D1), so using it would raise a runtime
            // requirement nothing else in the workspace imposes. `ON CONFLICT
            // DO UPDATE` rules out `last_insert_rowid`, which is not updated
            // when the row was updated rather than inserted.
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
    /// This is where a caller who decoded the samples puts what it found: the
    /// four statistics are optional in the table definition, and a workspace
    /// that decodes nothing cannot compute them, so they arrive here or not at
    /// all.
    ///
    /// # What is checked, and what is not
    ///
    /// The payload is checked against the encoding profile (Requirements 15 to
    /// 20, or 13 for PNG), against the coverage's `datatype` (Requirements 13
    /// and 14), and against the pixel size its zoom level declares. Deflate is
    /// refused here though it is accepted on read: Requirement 15 asks for
    /// baseline TIFF, which Deflate is not, and a writer is where strictness
    /// is cheap.
    ///
    /// **Requirement 21 is not checked** — "all pixels … set with a valid
    /// component value … NaN and Inf SHALL NOT be used" is a statement about
    /// samples, and this crate decodes none. A caller writing a payload it
    /// produced elsewhere owns that requirement.
    ///
    /// # Errors
    ///
    /// - [`Error::UnknownZoomLevel`] if the coverage declares no such level.
    /// - [`Error::Tile`] if the address is outside the grid, or the payload
    ///   breaks the profile, contradicts the `datatype`, or is the wrong size.
    /// - [`Error::UnwritableCoveragePayload`] for a payload that may be read
    ///   but not written.
    /// - [`Error::FloatCoverageScaled`] if the ancillary row carries a scale
    ///   or offset on a `float` coverage (Requirement 11).
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
                    "{:?} compression is a TIFF extension, and Requirement 15 asks for baseline TIFF",
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
        // Requirement 10: the tile's row, keyed by the id the tile just got.
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

    /// Deletes one tile and its ancillary row, returning whether there was a
    /// tile to delete.
    ///
    /// Both, and in that order: the ancillary row is found through the tile,
    /// so it goes first.
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

    /// Refreshes `gpkg_contents.last_change` and commits.
    ///
    /// As [`crate::TileWriter::commit`], including its behaviour when the
    /// transaction was the caller's.
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
