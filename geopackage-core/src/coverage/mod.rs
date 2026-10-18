//! The payload profile of the tiled gridded coverage extension (OGC 17-066r2,
//! version 1.1).
//!
//! A coverage tile is **not a GeoTIFF**. The georeferencing is in
//! `gpkg_tile_matrix_set`, and the extension does not use GeoKeys,
//! `ModelPixelScale` or `ModelTiepoint`. The extension specifies a small
//! profile of baseline TIFF, in which each constraint is a tag value:
//!
//! | Requirement | Constraint |
//! |---|---|
//! | 15 | Conforms to the TIFF specification, so not BigTIFF |
//! | 16 | `SamplesPerPixel` = 1, "a single sample per grid cell" |
//! | 17 | 32-bit float, or `SampleFormat` 1 or 2 with `BitsPerSample` 8, 16 or 32 |
//! | 18 | LZW compression *MAY* be used |
//! | 19 | One image per file: "Multiple image files are not allowed" |
//! | 20 | *SHALL NOT* contain internal tiles |
//! | 21 | Every pixel valid; no NaN, no Inf |
//!
//! The header checks therefore need no image codec. [`coverage_tiff`] checks
//! Requirements 15 to 20 from the first IFD, the same header that
//! [`crate::tiles::probe`] reads, and does not decode samples.
//!
//! An integer coverage can use PNG instead (Requirement 13): 16-bit unsigned,
//! single channel. [`coverage_png`] reads the `IHDR` chunk and nothing more.
//! [`coverage_payload`] accepts either encoding, and
//! [`CoverageDatatype::check_payload`] compares a payload with the `datatype`
//! of its coverage.
//!
//! **The header checks do not check Requirement 21.** "All pixels in a tile of
//! coverage data _SHALL_ be set with a valid component value … Special
//! floating point values such as NaN and Inf SHALL NOT be used" is a statement
//! about samples, and checking samples needs a decoder. A payload that the
//! header checks accept satisfies the structural requirements only.
//!
//! # Samples
//!
//! The `coverage-samples` feature adds the decode, with the `tiff` and `png`
//! crates: `decode_samples` returns a tile's `TileSamples`, which check
//! Requirement 21 and calculate the tile's statistics, and `SampleLookup`
//! finds the samples for the value at a position. Image tiles stay opaque:
//! this crate never decodes a PNG, JPEG or WebP map tile, whose pixels the
//! spec does not constrain.
//!
//! [`CoverageAncillary`] and [`TileAncillary`] are the rows that state what the
//! samples mean, and convert a stored sample to a value, with or without the
//! feature.
//!
//! The requirement numbers are those of the migrated spec source,
//! [`spec/2d-gridded-coverage/requirements`](https://github.com/opengeospatial/geopackage/tree/master/spec/2d-gridded-coverage/requirements).
//!
//! # Untrusted payloads
//!
//! This crate reads `tile_data` BLOBs that it did not write, and no schema
//! constrains them, so the walk of the first IFD is bounded: one pass, no
//! recursion, no allocation from a count in the file, and no value offset
//! followed. A tag whose value does not fit in its entry is not read. This does
//! not change any result, because each tag that the profile constrains is a
//! single number.

mod ancillary;
mod extension;
mod payload;
mod png;
#[cfg(feature = "coverage-samples")]
mod samples;
mod tiff;
mod tiff_tag;

pub use ancillary::{CoverageAncillary, TileAncillary};
pub use extension::{
    COVERAGE_ANCILLARY_TABLE, COVERAGE_DATA_TYPE, COVERAGE_EXTENSION_DEFINITION,
    COVERAGE_EXTENSION_NAME, COVERAGE_EXTENSION_SCOPE, TILE_ANCILLARY_TABLE,
};
pub use payload::{CoverageDatatype, CoveragePayload, coverage_payload};
pub use png::{CoveragePng, coverage_png};
#[cfg(feature = "coverage-samples")]
pub use samples::{
    DEFAULT_MAX_DECODED_SAMPLES, Interpolation, SampleLookup, SampleStatistics, TileSamples,
    WeightedSample, decode_payload, decode_samples,
};
pub use tiff::{CoverageTiff, SampleType, TiffCompression, coverage_tiff};

pub(crate) use tiff::dimensions;
