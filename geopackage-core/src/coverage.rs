//! The tiled gridded coverage extension's TIFF payload profile (OGC 17-066r2,
//! version 1.1).
//!
//! A coverage tile is not a picture and, despite what the file format
//! suggests, **not a GeoTIFF**: georeferencing comes from
//! `gpkg_tile_matrix_set`, and the extension asks for no GeoKey, no
//! `ModelPixelScale` and no `ModelTiepoint`. What it asks for is a small
//! profile of baseline TIFF, and every constraint in it is a tag value:
//!
//! | Requirement | Constraint |
//! |---|---|
//! | 15 | Conforms to the TIFF specification, so BigTIFF is out |
//! | 16 | `SamplesPerPixel` = 1, "a single sample per grid cell" |
//! | 17 | 32-bit float, or `SampleFormat` 1 or 2 with `BitsPerSample` 8, 16 or 32 |
//! | 18 | LZW compression *MAY* be used |
//! | 19 | One image per file: "Multiple image files are not allowed" |
//! | 20 | *SHALL NOT* contain internal tiles |
//! | 21 | Every pixel valid; no NaN, no Inf |
//!
//! That is why this module exists and no image codec does: [`coverage_tiff`]
//! answers Requirements 15 to 20 by walking the first IFD, which is a header
//! read of the kind [`crate::tiles::probe`] already does, and no sample is
//! ever decoded.
//!
//! **Requirement 21 is not checked here, and cannot be.** "All pixels in a
//! tile of coverage data _SHALL_ be set with a valid component value … Special
//! floating point values such as NaN and Inf SHALL NOT be used" is a statement
//! about samples, and reaching them means decoding the strip. A payload this
//! module accepts satisfies the profile's structural requirements and says
//! nothing about its contents.
//!
//! The requirement numbers are those of the migrated spec source,
//! [`spec/2d-gridded-coverage/requirements`](https://github.com/opengeospatial/geopackage/tree/master/spec/2d-gridded-coverage/requirements).
//!
//! # Reading untrusted payloads
//!
//! A `tile_data` BLOB is the one part of a GeoPackage this crate reads without
//! having written it and without a schema to constrain it, so the walk is
//! bounded by construction: one pass, no recursion, nothing allocated from a
//! count the file declares, and no value offset ever followed. A tag whose
//! value does not fit its entry is treated as unread, which costs nothing
//! here, because every tag this profile constrains holds a single number.

use crate::tiles::TileError;

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
    /// `SubIFDs`, which points at further images (Requirement 19).
    pub const SUB_IFDS: u16 = 330;
    /// `SampleFormat`.
    pub const SAMPLE_FORMAT: u16 = 339;

    /// The name of one of the internal-tile tags, for an error message.
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

/// Bytes in one IFD entry: tag, type, count, then the value or its offset.
const ENTRY_LEN: usize = 12;

/// The sample encoding a coverage tile declares (Requirement 17).
///
/// 17-066r2 widened this: r1 allowed 32-bit float alone, and version 1.1 added
/// the integer forms, which is a good reason to keep the type open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SampleType {
    /// IEEE floating point, 32 bits: `SampleFormat` 3, `BitsPerSample` 32.
    ///
    /// The default encoding for a coverage whose `datatype` is `float`
    /// (Requirement 14).
    Float32,
    /// Unsigned integer samples of 8, 16 or 32 bits (`SampleFormat` 1).
    Unsigned(u8),
    /// Two's complement signed integer samples of 8, 16 or 32 bits
    /// (`SampleFormat` 2).
    Signed(u8),
}

impl SampleType {
    /// Bits per sample: 32 for [`SampleType::Float32`], and the declared width
    /// for the integer forms.
    pub fn bits(self) -> u8 {
        match self {
            Self::Float32 => 32,
            Self::Unsigned(bits) | Self::Signed(bits) => bits,
        }
    }
}

/// The compression a coverage tile's samples are stored under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TiffCompression {
    /// Uncompressed (`Compression` 1).
    None,
    /// LZW (`Compression` 5), the scheme Requirement 18 names and the one GDAL
    /// writes.
    Lzw,
    /// Deflate (`Compression` 8, or `0x80B2` for the pre-standard code).
    ///
    /// Read, but not baseline: see [`TiffCompression::is_baseline`].
    Deflate,
}

