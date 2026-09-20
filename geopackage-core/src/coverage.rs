//! The payload profile of the tiled gridded coverage extension (OGC 17-066r2,
//! version 1.1).
//!
//! A coverage tile is **not a GeoTIFF**. The georeferencing is in
//! `gpkg_tile_matrix_set`, and the extension does not use GeoKeys,
//! `ModelPixelScale` or `ModelTiepoint`. The extension specifies a small
//! profile of baseline TIFF, and each constraint in the profile is a tag
//! value:
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
//! For this reason, the module does not need an image codec.
//! [`coverage_tiff`] checks Requirements 15 to 20 from the first IFD, the same
//! header that [`crate::tiles::probe`] reads, and does not decode samples.
//!
//! An integer coverage can use PNG instead (Requirement 13): 16-bit unsigned,
//! single channel. [`coverage_png`] reads the `IHDR` chunk and nothing more.
//! [`coverage_payload`] accepts either encoding, and
//! [`CoverageDatatype::check_payload`] compares a payload with the `datatype`
//! of its coverage.
//!
//! **This module does not check Requirement 21.** "All pixels in a tile of
//! coverage data _SHALL_ be set with a valid component value … Special
//! floating point values such as NaN and Inf SHALL NOT be used" is a statement
//! about samples, and a check of the samples needs a decoder. A payload that
//! this module accepts satisfies the structural requirements only.
//!
//! The requirement numbers are those of the migrated spec source,
//! [`spec/2d-gridded-coverage/requirements`](https://github.com/opengeospatial/geopackage/tree/master/spec/2d-gridded-coverage/requirements).
//!
//! # Untrusted payloads
//!
//! This crate reads `tile_data` BLOBs that it did not write, and no schema
//! constrains them. The walk of the first IFD is therefore bounded: one pass,
//! no recursion, no allocation from a count in the file, and no value offset
//! followed. A tag with a value that does not fit in its entry is unread. This
//! does not change a result, because each tag that the profile constrains is a
//! single number.

use crate::tiles::TileError;

/// The registered extension name for tiled gridded coverages (OGC 17-066r2).
pub const COVERAGE_EXTENSION_NAME: &str = "gpkg_2d_gridded_coverage";
/// The `gpkg_extensions.definition` value for [`COVERAGE_EXTENSION_NAME`].
///
/// This is the r1 URL. The Extension Table Record in the r2 spec source gives
/// this URL, and GDAL writes it. The value is copied and not corrected, as with
/// all normative text in this workspace.
pub const COVERAGE_EXTENSION_DEFINITION: &str =
    "http://docs.opengeospatial.org/is/17-066r1/17-066r1.html";
/// The `gpkg_extensions.scope` value for the three rows that a coverage
/// registers.
pub const COVERAGE_EXTENSION_SCOPE: &str = "read-write";
/// The `gpkg_contents.data_type` of a tiled gridded coverage (Requirement 5).
///
/// The value is not `tiles`: a coverage is a different kind of content from a
/// tile pyramid.
pub const COVERAGE_DATA_TYPE: &str = "2d-gridded-coverage";
/// The per-coverage ancillary table (Requirement 1).
pub const COVERAGE_ANCILLARY_TABLE: &str = "gpkg_2d_gridded_coverage_ancillary";
/// The per-tile ancillary table (Requirement 2).
pub const TILE_ANCILLARY_TABLE: &str = "gpkg_2d_gridded_tile_ancillary";

/// The TIFF tags this profile constrains, by number.
mod tag {
    /// `ImageWidth`.
    pub const IMAGE_WIDTH: u16 = 256;
    /// `ImageLength`.
    pub const IMAGE_LENGTH: u16 = 257;
    /// `BitsPerSample`.
    pub const BITS_PER_SAMPLE: u16 = 258;
    /// `Compression`.
    pub const COMPRESSION: u16 = 259;
    /// `SamplesPerPixel`.
    pub const SAMPLES_PER_PIXEL: u16 = 277;
    /// `TileWidth`, the first of the four tags that organise an image into
    /// internal tiles (Requirement 20).
    pub const TILE_WIDTH: u16 = 322;
    /// `TileLength`.
    pub const TILE_LENGTH: u16 = 323;
    /// `TileOffsets`.
    pub const TILE_OFFSETS: u16 = 324;
    /// `TileByteCounts`.
    pub const TILE_BYTE_COUNTS: u16 = 325;
    /// `SubIFDs`, which points to more images (Requirement 19).
    pub const SUB_IFDS: u16 = 330;
    /// `SampleFormat`.
    pub const SAMPLE_FORMAT: u16 = 339;

    /// Returns the name of an internal-tile tag, for an error message.
    pub fn name(tag: u16) -> &'static str {
        match tag {
            TILE_WIDTH => "TileWidth",
            TILE_LENGTH => "TileLength",
            TILE_OFFSETS => "TileOffsets",
            TILE_BYTE_COUNTS => "TileByteCounts",
            _ => "tag",
        }
    }
}

/// The length in bytes of one IFD entry: tag, type, count, and the value or
/// its offset.
const ENTRY_LEN: usize = 12;

/// The sample encoding of a coverage tile (Requirement 17).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SampleType {
    /// IEEE floating point, 32 bits: `SampleFormat` 3, `BitsPerSample` 32.
    ///
    /// The default encoding of a coverage with the `datatype` `float`
    /// (Requirement 14).
    Float32,
    /// Unsigned integer samples of 8, 16 or 32 bits (`SampleFormat` 1).
    Unsigned(u8),
    /// Two's complement signed integer samples of 8, 16 or 32 bits
    /// (`SampleFormat` 2).
    Signed(u8),
}

impl SampleType {
    /// Returns the number of bits per sample: 32 for [`SampleType::Float32`],
    /// and the declared width for the integer forms.
    pub fn bits(self) -> u8 {
        match self {
            Self::Float32 => 32,
            Self::Unsigned(bits) | Self::Signed(bits) => bits,
        }
    }
}

