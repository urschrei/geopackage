//! Tile pyramids: the user data table template (spec clause 2.2.8, Annex C),
//! the tile matrix model, and the consistency rules a conforming pyramid
//! satisfies.
//!
//! A tile pyramid is a `gpkg_contents` row with `data_type = 'tiles'`, one
//! `gpkg_tile_matrix_set` row fixing the pyramid's bounding box and spatial
//! reference system, one `gpkg_tile_matrix` row per zoom level, and a user
//! table containing the tiles themselves. The table definition SQL for the two
//! catalogue tables is in [`crate::ddl`]; the user table is per-pyramid, so it
//! is built here.
//!
//! [`TileMatrixSet`] and [`TileMatrix`] hold no table name: they describe the
//! shape of a pyramid, and the container decides which table it belongs to. [`TileMatrixSet::validate`] checks a pyramid against Requirements
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

/// Half the side of the web mercator quad in metres: the extent every XYZ
/// basemap is tiled over, and the `EPSG:3857` extent
/// [`TileMatrixSet::web_mercator_quad`] builds.
pub const WEB_MERCATOR_HALF_SPAN: f64 = 20_037_508.342_789_244;

/// The tile payload column of a tile pyramid user data table.
///
/// Fixed by the spec, and the `gpkg_extensions.column_name` value the tile
/// extensions (`gpkg_webp`, `gpkg_zoom_other`) register against.
pub const TILE_DATA_COLUMN: &str = "tile_data";

/// Registered extension name for zoom levels that do not step by factors of
/// two (Annex F.6).
pub const ZOOM_OTHER_EXTENSION_NAME: &str = "gpkg_zoom_other";
/// `gpkg_extensions.definition` value for [`ZOOM_OTHER_EXTENSION_NAME`].
pub const ZOOM_OTHER_EXTENSION_DEFINITION: &str =
    "http://www.geopackage.org/spec140/#extension_zoom_other_intervals";
/// Registered extension name for WebP tile payloads (Annex F.7).
pub const WEBP_EXTENSION_NAME: &str = "gpkg_webp";
/// `gpkg_extensions.definition` value for [`WEBP_EXTENSION_NAME`].
pub const WEBP_EXTENSION_DEFINITION: &str =
    "http://www.geopackage.org/spec140/#extension_tiles_webp";
/// `gpkg_extensions.scope` value both tile extensions use.
pub const TILE_EXTENSION_SCOPE: &str = "read-write";

/// Returns `true` if these zoom levels form the spec's default ladder, each
/// level's pixel size exactly half the level below it.
///
/// A pyramid that is not one is legal, but only with `gpkg_zoom_other`
/// registered against its table. Compared with the same relative tolerance
/// Requirement 45 uses, since a halved pixel size is usually a derived value
/// rather than a written-down one. A pyramid of fewer than two levels has no
/// interval to be other than a factor of two, so it counts as one.
pub fn is_power_of_two_ladder(matrices: &[TileMatrix]) -> bool {
    let mut sorted: Vec<&TileMatrix> = matrices.iter().collect();
    sorted.sort_unstable_by_key(|matrix| matrix.zoom_level);
    sorted.windows(2).all(|pair| {
        let [lower, upper] = pair else {
            return true;
        };
        [
            (lower.pixel_x_size, upper.pixel_x_size),
            (lower.pixel_y_size, upper.pixel_y_size),
        ]
        .iter()
        .all(|(lower, upper)| {
            let halved = lower / 2.0;
            (halved - upper).abs() <= EXTENT_TOLERANCE * halved.abs().max(upper.abs())
        })
    })
}

