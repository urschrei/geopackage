//! Decoded coverage samples, behind the `coverage-samples` feature.
//!
//! Without the feature this crate reads a coverage as bytes and leaves the
//! decode to you. With it, the crate decodes the two encodings the extension
//! permits (a TIFF of the profile in [`geopackage_core::coverage`], or a
//! 16-bit greyscale PNG), and three things that need samples become possible:
//!
//! - **Values at a position**: [`Coverage::value_at`], and
//!   [`CoverageSampler`] for many positions, which keeps decoded tiles between
//!   lookups.
//! - **Per-tile statistics**: [`Coverage::statistics`], and
//!   [`CoverageWriter::put_with_statistics`], which records them on write.
//! - **Requirement 21** (no NaN or infinite samples): [`Coverage::check_samples`],
//!   the writer above, and [`GeoPackage::validate`](crate::GeoPackage::validate).
//!
//! The decode, the statistics and the arithmetic of a lookup are format rules,
//! and are in [`geopackage_core::coverage`], whose feature of the same name
//! this one turns on. This module reads the tiles they need.

use std::collections::VecDeque;

use geopackage_core::TileError;
use geopackage_core::coverage::{
    Interpolation, SampleLookup, SampleStatistics, TileSamples, WeightedSample, decode_payload,
    decode_samples,
};
use geopackage_core::tiles::{TileCoord, TileMatrix};

use super::{Coverage, CoverageWriter, TileAncillary};
use crate::{Error, Result};

impl<'a> Coverage<'a> {
    /// Decodes a payload to its stored samples.
    ///
    /// Checks the payload first, as [`Coverage::check_payload`] does, so that
    /// a payload this coverage does not permit is not decoded.
    ///
    /// # Errors
    ///
    /// Those of [`Coverage::check_payload`], and [`Error::Tile`] with:
    ///
    /// - [`TileError::UnreadablePayload`] for a payload whose header is
    ///   conformant but whose data cannot be decoded.
    /// - [`TileError::PayloadTooLarge`] for a payload whose header declares
    ///   more than
    ///   [`MAX_DECODED_SAMPLES`](geopackage_core::coverage::MAX_DECODED_SAMPLES)
    ///   samples, which is not decoded.
    pub fn decode(&self, bytes: &[u8]) -> Result<TileSamples> {
        let payload = self.check_payload(bytes)?;
        Ok(decode_payload(bytes, &payload)?)
    }

    /// Reads and decodes one tile, or returns `None` if the coverage has no
    /// tile there.
    ///
    /// Does not compare the size of the tile with the `tile_width` and
    /// `tile_height` of its zoom level, because a read does not reject a file.
    /// [`CoverageSampler`] does, because it addresses samples through the
    /// grid.
    ///
    /// # Errors
    ///
    /// As [`Coverage::decode`], and [`Error`] if the row cannot be read.
    pub fn samples(&self, coord: TileCoord) -> Result<Option<TileSamples>> {
        self.get_tile(coord)?
            .map(|bytes| self.decode(&bytes))
            .transpose()
    }

    /// Checks Requirement 21: "Special floating point values such as NaN and
    /// Inf SHALL NOT be used".
    ///
    /// Integer samples cannot break the requirement, so only a `float`
    /// coverage can fail it. A missing value is `data_null`, not NaN.
    ///
    /// # Errors
    ///
    /// [`Error::Tile`] with
    /// [`TileError::CoverageProfileViolation`] for Requirement 21, naming the
    /// first sample that breaks it.
    pub fn check_samples(&self, samples: &TileSamples) -> Result<()> {
        Ok(samples.check_finite()?)
    }

    /// Calculates the statistics of a tile's values, or returns `None` if
    /// every sample is `data_null` or not finite.
    ///
    /// `tile` is the tile's ancillary row, whose scale and offset apply to its
    /// samples. [`SampleStatistics`] describes what the statistics include.
    pub fn statistics(
        &self,
        samples: &TileSamples,
        tile: &TileAncillary,
    ) -> Option<SampleStatistics> {
        samples.statistics(&self.ancillary, tile)
    }

