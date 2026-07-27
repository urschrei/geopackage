//! Tile pyramids: the user data table template (spec clause 2.2.8, Annex C),
//! the tile matrix model, and the consistency rules a conforming pyramid
//! satisfies.
//!
//! A tile pyramid is a `gpkg_contents` row with `data_type = 'tiles'`, one
//! `gpkg_tile_matrix_set` row fixing the pyramid's bounding box and spatial
//! reference system, one `gpkg_tile_matrix` row per zoom level, and a user
//! table holding the tiles themselves. The table definition SQL for the two
//! catalogue tables is in [`crate::ddl`]; the user table is per-pyramid, so it
//! is built here.
//!
//! [`TileMatrixSet`] and [`TileMatrix`] carry no table name: they describe the
//! shape of a pyramid, and which table it belongs to is the container's
//! business. [`TileMatrixSet::validate`] checks a pyramid against Requirements
//! 45 to 53. Nothing here reads or writes a database.

use crate::Error;
use crate::ident::quote;

/// Relative tolerance for the tile matrix extent identity (Requirement 45).
///
/// The requirement is stated as an equality between the matrix set's width and
/// `matrix_width * tile_width * pixel_x_size`, but a pixel size is almost
/// always derived by division, so an exact comparison would reject files that
/// only differ in the last bits. A relative tolerance of `1e-9` is far above
/// that rounding and far below any real disagreement, which is off by whole
/// tiles.
const EXTENT_TOLERANCE: f64 = 1e-9;

/// The tile payload column of a tile pyramid user data table.
///
/// Fixed by the spec, and the `gpkg_extensions.column_name` value the tile
/// extensions (`gpkg_webp`, `gpkg_zoom_other`) register against.
pub const TILE_DATA_COLUMN: &str = "tile_data";

/// `CREATE TABLE` statement for a tile pyramid user data table, in the form
/// given by Annex C (Requirement 54): an `INTEGER PRIMARY KEY` acting as the
/// rowid alias, the zoom/column/row index, the payload, and the uniqueness
/// constraint over the three index columns.
///
/// The column names are not caller-configurable: unlike a feature table, whose
/// geometry column is named in `gpkg_geometry_columns`, a tile table's shape is
/// fixed by the spec and every reader assumes it.
pub fn create_tile_table_sql(table: &str) -> Result<String, Error> {
    Ok(format!(
        "CREATE TABLE {} (\
         id INTEGER PRIMARY KEY AUTOINCREMENT, \
         zoom_level INTEGER NOT NULL, \
         tile_column INTEGER NOT NULL, \
         tile_row INTEGER NOT NULL, \
         tile_data BLOB NOT NULL, \
         UNIQUE (zoom_level, tile_column, tile_row))",
        quote(table)?
    ))
}

/// A `gpkg_tile_matrix_set` row: the exact extent of a tile pyramid, and the
/// spatial reference system its coordinates are in.
///
/// The extent is exact rather than informative (Requirement 144): every tile's
/// own bounding box is calculated from it, and `(min_x, max_y)` is the
/// upper-left corner of tile `(0, 0)` at every zoom level. Tiles may be sparse
/// within it, so the extent may be larger than the tiles actually present.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileMatrixSet {
    /// Spatial reference system of the extent, and of the pyramid.
    pub srs_id: i32,
    /// Minimum x (west edge).
    pub min_x: f64,
    /// Minimum y (south edge).
    pub min_y: f64,
    /// Maximum x (east edge).
    pub max_x: f64,
    /// Maximum y (north edge).
    pub max_y: f64,
}

/// A `gpkg_tile_matrix` row: one zoom level of a pyramid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileMatrix {
    /// Zoom level, ascending with resolution.
    pub zoom_level: i64,
    /// Number of tile columns at this zoom level.
    pub matrix_width: i64,
    /// Number of tile rows at this zoom level.
    pub matrix_height: i64,
    /// Tile width in pixels.
    pub tile_width: i64,
    /// Tile height in pixels.
    pub tile_height: i64,
    /// Ground units per pixel in x.
    pub pixel_x_size: f64,
    /// Ground units per pixel in y.
    pub pixel_y_size: f64,
}