/// Returns the `CREATE TABLE` statement for a tile pyramid user data table,
/// in the form
/// given by Annex C (Requirement 54): an `INTEGER PRIMARY KEY` acting as the
/// rowid alias, the zoom/column/row index, the payload, and the uniqueness
/// constraint over the three index columns.
///
/// The column names are not caller-configurable: unlike a feature table, whose
/// geometry column is named in `gpkg_geometry_columns`, a tile table's shape is
/// fixed by the spec and every reader assumes it.
///
/// # Errors
///
/// [`Error::InvalidIdentifier`] if `table` cannot be quoted.
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
    /// A tile index outside the grid its zoom level declares.
    #[error(
        "tile ({column}, {row}) is outside the {matrix_width} by {matrix_height} grid of zoom level {zoom_level}"
    )]
    CoordOutsideMatrix {
        /// Zoom level the tile was addressed at.
        zoom_level: i64,
        /// The offending column index.
        column: i64,
        /// The offending row index.
        row: i64,
        /// Columns the zoom level has.
        matrix_width: i64,
        /// Rows the zoom level has.
        matrix_height: i64,
    },
    /// An XYZ conversion requested on a pyramid whose grid is not the standard
    /// web mercator quad, where GeoPackage and XYZ indices do not coincide.
    #[error(
        "zoom level {zoom_level} is not addressed as an XYZ grid: that needs a web mercator quad extent and a 2^zoom square matrix"
    )]
    NotAnXyzGrid {
        /// Zoom level the conversion was requested for.
        zoom_level: i64,
    },
    /// A tile payload whose header could not be read.
    #[error("tile payload is not a readable image: {reason}")]
    UnreadablePayload {
        /// The header reader's error message.
        reason: String,
    },
    /// A tile payload whose pixel dimensions are not the ones its zoom level
    /// declares.
    #[error(
        "tile is {actual_width} by {actual_height} pixels, but zoom level {zoom_level} declares {expected_width} by {expected_height}"
    )]
    PayloadSizeMismatch {
        /// Zoom level the tile was written at.
        zoom_level: i64,
        /// `tile_width` of that zoom level.
        expected_width: i64,
        /// `tile_height` of that zoom level.
        expected_height: i64,
        /// Width the payload's header declares.
        actual_width: i64,
        /// Height the payload's header declares.
        actual_height: i64,
    },
    /// A zoom ladder whose range is empty, negative, or too wide for the grid
    /// to stay representable.
    #[error(
        "zoom range {min_zoom} to {max_zoom} is not usable: it must be non-negative, ascending, and narrow enough for the doubled grid to fit an i64"
    )]
    InvalidZoomRange {
        /// Lowest zoom level requested.
        min_zoom: i64,
        /// Highest zoom level requested.
        max_zoom: i64,
    },
}

/// The encoding of a tile payload, as its first bytes declare it.
///
/// The base tiles requirement class allows PNG (Requirement 36) and JPEG
/// (Requirement 37), mixed freely within one table. WebP is allowed under the
/// `gpkg_webp` extension, and TIFF only inside the tiled gridded coverage
/// extension, which this crate does not implement. Whether a given payload may
/// be written is therefore a container question, not a format one: this enum
/// only says what the bytes are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TileFormat {
    /// `image/png`.
    Png,
    /// `image/jpeg`.
    Jpeg,
    /// WebP, which needs the `gpkg_webp` extension registered.
    Webp,
    /// TIFF, which appears only in tiled gridded coverages.
    Tiff,
    /// An image this crate recognises but no tile table may hold.
    Other,
}

impl TileFormat {
    /// Whether the base tiles requirement class permits this encoding with no
    /// extension registered.
    pub fn is_core(self) -> bool {
        matches!(self, Self::Png | Self::Jpeg)
    }

    /// The MIME type the spec names for this encoding, where it names one.
    pub fn mime_type(self) -> Option<&'static str> {
        match self {
            Self::Png => Some("image/png"),
            Self::Jpeg => Some("image/jpeg"),
            Self::Webp => Some("image/webp"),
            Self::Tiff => Some("image/tiff"),
            Self::Other => None,
        }
    }
}

/// What a tile payload's header declares: its encoding and its pixel size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TilePayload {
    /// The encoding identified from the payload's magic bytes.
    pub format: TileFormat,
    /// Width in pixels.
    pub width: i64,
    /// Height in pixels.
    pub height: i64,
}

