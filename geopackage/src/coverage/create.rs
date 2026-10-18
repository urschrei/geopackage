//! Creating a coverage: the builder, and the catalogue rows it writes.

use geopackage_core::coverage::{
    COVERAGE_ANCILLARY_TABLE, COVERAGE_DATA_TYPE, COVERAGE_EXTENSION_DEFINITION,
    COVERAGE_EXTENSION_NAME, COVERAGE_EXTENSION_SCOPE, CoverageDatatype, TILE_ANCILLARY_TABLE,
};
use geopackage_core::ddl;
use geopackage_core::tiles::{self, TileMatrix, TileMatrixSet};

use crate::tiles::PyramidSpec;
use crate::transaction::WriteTransaction;
use crate::{GeoPackage, Result, table_exists};

use super::Coverage;
use super::ancillary::check_float_defaults;
use super::ancillary::check_not_nan;

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
    ///
    /// The value cannot be NaN: SQLite stores NaN as NULL, which in this column
    /// means that there is no such sample, so [`GeoPackage::create_coverage`]
    /// rejects it.
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
    /// The file contains per-tile statistics only if they are written. `min`,
    /// `max`, `mean` and `std_dev` come from the samples, and the table
    /// definition makes all four optional. Supply them through
    /// [`CoverageWriter::put_with_ancillary`](crate::CoverageWriter::put_with_ancillary), or, with the `coverage-samples`
    /// feature, have [`CoverageWriter::put_with_statistics`](crate::CoverageWriter::put_with_statistics) calculate them.
    ///
    /// # Errors
    ///
    /// Those of [`GeoPackage::create_tile_pyramid`],
    /// [`Error::FloatCoverageScaled`](crate::Error::FloatCoverageScaled) if a `float` coverage has a scale or
    /// offset other than the defaults (Requirement 11), and
    /// [`Error::NanAncillaryValue`](crate::Error::NanAncillaryValue) if the scale, offset, precision or
    /// `data_null` is NaN.
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
        check_not_nan(
            &builder.table_name,
            &[
                (
                    "gpkg_2d_gridded_coverage_ancillary.scale",
                    Some(builder.scale),
                ),
                (
                    "gpkg_2d_gridded_coverage_ancillary.offset",
                    Some(builder.offset),
                ),
                (
                    "gpkg_2d_gridded_coverage_ancillary.precision",
                    builder.precision,
                ),
                (
                    "gpkg_2d_gridded_coverage_ancillary.data_null",
                    builder.data_null,
                ),
            ],
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