/// A tile pyramid that does not satisfy the spec's consistency rules.
///
/// Raised by [`TileMatrixSet::validate`], which the container layer runs before
/// writing a pyramid. Reading never validates: a file written by someone else
/// is reported as it is.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TileError {
    /// The matrix set extent is not a well-ordered, finite box.
    #[error(
        "tile matrix set extent [{min_x}, {min_y}, {max_x}, {max_y}] is not a finite box with min < max on both axes"
    )]
    InvalidExtent {
        /// Minimum x of the offending extent.
        min_x: f64,
        /// Minimum y.
        min_y: f64,
        /// Maximum x.
        max_x: f64,
        /// Maximum y.
        max_y: f64,
    },
    /// A negative `zoom_level` (Requirement 46).
    #[error("zoom level {zoom_level} is negative")]
    NegativeZoomLevel {
        /// The offending zoom level.
        zoom_level: i64,
    },
    /// A `matrix_width`, `matrix_height`, `tile_width` or `tile_height` that is
    /// not greater than zero (Requirements 47 to 50).
    #[error("zoom level {zoom_level} has {field} {value}, which must be greater than 0")]
    NonPositiveDimension {
        /// Zoom level of the offending tile matrix.
        zoom_level: i64,
        /// Which column: `matrix_width`, `matrix_height`, `tile_width` or
        /// `tile_height`.
        field: &'static str,
        /// The offending value.
        value: i64,
    },
    /// A `pixel_x_size` or `pixel_y_size` that is not greater than zero
    /// (Requirements 51 and 52).
    #[error("zoom level {zoom_level} has {field} {value}, which must be greater than 0")]
    NonPositivePixelSize {
        /// Zoom level of the offending tile matrix.
        zoom_level: i64,
        /// Which column: `pixel_x_size` or `pixel_y_size`.
        field: &'static str,
        /// The offending value.
        value: f64,
    },
    /// Pixel sizes that do not decrease as zoom level increases
    /// (Requirement 53).
    #[error(
        "zoom level {zoom_level} has {field} {value}, which is not smaller than the {previous_value} of zoom level {previous_zoom_level}"
    )]
    PixelSizeNotDescending {
        /// Zoom level of the offending tile matrix.
        zoom_level: i64,
        /// The next lower zoom level present.
        previous_zoom_level: i64,
        /// Which column: `pixel_x_size` or `pixel_y_size`.
        field: &'static str,
        /// The offending value.
        value: f64,
        /// The value at `previous_zoom_level`.
        previous_value: f64,
    },
    /// A zoom level whose tile grid does not span the matrix set extent
    /// (Requirement 45).
    #[error(
        "zoom level {zoom_level} spans {actual} in {axis}, but the tile matrix set extent is {expected}"
    )]
    ExtentMismatch {
        /// Zoom level of the offending tile matrix.
        zoom_level: i64,
        /// Which axis: `x` or `y`.
        axis: &'static str,
        /// The extent implied by the matrix set.
        expected: f64,
        /// The extent the tile grid spans.
        actual: f64,
    },
    /// Two tile matrices for the same zoom level (the `gpkg_tile_matrix`
    /// primary key is `(table_name, zoom_level)`).
    #[error("zoom level {zoom_level} appears more than once")]
    DuplicateZoomLevel {
        /// The repeated zoom level.
        zoom_level: i64,
    },
}

impl TileMatrixSet {
    /// A matrix set from its spatial reference system and its four edges.
    pub fn new(srs_id: i32, min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Self {
        Self {
            srs_id,
            min_x,
            min_y,
            max_x,
            max_y,
        }
    }

    /// Width of the extent in ground units.
    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }

    /// Height of the extent in ground units.
    pub fn height(&self) -> f64 {
        self.max_y - self.min_y
    }