/// Reads the encoding and pixel dimensions from a tile payload's header.
///
/// Reads a header, not an image: no pixel data is decoded, nothing is copied,
/// and the slice is borrowed throughout. A tile whose dimensions disagree with
/// its zoom level's `tile_width`/`tile_height` is a fault magic-byte sniffing
/// cannot catch, which is why the size is returned with the format
/// ([`TileMatrix::check_payload`]).
///
/// # Errors
///
/// [`TileError::UnreadablePayload`] when the bytes are not a recognisable
/// image, or are truncated before the header ends.
pub fn probe(bytes: &[u8]) -> Result<TilePayload, TileError> {
    let unreadable = |error: imagesize::ImageError| TileError::UnreadablePayload {
        reason: error.to_string(),
    };
    let format = match imagesize::image_type(bytes).map_err(unreadable)? {
        imagesize::ImageType::Png => TileFormat::Png,
        imagesize::ImageType::Jpeg => TileFormat::Jpeg,
        imagesize::ImageType::Webp => TileFormat::Webp,
        imagesize::ImageType::Tiff => TileFormat::Tiff,
        _ => TileFormat::Other,
    };
    let size = imagesize::blob_size(bytes).map_err(unreadable)?;
    let (Ok(width), Ok(height)) = (i64::try_from(size.width), i64::try_from(size.height)) else {
        return Err(TileError::UnreadablePayload {
            reason: format!("header declares {} by {} pixels", size.width, size.height),
        });
    };
    Ok(TilePayload {
        format,
        width,
        height,
    })
}

/// A tile's address within a pyramid.
///
/// Rows count from the **top** of the tile matrix set downwards: `(0, 0)` is
/// the tile whose upper-left corner is the matrix set's `(min_x, max_y)`. That
/// is the WMTS and XYZ sense, and the opposite of TMS, which counts rows up
/// from the bottom. [`TileMatrix::flip_row`] converts between the two.
///
/// The indices are relative to the tile matrix set's own extent, not to a
/// global grid, so they equal XYZ indices only for a pyramid over the standard
/// web mercator quad. [`TileMatrixSet::xyz_to_tile`] checks that rather than
/// assuming it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileCoord {
    /// Zoom level, matching a `gpkg_tile_matrix` row.
    pub zoom_level: i64,
    /// Column index, counting east from `min_x`.
    pub column: i64,
    /// Row index, counting south from `max_y`.
    pub row: i64,
}

impl TileCoord {
    /// Creates a tile address from its zoom level, column and row.
    pub fn new(zoom_level: i64, column: i64, row: i64) -> Self {
        Self {
            zoom_level,
            column,
            row,
        }
    }
}

/// The ground extent of a single tile.
///
/// Named fields rather than an array, because this crate's other envelope
/// convention ([`crate::gpb::Envelope`]) orders its four values differently and
/// a bare `[f64; 4]` would invite the two to be crossed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileBounds {
    /// Minimum x (west edge).
    pub min_x: f64,
    /// Minimum y (south edge).
    pub min_y: f64,
    /// Maximum x (east edge).
    pub max_x: f64,
    /// Maximum y (north edge).
    pub max_y: f64,
}

/// A rectangle of tile indices within one zoom level, inclusive at both ends.
///
/// The container turns a bounding-box query into one range predicate over
/// this rather than a tile-by-tile lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileRange {
    /// Westmost column.
    pub min_column: i64,
    /// Eastmost column.
    pub max_column: i64,
    /// Northmost row.
    pub min_row: i64,
    /// Southmost row.
    pub max_row: i64,
}

/// The shape of a power-of-two zoom ladder, which is the spec's default
/// arrangement: each level doubles the grid of the one below it.
///
/// The grid at the lowest zoom level defaults to a single tile, the web
/// mercator convention. A geographic (EPSG:4326) pyramid conventionally starts
/// two tiles wide and one tall, which is [`Self::base_grid`]. Tiles default to
/// 256 pixels square.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoomLadder {
    min_zoom: i64,
    max_zoom: i64,
    base_matrix_width: i64,
    base_matrix_height: i64,
    tile_width: i64,
    tile_height: i64,
}

impl ZoomLadder {
    /// Creates a ladder over the inclusive zoom range, one 256-pixel tile at
    /// `min_zoom`.
    pub fn new(min_zoom: i64, max_zoom: i64) -> Self {
        Self {
            min_zoom,
            max_zoom,
            base_matrix_width: 1,
            base_matrix_height: 1,
            tile_width: 256,
            tile_height: 256,
        }
    }