impl TiffCompression {
    /// Whether the scheme belongs to baseline TIFF, which Requirement 15 asks
    /// the payload to conform to.
    ///
    /// Deflate does not. Requirement 18 says LZW *MAY* be used and does not
    /// say "only LZW", so the two clauses point opposite ways and the tie goes
    /// to this workspace's standing asymmetry: a reader that turns a file away
    /// helps nobody, and a writer is where strictness is cheap. A
    /// Deflate-compressed payload is therefore read, and this is the predicate
    /// a write path uses to refuse writing one.
    pub fn is_baseline(self) -> bool {
        matches!(self, Self::None | Self::Lzw)
    }
}

/// What a conforming coverage TIFF payload declares in its header.
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

/// Checks a TIFF tile payload against the coverage profile, Requirements 15 to
/// 20.
///
/// Reads the file header and the first IFD, and no pixel data, so what comes
/// back describes what the payload *declares*. Requirement 21 constrains the
/// samples themselves, is out of reach of a header read, and is not checked.
///
/// Violations are reported in requirement order 15, 19, 20, 16, 17, 18: the
/// payload's structure before its sample description, so one that is the wrong
/// shape entirely is not first reported as, say, a `SampleFormat` problem.
///
/// # Errors
///
/// [`TileError::CoverageProfileViolation`], naming the requirement, when the
/// payload is a TIFF this profile does not allow.
///
/// [`TileError::UnreadablePayload`] when the bytes are not a TIFF at all, or
/// are truncated or malformed before the first IFD ends.
///
/// # Examples
///
/// A BigTIFF is a separate format with a wider header rather than an extension
/// of this one, so it fails the first requirement instead of being read:
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
    // Requirement 15, before anything else: BigTIFF has its own 16-byte header
    // and 20-byte entries, so it is a separate format rather than an extension
    // of this one. The walk below cannot read it, and the profile would not
    // accept it if it could.
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
    // (unsigned integer), and an absent BitsPerSample means one bit, which
    // this profile does not allow: baseline TIFF's default is a bilevel image.
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

    // Requirement 18, read beside Requirement 15: uncompressed and LZW are
    // baseline, Deflate is read but flagged, anything else is refused.
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

/// Reads a TIFF payload's pixel dimensions from its first IFD.
///
/// The profile check is not applied: this answers "how big is it", which
/// [`crate::tiles::probe`] asks of every payload, coverage or not. Answering it
/// here rather than in a second parser is deliberate, since two IFD readers can
/// disagree on a malformed file and this crate's fuzzing would eventually find
/// the input where they do.
pub(crate) fn dimensions(bytes: &[u8]) -> Result<(i64, i64), TileError> {
    FirstIfd::read(bytes)?.dimensions()
}

/// The tags of the first IFD this module reads, gathered in one pass.
#[derive(Debug, Default, Clone, Copy)]
struct FirstIfd {
    image_width: Tag,
    image_length: Tag,
    bits_per_sample: Tag,
    sample_format: Tag,
    samples_per_pixel: Tag,
    compression: Tag,
    /// The first internal-tile tag seen, if any (Requirement 20).
    tile_tag: Option<u16>,
    /// Whether `SubIFDs` is present (Requirement 19).
    sub_ifds: bool,
    /// Whether the IFD chain continues past the first image (Requirement 19).
    more_ifds: bool,
}

/// One tag's value, as far as this module needs it.
///
/// `Present(None)` is a tag whose value this module declines to read: a vector,
/// or a type wider than its entry, either of which would mean following an
/// offset. Every tag this profile constrains holds a single number, so
/// declining gives the same answer reading would.
#[derive(Debug, Default, Clone, Copy)]
enum Tag {
    #[default]
    Absent,
    Present(Option<u32>),
}