    /// Returns the value at a position in the coverage's spatial reference
    /// system, from the samples of one zoom level, or `None` where there is no
    /// value.
    ///
    /// There is no value outside the coverage's extent, in a tile the
    /// coverage does not store, or where a sample the lookup uses is
    /// `data_null`, NaN or infinite. For [`Interpolation::Bilinear`] that is
    /// any of the four samples with a weight above zero, so a position on a
    /// sample needs only that sample.
    ///
    /// Each call decodes the tiles it reads. For more than one position, use
    /// [`Coverage::sampler`], which keeps them.
    ///
    /// # Errors
    ///
    /// As [`Coverage::sampler`] and [`CoverageSampler::value_at`].
    pub fn value_at(
        &self,
        zoom_level: i64,
        x: f64,
        y: f64,
        interpolation: Interpolation,
    ) -> Result<Option<f64>> {
        self.sampler(zoom_level)?.value_at(x, y, interpolation)
    }

    /// Opens a [`CoverageSampler`] over one zoom level, for many lookups.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownZoomLevel`] if the coverage declares no such level.
    pub fn sampler(&self, zoom_level: i64) -> Result<CoverageSampler<'_>> {
        let matrix = *self
            .matrix(zoom_level)
            .ok_or_else(|| Error::UnknownZoomLevel {
                table_name: self.table_name().to_owned(),
                zoom_level,
            })?;
        Ok(CoverageSampler {
            coverage: self,
            matrix,
            cache: VecDeque::with_capacity(SAMPLER_TILES),
        })
    }
}

/// How many decoded tiles a [`CoverageSampler`] keeps. Four is the most one
/// bilinear lookup reads; the rest serve positions that move across a tile
/// edge and back. A 256-pixel float tile is half a megabyte decoded.
const SAMPLER_TILES: usize = 16;

/// Value lookups over one zoom level of a coverage, keeping the tiles it has
/// decoded.
///
/// You get one from [`Coverage::sampler`]. Lookups that fall in the same
/// tiles decode them once. The sampler keeps the 16 tiles it read most
/// recently, and remembers a tile the coverage does not store as well as one
/// it does.
pub struct CoverageSampler<'c> {
    coverage: &'c Coverage<'c>,
    matrix: TileMatrix,
    cache: VecDeque<CachedTile>,
}

/// One tile a [`CoverageSampler`] has read: its samples and ancillary row, or
/// `None` where the coverage stores no tile.
struct CachedTile {
    coord: TileCoord,
    tile: Option<(TileSamples, TileAncillary)>,
}

