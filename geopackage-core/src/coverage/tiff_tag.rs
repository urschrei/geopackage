//! The TIFF tags this profile constrains, by number.

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
