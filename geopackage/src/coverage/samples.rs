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
//! The decoders are `tiff` and `png`. `geopackage-core` still decodes
//! nothing.

use std::collections::VecDeque;
use std::io::Cursor;

use geopackage_core::TileError;
use geopackage_core::coverage::{CoveragePayload, coverage_payload};
use geopackage_core::tiles::{TileCoord, TileMatrix};

use super::{Coverage, CoverageAncillary, CoverageWriter, TileAncillary, is_null, natural_value};
use crate::{Error, Result};

/// The decoded samples of one coverage tile, as stored: before the scale and
/// offset, and with `data_null` samples left in place.
///
/// Every sample type the profile permits (8, 16 and 32-bit integers, and
/// 32-bit floats) converts to `f64` exactly, so one type holds them all.
/// [`Coverage::value`] converts a stored sample to a value, and
/// [`Coverage::is_null`] recognises the sentinel.
#[derive(Debug, Clone, PartialEq)]
pub struct TileSamples {
    width: usize,
    height: usize,
    /// Row by row from the top-left, as both encodings store them.
    samples: Vec<f64>,
}

impl TileSamples {
    /// Returns the width in samples.
    pub fn width(&self) -> usize {
        self.width
    }

    /// Returns the height in samples.
    pub fn height(&self) -> usize {
        self.height
    }

    /// Returns the stored sample at a column and row, counted from the
    /// top-left, or `None` outside the tile.
    pub fn get(&self, column: usize, row: usize) -> Option<f64> {
        if column >= self.width {
            return None;
        }
        let index = row.checked_mul(self.width)?.checked_add(column)?;
        self.samples.get(index).copied()
    }

    /// Returns the stored samples row by row from the top-left.
    pub fn as_slice(&self) -> &[f64] {
        &self.samples
    }

    /// Returns the stored samples row by row from the top-left.
    pub fn into_vec(self) -> Vec<f64> {
        self.samples
    }
}

/// The statistics of one tile's values, in the form the
/// `gpkg_2d_gridded_tile_ancillary` columns record them.
///
/// The values are natural: the scale and offset of the tile and of the
/// coverage are applied first, as the extension specifies ("The min, max, and
/// mean values are natural, i.e., not scaled or offset"). GDAL writes them so.
/// `data_null` samples are left out, and so are NaN and infinite samples,
/// which Requirement 21 forbids and [`Coverage::check_samples`] reports.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct SampleStatistics {
    /// The smallest value.
    pub min: f64,
    /// The largest value.
    pub max: f64,
    /// The arithmetic mean.
    pub mean: f64,
    /// The population standard deviation, which is what GDAL records.
    pub std_dev: f64,
    /// How many samples the statistics describe.
    pub count: usize,
}

/// How [`Coverage::value_at`] derives a value from the samples around a
/// position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Interpolation {
    /// The value of the nearest sample.
    #[default]
    Nearest,
    /// The distance-weighted value of the four samples around the position.
    ///
    /// On a `grid-value-is-area` coverage this is the same as
    /// [`Interpolation::Nearest`]: the extension says that such a cell has one
    /// value across its whole area, so there is nothing between samples to
    /// interpolate.
    Bilinear,
}

/// Where a sample sits in its cell (`grid_cell_encoding`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellEncoding {
    /// `grid-value-is-center`, the default.
    Center,
    /// `grid-value-is-area`: the value holds across the whole cell.
    Area,
    /// `grid-value-is-corner`. The extension does not say which corner. This
    /// crate reads the lower-left, as NGA's implementation does, after the CDB
    /// elevation mesh the extension cites.
    Corner,
}