    /// Sets the grid at `min_zoom`, in tiles (default `1` by `1`).
    #[must_use]
    pub fn base_grid(mut self, columns: i64, rows: i64) -> Self {
        self.base_matrix_width = columns;
        self.base_matrix_height = rows;
        self
    }

    /// Sets the tile size in pixels (default `256` by `256`).
    #[must_use]
    pub fn tile_size(mut self, width: i64, height: i64) -> Self {
        self.tile_width = width;
        self.tile_height = height;
        self
    }
}

impl TileMatrixSet {
    /// Creates a matrix set from its spatial reference system and its four
    /// edges.
    pub fn new(srs_id: i32, min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Self {
        Self {
            srs_id,
            min_x,
            min_y,
            max_x,
            max_y,
        }
    }

    /// Returns the width of the extent in ground units.
    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }

    /// Returns the height of the extent in ground units.
    pub fn height(&self) -> f64 {
        self.max_y - self.min_y
    }

    /// Checks a whole pyramid: the extent, then every tile matrix against
    /// Requirements 45 to 53, in ascending zoom order.
    ///
    /// `matrices` may be in any order and may be empty: a pyramid with no zoom
    /// levels declared breaks no rule, since the spec requires a
    /// `gpkg_tile_matrix` row only for a level that contains tiles.
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

    /// Returns the ground extent of one tile.
    ///
    /// Not bounds-checked: a column or row outside the grid still has a
    /// calculable box, and a caller walking outwards from a tile may need it.
    /// [`TileMatrix::check_contains`] is the check.
    pub fn tile_bounds(&self, matrix: &TileMatrix, column: i64, row: i64) -> TileBounds {
        let span_x = matrix.tile_span_x();
        let span_y = matrix.tile_span_y();
        let min_x = self.min_x + column as f64 * span_x;
        // Rows count south from the top edge, so the row index is subtracted
        // from max_y rather than added to min_y.
        let max_y = self.max_y - row as f64 * span_y;
        TileBounds {
            min_x,
            min_y: max_y - span_y,
            max_x: min_x + span_x,
            max_y,
        }
    }

    /// Returns the tile containing a position, or `None` for a position
    /// outside the extent.
    ///
    /// Inclusive at every edge: a position on `max_x` belongs to the last
    /// column rather than to one past it, and a position on `min_y` to the last
    /// row.
    pub fn tile_at(&self, matrix: &TileMatrix, x: f64, y: f64) -> Option<(i64, i64)> {
        if x < self.min_x || x > self.max_x || y < self.min_y || y > self.max_y {
            return None;
        }
        let column = ((x - self.min_x) / matrix.tile_span_x()).floor() as i64;
        let row = ((self.max_y - y) / matrix.tile_span_y()).floor() as i64;
        Some((
            column.clamp(0, matrix.matrix_width.saturating_sub(1)),
            row.clamp(0, matrix.matrix_height.saturating_sub(1)),
        ))
    }

    /// Returns the tiles a bounding box touches at one zoom level, clamped to
    /// the grid, or `None` when the box misses the extent entirely.
    ///
    /// Inclusive at the boundary, as [`Self::tile_at`] is: a box touching an
    /// edge selects the tile it touches.
    pub fn tile_range(
        &self,
        matrix: &TileMatrix,
        min_x: f64,
        min_y: f64,
        max_x: f64,
        max_y: f64,
    ) -> Option<TileRange> {
        if min_x > max_x || min_y > max_y {
            return None;
        }
        if max_x < self.min_x || min_x > self.max_x || max_y < self.min_y || min_y > self.max_y {
            return None;
        }
        // Clamping into the extent first means the divisions below cannot run
        // away on a box that covers the world.
        let (west, east) = (min_x.max(self.min_x), max_x.min(self.max_x));
        let (south, north) = (min_y.max(self.min_y), max_y.min(self.max_y));
        // North-west corner gives the low indices on both axes, because rows
        // count southwards; south-east gives the high ones.
        let (min_column, min_row) = self.tile_at(matrix, west, north)?;
        let (max_column, max_row) = self.tile_at(matrix, east, south)?;
        Some(TileRange {
            min_column,
            max_column,
            min_row,
            max_row,
        })
    }

