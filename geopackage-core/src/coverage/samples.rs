//! Decoded coverage samples, and the rules that need them, behind the
//! `coverage-samples` feature.
//!
//! The extension permits two encodings: a TIFF of the profile in
//! [`super`], or a 16-bit greyscale PNG. [`decode_samples`] decodes either to
//! [`TileSamples`], with the `tiff` and `png` crates. From the samples:
//!
//! - **Requirement 21** (no NaN or infinite samples):
//!   [`TileSamples::check_finite`].
//! - **Per-tile statistics**, in the form the tile ancillary row records them:
//!   [`TileSamples::statistics`].
//! - **Values at a position**: [`SampleLookup`] finds the samples a position
//!   reads and their weights, following the coverage's `grid_cell_encoding`,
//!   and combines their values. Finding the tiles that hold the samples is
//!   left to the caller, which knows where the tiles are stored.

use std::io::Cursor;

use super::{CoverageAncillary, CoveragePayload, TileAncillary, coverage_payload};
use crate::tiles::{TileCoord, TileError, TileMatrix, TileMatrixSet};

/// The decoded samples of one coverage tile, as stored: before the scale and
/// offset, and with `data_null` samples left in place.
///
/// Every sample type the profile permits (8, 16 and 32-bit integers, and
/// 32-bit floats) converts to `f64` exactly, so one type holds them all.
/// [`CoverageAncillary::value`] converts a stored sample to a value, and
/// [`CoverageAncillary::is_null`] recognises the sentinel.
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

    /// Returns the value at a column and row, after the scale and offset of
    /// `tile` and `coverage`, or `None` outside the tile and where the sample
    /// is `data_null`, NaN or infinite.
    pub fn value(
        &self,
        column: usize,
        row: usize,
        coverage: &CoverageAncillary,
        tile: &TileAncillary,
    ) -> Option<f64> {
        self.get(column, row)
            .filter(|&stored| stored.is_finite() && !coverage.is_null(stored))
            .map(|stored| coverage.value(tile, stored))
    }

    /// Returns the stored samples row by row from the top-left.
    pub fn as_slice(&self) -> &[f64] {
        &self.samples
    }

    /// Returns the stored samples row by row from the top-left.
    pub fn into_vec(self) -> Vec<f64> {
        self.samples
    }

    /// Checks Requirement 21: "Special floating point values such as NaN and
    /// Inf SHALL NOT be used".
    ///
    /// Integer samples cannot break the requirement, so only a `float`
    /// coverage can fail it. A missing value is `data_null`, not NaN.
    ///
    /// # Errors
    ///
    /// [`TileError::CoverageProfileViolation`] for Requirement 21, naming the
    /// first sample that breaks it.
    pub fn check_finite(&self) -> Result<(), TileError> {
        let Some((index, sample)) = self
            .samples
            .iter()
            .enumerate()
            .find(|(_, sample)| !sample.is_finite())
        else {
            return Ok(());
        };
        // `width` is not zero, since there is a sample.
        let (column, row) = (index % self.width, index / self.width);
        Err(TileError::CoverageProfileViolation {
            requirement: 21,
            detail: format!(
                "the sample at column {column}, row {row} is {sample}, and NaN and Inf SHALL NOT be used"
            ),
        })
    }

    /// Calculates the statistics of the tile's values, or returns `None` if
    /// every sample is `data_null` or not finite.
    ///
    /// `tile` is the tile's ancillary row, whose scale and offset apply to its
    /// samples. [`SampleStatistics`] describes what the statistics include.
    pub fn statistics(
        &self,
        coverage: &CoverageAncillary,
        tile: &TileAncillary,
    ) -> Option<SampleStatistics> {
        // Two passes over the values: the mean first, then the deviations from
        // it, which loses less precision than a sum of squares.
        let values = || {
            self.samples
                .iter()
                .copied()
                .filter(|&stored| stored.is_finite() && !coverage.is_null(stored))
                .map(|stored| coverage.value(tile, stored))
        };
        let (mut count, mut min, mut max, mut sum) =
            (0_usize, f64::INFINITY, f64::NEG_INFINITY, 0.0);
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
}