    /// Check a whole pyramid: the extent, then every tile matrix against
    /// Requirements 45 to 53, in ascending zoom order.
    ///
    /// `matrices` may be in any order and may be empty: a pyramid with no zoom
    /// levels declared breaks no rule, since the spec requires a
    /// `gpkg_tile_matrix` row only for a level that holds tiles.
    ///
    /// # Errors
    ///
    /// The first [`TileError`] the pyramid violates, checked lowest zoom level
    /// first.
    pub fn validate(&self, matrices: &[TileMatrix]) -> Result<(), TileError> {
        self.validate_extent()?;
        // Sorted rather than compared pairwise in place: Requirement 53 is
        // stated over the zoom-ascending sequence, and a caller assembling a
        // ladder has no reason to hand it over in order. Tens of levels at
        // most, and this runs once per pyramid, not once per tile.
        let mut sorted: Vec<&TileMatrix> = matrices.iter().collect();
        sorted.sort_unstable_by_key(|matrix| matrix.zoom_level);
        let mut previous: Option<&TileMatrix> = None;
        for matrix in sorted {
            matrix.validate()?;
            self.validate_span(matrix)?;
            if let Some(previous) = previous {
                previous.validate_precedes(matrix)?;
            }
            previous = Some(matrix);
        }
        Ok(())
    }

    /// Requirement 144: the extent is exact, so it has to be a finite box with
    /// positive width and height for any tile bound to be calculable from it.
    fn validate_extent(&self) -> Result<(), TileError> {
        let ordered = self.min_x < self.max_x && self.min_y < self.max_y;
        let finite = self.min_x.is_finite()
            && self.min_y.is_finite()
            && self.max_x.is_finite()
            && self.max_y.is_finite();
        if ordered && finite {
            Ok(())
        } else {
            Err(TileError::InvalidExtent {
                min_x: self.min_x,
                min_y: self.min_y,
                max_x: self.max_x,
                max_y: self.max_y,
            })
        }
    }

    /// Requirement 45: the tile grid at each zoom level spans the matrix set
    /// extent exactly, within [`EXTENT_TOLERANCE`].
    fn validate_span(&self, matrix: &TileMatrix) -> Result<(), TileError> {
        for (axis, expected, actual) in [
            ("x", self.width(), matrix.span_x()),
            ("y", self.height(), matrix.span_y()),
        ] {
            if (expected - actual).abs() > EXTENT_TOLERANCE * expected.abs().max(actual.abs()) {
                return Err(TileError::ExtentMismatch {
                    zoom_level: matrix.zoom_level,
                    axis,
                    expected,
                    actual,
                });
            }
        }
        Ok(())
    }
}

impl TileMatrix {
    /// A tile matrix from its zoom level, grid size, tile size and pixel sizes.
    pub fn new(
        zoom_level: i64,
        matrix_width: i64,
        matrix_height: i64,
        tile_width: i64,
        tile_height: i64,
        pixel_x_size: f64,
        pixel_y_size: f64,
    ) -> Self {
        Self {
            zoom_level,
            matrix_width,
            matrix_height,
            tile_width,
            tile_height,
            pixel_x_size,
            pixel_y_size,
        }
    }

    /// Ground units this zoom level's tile grid spans in x.
    pub fn span_x(&self) -> f64 {
        self.matrix_width as f64 * self.tile_width as f64 * self.pixel_x_size
    }

    /// Ground units this zoom level's tile grid spans in y.
    pub fn span_y(&self) -> f64 {
        self.matrix_height as f64 * self.tile_height as f64 * self.pixel_y_size
    }

