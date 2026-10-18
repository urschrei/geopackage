//! The PNG encoding, which an integer coverage can use (Requirement 13).

use super::payload::unreadable;
use super::payload::violation;
use crate::tiles::TileError;

/// The header values of a conforming PNG coverage payload.
///
/// The struct has no sample-type field, because Requirement 13 permits only one
/// form: "If type `png` is being used, the data _SHALL_ be 16-bit unsigned
/// integer (single channel - "greyscale")". A `CoveragePng` is always 16-bit
/// greyscale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct CoveragePng {
    /// Width in pixels, from `IHDR`.
    pub width: i64,
    /// Height in pixels, from `IHDR`.
    pub height: i64,
}

/// The first eight bytes of every PNG file.
pub(crate) const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Checks a PNG tile payload against Requirement 13: 16-bit unsigned, single
/// channel.
///
/// Reads the signature and the `IHDR` fields up to the colour type only. It
/// does not read `IDAT`, so this function does not check Requirement 21, as
/// with [`coverage_tiff`](super::coverage_tiff).
///
/// # Errors
///
/// [`TileError::CoverageProfileViolation`], with Requirement 13, if the bit
/// depth or colour type is not 16-bit greyscale.
///
/// [`TileError::UnreadablePayload`] if the bytes are not a PNG, or if they end
/// before the colour type, the last `IHDR` field that this function reads.
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A PNG header: the signature, then an IHDR chunk with the given fields.
    /// The reader reads nothing after the header, so the function adds nothing.
    pub(crate) fn png(width: u32, height: u32, bit_depth: u8, colour_type: u8) -> Vec<u8> {
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
}