/// The statistics of one tile's values, in the form the
/// `gpkg_2d_gridded_tile_ancillary` columns record them.
///
/// The values are natural: the scale and offset of the tile and of the
/// coverage are applied first, as the extension specifies ("The min, max, and
/// mean values are natural, i.e., not scaled or offset"). GDAL writes them so.
/// `data_null` samples are left out, and so are NaN and infinite samples,
/// which Requirement 21 forbids and [`TileSamples::check_finite`] reports.
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

/// The most samples [`decode_payload`] decodes from one tile: 4096 by 4096.
///
/// The header of a payload states its size, and the decoders allocate for that
/// size before reading the data, so a few bytes that declare a large tile would
/// otherwise cost as much memory as a real one. 4096 is the largest tile width
/// and height GDAL's GeoPackage driver supports, and 2^24 samples are
/// 128 MiB as `f64`.
pub const MAX_DECODED_SAMPLES: usize = 1 << 24;

/// Decodes a coverage payload to its stored samples, after checking it
/// against the payload profile as [`coverage_payload`] does.
///
/// # Errors
///
/// Those of [`coverage_payload`] and [`decode_payload`].
pub fn decode_samples(bytes: &[u8]) -> Result<TileSamples, TileError> {
    decode_payload(bytes, &coverage_payload(bytes)?)
}