/// The compression scheme of the samples in a coverage tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TiffCompression {
    /// Uncompressed (`Compression` 1).
    None,
    /// LZW (`Compression` 5), the scheme that Requirement 18 names.
    Lzw,
    /// Deflate (`Compression` 8, or `0x80B2` for the pre-standard code).
    ///
    /// This crate reads Deflate, but Deflate is not baseline TIFF. See
    /// [`TiffCompression::is_baseline`].
    Deflate,
}

impl TiffCompression {
    /// Returns `true` if the scheme is part of baseline TIFF, which
    /// Requirement 15 specifies.
    ///
    /// Deflate is not baseline. Requirement 18 says that LZW *MAY* be used, and
    /// it does not say "only LZW", so Requirements 15 and 18 do not agree. A
    /// reader accepts a Deflate payload, and a writer uses this predicate to
    /// reject one.
    pub fn is_baseline(self) -> bool {
        matches!(self, Self::None | Self::Lzw)
    }
}

/// The header values of a conforming coverage TIFF payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct CoverageTiff {
    /// The sample encoding (Requirement 17).
    pub sample_type: SampleType,
    /// The compression scheme (Requirement 18).
    pub compression: TiffCompression,
    /// Width in pixels, from `ImageWidth`.
    pub width: i64,
    /// Height in pixels, from `ImageLength`.
    pub height: i64,
}

/// Checks a TIFF tile payload against Requirements 15 to 20 of the coverage
/// profile.
///
/// Reads the file header and the first IFD, and no pixel data. The result
/// contains the values that the payload *declares*. This function does not
/// check Requirement 21, which applies to the samples.
///
/// The checks run in the order 15, 19, 20, 16, 17, 18: the structure first,
/// then the sample description. A payload with the wrong structure therefore
/// does not fail first on, for example, `SampleFormat`.
///
/// # Errors
///
/// [`TileError::CoverageProfileViolation`], with the requirement number, if
/// the payload is a TIFF that the profile does not permit.
///
/// [`TileError::UnreadablePayload`] if the bytes are not a TIFF, or if they are
/// truncated or malformed before the end of the first IFD.
///
/// # Examples
///
/// A BigTIFF is a different format with a wider header. It fails
/// Requirement 15, and the function does not read it further:
///
/// ```
/// use geopackage_core::TileError;
/// use geopackage_core::coverage::coverage_tiff;
///
/// let bigtiff = b"II\x2b\x00\x08\x00\x00\x00";
/// assert!(matches!(
///     coverage_tiff(bigtiff),
///     Err(TileError::CoverageProfileViolation { requirement: 15, .. })
/// ));
/// ```
pub fn coverage_tiff(bytes: &[u8]) -> Result<CoverageTiff, TileError> {
    // Requirement 15 first. BigTIFF has a 16-byte header and 20-byte entries,
    // so it is a different format. The walk below cannot read it, and the
    // profile does not permit it.
    if let Some((_, 43)) = header(bytes) {
        return Err(violation(
            15,
            "BigTIFF (version 43) is not the baseline TIFF the profile requires",
        ));
    }
    let ifd = FirstIfd::read(bytes)?;

    // Requirement 19: one image per payload.
    if ifd.more_ifds {
        return Err(violation(
            19,
            "the first IFD points to a second image, and multiple images are not allowed",
        ));
    }
    if ifd.sub_ifds {
        return Err(violation(
            19,
            "SubIFDs (tag 330) declares further images, and multiple images are not allowed",
        ));
    }

    // Requirement 20: no internal tiles.
    if let Some(tile_tag) = ifd.tile_tag {
        return Err(violation(
            20,
            format!(
                "{} (tag {tile_tag}) organises the image into internal tiles",
                tag::name(tile_tag)
            ),
        ));
    }

    // Requirement 16: one sample per grid cell. An absent tag means 1, the
    // baseline default.
    match ifd.samples_per_pixel.value(1) {
        Some(1) => {}
        Some(samples) => {
            return Err(violation(
                16,
                format!(
                    "SamplesPerPixel is {samples}, and a coverage tile has one sample per grid cell"
                ),
            ));
        }
        None => return Err(violation(16, "SamplesPerPixel is not a single number")),
    }

    // Requirement 17: the sample encoding. An absent SampleFormat means 1
    // (unsigned integer). An absent BitsPerSample means 1 bit, the baseline
    // default for a bilevel image, and the profile does not permit it.
    let format = ifd
        .sample_format
        .value(1)
        .ok_or_else(|| violation(17, "SampleFormat is not a single number"))?;
    let bits = ifd
        .bits_per_sample
        .value(1)
        .ok_or_else(|| violation(17, "BitsPerSample is not a single number"))?;
    let sample_type = match (format, bits) {
        (3, 32) => SampleType::Float32,
        (3, other) => {
            return Err(violation(
                17,
                format!("SampleFormat 3 (IEEE floating point) needs BitsPerSample 32, not {other}"),
            ));
        }
        (1, 8) => SampleType::Unsigned(8),
        (1, 16) => SampleType::Unsigned(16),
        (1, 32) => SampleType::Unsigned(32),
        (2, 8) => SampleType::Signed(8),
        (2, 16) => SampleType::Signed(16),
        (2, 32) => SampleType::Signed(32),
        (1 | 2, other) => {
            return Err(violation(
                17,
                format!(
                    "BitsPerSample is {other}, and an integer coverage sample is 8, 16 or 32 bits"
                ),
            ));
        }
        (other, _) => {
            return Err(violation(
                17,
                format!(
                    "SampleFormat is {other}, and a coverage sample is 1 (unsigned integer), 2 (signed integer) or 3 (IEEE floating point)"
                ),
            ));
        }
    };

    // Requirement 18, with Requirement 15: no compression and LZW are
    // baseline. Deflate is accepted and flagged. Other schemes are rejected.
    let compression = match ifd.compression.value(1) {
        Some(1) => TiffCompression::None,
        Some(5) => TiffCompression::Lzw,
        Some(8 | 0x80B2) => TiffCompression::Deflate,
        Some(other) => {
            return Err(violation(
                18,
                format!(
                    "Compression is {other}, and a coverage tile is uncompressed or LZW compressed"
                ),
            ));
        }
        None => return Err(violation(18, "Compression is not a single number")),
    };

    let (width, height) = ifd.dimensions()?;
    Ok(CoverageTiff {
        sample_type,
        compression,
        width,
        height,
    })
}