impl CellEncoding {
    /// Parses the column. A missing value is the default of the table
    /// definition, and an unknown one is read as the default too, as NGA's
    /// implementation does, because a read does not reject a file.
    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("grid-value-is-area") => Self::Area,
            Some("grid-value-is-corner") => Self::Corner,
            _ => Self::Center,
        }
    }

    /// The position of sample (0, 0) in pixels, right and down from the
    /// top-left corner of the grid.
    fn origin(self) -> (f64, f64) {
        match self {
            Self::Center | Self::Area => (0.5, 0.5),
            Self::Corner => (0.0, 1.0),
        }
    }
}

impl<'a> Coverage<'a> {
    /// Decodes a payload to its stored samples.
    ///
    /// Checks the payload first, as [`Coverage::check_payload`] does, so that
    /// a payload this coverage does not permit is not decoded.
    ///
    /// # Errors
    ///
    /// Those of [`Coverage::check_payload`], and [`Error::Tile`] with
    /// [`TileError::UnreadablePayload`] for a payload whose header is
    /// conformant but whose data cannot be decoded.
    pub fn decode(&self, bytes: &[u8]) -> Result<TileSamples> {
        let payload = self.check_payload(bytes)?;
        decode_payload(bytes, &payload)
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
        Ok(check_finite(samples)?)
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
        statistics(&self.ancillary, tile, samples)
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
            encoding: CellEncoding::parse(self.ancillary.grid_cell_encoding.as_deref()),
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
    encoding: CellEncoding,
    cache: VecDeque<CachedTile>,
}

/// One tile a [`CoverageSampler`] has read: its samples and ancillary row, or
/// `None` where the coverage stores no tile.
struct CachedTile {
    column: i64,
    row: i64,
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
        let set = self.coverage.matrix_set();
        // `contains` is false for NaN, which a pair of `<` tests would let
        // through.
        if !(set.min_x..=set.max_x).contains(&x) || !(set.min_y..=set.max_y).contains(&y) {
            return Ok(None);
        }
        let matrix = self.matrix;
        // A grid the core spec forbids (Requirements 47 to 52), from a file
        // another writer made, has no samples to find. Each count is checked
        // on its own, because two negative ones multiply to a positive one.
        let positive = |size: f64| size.is_finite() && size > 0.0;
        if [
            matrix.matrix_width,
            matrix.matrix_height,
            matrix.tile_width,
            matrix.tile_height,
        ]
        .iter()
        .any(|&count| count <= 0)
            || !positive(matrix.pixel_x_size)
            || !positive(matrix.pixel_y_size)
        {
            return Ok(None);
        }
        let columns = matrix.matrix_width.saturating_mul(matrix.tile_width);
        let rows = matrix.matrix_height.saturating_mul(matrix.tile_height);

        // The position in samples: sample (i, j) is at (i, j).
        let (origin_x, origin_y) = self.encoding.origin();
        let u = (x - set.min_x) / matrix.pixel_x_size - origin_x;
        let v = (set.max_y - y) / matrix.pixel_y_size - origin_y;
        let clamp = |position: f64, limit: i64| (position as i64).clamp(0, limit - 1);

        let interpolation = if self.encoding == CellEncoding::Area {
            Interpolation::Nearest
        } else {
            interpolation
        };
        match interpolation {
            Interpolation::Nearest => self.sample(
                clamp((u + 0.5).floor(), columns),
                clamp((v + 0.5).floor(), rows),
            ),
            Interpolation::Bilinear => {
                let (left, top) = (u.floor(), v.floor());
                let (fx, fy) = (u - left, v - top);
                let mut value = 0.0;
                for (dx, dy, weight) in [
                    (0.0, 0.0, (1.0 - fx) * (1.0 - fy)),
                    (1.0, 0.0, fx * (1.0 - fy)),
                    (0.0, 1.0, (1.0 - fx) * fy),
                    (1.0, 1.0, fx * fy),
                ] {
                    if weight <= 0.0 {
                        continue;
                    }
                    let column = clamp(left + dx, columns);
                    let row = clamp(top + dy, rows);
                    let Some(sample) = self.sample(column, row)? else {
                        return Ok(None);
                    };
                    value += weight * sample;
                }
                Ok(Some(value))
            }
        }
    }