/// Decodes a payload whose header has been read, and checks that the decoder
/// agrees with that header.
///
/// # Errors
///
/// - [`TileError::PayloadTooLarge`] if the header declares more than
///   [`MAX_DECODED_SAMPLES`] samples. Nothing is decoded.
/// - [`TileError::UnreadablePayload`] if the data cannot be decoded, or
///   decodes to a size or sample type other than the header's.
pub fn decode_payload(bytes: &[u8], payload: &CoveragePayload) -> Result<TileSamples, TileError> {
    let declared = (payload.width(), payload.height());
    let count = usize::try_from(declared.0)
        .ok()
        .zip(usize::try_from(declared.1).ok())
        .and_then(|(width, height)| width.checked_mul(height));
    if count.is_none_or(|count| count > MAX_DECODED_SAMPLES) {
        return Err(TileError::PayloadTooLarge {
            width: declared.0,
            height: declared.1,
            limit: MAX_DECODED_SAMPLES,
        });
    }
    let (width, height, samples) = match payload {
        CoveragePayload::Tiff(_) => decode_tiff(bytes)?,
        CoveragePayload::Png(_) => decode_png(bytes)?,
    };
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

fn decode_tiff(bytes: &[u8]) -> Result<(usize, usize, Vec<f64>), TileError> {
    use tiff::decoder::{Decoder, DecodingResult};

    let tiff_error = |error: tiff::TiffError| unreadable(error.to_string());
    // The widest sample the profile permits is four bytes. The header check
    // has bounded the size the IFD declares, and this bounds what the decoder
    // allocates by the same measure.
    let mut limits = tiff::decoder::Limits::default();
    limits.decoding_buffer_size = MAX_DECODED_SAMPLES * 4;
    let mut decoder = Decoder::new(Cursor::new(bytes))
        .map_err(tiff_error)?
        .with_limits(limits);
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

fn decode_png(bytes: &[u8]) -> Result<(usize, usize, Vec<f64>), TileError> {
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
    // The decoder's own limits do not cover this buffer, which is the
    // caller's. Two bytes a sample bounds it as the header check bounded the
    // size.
    let size = reader
        .output_buffer_size()
        .filter(|&size| size <= MAX_DECODED_SAMPLES * 2)
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

fn unreadable(reason: String) -> TileError {
    TileError::UnreadablePayload { reason }
}

/// How a [`SampleLookup`] derives a value from the samples around a position.
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

/// One sample a [`SampleLookup`] reads, and its weight.
///
/// The column and row count samples across the whole zoom level, from the
/// top-left of the tile matrix set. [`WeightedSample::tile`] splits them into
/// a tile and a position in it.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct WeightedSample {
    /// The column, counted in samples across the zoom level.
    pub column: i64,
    /// The row, counted in samples across the zoom level.
    pub row: i64,
    /// The weight of the sample's value, above zero and at most one.
    pub weight: f64,
}

impl WeightedSample {
    /// Returns the tile that holds the sample, and the sample's column and
    /// row in that tile, or `None` for a matrix whose tile size is not
    /// positive.
    pub fn tile(&self, matrix: &TileMatrix) -> Option<(TileCoord, usize, usize)> {
        let (width, height) = (matrix.tile_width, matrix.tile_height);
        if width <= 0 || height <= 0 {
            return None;
        }
        let coord = TileCoord::new(
            matrix.zoom_level,
            self.column.div_euclid(width),
            self.row.div_euclid(height),
        );
        let column = usize::try_from(self.column.rem_euclid(width)).ok()?;
        let row = usize::try_from(self.row.rem_euclid(height)).ok()?;
        Some((coord, column, row))
    }
}

/// The samples that the value at a position reads, with their weights.
///
/// Finds the samples from the grid alone: the tile matrix set, one zoom
/// level's matrix, and the coverage's `grid_cell_encoding`. Reading them is
/// the caller's part, because the tiles are stored elsewhere;
/// [`SampleLookup::value`] combines what the caller reads.
///
/// ```
/// use geopackage_core::coverage::{CoverageAncillary, Interpolation, SampleLookup};
/// use geopackage_core::tiles::{TileMatrix, TileMatrixSet};
///
/// // One 2x2 tile over a 2x2 extent: a sample per unit, centred in its cell.
/// let set = TileMatrixSet::new(0, 0.0, 0.0, 2.0, 2.0);
/// let matrix = TileMatrix::new(0, 1, 1, 2, 2, 1.0, 1.0);
/// let coverage = CoverageAncillary::new("dem", "float");
/// let samples = [[10.0, 20.0], [30.0, 40.0]];
///
/// // Halfway between the four sample centres.
/// let lookup =
///     SampleLookup::new(&set, &matrix, &coverage, 1.0, 1.0, Interpolation::Bilinear)
///         .expect("inside the extent");
/// let value = lookup.value(|sample| {
///     Ok::<_, ()>(Some(samples[sample.row as usize][sample.column as usize]))
/// });
/// assert_eq!(value, Ok(Some(25.0)));
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SampleLookup {
    interpolation: Interpolation,
    samples: [WeightedSample; 4],
    len: usize,
}

impl SampleLookup {
    /// Finds the samples for the value at a position in the coverage's
    /// spatial reference system, or returns `None` where there is none to
    /// find: outside the extent of `set`, or on a grid whose counts or pixel
    /// sizes are not positive.
    ///
    /// The position of each sample in its cell follows the coverage's
    /// `grid_cell_encoding`. Between the outermost samples and the edge of the
    /// extent there is nothing to interpolate towards, so a bilinear lookup
    /// there uses the outermost samples. A bilinear lookup reads only the
    /// samples with a weight above zero, so a position on a sample reads only
    /// that sample.
    pub fn new(
        set: &TileMatrixSet,
        matrix: &TileMatrix,
        coverage: &CoverageAncillary,
        x: f64,
        y: f64,
        interpolation: Interpolation,
    ) -> Option<Self> {
        // `contains` is false for NaN, which a pair of `<` tests would let
        // through.
        if !(set.min_x..=set.max_x).contains(&x) || !(set.min_y..=set.max_y).contains(&y) {
            return None;
        }
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
            return None;
        }
        let columns = matrix.matrix_width.saturating_mul(matrix.tile_width);
        let rows = matrix.matrix_height.saturating_mul(matrix.tile_height);

        // The position in samples: sample (i, j) is at (i, j).
        let encoding = CellEncoding::parse(coverage.grid_cell_encoding.as_deref());
        let (origin_x, origin_y) = encoding.origin();
        let u = (x - set.min_x) / matrix.pixel_x_size - origin_x;
        let v = (set.max_y - y) / matrix.pixel_y_size - origin_y;
        let clamp = |position: f64, limit: i64| (position as i64).clamp(0, limit - 1);

        let interpolation = if encoding == CellEncoding::Area {
            Interpolation::Nearest
        } else {
            interpolation
        };
        let unread = WeightedSample {
            column: 0,
            row: 0,
            weight: 0.0,
        };
        let mut lookup = Self {
            interpolation,
            samples: [unread; 4],
            len: 0,
        };
        match interpolation {
            Interpolation::Nearest => {
                lookup.samples[0] = WeightedSample {
                    column: clamp((u + 0.5).floor(), columns),
                    row: clamp((v + 0.5).floor(), rows),
                    weight: 1.0,
                };
                lookup.len = 1;
            }
            Interpolation::Bilinear => {
                let (left, top) = (u.floor(), v.floor());
                let (fx, fy) = (u - left, v - top);
                for (dx, dy, weight) in [
                    (0.0, 0.0, (1.0 - fx) * (1.0 - fy)),
                    (1.0, 0.0, fx * (1.0 - fy)),
                    (0.0, 1.0, (1.0 - fx) * fy),
                    (1.0, 1.0, fx * fy),
                ] {
                    // Four corners fill the four slots at most.
                    let Some(slot) = lookup.samples.get_mut(lookup.len) else {
                        break;
                    };
                    if weight <= 0.0 {
                        continue;
                    }
                    *slot = WeightedSample {
                        column: clamp(left + dx, columns),
                        row: clamp(top + dy, rows),
                        weight,
                    };
                    lookup.len += 1;
                }
            }
        }
        Some(lookup)
    }

    /// Returns the samples the lookup reads, in the order
    /// [`SampleLookup::value`] reads them.
    pub fn samples(&self) -> &[WeightedSample] {
        self.samples.get(..self.len).unwrap_or_default()
    }

    /// Combines the values of the samples into the value at the position.
    ///
    /// `value_of` returns the value of one sample, after its scale and
    /// offset, or `None` where there is none: a tile the coverage does not
    /// store, or a sample that is `data_null`, NaN or infinite.
    /// [`TileSamples::value`] returns exactly that. Any sample without a value
    /// makes the result `None`, and the samples after it are not read.
    ///
    /// # Errors
    ///
    /// The first error `value_of` returns.
    pub fn value<E>(
        &self,
        mut value_of: impl FnMut(&WeightedSample) -> Result<Option<f64>, E>,
    ) -> Result<Option<f64>, E> {
        match self.interpolation {
            Interpolation::Nearest => value_of(&self.samples[0]),
            Interpolation::Bilinear => {
                let mut value = 0.0;
                for sample in self.samples() {
                    let Some(sample_value) = value_of(sample)? else {
                        return Ok(None);
                    };
                    value += sample.weight * sample_value;
                }
                Ok(Some(value))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coverage::CoverageDatatype;

    fn coverage(encoding: Option<&str>) -> CoverageAncillary {
        let mut coverage = CoverageAncillary::new("dem", CoverageDatatype::Float.as_str());
        coverage.grid_cell_encoding = encoding.map(str::to_owned);
        coverage
    }

    /// A float32 TIFF, single strip, uncompressed, made by the `tiff` encoder.
    fn float_tiff(width: u32, height: u32, samples: &[f32]) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        tiff::encoder::TiffEncoder::new(&mut bytes)
            .unwrap()
            .write_image::<tiff::encoder::colortype::Gray32Float>(width, height, samples)
            .unwrap();
        bytes.into_inner()
    }

    /// A 16-bit greyscale PNG, made by the `png` encoder.
    fn grey16_png(width: u32, height: u32, samples: &[u16]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Sixteen);
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_be_bytes()).collect();
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&data)
            .unwrap();
        bytes
    }

    #[test]
    fn a_tiff_decodes_to_its_samples() {
        let samples = decode_samples(&float_tiff(3, 2, &[1.0, 2.5, -3.0, 4.0, 5.0, 6.25])).unwrap();
        assert_eq!((samples.width(), samples.height()), (3, 2));
        assert_eq!(samples.as_slice(), &[1.0, 2.5, -3.0, 4.0, 5.0, 6.25]);
        assert_eq!(samples.get(2, 0), Some(-3.0));
        assert_eq!(samples.get(3, 0), None);
        assert_eq!(samples.get(0, 2), None);
    }

    #[test]
    fn a_png_decodes_to_its_samples() {
        let samples = decode_samples(&grey16_png(2, 2, &[0, 1, 65_535, 300])).unwrap();
        assert_eq!(samples.into_vec(), vec![0.0, 1.0, 65_535.0, 300.0]);
    }

    #[test]
    fn a_truncated_payload_is_an_error_not_a_panic() {
        let tiff = float_tiff(4, 4, &[0.0; 16]);
        let png = grey16_png(4, 4, &[0; 16]);
        // The PNG decoder does not need the 12-byte `IEND` chunk at the end,
        // so a cut there still decodes; every earlier cut takes image data.
        for (bytes, trailer) in [(&tiff, 0), (&png, 12)] {
            for end in 0..bytes.len() - trailer {
                assert!(decode_samples(&bytes[..end]).is_err(), "{end}");
            }
        }
    }

    #[test]
    fn a_header_that_declares_too_many_samples_is_not_decoded() {
        // An IHDR that declares 4097 by 4096 samples, and no image data.
        let mut bytes = grey16_png(1, 1, &[0]);
        bytes[16..20].copy_from_slice(&4097_u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&4096_u32.to_be_bytes());
        let error = decode_samples(&bytes).unwrap_err();
        assert!(matches!(
            error,
            TileError::PayloadTooLarge {
                width: 4097,
                height: 4096,
                limit: MAX_DECODED_SAMPLES
            }
        ));
    }

    #[test]
    fn requirement_21_names_the_first_non_finite_sample() {
        let samples = decode_samples(&float_tiff(2, 2, &[1.0, 2.0, f32::NAN, 4.0])).unwrap();
        let error = samples.check_finite().unwrap_err();
        assert!(matches!(
            error,
            TileError::CoverageProfileViolation { requirement: 21, ref detail }
                if detail.contains("column 0, row 1")
        ));
        let finite = decode_samples(&float_tiff(1, 1, &[1.0])).unwrap();
        finite.check_finite().unwrap();
    }

    #[test]
    fn statistics_are_of_natural_values_without_nulls() {
        let mut coverage = coverage(None);
        coverage.data_null = Some(-9999.0);
        coverage.scale = 2.0;
        coverage.offset = 1.0;
        let tile = TileAncillary::new(1.0, 1.0);
        let samples =
            decode_samples(&float_tiff(2, 2, &[0.0, 1.0, -9999.0, f32::INFINITY])).unwrap();
        // Natural values: (0 + 1) * 2 + 1 = 3 and (1 + 1) * 2 + 1 = 5.
        let statistics = samples.statistics(&coverage, &tile).unwrap();
        assert_eq!(
            (
                statistics.min,
                statistics.max,
                statistics.mean,
                statistics.count
            ),
            (3.0, 5.0, 4.0, 2)
        );
        assert_eq!(statistics.std_dev, 1.0);
        assert_eq!(samples.value(0, 1, &coverage, &tile), None, "data_null");
        assert_eq!(samples.value(1, 1, &coverage, &tile), None, "infinite");
        assert_eq!(samples.value(1, 0, &coverage, &tile), Some(5.0));

        let empty = decode_samples(&float_tiff(1, 1, &[-9999.0])).unwrap();
        assert_eq!(empty.statistics(&coverage, &tile), None);
    }

    /// Two tiles of 2x2 samples side by side, over a 4x2 extent.
    fn grid() -> (TileMatrixSet, TileMatrix) {
        (
            TileMatrixSet::new(0, 0.0, 0.0, 4.0, 2.0),
            TileMatrix::new(3, 2, 1, 2, 2, 1.0, 1.0),
        )
    }

    fn read(lookup: &SampleLookup) -> Vec<(i64, i64, f64)> {
        lookup
            .samples()
            .iter()
            .map(|s| (s.column, s.row, s.weight))
            .collect()
    }

    #[test]
    fn nearest_reads_the_sample_whose_cell_holds_the_position() {
        let (set, matrix) = grid();
        let lookup = |x, y| {
            SampleLookup::new(&set, &matrix, &coverage(None), x, y, Interpolation::Nearest)
                .map(|lookup| read(&lookup))
        };
        assert_eq!(lookup(0.1, 1.9), Some(vec![(0, 0, 1.0)]));
        assert_eq!(lookup(2.6, 0.4), Some(vec![(2, 1, 1.0)]));
        // The edges of the extent are inside it, and clamp to the last sample.
        assert_eq!(lookup(4.0, 0.0), Some(vec![(3, 1, 1.0)]));
        assert_eq!(lookup(4.01, 1.0), None);
        assert_eq!(lookup(f64::NAN, 1.0), None);
    }

    #[test]
    fn bilinear_weights_the_samples_around_the_position() {
        let (set, matrix) = grid();
        let coverage = coverage(None);
        // Between the centres of samples (1, 0), (2, 0), (1, 1) and (2, 1),
        // a quarter of the way right and halfway down.
        let lookup =
            SampleLookup::new(&set, &matrix, &coverage, 1.75, 1.0, Interpolation::Bilinear)
                .unwrap();
        assert_eq!(
            read(&lookup),
            vec![(1, 0, 0.375), (2, 0, 0.125), (1, 1, 0.375), (2, 1, 0.125)]
        );
        // On a sample centre only that sample has a weight.
        let on =
            SampleLookup::new(&set, &matrix, &coverage, 2.5, 0.5, Interpolation::Bilinear).unwrap();
        assert_eq!(read(&on), vec![(2, 1, 1.0)]);
    }

    #[test]
    fn the_cell_encoding_moves_the_samples() {
        let (set, matrix) = grid();
        // An area cell has one value: bilinear reads the nearest sample.
        let area = SampleLookup::new(
            &set,
            &matrix,
            &coverage(Some("grid-value-is-area")),
            1.75,
            1.0,
            Interpolation::Bilinear,
        )
        .unwrap();
        assert_eq!(read(&area), vec![(1, 1, 1.0)]);
        // A corner sample is at the lower-left of its cell, so sample (0, 0)
        // is at (0, 1).
        let corner = SampleLookup::new(
            &set,
            &matrix,
            &coverage(Some("grid-value-is-corner")),
            0.0,
            1.0,
            Interpolation::Bilinear,
        )
        .unwrap();
        assert_eq!(read(&corner), vec![(0, 0, 1.0)]);
    }

    #[test]
    fn a_grid_the_spec_forbids_has_no_samples() {
        let (set, _) = grid();
        let coverage = coverage(None);
        for matrix in [
            TileMatrix::new(0, 0, 1, 2, 2, 1.0, 1.0),
            TileMatrix::new(0, -1, -1, 2, 2, 1.0, 1.0),
            TileMatrix::new(0, 1, 1, 2, 2, 0.0, 1.0),
            TileMatrix::new(0, 1, 1, 2, 2, 1.0, f64::NAN),
        ] {
            assert_eq!(
                SampleLookup::new(&set, &matrix, &coverage, 1.0, 1.0, Interpolation::Nearest),
                None,
                "{matrix:?}"
            );
        }
    }

    #[test]
    fn a_weighted_sample_splits_into_a_tile_and_a_position() {
        let (_, matrix) = grid();
        let sample = WeightedSample {
            column: 3,
            row: 1,
            weight: 1.0,
        };
        assert_eq!(sample.tile(&matrix), Some((TileCoord::new(3, 1, 0), 1, 1)));
    }

    #[test]
    fn the_value_is_none_when_any_weighted_sample_has_none() {
        let (set, matrix) = grid();
        let lookup = SampleLookup::new(
            &set,
            &matrix,
            &coverage(None),
            1.75,
            1.0,
            Interpolation::Bilinear,
        )
        .unwrap();
        let mut reads = 0;
        let value = lookup.value(|sample| {
            reads += 1;
            Ok::<_, ()>((sample.column != 2).then_some(1.0))
        });
        assert_eq!(value, Ok(None));
        assert_eq!(reads, 2, "stops at the first sample without a value");
        assert_eq!(lookup.value(|_| Ok::<_, ()>(Some(8.0))), Ok(Some(8.0)));
        assert_eq!(lookup.value(|_| Err::<Option<f64>, _>("io")), Err("io"));
    }
}