/// The header values of a conforming PNG coverage payload.
///
/// The struct has no sample-type field. Requirement 13 permits one form only:
/// "If type `png` is being used, the data _SHALL_ be 16-bit unsigned integer
/// (single channel - "greyscale")". A `CoveragePng` is always 16-bit
/// greyscale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct CoveragePng {
    /// Width in pixels, from `IHDR`.
    pub width: i64,
    /// Height in pixels, from `IHDR`.
    pub height: i64,
}

/// A coverage tile payload in one of the two encodings that the extension
/// permits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CoveragePayload {
    /// `image/tiff`. A `float` coverage uses this encoding (Requirement 14),
    /// and an `integer` coverage can use it (Requirement 13).
    Tiff(CoverageTiff),
    /// `image/png`, 16-bit greyscale. Only an `integer` coverage can use this
    /// encoding.
    Png(CoveragePng),
}

impl CoveragePayload {
    /// Returns the sample encoding that the payload declares.
    ///
    /// For a PNG payload this is always [`SampleType::Unsigned`] with 16 bits
    /// (Requirement 13).
    pub fn sample_type(self) -> SampleType {
        match self {
            Self::Tiff(tiff) => tiff.sample_type,
            Self::Png(_) => SampleType::Unsigned(16),
        }
    }

    /// Returns the width in pixels.
    pub fn width(self) -> i64 {
        match self {
            Self::Tiff(tiff) => tiff.width,
            Self::Png(png) => png.width,
        }
    }

    /// Returns the height in pixels.
    pub fn height(self) -> i64 {
        match self {
            Self::Tiff(tiff) => tiff.height,
            Self::Png(png) => png.height,
        }
    }

    /// Returns the MIME type that Requirements 13 and 14 give for this
    /// encoding.
    pub fn mime_type(self) -> &'static str {
        match self {
            Self::Tiff(_) => "image/tiff",
            Self::Png(_) => "image/png",
        }
    }
}

/// The `gpkg_2d_gridded_coverage_ancillary.datatype` of a coverage
/// (Requirement 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CoverageDatatype {
    /// `integer`: the samples are integers. The scale and offset columns
    /// convert them to values.
    Integer,
    /// `float`: the samples are values. Requirement 11 keeps the scale and
    /// offset at their default values.
    Float,
}

impl CoverageDatatype {
    /// Parses the column value. Requirement 9 permits `integer` and `float`
    /// only, and this function returns `None` for other values.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "integer" => Some(Self::Integer),
            "float" => Some(Self::Float),
            _ => None,
        }
    }

    /// Returns the column value as the spec writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Integer => "integer",
            Self::Float => "float",
        }
    }

    /// Checks a payload against the `datatype` of its coverage (Requirements 13
    /// and 14).
    ///
    /// The spec text is explicit for one direction only:
    ///
    /// - **float**: Requirement 14 says that the payload "_SHALL_ be of _MIME
    ///   type_ `image/tiff` and the default data encoding _SHALL_ be 32-bit
    ///   floating point as described in the TIFF Encoding". A PNG, or a TIFF
    ///   with integer samples, does not conform.
    /// - **integer**: Requirement 13 permits `image/png` or `image/tiff`, and
    ///   gives a sample type for PNG only (16-bit unsigned). It does not state
    ///   that a TIFF payload must have integer samples. This function
    ///   interprets Requirement 13 to include that rule, because floating-point
    ///   samples under the `datatype` `integer` make the column meaningless.
    ///
    /// This is a predicate. A read does not enforce it, and the caller decides
    /// what to do with a failure.
    ///
    /// # Errors
    ///
    /// [`TileError::CoverageProfileViolation`], with Requirement 13 or 14.
    pub fn check_payload(self, payload: &CoveragePayload) -> Result<(), TileError> {
        match (self, payload.sample_type()) {
            (Self::Float, SampleType::Float32) if matches!(payload, CoveragePayload::Tiff(_)) => {
                Ok(())
            }
            (Self::Float, _) => Err(violation(
                14,
                format!(
                    "the coverage declares datatype float, and the payload is {} with {:?} samples",
                    payload.mime_type(),
                    payload.sample_type()
                ),
            )),
            (Self::Integer, SampleType::Unsigned(_) | SampleType::Signed(_)) => Ok(()),
            (Self::Integer, sample_type) => Err(violation(
                13,
                format!(
                    "the coverage declares datatype integer, and the payload has {sample_type:?} samples"
                ),
            )),
        }
    }
}

/// Checks a coverage tile payload in either encoding.
///
/// A payload with a PNG signature goes to [`coverage_png`]. All other payloads
/// go to [`coverage_tiff`], which rejects a payload that is neither encoding.
///
/// This function does not check that the encoding agrees with the coverage.
/// [`CoverageDatatype::check_payload`] does that check.
///
/// # Errors
///
/// [`TileError::CoverageProfileViolation`] for a payload that the profile does
/// not permit, and [`TileError::UnreadablePayload`] for bytes in neither
/// encoding.
pub fn coverage_payload(bytes: &[u8]) -> Result<CoveragePayload, TileError> {
    if bytes.starts_with(&PNG_SIGNATURE) {
        return Ok(CoveragePayload::Png(coverage_png(bytes)?));
    }
    Ok(CoveragePayload::Tiff(coverage_tiff(bytes)?))
}