    /// Returns the tile matrices of a power-of-two ladder over this extent.
    ///
    /// Pixel sizes are derived from the extent rather than supplied, so
    /// Requirement 45 holds by construction: each level's grid spans the matrix
    /// set exactly.
    ///
    /// # Errors
    ///
    /// [`TileError::InvalidExtent`] for an extent no tile can be measured
    /// against, [`TileError::NonPositiveDimension`] for a non-positive base
    /// grid or tile size, and [`TileError::InvalidZoomRange`] for a range that
    /// is empty, negative, or wide enough to overflow the grid.
    pub fn ladder(&self, ladder: ZoomLadder) -> Result<Vec<TileMatrix>, TileError> {
        self.validate_extent()?;
        if ladder.min_zoom < 0 || ladder.max_zoom < ladder.min_zoom {
            return Err(TileError::InvalidZoomRange {
                min_zoom: ladder.min_zoom,
                max_zoom: ladder.max_zoom,
            });
        }
        for (field, value) in [
            ("matrix_width", ladder.base_matrix_width),
            ("matrix_height", ladder.base_matrix_height),
            ("tile_width", ladder.tile_width),
            ("tile_height", ladder.tile_height),
        ] {
            if value <= 0 {
                return Err(TileError::NonPositiveDimension {
                    zoom_level: ladder.min_zoom,
                    field,
                    value,
                });
            }
        }

        let levels = usize::try_from(ladder.max_zoom - ladder.min_zoom + 1).unwrap_or(0);
        let mut matrices = Vec::with_capacity(levels);
        let (mut matrix_width, mut matrix_height) =
            (ladder.base_matrix_width, ladder.base_matrix_height);
        for zoom_level in ladder.min_zoom..=ladder.max_zoom {
            matrices.push(TileMatrix {
                zoom_level,
                matrix_width,
                matrix_height,
                tile_width: ladder.tile_width,
                tile_height: ladder.tile_height,
                pixel_x_size: self.width() / (matrix_width as f64 * ladder.tile_width as f64),
                pixel_y_size: self.height() / (matrix_height as f64 * ladder.tile_height as f64),
            });
            let overflow = || TileError::InvalidZoomRange {
                min_zoom: ladder.min_zoom,
                max_zoom: ladder.max_zoom,
            };
            matrix_width = matrix_width.checked_mul(2).ok_or_else(overflow)?;
            matrix_height = matrix_height.checked_mul(2).ok_or_else(overflow)?;
        }
        Ok(matrices)
    }

    /// Returns the extent of the standard web mercator quad (EPSG:3857), the
    /// tiling scheme every XYZ basemap uses.
    pub fn web_mercator_quad() -> Self {
        Self::new(
            3857,
            -WEB_MERCATOR_HALF_SPAN,
            -WEB_MERCATOR_HALF_SPAN,
            WEB_MERCATOR_HALF_SPAN,
            WEB_MERCATOR_HALF_SPAN,
        )
    }

    /// Returns `true` if this extent is the standard web mercator quad.
    ///
    /// Compares `srs_id` against 3857 literally. A file that registers web
    /// mercator under some other `srs_id`, which the format allows, returns
    /// `false`; the alternative would be resolving CRS definitions, which this
    /// crate does not do.
    pub fn is_web_mercator_quad(&self) -> bool {
        self.srs_id == 3857
            && [
                (self.min_x, -WEB_MERCATOR_HALF_SPAN),
                (self.min_y, -WEB_MERCATOR_HALF_SPAN),
                (self.max_x, WEB_MERCATOR_HALF_SPAN),
                (self.max_y, WEB_MERCATOR_HALF_SPAN),
            ]
            .iter()
            .all(|(actual, expected)| {
                (actual - expected).abs() <= EXTENT_TOLERANCE * expected.abs()
            })
    }