    /// Requirements 46 to 52: a non-negative zoom level, positive grid and tile
    /// dimensions, positive pixel sizes.
    ///
    /// # Errors
    ///
    /// The first rule this tile matrix violates.
    pub fn validate(&self) -> Result<(), TileError> {
        if self.zoom_level < 0 {
            return Err(TileError::NegativeZoomLevel {
                zoom_level: self.zoom_level,
            });
        }
        for (field, value) in [
            ("matrix_width", self.matrix_width),
            ("matrix_height", self.matrix_height),
            ("tile_width", self.tile_width),
            ("tile_height", self.tile_height),
        ] {
            if value <= 0 {
                return Err(TileError::NonPositiveDimension {
                    zoom_level: self.zoom_level,
                    field,
                    value,
                });
            }
        }
        for (field, value) in [
            ("pixel_x_size", self.pixel_x_size),
            ("pixel_y_size", self.pixel_y_size),
        ] {
            // Finiteness first: a NaN compares false against every bound, so
            // testing `value <= 0.0` alone would let one through.
            if !value.is_finite() || value <= 0.0 {
                return Err(TileError::NonPositivePixelSize {
                    zoom_level: self.zoom_level,
                    field,
                    value,
                });
            }
        }
        Ok(())
    }

    /// Requirement 53: pixel sizes decrease as zoom level increases.
    ///
    /// Read strictly: two zoom levels of identical resolution are not a
    /// pyramid, and the OGC test suite compares them as decreasing. `self` is
    /// the lower zoom level.
    fn validate_precedes(&self, next: &TileMatrix) -> Result<(), TileError> {
        if self.zoom_level == next.zoom_level {
            return Err(TileError::DuplicateZoomLevel {
                zoom_level: self.zoom_level,
            });
        }
        for (field, value, previous_value) in [
            ("pixel_x_size", next.pixel_x_size, self.pixel_x_size),
            ("pixel_y_size", next.pixel_y_size, self.pixel_y_size),
        ] {
            if value >= previous_value {
                return Err(TileError::PixelSizeNotDescending {
                    zoom_level: next.zoom_level,
                    previous_zoom_level: self.zoom_level,
                    field,
                    value,
                    previous_value,
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A two-level pyramid over a 256-unit square: 1x1 tiles of 256 pixels at
    /// zoom 0, 2x2 at zoom 1.
    fn square_pyramid() -> (TileMatrixSet, Vec<TileMatrix>) {
        let set = TileMatrixSet::new(4326, 0.0, 0.0, 256.0, 256.0);
        let matrices = vec![
            TileMatrix::new(0, 1, 1, 256, 256, 1.0, 1.0),
            TileMatrix::new(1, 2, 2, 256, 256, 0.5, 0.5),
        ];
        (set, matrices)
    }

    #[test]
    fn valid_pyramid_passes() {
        let (set, matrices) = square_pyramid();
        set.validate(&matrices).unwrap();
    }

    #[test]
    fn unordered_input_is_validated_in_zoom_order() {
        let (set, mut matrices) = square_pyramid();
        matrices.reverse();
        set.validate(&matrices).unwrap();
    }

    #[test]
    fn empty_pyramid_breaks_no_rule() {
        let (set, _) = square_pyramid();
        set.validate(&[]).unwrap();
    }

    #[test]
    fn requirement_144_extent_must_be_a_well_ordered_box() {
        let inverted = TileMatrixSet::new(4326, 256.0, 0.0, 0.0, 256.0);
        assert!(matches!(
            inverted.validate(&[]),
            Err(TileError::InvalidExtent { .. })
        ));
        let degenerate = TileMatrixSet::new(4326, 0.0, 0.0, 0.0, 256.0);
        assert!(matches!(
            degenerate.validate(&[]),
            Err(TileError::InvalidExtent { .. })
        ));
        let infinite = TileMatrixSet::new(4326, 0.0, 0.0, f64::INFINITY, 256.0);
        assert!(matches!(
            infinite.validate(&[]),
            Err(TileError::InvalidExtent { .. })
        ));
    }

    #[test]
    fn requirement_45_tile_grid_spans_the_extent() {
        let (set, _) = square_pyramid();
        // Half the columns needed to cover 256 units at one unit per pixel.
        let short = TileMatrix::new(1, 1, 2, 256, 256, 0.5, 0.5);
        assert!(matches!(
            set.validate(&[short]),
            Err(TileError::ExtentMismatch { axis: "x", .. })
        ));
    }

    #[test]
    fn requirement_45_tolerates_rounding_in_a_derived_pixel_size() {
        let set = TileMatrixSet::new(
            3857,
            -20_037_508.34,
            -20_037_508.34,
            20_037_508.34,
            20_037_508.34,
        );
        // The pixel size a caller would compute by division, which does not
        // multiply back to the extent exactly.
        let pixel = set.width() / 256.0;
        set.validate(&[TileMatrix::new(0, 1, 1, 256, 256, pixel, pixel)])
            .unwrap();
    }

    #[test]
    fn requirements_46_to_50_reject_non_positive_dimensions() {
        let (set, _) = square_pyramid();
        assert!(matches!(
            set.validate(&[TileMatrix::new(-1, 1, 1, 256, 256, 1.0, 1.0)]),
            Err(TileError::NegativeZoomLevel { zoom_level: -1 })
        ));
        for matrix in [
            TileMatrix::new(0, 0, 1, 256, 256, 1.0, 1.0),
            TileMatrix::new(0, 1, 0, 256, 256, 1.0, 1.0),
            TileMatrix::new(0, 1, 1, 0, 256, 1.0, 1.0),
            TileMatrix::new(0, 1, 1, 256, 0, 1.0, 1.0),
        ] {
            assert!(matches!(
                set.validate(&[matrix]),
                Err(TileError::NonPositiveDimension { .. })
            ));
        }
    }

    #[test]
    fn requirements_51_and_52_reject_non_positive_pixel_sizes() {
        let (set, _) = square_pyramid();
        for matrix in [
            TileMatrix::new(0, 1, 1, 256, 256, 0.0, 1.0),
            TileMatrix::new(0, 1, 1, 256, 256, 1.0, -1.0),
            TileMatrix::new(0, 1, 1, 256, 256, f64::NAN, 1.0),
            TileMatrix::new(0, 1, 1, 256, 256, 1.0, f64::INFINITY),
        ] {
            assert!(matches!(
                set.validate(&[matrix]),
                Err(TileError::NonPositivePixelSize { .. })
            ));
        }
    }

    #[test]
    fn requirement_53_pixel_sizes_descend_with_zoom() {
        let set = TileMatrixSet::new(4326, 0.0, 0.0, 256.0, 256.0);
        // Zoom 1 at the same resolution as zoom 0, which still spans the
        // extent but is not a pyramid.
        let flat = [
            TileMatrix::new(0, 1, 1, 256, 256, 1.0, 1.0),
            TileMatrix::new(1, 1, 1, 256, 256, 1.0, 1.0),
        ];
        assert!(matches!(
            set.validate(&flat),
            Err(TileError::PixelSizeNotDescending { zoom_level: 1, .. })
        ));
    }

    #[test]
    fn duplicate_zoom_levels_are_rejected() {
        let (set, _) = square_pyramid();
        let duplicated = [
            TileMatrix::new(0, 1, 1, 256, 256, 1.0, 1.0),
            TileMatrix::new(0, 1, 1, 256, 256, 1.0, 1.0),
        ];
        assert!(matches!(
            set.validate(&duplicated),
            Err(TileError::DuplicateZoomLevel { zoom_level: 0 })
        ));
    }

    #[test]
    fn tile_table_ddl() {
        assert_eq!(
            create_tile_table_sql("basemap").unwrap(),
            "CREATE TABLE \"basemap\" (\
             id INTEGER PRIMARY KEY AUTOINCREMENT, \
             zoom_level INTEGER NOT NULL, \
             tile_column INTEGER NOT NULL, \
             tile_row INTEGER NOT NULL, \
             tile_data BLOB NOT NULL, \
             UNIQUE (zoom_level, tile_column, tile_row))"
        );
    }

    #[test]
    fn quotes_awkward_identifiers() {
        assert!(
            create_tile_table_sql("we\"ird")
                .unwrap()
                .starts_with("CREATE TABLE \"we\"\"ird\" (")
        );
        create_tile_table_sql("").unwrap_err();
    }
}