/// The first eight bytes of every PNG file.
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Checks a PNG tile payload against Requirement 13: 16-bit unsigned, single
/// channel.
///
/// Reads the signature and the `IHDR` chunk only. It does not read `IDAT`, so
/// this function does not check Requirement 21, as with [`coverage_tiff`].
///
/// # Errors
///
/// [`TileError::CoverageProfileViolation`], with Requirement 13, if the bit
/// depth or colour type is not 16-bit greyscale.
///
/// [`TileError::UnreadablePayload`] if the bytes are not a PNG, or if they are
/// truncated before the end of `IHDR`.
///
/// # Examples
///
/// ```
/// use geopackage_core::TileError;
/// use geopackage_core::coverage::coverage_png;
///
/// // A PNG signature and nothing after it: unreadable, not non-conformant.
/// let truncated = b"\x89PNG\r\n\x1a\n";
/// assert!(matches!(
///     coverage_png(truncated),
///     Err(TileError::UnreadablePayload { .. })
/// ));
/// ```
pub fn coverage_png(bytes: &[u8]) -> Result<CoveragePng, TileError> {
    if !bytes.starts_with(&PNG_SIGNATURE) {
        return Err(unreadable("no PNG signature"));
    }
    // IHDR is always the first chunk (PNG clause 5.6), after the 8-byte
    // signature and the 4-byte chunk length. The chunk type, width, height,
    // bit depth and colour type follow in 14 bytes.
    let Some(ihdr) = bytes.get(12..).and_then(|rest| rest.first_chunk::<14>()) else {
        return Err(unreadable("truncated before the IHDR header ends"));
    };
    let [
        type0,
        type1,
        type2,
        type3,
        width0,
        width1,
        width2,
        width3,
        height0,
        height1,
        height2,
        height3,
        bit_depth,
        colour_type,
    ] = *ihdr;
    if [type0, type1, type2, type3] != *b"IHDR" {
        return Err(unreadable("the first PNG chunk is not IHDR"));
    }
    if bit_depth != 16 {
        return Err(violation(
            13,
            format!("PNG bit depth is {bit_depth}, and a coverage tile is 16-bit"),
        ));
    }
    // Colour type 0 is greyscale; 2, 3, 4 and 6 add channels or a palette.
    if colour_type != 0 {
        return Err(violation(
            13,
            format!(
                "PNG colour type is {colour_type}, and a coverage tile is single channel (greyscale, colour type 0)"
            ),
        ));
    }
    Ok(CoveragePng {
        width: i64::from(u32::from_be_bytes([width0, width1, width2, width3])),
        height: i64::from(u32::from_be_bytes([height0, height1, height2, height3])),
    })
}

/// Reads the pixel dimensions of a TIFF payload from its first IFD.
///
/// Does not apply the profile check. [`crate::tiles::probe`] uses this function
/// for every TIFF payload, coverage or not, so that the crate has one IFD
/// reader only. Two readers can disagree on a malformed file.
pub(crate) fn dimensions(bytes: &[u8]) -> Result<(i64, i64), TileError> {
    FirstIfd::read(bytes)?.dimensions()
}

/// The tags from the first IFD that this module reads, collected in one pass.
#[derive(Debug, Default, Clone, Copy)]
struct FirstIfd {
    image_width: Tag,
    image_length: Tag,
    bits_per_sample: Tag,
    sample_format: Tag,
    samples_per_pixel: Tag,
    compression: Tag,
    /// The first internal-tile tag, if there is one (Requirement 20).
    tile_tag: Option<u16>,
    /// Whether `SubIFDs` is present (Requirement 19).
    sub_ifds: bool,
    /// Whether the IFD chain continues past the first image (Requirement 19).
    more_ifds: bool,
}

/// The value of one tag, to the extent that this module needs it.
///
/// `Present(None)` is a tag with a value that this module does not read: a
/// vector, or a type wider than its entry. Both need an offset to follow. Each
/// tag that the profile constrains is a single number, so the result is the
/// same as the result of a full read.
#[derive(Debug, Default, Clone, Copy)]
enum Tag {
    #[default]
    Absent,
    Present(Option<u32>),
}

impl Tag {
    /// Returns the value of the tag, or `default` if the tag is absent.
    /// Returns `None` if the tag is present in a form that this module does not
    /// read.
    fn value(self, default: u32) -> Option<u32> {
        match self {
            Self::Absent => Some(default),
            Self::Present(value) => value,
        }
    }
}

impl FirstIfd {
    /// Reads the TIFF header and the first IFD.
    fn read(bytes: &[u8]) -> Result<Self, TileError> {
        let Some((endian, version)) = header(bytes) else {
            return Err(unreadable(if bytes.len() < 4 {
                "shorter than a TIFF header"
            } else {
                "no TIFF byte order mark"
            }));
        };
        if version != 42 {
            // 43 is BigTIFF, with a 16-byte header and 20-byte entries. This
            // walker does not read it. For a coverage, Requirement 15 rejects
            // it; here it is unreadable.
            return Err(unreadable(format!("TIFF version is {version}, not 42")));
        }
        let Some(offset_bytes) = bytes.get(4..).and_then(|rest| rest.first_chunk::<4>()) else {
            return Err(unreadable("truncated before the first IFD pointer"));
        };
        let Ok(offset) = usize::try_from(endian.u32(*offset_bytes)) else {
            return Err(unreadable("the first IFD lies beyond this address space"));
        };
        let Some(count) = bytes
            .get(offset..)
            .and_then(|rest| rest.first_chunk::<2>())
            .map(|count| endian.u16(*count))
        else {
            return Err(unreadable("truncated before the first IFD"));
        };
        // An entry count is a u16, so the entries are at most 768 KiB, and
        // neither sum below can overflow a usize on a supported target. The
        // code checks both sums all the same.
        let entries_start = offset
            .checked_add(2)
            .ok_or_else(|| unreadable("the first IFD lies beyond this address space"))?;
        let entries_end = entries_start
            .checked_add(usize::from(count) * ENTRY_LEN)
            .ok_or_else(|| unreadable("the first IFD extends beyond this address space"))?;
        let Some(entries) = bytes.get(entries_start..entries_end) else {
            return Err(unreadable("truncated inside the first IFD"));
        };
        let Some(next) = bytes
            .get(entries_end..)
            .and_then(|rest| rest.first_chunk::<4>())
            .map(|next| endian.u32(*next))
        else {
            return Err(unreadable("truncated before the next-IFD pointer"));
        };

        let mut ifd = Self {
            more_ifds: next != 0,
            ..Self::default()
        };
        let (chunks, _) = entries.as_chunks::<ENTRY_LEN>();
        for entry in chunks {
            ifd.read_entry(*entry, endian);
        }
        Ok(ifd)
    }