impl Tag {
    /// The tag's value, or `default` when it is absent; `None` when it is
    /// present in a form this module does not read.
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
            // 43 is BigTIFF, whose header is 16 bytes wide and whose entries
            // are 20: a separate format, and one this walker does not read.
            // Requirement 15 is what refuses it to a coverage; here it is
            // simply unreadable.
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
        // An entry count is a u16, so the entries span at most 768 KiB and
        // neither sum below can overflow a usize on any target this crate
        // builds for. Both are checked rather than assumed.
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

    /// Records one IFD entry, when it carries a tag this profile constrains.
    ///
    /// A repeated tag keeps its first value: baseline TIFF allows each tag
    /// once, and a payload repeating one has forfeited a generous reading.
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

    /// `ImageWidth` and `ImageLength`, which every TIFF carries.
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

/// Reads the byte order and version from a TIFF header, for the two callers
/// that need them: the walk itself, and the Requirement 15 check that runs
/// before it.
fn header(bytes: &[u8]) -> Option<(Endian, u16)> {
    let [order0, order1, version0, version1] = *bytes.first_chunk::<4>()?;
    let endian = match (order0, order1) {
        (b'I', b'I') => Endian::Little,
        (b'M', b'M') => Endian::Big,
        _ => return None,
    };
    Some((endian, endian.u16([version0, version1])))
}

/// Reads an entry's value when it is a single number held in the entry itself.
///
/// Anything else is `None`: a vector, or a type this profile never uses. A
/// byte, short or long value begins at the start of the field in both byte
/// orders, so no justification arithmetic is needed.
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

/// A payload whose bytes are not a TIFF this module can walk.
fn unreadable(reason: impl Into<String>) -> TileError {
    TileError::UnreadablePayload {
        reason: reason.into(),
    }
}

/// A TIFF that breaks one of the profile's requirements.
fn violation(requirement: u8, detail: impl Into<String>) -> TileError {
    TileError::CoverageProfileViolation {
        requirement,
        detail: detail.into(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A TIFF header and single IFD, built entry by entry.
    ///
    /// Only the header is built: this profile is a statement about tags, so
    /// every test here is about the bytes before the first sample. The strip
    /// tags a real payload carries are present for shape, and never read.
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
    /// LZW compressed, in strips. What GDAL writes for a `float` coverage.
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
        /// Replaces the value of a tag the payload already carries.
        fn set(mut self, tag: u16, value: u32) -> Self {
            for entry in &mut self.entries {
                if entry.0 == tag {
                    entry.3 = value;
                }
            }
            self
        }

        /// Removes a tag, so the reader sees its default.
        fn without(mut self, tag: u16) -> Self {
            self.entries.retain(|entry| entry.0 != tag);
            self
        }

        /// Adds an entry, keeping the entries in ascending tag order as
        /// baseline TIFF asks.
        fn with(mut self, tag: u16, field_type: u16, count: u32, value: u32) -> Self {
            self.entries.push((tag, field_type, count, value));
            self.entries.sort_by_key(|entry| entry.0);
            self
        }

        /// Changes the type an existing entry declares.
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

    /// The requirement a payload violates, for a test that expects one.
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
        // Requirement 17 as 17-066r2 widened it: r1 allowed float32 alone.
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

            // A tile large enough to need LONG dimensions reads the same.
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
        // A vector SamplesPerPixel is not read rather than followed, and a tag
        // that cannot be read cannot satisfy the requirement either.
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
        // Baseline TIFF's default is one bit, which this profile does not
        // allow: an absent tag is a violation, not a pass.
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

        // A BigTIFF is a TIFF to the magic-byte sniffer and unreadable to this
        // walker, and the probe reports the latter: refusing a payload for
        // breaking a coverage requirement is `coverage_tiff`'s business, not
        // every tile's.
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

    #[test]
    fn dimensions_ignore_the_profile() {
        // A payload the profile refuses still has a size, which is what the
        // tile size check needs from a payload it is about to reject.
        let tiff = conformant()
            .set(tag::SAMPLES_PER_PIXEL, 3)
            .set(tag::IMAGE_WIDTH, 512);
        assert_eq!(dimensions(&tiff.build()).unwrap(), (512, 256));
    }
}