impl CoverageSampler<'_> {
    /// Returns the value at a position, as [`Coverage::value_at`] does.
    ///
    /// The position of each sample in its cell follows the coverage's
    /// `grid_cell_encoding`. Between the outermost samples and the edge of the
    /// extent there is nothing to interpolate towards, so a bilinear lookup
    /// there uses the outermost samples.
    ///
    /// A tile with no `gpkg_2d_gridded_tile_ancillary` row is read with the
    /// default scale and offset, as other readers do.
    ///
    /// # Errors
    ///
    /// As [`Coverage::decode`] for a tile the lookup reads, and
    /// [`Error::Tile`] with [`TileError::PayloadSizeMismatch`] for a tile
    /// whose size is not the one its zoom level declares, because its samples
    /// would then be in the wrong places.
    pub fn value_at(
        &mut self,
        x: f64,
        y: f64,
        interpolation: Interpolation,
    ) -> Result<Option<f64>> {
        let coverage = self.coverage;
        let Some(lookup) = SampleLookup::new(
            coverage.matrix_set(),
            &self.matrix,
            &coverage.ancillary,
            x,
            y,
            interpolation,
        ) else {
            return Ok(None);
        };
        lookup.value(|sample| self.sample(sample))
    }

    /// Returns the value of one sample the lookup reads.
    fn sample(&mut self, sample: &WeightedSample) -> Result<Option<f64>> {
        let coverage = self.coverage;
        let Some((coord, column, row)) = sample.tile(&self.matrix) else {
            return Ok(None);
        };
        Ok(self
            .tile(coord)?
            .and_then(|(samples, tile)| samples.value(column, row, &coverage.ancillary, tile)))
    }

    /// Returns one tile, reading and decoding it if the sampler does not
    /// keep it.
    fn tile(&mut self, coord: TileCoord) -> Result<Option<&(TileSamples, TileAncillary)>> {
        let index = match self.cache.iter().position(|cached| cached.coord == coord) {
            Some(index) => index,
            None => {
                let tile = self.read(coord)?;
                if self.cache.len() == SAMPLER_TILES {
                    self.cache.pop_front();
                }
                self.cache.push_back(CachedTile { coord, tile });
                self.cache.len() - 1
            }
        };
        Ok(self
            .cache
            .get(index)
            .and_then(|cached| cached.tile.as_ref()))
    }

    fn read(&self, coord: TileCoord) -> Result<Option<(TileSamples, TileAncillary)>> {
        let Some(samples) = self.coverage.samples(coord)? else {
            return Ok(None);
        };
        let (width, height) = (
            i64::try_from(samples.width()),
            i64::try_from(samples.height()),
        );
        if width != Ok(self.matrix.tile_width) || height != Ok(self.matrix.tile_height) {
            return Err(TileError::PayloadSizeMismatch {
                zoom_level: self.matrix.zoom_level,
                expected_width: self.matrix.tile_width,
                expected_height: self.matrix.tile_height,
                actual_width: width.unwrap_or(i64::MAX),
                actual_height: height.unwrap_or(i64::MAX),
            }
            .into());
        }
        let tile = self
            .coverage
            .tile_ancillary(coord)?
            .unwrap_or_else(TileAncillary::defaults);
        Ok(Some((samples, tile)))
    }
}

impl std::fmt::Debug for CoverageSampler<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoverageSampler")
            .field("table_name", &self.coverage.table_name())
            .field("zoom_level", &self.matrix.zoom_level)
            .field("cached_tiles", &self.cache.len())
            .finish_non_exhaustive()
    }
}

impl CoverageWriter<'_> {
    /// Writes one tile with statistics calculated from its samples, after
    /// checking Requirement 21.
    ///
    /// Decodes the payload, rejects a NaN or infinite sample, and records the
    /// [`SampleStatistics`] of the values in the ancillary row. The scale and
    /// offset come from `ancillary`, and any statistics in it are replaced. A
    /// tile whose samples are all `data_null` gets no statistics.
    ///
    /// # Errors
    ///
    /// As [`CoverageWriter::put_with_ancillary`], and:
    ///
    /// - [`Error::Tile`] with [`TileError::CoverageProfileViolation`] for
    ///   Requirement 21.
    /// - [`Error::Tile`] with [`TileError::UnreadablePayload`] or
    ///   [`TileError::PayloadTooLarge`], as for [`Coverage::decode`].
    pub fn put_with_statistics(
        &mut self,
        coord: TileCoord,
        data: &[u8],
        ancillary: &TileAncillary,
    ) -> Result<()> {
        let samples = decode_samples(data)?;
        samples.check_finite()?;
        let statistics = samples.statistics(&self.ancillary, ancillary);
        let mut ancillary = *ancillary;
        ancillary.min = statistics.map(|s| s.min);
        ancillary.max = statistics.map(|s| s.max);
        ancillary.mean = statistics.map(|s| s.mean);
        ancillary.std_dev = statistics.map(|s| s.std_dev);
        self.put_with_ancillary(coord, data, &ancillary)
    }
}