    /// Records one IFD entry if the profile constrains its tag.
    ///
    /// A repeated tag keeps its first value. Baseline TIFF permits each tag
    /// once only.
    fn read_entry(&mut self, entry: [u8; ENTRY_LEN], endian: Endian) {
        let [
            tag0,
            tag1,
            type0,
            type1,
            count0,
            count1,
            count2,
            count3,
            field @ ..,
        ] = entry;
        let tag = endian.u16([tag0, tag1]);
        let field_type = endian.u16([type0, type1]);
        let count = endian.u32([count0, count1, count2, count3]);
        let slot = match tag {
            tag::IMAGE_WIDTH => &mut self.image_width,
            tag::IMAGE_LENGTH => &mut self.image_length,
            tag::BITS_PER_SAMPLE => &mut self.bits_per_sample,
            tag::SAMPLE_FORMAT => &mut self.sample_format,
            tag::SAMPLES_PER_PIXEL => &mut self.samples_per_pixel,
            tag::COMPRESSION => &mut self.compression,
            tag::TILE_WIDTH | tag::TILE_LENGTH | tag::TILE_OFFSETS | tag::TILE_BYTE_COUNTS => {
                if self.tile_tag.is_none() {
                    self.tile_tag = Some(tag);
                }
                return;
            }
            tag::SUB_IFDS => {
                self.sub_ifds = true;
                return;
            }
            _ => return,
        };
        if matches!(slot, Tag::Absent) {
            *slot = Tag::Present(scalar(field, field_type, count, endian));
        }
    }

    /// Returns `ImageWidth` and `ImageLength`, which every TIFF must have.
    fn dimensions(self) -> Result<(i64, i64), TileError> {
        let read = |tag: Tag, name: &str| match tag {
            Tag::Absent => Err(unreadable(format!("no {name} tag"))),
            Tag::Present(None) => Err(unreadable(format!("{name} is not a single number"))),
            Tag::Present(Some(value)) => Ok(i64::from(value)),
        };
        Ok((
            read(self.image_width, "ImageWidth")?,
            read(self.image_length, "ImageLength")?,
        ))
    }
}

/// Reads the byte order and the version from a TIFF header. The walk uses this
/// function, and so does the Requirement 15 check that runs before the walk.
fn header(bytes: &[u8]) -> Option<(Endian, u16)> {
    let [order0, order1, version0, version1] = *bytes.first_chunk::<4>()?;
    let endian = match (order0, order1) {
        (b'I', b'I') => Endian::Little,
        (b'M', b'M') => Endian::Big,
        _ => return None,
    };
    Some((endian, endian.u16([version0, version1])))
}

/// Reads the value of an entry if the value is a single number in the entry.
///
/// Returns `None` for all other values: a vector, or a type that the profile
/// does not use. A byte, short or long value starts at the first byte of the
/// field in both byte orders.
fn scalar(field: [u8; 4], field_type: u16, count: u32, endian: Endian) -> Option<u32> {
    if count != 1 {
        return None;
    }
    let [byte0, byte1, byte2, byte3] = field;
    match field_type {
        // BYTE, ASCII, SBYTE.
        1 | 2 | 6 => Some(u32::from(byte0)),
        // SHORT, SSHORT.
        3 | 8 => Some(u32::from(endian.u16([byte0, byte1]))),
        // LONG, SLONG.
        4 | 9 => Some(endian.u32([byte0, byte1, byte2, byte3])),
        _ => None,
    }
}

/// The byte order the TIFF header declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Endian {
    Little,
    Big,
}

impl Endian {
    fn u16(self, bytes: [u8; 2]) -> u16 {
        match self {
            Self::Little => u16::from_le_bytes(bytes),
            Self::Big => u16::from_be_bytes(bytes),
        }
    }

    fn u32(self, bytes: [u8; 4]) -> u32 {
        match self {
            Self::Little => u32::from_le_bytes(bytes),
            Self::Big => u32::from_be_bytes(bytes),
        }
    }
}

/// Creates a [`TileError::UnreadablePayload`] with the given reason.
fn unreadable(reason: impl Into<String>) -> TileError {
    TileError::UnreadablePayload {
        reason: reason.into(),
    }
}

