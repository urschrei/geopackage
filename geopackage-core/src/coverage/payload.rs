//! A coverage payload in either encoding, and the datatype it must agree
//! with.

use super::png::CoveragePng;
use super::png::PNG_SIGNATURE;
use super::png::coverage_png;
use super::tiff::CoverageTiff;
use super::tiff::SampleType;
use super::tiff::coverage_tiff;
use crate::tiles::TileError;
use crate::tiles::TileFormat;
use crate::tiles::TilePayload;

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

    /// Returns the payload as a tile payload: its format and pixel size, for
    /// [`TileMatrix::check_payload`](crate::tiles::TileMatrix::check_payload).
    ///
    /// A reader that knows the tile's zoom level checks its size this way
    /// before decoding it, so that the decoder never allocates for a size the
    /// zoom level does not declare.
    pub fn tile_payload(self) -> TilePayload {
        TilePayload {
            format: match self {
                Self::Tiff(_) => TileFormat::Tiff,
                Self::Png(_) => TileFormat::Png,
            },
            width: self.width(),
            height: self.height(),
        }
    }
}

/// Checks a coverage tile payload in either encoding.
///
/// A payload with a PNG signature goes to [`coverage_png`]. All other payloads
/// go to [`coverage_tiff`], which rejects a payload that is neither encoding.
///
/// This function does not check that the encoding agrees with the coverage.
/// Use [`CoverageDatatype::check_payload`] for that.
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
    /// This is a predicate only. A read does not enforce it, and you decide
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

/// Creates a [`TileError::UnreadablePayload`] with the given reason.
pub(crate) fn unreadable(reason: impl Into<String>) -> TileError {
    TileError::UnreadablePayload {
        reason: reason.into(),
    }
}

/// Creates a [`TileError::CoverageProfileViolation`] for the given
/// requirement.
pub(crate) fn violation(requirement: u8, detail: impl Into<String>) -> TileError {
    TileError::CoverageProfileViolation {
        requirement,
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coverage::png::tests::png;
    use crate::coverage::tiff::tests::conformant;
    use crate::coverage::tiff_tag as tag;

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
}
