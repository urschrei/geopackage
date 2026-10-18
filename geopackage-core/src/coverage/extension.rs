//! The names and tables that the coverage extension registers.

/// The registered extension name for tiled gridded coverages (OGC 17-066r2).
pub const COVERAGE_EXTENSION_NAME: &str = "gpkg_2d_gridded_coverage";
/// The `gpkg_extensions.definition` value for [`COVERAGE_EXTENSION_NAME`].
///
/// This is the r1 URL. The Extension Table Record in the r2 spec source gives
/// this URL, and GDAL writes it. As with all normative text in this workspace,
/// the value is copied, not corrected.
pub const COVERAGE_EXTENSION_DEFINITION: &str =
    "http://docs.opengeospatial.org/is/17-066r1/17-066r1.html";
/// The `gpkg_extensions.scope` value for the three rows that a coverage
/// registers.
pub const COVERAGE_EXTENSION_SCOPE: &str = "read-write";
/// The `gpkg_contents.data_type` of a tiled gridded coverage (Requirement 5).
///
/// The value is not `tiles`, because a coverage is a different kind of content
/// from a tile pyramid.
pub const COVERAGE_DATA_TYPE: &str = "2d-gridded-coverage";
/// The per-coverage ancillary table (Requirement 1).
pub const COVERAGE_ANCILLARY_TABLE: &str = "gpkg_2d_gridded_coverage_ancillary";
/// The per-tile ancillary table (Requirement 2).
pub const TILE_ANCILLARY_TABLE: &str = "gpkg_2d_gridded_tile_ancillary";