    /// Returns `true` if tiles at this zoom level have the same indices an
    /// XYZ service would give them: the standard web mercator quad, tiled as a
    /// `2^zoom` square.
    ///
    /// Tile pixel dimensions play no part: they set resolution, not addressing,
    /// so a 512-pixel quad grid is still XYZ-addressed.
    pub fn matches_xyz_grid(&self, matrix: &TileMatrix) -> bool {
        self.is_web_mercator_quad() && matrix.is_quad_grid()
    }

    /// Reads an XYZ `z/x/y` address as a tile of this pyramid.
    ///
    /// # Errors
    ///
    /// [`TileError::NotAnXyzGrid`] when this pyramid does not address tiles the
    /// way XYZ does, rather than returning indices that silently point
    /// somewhere else, and [`TileError::CoordOutsideMatrix`] for an address
    /// outside the grid.
    pub fn xyz_to_tile(
        &self,
        matrix: &TileMatrix,
        z: i64,
        x: i64,
        y: i64,
    ) -> Result<TileCoord, TileError> {
        if matrix.zoom_level != z || !self.matches_xyz_grid(matrix) {
            return Err(TileError::NotAnXyzGrid { zoom_level: z });
        }
        matrix.check_contains(x, y)?;
        Ok(TileCoord::new(z, x, y))
    }

    /// Returns the XYZ `z/x/y` address of one of this pyramid's tiles.
    ///
    /// # Errors
    ///
    /// As [`Self::xyz_to_tile`].
    pub fn tile_to_xyz(
        &self,
        matrix: &TileMatrix,
        coord: TileCoord,
    ) -> Result<(i64, i64, i64), TileError> {
        if matrix.zoom_level != coord.zoom_level || !self.matches_xyz_grid(matrix) {
            return Err(TileError::NotAnXyzGrid {
                zoom_level: coord.zoom_level,
            });
        }
        matrix.check_contains(coord.column, coord.row)?;
        Ok((coord.zoom_level, coord.column, coord.row))
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
    /// Creates a tile matrix from its zoom level, grid size, tile size and
    /// pixel sizes.
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

    /// Returns the ground units one tile spans in x.
    pub fn tile_span_x(&self) -> f64 {
        self.tile_width as f64 * self.pixel_x_size
    }

    /// Returns the ground units one tile spans in y.
    pub fn tile_span_y(&self) -> f64 {
        self.tile_height as f64 * self.pixel_y_size
    }

    /// Returns `true` if a column and row fall inside this zoom level's grid.
    pub fn contains(&self, column: i64, row: i64) -> bool {
        (0..self.matrix_width).contains(&column) && (0..self.matrix_height).contains(&row)
    }

    /// [`Self::contains`] as a typed check.
    ///
    /// # Errors
    ///
    /// [`TileError::CoordOutsideMatrix`] for an index outside the grid.
    pub fn check_contains(&self, column: i64, row: i64) -> Result<(), TileError> {
        if self.contains(column, row) {
            Ok(())
        } else {
            Err(TileError::CoordOutsideMatrix {
                zoom_level: self.zoom_level,
                column,
                row,
                matrix_width: self.matrix_width,
                matrix_height: self.matrix_height,
            })
        }
    }

    /// Checks a payload's pixel dimensions against the tile size this zoom
    /// level declares.
    ///
    /// # Errors
    ///
    /// [`TileError::PayloadSizeMismatch`] when they disagree.
    pub fn check_payload(&self, payload: &TilePayload) -> Result<(), TileError> {
        if payload.width == self.tile_width && payload.height == self.tile_height {
            Ok(())
        } else {
            Err(TileError::PayloadSizeMismatch {
                zoom_level: self.zoom_level,
                expected_width: self.tile_width,
                expected_height: self.tile_height,
                actual_width: payload.width,
                actual_height: payload.height,
            })
        }
    }

    /// Converts a row index between the GeoPackage sense, counting south from
    /// the top, and the TMS sense, counting north from the bottom.
    ///
    /// Its own inverse, so one function covers both directions.
    pub fn flip_row(&self, row: i64) -> i64 {
        self.matrix_height - 1 - row
    }

    /// Returns `true` if this zoom level tiles its extent as a `2^zoom`
    /// square, the grid an XYZ service assumes.
    fn is_quad_grid(&self) -> bool {
        u32::try_from(self.zoom_level)
            .ok()
            .filter(|zoom| *zoom < 63)
            .and_then(|zoom| 1_i64.checked_shl(zoom))
            .is_some_and(|side| self.matrix_width == side && self.matrix_height == side)
    }

    /// Returns the ground units this zoom level's tile grid spans in x.
    pub fn span_x(&self) -> f64 {
        self.matrix_width as f64 * self.tile_width as f64 * self.pixel_x_size
    }

    /// Returns the ground units this zoom level's tile grid spans in y.
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

    /// A PNG header: signature and IHDR, with a zeroed CRC, which a header
    /// probe does not check.
    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&13_u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        // Bit depth, colour type, compression, filter, interlace, then CRC.
        bytes.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        bytes
    }