/// Creates a [`TileError::CoverageProfileViolation`] for the given
/// requirement.
fn violation(requirement: u8, detail: impl Into<String>) -> TileError {
    TileError::CoverageProfileViolation {
        requirement,
        detail: detail.into(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A TIFF header and one IFD, built entry by entry.
    ///
    /// The builder makes the header only, because the profile is about tags.
    /// The strip tags of a complete payload are present but are not read.
    #[derive(Debug, Clone)]
    pub(crate) struct Tiff {
        endian: Endian,
        version: u16,
        entries: Vec<(u16, u16, u32, u32)>,
        next_ifd: u32,
    }

    /// TIFF field types, by number.
    const SHORT: u16 = 3;
    const LONG: u16 = 4;

    /// A conforming payload: 256 by 256, one 32-bit float sample per cell,
    /// LZW compressed, in strips. GDAL writes this form for a `float` coverage.
    pub(crate) fn conformant() -> Tiff {
        Tiff {
            endian: Endian::Little,
            version: 42,
            entries: vec![
                (tag::IMAGE_WIDTH, SHORT, 1, 256),
                (tag::IMAGE_LENGTH, SHORT, 1, 256),
                (tag::BITS_PER_SAMPLE, SHORT, 1, 32),
                (tag::COMPRESSION, SHORT, 1, 5),
                (262, SHORT, 1, 1),   // PhotometricInterpretation: black is zero
                (273, LONG, 1, 1024), // StripOffsets
                (tag::SAMPLES_PER_PIXEL, SHORT, 1, 1),
                (278, SHORT, 1, 256),   // RowsPerStrip
                (279, LONG, 1, 262144), // StripByteCounts
                (tag::SAMPLE_FORMAT, SHORT, 1, 3),
            ],
            next_ifd: 0,
        }
    }

    impl Tiff {
        /// Replaces the value of a tag that the payload already has.
        fn set(mut self, tag: u16, value: u32) -> Self {
            for entry in &mut self.entries {
                if entry.0 == tag {
                    entry.3 = value;
                }
            }
            self
        }

        /// Removes a tag, so that the reader uses its default.
        fn without(mut self, tag: u16) -> Self {
            self.entries.retain(|entry| entry.0 != tag);
            self
        }

        /// Adds an entry, and keeps the entries in ascending tag order, as
        /// baseline TIFF specifies.
        fn with(mut self, tag: u16, field_type: u16, count: u32, value: u32) -> Self {
            self.entries.push((tag, field_type, count, value));
            self.entries.sort_by_key(|entry| entry.0);
            self
        }

        /// Changes the type that an existing entry declares.
        fn retype(mut self, tag: u16, field_type: u16, count: u32) -> Self {
            for entry in &mut self.entries {
                if entry.0 == tag {
                    entry.1 = field_type;
                    entry.2 = count;
                }
            }
            self
        }

        fn big_endian(mut self) -> Self {
            self.endian = Endian::Big;
            self
        }

        fn version(mut self, version: u16) -> Self {
            self.version = version;
            self
        }

        fn next_ifd(mut self, offset: u32) -> Self {
            self.next_ifd = offset;
            self
        }

        pub(crate) fn build(&self) -> Vec<u8> {
            let u16_bytes = |value: u16| match self.endian {
                Endian::Little => value.to_le_bytes(),
                Endian::Big => value.to_be_bytes(),
            };
            let u32_bytes = |value: u32| match self.endian {
                Endian::Little => value.to_le_bytes(),
                Endian::Big => value.to_be_bytes(),
            };

            let mut bytes = Vec::new();
            bytes.extend_from_slice(match self.endian {
                Endian::Little => b"II",
                Endian::Big => b"MM",
            });
            bytes.extend_from_slice(&u16_bytes(self.version));
            bytes.extend_from_slice(&u32_bytes(8));
            bytes.extend_from_slice(&u16_bytes(u16::try_from(self.entries.len()).unwrap()));
            for &(tag, field_type, count, value) in &self.entries {
                bytes.extend_from_slice(&u16_bytes(tag));
                bytes.extend_from_slice(&u16_bytes(field_type));
                bytes.extend_from_slice(&u32_bytes(count));
                // A short occupies the first half of the value field in both
                // byte orders; the rest is padding.
                if field_type == SHORT {
                    bytes.extend_from_slice(&u16_bytes(u16::try_from(value).unwrap()));
                    bytes.extend_from_slice(&[0, 0]);
                } else {
                    bytes.extend_from_slice(&u32_bytes(value));
                }
            }
            bytes.extend_from_slice(&u32_bytes(self.next_ifd));
            bytes
        }
    }

    /// Returns the requirement that a payload violates. Panics if the payload
    /// is not a violation.
    fn violated(tiff: &Tiff) -> u8 {
        match coverage_tiff(&tiff.build()) {
            Err(TileError::CoverageProfileViolation { requirement, .. }) => requirement,
            other => panic!("expected a profile violation, got {other:?}"),
        }
    }

    fn accepted(tiff: &Tiff) -> CoverageTiff {
        coverage_tiff(&tiff.build()).unwrap()
    }

    #[test]
    fn conforming_float32_tile_is_accepted() {
        for tiff in [conformant(), conformant().big_endian()] {
            let coverage = accepted(&tiff);
            assert_eq!(coverage.sample_type, SampleType::Float32);
            assert_eq!(coverage.sample_type.bits(), 32);
            assert_eq!(coverage.compression, TiffCompression::Lzw);
            assert!(coverage.compression.is_baseline());
            assert_eq!((coverage.width, coverage.height), (256, 256));
        }
    }

    #[test]
    fn integer_samples_are_accepted() {
        // Requirement 17 in 17-066r2. The r1 version permitted float32 only.
        for (format, bits, expected) in [
            (1, 8, SampleType::Unsigned(8)),
            (1, 16, SampleType::Unsigned(16)),
            (1, 32, SampleType::Unsigned(32)),
            (2, 8, SampleType::Signed(8)),
            (2, 16, SampleType::Signed(16)),
            (2, 32, SampleType::Signed(32)),
        ] {
            let tiff = conformant()
                .set(tag::SAMPLE_FORMAT, format)
                .set(tag::BITS_PER_SAMPLE, bits);
            assert_eq!(accepted(&tiff).sample_type, expected);
            assert_eq!(
                accepted(&tiff).sample_type.bits(),
                u8::try_from(bits).unwrap()
            );
        }
    }

    #[test]
    fn dimensions_are_read_in_both_byte_orders_and_widths() {
        for tiff in [conformant(), conformant().big_endian()] {
            let rectangular = tiff
                .clone()
                .set(tag::IMAGE_WIDTH, 640)
                .set(tag::IMAGE_LENGTH, 480);
            let coverage = accepted(&rectangular);
            assert_eq!((coverage.width, coverage.height), (640, 480));

            // A tile large enough for LONG dimensions gives the same result.
            let wide = tiff
                .retype(tag::IMAGE_WIDTH, LONG, 1)
                .set(tag::IMAGE_WIDTH, 70_000);
            assert_eq!(accepted(&wide).width, 70_000);
        }
    }

    #[test]
    fn bigtiff_is_not_baseline() {
        assert_eq!(violated(&conformant().version(43)), 15);
    }

    #[test]
    fn a_second_image_is_refused() {
        assert_eq!(violated(&conformant().next_ifd(8)), 19);
        assert_eq!(
            violated(&conformant().with(tag::SUB_IFDS, LONG, 1, 4096)),
            19
        );
    }

    #[test]
    fn internal_tiles_are_refused() {
        for tag in [
            tag::TILE_WIDTH,
            tag::TILE_LENGTH,
            tag::TILE_OFFSETS,
            tag::TILE_BYTE_COUNTS,
        ] {
            assert_eq!(violated(&conformant().with(tag, LONG, 1, 256)), 20);
        }
    }

    #[test]
    fn more_than_one_sample_per_cell_is_refused() {
        assert_eq!(violated(&conformant().set(tag::SAMPLES_PER_PIXEL, 3)), 16);
        // The walk does not follow a vector SamplesPerPixel, and a tag that the
        // walk does not read cannot satisfy the requirement.
        assert_eq!(
            violated(&conformant().retype(tag::SAMPLES_PER_PIXEL, SHORT, 2)),
            16
        );
    }

    #[test]
    fn absent_samples_per_pixel_means_one() {
        let tiff = conformant().without(tag::SAMPLES_PER_PIXEL);
        assert_eq!(accepted(&tiff).sample_type, SampleType::Float32);
    }

    #[test]
    fn absent_bits_per_sample_is_a_bilevel_image() {
        // The baseline default is 1 bit, and the profile does not permit it.
        // An absent tag is therefore a violation.
        assert_eq!(violated(&conformant().without(tag::BITS_PER_SAMPLE)), 17);
    }

    #[test]
    fn float_samples_must_be_32_bits() {
        assert_eq!(violated(&conformant().set(tag::BITS_PER_SAMPLE, 16)), 17);
        assert_eq!(violated(&conformant().set(tag::BITS_PER_SAMPLE, 64)), 17);
    }

    #[test]
    fn integer_samples_must_be_8_16_or_32_bits() {
        let tiff = conformant().set(tag::SAMPLE_FORMAT, 1);
        assert_eq!(violated(&tiff.clone().set(tag::BITS_PER_SAMPLE, 4)), 17);
        assert_eq!(violated(&tiff.set(tag::BITS_PER_SAMPLE, 64)), 17);
    }

    #[test]
    fn unknown_sample_format_is_refused() {
        // 4 is "undefined data format" in TIFF Part 2 section 19.
        assert_eq!(violated(&conformant().set(tag::SAMPLE_FORMAT, 4)), 17);
    }

    #[test]
    fn absent_sample_format_means_unsigned_integer() {
        let tiff = conformant()
            .without(tag::SAMPLE_FORMAT)
            .set(tag::BITS_PER_SAMPLE, 16);
        assert_eq!(accepted(&tiff).sample_type, SampleType::Unsigned(16));
    }

    #[test]
    fn picture_compression_is_refused() {
        for compression in [6, 7, 32773] {
            assert_eq!(
                violated(&conformant().set(tag::COMPRESSION, compression)),
                18
            );
        }
    }

    #[test]
    fn deflate_is_read_but_is_not_baseline() {
        for code in [8, 0x80B2] {
            let coverage = accepted(&conformant().set(tag::COMPRESSION, code));
            assert_eq!(coverage.compression, TiffCompression::Deflate);
            assert!(!coverage.compression.is_baseline());
        }
    }

    #[test]
    fn absent_compression_means_uncompressed() {
        let tiff = conformant().without(tag::COMPRESSION);
        assert_eq!(accepted(&tiff).compression, TiffCompression::None);
    }

    #[test]
    fn a_repeated_tag_keeps_its_first_value() {
        let mut tiff = conformant();
        tiff.entries.push((tag::COMPRESSION, SHORT, 1, 7));
        assert_eq!(accepted(&tiff).compression, TiffCompression::Lzw);
    }

    #[test]
    fn payloads_that_are_not_tiffs_are_unreadable() {
        for bytes in [
            b"\x89PNG\r\n\x1a\n".as_slice(),
            b"II".as_slice(),
            b"".as_slice(),
            b"XX\x2a\x00\x08\x00\x00\x00".as_slice(),
        ] {
            assert!(matches!(
                coverage_tiff(bytes),
                Err(TileError::UnreadablePayload { .. })
            ));
        }
    }

    #[test]
    fn an_unknown_tiff_version_is_unreadable() {
        assert!(matches!(
            coverage_tiff(&conformant().version(7).build()),
            Err(TileError::UnreadablePayload { .. })
        ));
    }

    #[test]
    fn the_probe_reads_a_tiff_through_this_walker() {
        let bytes = conformant()
            .set(tag::IMAGE_WIDTH, 512)
            .set(tag::IMAGE_LENGTH, 128)
            .build();
        let payload = crate::tiles::probe(&bytes).unwrap();
        assert_eq!(payload.format, crate::tiles::TileFormat::Tiff);
        assert_eq!((payload.width, payload.height), (512, 128));

        // The magic-byte check identifies a BigTIFF as a TIFF, but this walker
        // cannot read it, and the probe reports that. A check of a coverage
        // requirement is the job of `coverage_tiff`, not of the probe.
        assert!(matches!(
            crate::tiles::probe(&conformant().version(43).build()),
            Err(TileError::UnreadablePayload { .. })
        ));
    }

    #[test]
    fn an_ifd_outside_the_payload_is_unreadable() {
        let mut bytes = conformant().build();
        // Point the header at an IFD past the end of the payload.
        bytes.splice(4..8, u32::to_le_bytes(0xFFFF));
        assert!(matches!(
            coverage_tiff(&bytes),
            Err(TileError::UnreadablePayload { .. })
        ));
    }

    #[test]
    fn a_missing_dimension_is_unreadable() {
        assert!(matches!(
            coverage_tiff(&conformant().without(tag::IMAGE_LENGTH).build()),
            Err(TileError::UnreadablePayload { .. })
        ));
    }

    #[test]
    fn every_truncation_is_an_error_and_never_a_panic() {
        let bytes = conformant().build();
        for length in 0..bytes.len() {
            let prefix = &bytes[..length];
            assert!(
                coverage_tiff(prefix).is_err(),
                "a payload truncated to {length} bytes was accepted"
            );
            dimensions(prefix).unwrap_err();
        }
        coverage_tiff(&bytes).unwrap();
    }

    /// A PNG header: the signature, then an IHDR chunk with the given fields.
    /// The reader reads nothing after the header, so the function adds nothing.
    fn png(width: u32, height: u32, bit_depth: u8, colour_type: u8) -> Vec<u8> {
        let mut bytes = PNG_SIGNATURE.to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[bit_depth, colour_type, 0, 0, 0]);
        // The chunk CRC. The reader does not check it, so the value is zero.
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes
    }

    #[test]
    fn a_16_bit_greyscale_png_conforms() {
        let payload = coverage_png(&png(256, 128, 16, 0)).unwrap();
        assert_eq!((payload.width, payload.height), (256, 128));
    }

    #[test]
    fn a_png_that_is_not_16_bit_greyscale_is_refused() {
        // Requirement 13 permits one PNG form only: 16-bit, single channel.
        for (bit_depth, colour_type) in [(8, 0), (16, 2), (8, 6), (16, 3)] {
            match coverage_png(&png(64, 64, bit_depth, colour_type)) {
                Err(TileError::CoverageProfileViolation { requirement, .. }) => {
                    assert_eq!(requirement, 13);
                }
                other => panic!("expected a Requirement 13 violation, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_truncated_png_is_unreadable() {
        let bytes = png(64, 64, 16, 0);
        // 8 signature + 4 length + 4 type + 10 of IHDR: the profile constrains
        // nothing after the first 26 bytes, and a shorter payload is
        // unreadable.
        const HEADER_LEN: usize = 26;
        for length in 0..HEADER_LEN {
            assert!(
                coverage_png(&bytes[..length]).is_err(),
                "a PNG truncated to {length} bytes was accepted"
            );
        }
        for length in HEADER_LEN..=bytes.len() {
            coverage_png(&bytes[..length]).unwrap();
        }
        // If the first chunk is not IHDR, the reader stops, and the payload is
        // unreadable, not non-conformant.
        let mut wrong_chunk = bytes.clone();
        wrong_chunk.splice(12..16, *b"sRGB");
        assert!(matches!(
            coverage_png(&wrong_chunk),
            Err(TileError::UnreadablePayload { .. })
        ));
    }

    #[test]
    fn a_payload_is_read_in_either_encoding() {
        let tiff = coverage_payload(&conformant().build()).unwrap();
        assert_eq!(tiff.sample_type(), SampleType::Float32);
        assert_eq!(tiff.mime_type(), "image/tiff");
        assert_eq!((tiff.width(), tiff.height()), (256, 256));

        let png = coverage_payload(&png(256, 256, 16, 0)).unwrap();
        // Requirement 13 permits one PNG form only, so the encoding gives the
        // sample type.
        assert_eq!(png.sample_type(), SampleType::Unsigned(16));
        assert_eq!(png.mime_type(), "image/png");
        assert_eq!((png.width(), png.height()), (256, 256));

        assert!(matches!(
            coverage_payload(b"\xff\xd8\xff\xe0 a JPEG"),
            Err(TileError::UnreadablePayload { .. })
        ));
    }

    #[test]
    fn a_float_coverage_takes_float32_tiff_and_nothing_else() {
        let float32 = coverage_payload(&conformant().build()).unwrap();
        CoverageDatatype::Float.check_payload(&float32).unwrap();

        // Requirement 14 is explicit: image/tiff, 32-bit floating point.
        let int16 = coverage_payload(
            &conformant()
                .set(tag::SAMPLE_FORMAT, 2)
                .set(tag::BITS_PER_SAMPLE, 16)
                .build(),
        )
        .unwrap();
        let png16 = coverage_payload(&png(64, 64, 16, 0)).unwrap();
        for payload in [int16, png16] {
            match CoverageDatatype::Float.check_payload(&payload) {
                Err(TileError::CoverageProfileViolation { requirement, .. }) => {
                    assert_eq!(requirement, 14);
                }
                other => panic!("expected a Requirement 14 violation, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_integer_coverage_takes_either_encoding_but_not_float_samples() {
        for payload in [
            coverage_payload(&png(64, 64, 16, 0)).unwrap(),
            coverage_payload(
                &conformant()
                    .set(tag::SAMPLE_FORMAT, 1)
                    .set(tag::BITS_PER_SAMPLE, 8)
                    .build(),
            )
            .unwrap(),
        ] {
            CoverageDatatype::Integer.check_payload(&payload).unwrap();
        }

        // An interpretation of Requirement 13, as the method documents:
        // floating-point samples under `datatype = integer` make the column
        // meaningless.
        let float32 = coverage_payload(&conformant().build()).unwrap();
        match CoverageDatatype::Integer.check_payload(&float32) {
            Err(TileError::CoverageProfileViolation { requirement, .. }) => {
                assert_eq!(requirement, 13);
            }
            other => panic!("expected a Requirement 13 violation, got {other:?}"),
        }
    }

    #[test]
    fn the_datatype_column_round_trips() {
        for datatype in [CoverageDatatype::Integer, CoverageDatatype::Float] {
            assert_eq!(CoverageDatatype::parse(datatype.as_str()), Some(datatype));
        }
        assert_eq!(CoverageDatatype::parse("elevation"), None);
    }

    #[test]
    fn dimensions_ignore_the_profile() {
        // A payload that the profile rejects still has a size. The tile size
        // check needs the size before it rejects the payload.
        let tiff = conformant()
            .set(tag::SAMPLES_PER_PIXEL, 3)
            .set(tag::IMAGE_WIDTH, 512);
        assert_eq!(dimensions(&tiff.build()).unwrap(), (512, 256));
    }
}