    /// Returns the value of one sample, addressed across the whole zoom level.
    fn sample(&mut self, column: i64, row: i64) -> Result<Option<f64>> {
        let (tile_width, tile_height) = (self.matrix.tile_width, self.matrix.tile_height);
        let ancillary = &self.coverage.ancillary;
        let Some((samples, tile)) = self.tile(column / tile_width, row / tile_height)? else {
            return Ok(None);
        };
        let (Ok(x), Ok(y)) = (
            usize::try_from(column % tile_width),
            usize::try_from(row % tile_height),
        ) else {
            return Ok(None);
        };
        Ok(samples
            .get(x, y)
            .filter(|stored| stored.is_finite() && !is_null(ancillary, *stored))
            .map(|stored| natural_value(ancillary, tile, stored)))
    }

    /// Returns one tile, reading and decoding it if the sampler does not
    /// keep it.
    fn tile(&mut self, column: i64, row: i64) -> Result<Option<&(TileSamples, TileAncillary)>> {
        let index = match self
            .cache
            .iter()
            .position(|cached| cached.column == column && cached.row == row)
        {
            Some(index) => index,
            None => {
                let tile = self.read(TileCoord::new(self.matrix.zoom_level, column, row))?;
                if self.cache.len() == SAMPLER_TILES {
                    self.cache.pop_front();
                }
                self.cache.push_back(CachedTile { column, row, tile });
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
        let (width, height) = (i64::try_from(samples.width), i64::try_from(samples.height));
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
    /// - [`Error::Tile`] with [`TileError::UnreadablePayload`] for a payload
    ///   whose header is conformant but whose data cannot be decoded.
    pub fn put_with_statistics(
        &mut self,
        coord: TileCoord,
        data: &[u8],
        ancillary: &TileAncillary,
    ) -> Result<()> {
        let samples = decode_payload(data, &coverage_payload(data)?)?;
        check_finite(&samples)?;
        let statistics = statistics(&self.ancillary, ancillary, &samples);
        let ancillary = TileAncillary {
            min: statistics.map(|s| s.min),
            max: statistics.map(|s| s.max),
            mean: statistics.map(|s| s.mean),
            std_dev: statistics.map(|s| s.std_dev),
            ..*ancillary
        };
        self.put_with_ancillary(coord, data, &ancillary)
    }
}

/// Decodes a payload whose header has been read, and checks that the decoder
/// agrees with that header.
pub(crate) fn decode_payload(bytes: &[u8], payload: &CoveragePayload) -> Result<TileSamples> {
    let (width, height, samples) = match payload {
        CoveragePayload::Tiff(_) => decode_tiff(bytes)?,
        CoveragePayload::Png(_) => decode_png(bytes)?,
        _ => return Err(unreadable(format!("cannot decode {}", payload.mime_type()))),
    };
    let declared = (payload.width(), payload.height());
    if (i64::try_from(width), i64::try_from(height)) != (Ok(declared.0), Ok(declared.1)) {
        return Err(unreadable(format!(
            "the header declares {} by {} samples, and the decoder read {width} by {height}",
            declared.0, declared.1
        )));
    }
    if Some(samples.len()) != width.checked_mul(height) {
        return Err(unreadable(format!(
            "the decoder read {} samples for {width} by {height}",
            samples.len()
        )));
    }
    Ok(TileSamples {
        width,
        height,
        samples,
    })
}

fn decode_tiff(bytes: &[u8]) -> Result<(usize, usize, Vec<f64>)> {
    use tiff::decoder::{Decoder, DecodingResult};

    let tiff_error = |error: tiff::TiffError| unreadable(error.to_string());
    let mut decoder = Decoder::new(Cursor::new(bytes)).map_err(tiff_error)?;
    let (width, height) = decoder.dimensions().map_err(tiff_error)?;
    let mut result = DecodingResult::U8(Vec::new());
    decoder
        .read_image_to_buffer(&mut result)
        .map_err(tiff_error)?;
    let samples = match result {
        DecodingResult::U8(samples) => widen(samples),
        DecodingResult::U16(samples) => widen(samples),
        DecodingResult::U32(samples) => widen(samples),
        DecodingResult::I8(samples) => widen(samples),
        DecodingResult::I16(samples) => widen(samples),
        DecodingResult::I32(samples) => widen(samples),
        DecodingResult::F32(samples) => widen(samples),
        // Requirement 17 permits none of the others, and the header check
        // has already rejected them.
        _ => {
            return Err(unreadable(
                "a sample type outside Requirement 17".to_owned(),
            ));
        }
    };
    Ok((width as usize, height as usize, samples))
}

fn decode_png(bytes: &[u8]) -> Result<(usize, usize, Vec<f64>)> {
    let png_error = |error: png::DecodingError| unreadable(error.to_string());
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    // No expansion and no stripping: the samples as stored, 16 bits each,
    // big-endian.
    decoder.set_transformations(png::Transformations::IDENTITY);
    let mut reader = decoder.read_info().map_err(png_error)?;
    if reader.output_color_type() != (png::ColorType::Grayscale, png::BitDepth::Sixteen) {
        return Err(unreadable(
            "the PNG is not 16-bit greyscale (Requirement 13)".to_owned(),
        ));
    }
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| unreadable("the PNG is too large to decode".to_owned()))?;
    let mut buffer = vec![0; size];
    let frame = reader.next_frame(&mut buffer).map_err(png_error)?;
    let data = buffer
        .get(..frame.buffer_size())
        .ok_or_else(|| unreadable("the PNG decoder overran its buffer".to_owned()))?;
    let (pairs, _) = data.as_chunks::<2>();
    let samples = pairs
        .iter()
        .map(|&pair| f64::from(u16::from_be_bytes(pair)))
        .collect();
    Ok((frame.width as usize, frame.height as usize, samples))
}

fn widen<T: Into<f64>>(samples: Vec<T>) -> Vec<f64> {
    samples.into_iter().map(Into::into).collect()
}

fn unreadable(reason: String) -> Error {
    TileError::UnreadablePayload { reason }.into()
}

/// Requirement 21, for [`Coverage::check_samples`].
pub(crate) fn check_finite(samples: &TileSamples) -> std::result::Result<(), TileError> {
    let Some((index, sample)) = samples
        .samples
        .iter()
        .enumerate()
        .find(|(_, sample)| !sample.is_finite())
    else {
        return Ok(());
    };
    // `width` is not zero, since there is a sample.
    let (column, row) = (index % samples.width, index / samples.width);
    Err(TileError::CoverageProfileViolation {
        requirement: 21,
        detail: format!(
            "the sample at column {column}, row {row} is {sample}, and NaN and Inf SHALL NOT be used"
        ),
    })
}

/// Two passes over the values: the mean first, then the deviations from it,
/// which loses less precision than a sum of squares.
fn statistics(
    coverage: &CoverageAncillary,
    tile: &TileAncillary,
    samples: &TileSamples,
) -> Option<SampleStatistics> {
    let values = || {
        samples
            .samples
            .iter()
            .copied()
            .filter(|&stored| stored.is_finite() && !is_null(coverage, stored))
            .map(|stored| natural_value(coverage, tile, stored))
    };
    let (mut count, mut min, mut max, mut sum) = (0_usize, f64::INFINITY, f64::NEG_INFINITY, 0.0);
    for value in values() {
        count += 1;
        min = min.min(value);
        max = max.max(value);
        sum += value;
    }
    if count == 0 {
        return None;
    }
    let n = count as f64;
    let mean = sum / n;
    let variance = values().map(|value| (value - mean).powi(2)).sum::<f64>() / n;
    Some(SampleStatistics {
        min,
        max,
        mean,
        std_dev: variance.sqrt(),
        count,
    })
}