    /// A JPEG start-of-image followed by a baseline SOF0 frame header.
    fn jpeg_bytes(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08];
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&[0x03, 0x01, 0x11, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01]);
        bytes
    }

    /// A WebP container in the extended (`VP8X`) form, whose dimensions are
    /// stored as 24-bit little-endian values one less than the real size.
    fn webp_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::from(*b"RIFF");
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(b"WEBP");
        bytes.extend_from_slice(b"VP8X");
        bytes.extend_from_slice(&10_u32.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
        bytes.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
        bytes
    }

    #[test]
    fn probe_reads_format_and_size() {
        assert_eq!(
            probe(&png_bytes(256, 256)).unwrap(),
            TilePayload {
                format: TileFormat::Png,
                width: 256,
                height: 256,
            }
        );
        assert_eq!(
            probe(&jpeg_bytes(512, 128)).unwrap(),
            TilePayload {
                format: TileFormat::Jpeg,
                width: 512,
                height: 128,
            }
        );
        assert_eq!(
            probe(&webp_bytes(256, 256)).unwrap(),
            TilePayload {
                format: TileFormat::Webp,
                width: 256,
                height: 256,
            }
        );
    }

    #[test]
    fn probe_rejects_what_is_not_a_readable_image() {
        let png = png_bytes(256, 256);
        for bytes in [b"not an image at all".as_slice(), &[], &png[..12]] {
            assert!(matches!(
                probe(bytes),
                Err(TileError::UnreadablePayload { .. })
            ));
        }
    }

    #[test]
    fn payload_size_is_checked_against_its_zoom_level() {
        let (_, matrices) = square_pyramid();
        let zoom0 = matrices[0];
        zoom0
            .check_payload(&probe(&png_bytes(256, 256)).unwrap())
            .unwrap();
        assert!(matches!(
            zoom0.check_payload(&probe(&png_bytes(512, 256)).unwrap()),
            Err(TileError::PayloadSizeMismatch {
                expected_width: 256,
                actual_width: 512,
                ..
            })
        ));
    }

    #[test]
    fn only_png_and_jpeg_need_no_extension() {
        assert!(TileFormat::Png.is_core());
        assert!(TileFormat::Jpeg.is_core());
        assert!(!TileFormat::Webp.is_core());
        assert!(!TileFormat::Tiff.is_core());
        assert_eq!(TileFormat::Png.mime_type(), Some("image/png"));
        assert_eq!(TileFormat::Other.mime_type(), None);
    }

    #[test]
    fn tile_bounds_count_rows_from_the_top() {
        let (set, matrices) = square_pyramid();
        let zoom1 = matrices[1];
        assert_eq!(
            set.tile_bounds(&zoom1, 0, 0),
            TileBounds {
                min_x: 0.0,
                min_y: 128.0,
                max_x: 128.0,
                max_y: 256.0,
            },
            "row 0 is the northern row"
        );
        assert_eq!(
            set.tile_bounds(&zoom1, 1, 1),
            TileBounds {
                min_x: 128.0,
                min_y: 0.0,
                max_x: 256.0,
                max_y: 128.0,
            }
        );
    }

    #[test]
    fn tile_at_is_inclusive_at_the_edges() {
        let (set, matrices) = square_pyramid();
        let zoom1 = matrices[1];
        assert_eq!(set.tile_at(&zoom1, 0.0, 256.0), Some((0, 0)));
        assert_eq!(
            set.tile_at(&zoom1, 256.0, 0.0),
            Some((1, 1)),
            "the far corner belongs to the last tile, not to one past it"
        );
        assert_eq!(set.tile_at(&zoom1, 200.0, 200.0), Some((1, 0)));
        assert_eq!(set.tile_at(&zoom1, -0.5, 100.0), None);
    }

    #[test]
    fn tile_range_clamps_to_the_grid() {
        let (set, matrices) = square_pyramid();
        let zoom1 = matrices[1];
        assert_eq!(
            set.tile_range(&zoom1, -1000.0, -1000.0, 1000.0, 1000.0),
            Some(TileRange {
                min_column: 0,
                max_column: 1,
                min_row: 0,
                max_row: 1,
            })
        );
        assert_eq!(
            set.tile_range(&zoom1, 1.0, 200.0, 2.0, 210.0),
            Some(TileRange {
                min_column: 0,
                max_column: 0,
                min_row: 0,
                max_row: 0,
            })
        );
        assert_eq!(set.tile_range(&zoom1, 300.0, 300.0, 400.0, 400.0), None);
        assert_eq!(set.tile_range(&zoom1, 100.0, 100.0, 0.0, 0.0), None);
    }

    #[test]
    fn flip_row_converts_tms_both_ways() {
        let (_, matrices) = square_pyramid();
        let zoom1 = matrices[1];
        assert_eq!(zoom1.flip_row(0), 1);
        assert_eq!(zoom1.flip_row(zoom1.flip_row(0)), 0);
    }

    #[test]
    fn xyz_indices_hold_only_on_the_quad() {
        let quad = TileMatrixSet::web_mercator_quad();
        let matrices = quad.ladder(ZoomLadder::new(0, 2)).unwrap();
        let zoom2 = matrices[2];
        assert_eq!(
            quad.xyz_to_tile(&zoom2, 2, 3, 1).unwrap(),
            TileCoord::new(2, 3, 1)
        );
        assert_eq!(
            quad.tile_to_xyz(&zoom2, TileCoord::new(2, 3, 1)).unwrap(),
            (2, 3, 1)
        );
        assert!(matches!(
            quad.xyz_to_tile(&zoom2, 2, 4, 0),
            Err(TileError::CoordOutsideMatrix { .. })
        ));

        // A geographic pyramid is two tiles wide at zoom 0, so its indices are
        // its own and the conversion errors rather than mis-addressing.
        let geographic = TileMatrixSet::new(4326, -180.0, -90.0, 180.0, 90.0);
        let geographic_matrices = geographic
            .ladder(ZoomLadder::new(0, 1).base_grid(2, 1))
            .unwrap();
        assert!(matches!(
            geographic.xyz_to_tile(&geographic_matrices[0], 0, 0, 0),
            Err(TileError::NotAnXyzGrid { .. })
        ));
    }

    #[test]
    fn ladder_levels_span_the_extent() {
        let quad = TileMatrixSet::web_mercator_quad();
        let matrices = quad.ladder(ZoomLadder::new(0, 5)).unwrap();
        assert_eq!(matrices.len(), 6);
        assert_eq!(matrices[5].matrix_width, 32);
        assert_eq!(matrices[5].matrix_height, 32);
        assert_eq!(matrices[5].tile_width, 256);
        quad.validate(&matrices).unwrap();
    }

    #[test]
    fn ladder_rejects_unusable_shapes() {
        let quad = TileMatrixSet::web_mercator_quad();
        for ladder in [
            ZoomLadder::new(3, 1),
            ZoomLadder::new(-1, 1),
            ZoomLadder::new(0, 70),
        ] {
            assert!(matches!(
                quad.ladder(ladder),
                Err(TileError::InvalidZoomRange { .. })
            ));
        }
        assert!(matches!(
            quad.ladder(ZoomLadder::new(0, 1).tile_size(0, 256)),
            Err(TileError::NonPositiveDimension { .. })
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
