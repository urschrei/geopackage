//! The two ancillary tables of a coverage, as rows, and the arithmetic the
//! extension defines on them.

use super::CoverageDatatype;

/// A `gpkg_2d_gridded_coverage_ancillary` row, which states what the samples of
/// a coverage mean.
///
/// There is one row for each coverage (Requirements 1 and 7). The fields are
/// the columns of the spec, including the optional columns. Requirement 1 lets
/// an implementation add columns, and tells clients to ignore columns that they
/// do not recognise. This struct holds a fixed set of columns, so it ignores
/// other columns.
///
/// Each number is stored as an SQLite `REAL`. SQLite stores NaN as NULL, and a
/// negative zero as zero, so a `-0.0` reads back as `0.0`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct CoverageAncillary {
    /// The `gpkg_tile_matrix_set` and `gpkg_contents` row that this row
    /// describes (Requirements 7 and 8).
    pub tile_matrix_set_name: String,
    /// `integer` or `float` (Requirement 9), as stored.
    ///
    /// The field is text, because a read does not reject a file.
    /// [`CoverageAncillary::datatype`] returns the parsed form, or `None` for
    /// a value that the requirement does not permit.
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

impl CoverageAncillary {
    /// Returns a row with the given name and `datatype`, and every other
    /// column at the default of the table definition: scale 1.0, offset 0.0,
    /// and no optional column set.
    ///
    /// `datatype` is the stored text, as the field is.
    pub fn new(tile_matrix_set_name: impl Into<String>, datatype: impl Into<String>) -> Self {
        Self {
            tile_matrix_set_name: tile_matrix_set_name.into(),
            datatype: datatype.into(),
            scale: 1.0,
            offset: 0.0,
            precision: None,
            data_null: None,
            grid_cell_encoding: None,
            uom: None,
            field_name: None,
            quantity_definition: None,
        }
    }

    /// Returns the parsed `datatype`.
    ///
    /// Returns `None` for a value that Requirement 9 does not permit. The
    /// `CHECK` constraint of the table definition prevents such a value, but a
    /// file written without the constraint can contain one.
    pub fn datatype(&self) -> Option<CoverageDatatype> {
        CoverageDatatype::parse(&self.datatype)
    }

    /// Converts a stored sample to its value, with the scale and offset of the
    /// tile and of the coverage.
    ///
    /// The spec gives the calculation as pseudo-code: multiply the stored value
    /// by the `scale` of the tile and add the `offset` of the tile, then do the
    /// same with the pair of the coverage.
    ///
    /// Do not use this function for `data_null`, which the requirement
    /// excludes. Use [`CoverageAncillary::is_null`].
    pub fn value(&self, tile: &TileAncillary, stored: f64) -> f64 {
        (stored * tile.scale + tile.offset) * self.scale + self.offset
    }

    /// Returns `true` if a stored sample is the "no data" value of the
    /// coverage.
    ///
    /// The comparison is exact, and uses the stored sample, not the value from
    /// [`CoverageAncillary::value`]: "The scale and offset do not apply to the
    /// `data_null` value". A tolerance would also match valid samples near the
    /// sentinel.
    pub fn is_null(&self, stored: f64) -> bool {
        self.data_null == Some(stored)
    }
}

/// A `gpkg_2d_gridded_tile_ancillary` row: the scale, offset and statistics of
/// one tile.
///
/// Requirement 10 specifies one row for each tile. The table definition makes
/// the four statistics optional, and a writer that does not decode samples
/// cannot calculate them. `None` therefore means "not recorded", not "no
/// value". The statistics are of values, after the scale and offset, as the
/// extension specifies.
///
/// Each value is stored as an SQLite `REAL`. SQLite stores NaN as NULL, and a
/// negative zero as zero, so a `-0.0` reads back as `0.0`.
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

    /// Sets the statistics of the values in the tile.
    ///
    /// Without the `coverage-samples` feature this crate does not decode
    /// samples, so you supply the statistics. With it, `TileSamples::statistics`
    /// calculates them.
    #[must_use]
    pub fn with_statistics(mut self, min: f64, max: f64, mean: f64, std_dev: f64) -> Self {
        self.min = Some(min);
        self.max = Some(max);
        self.mean = Some(mean);
        self.std_dev = Some(std_dev);
        self
    }
}
