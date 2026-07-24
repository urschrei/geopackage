//! GeoPackage Binary (GPB) header codec.
//!
//! A GPB blob is an 8-byte header, an optional envelope, and an ISO WKB body:
//!
//! ```text
//! byte 0..2   magic "GP" (0x47 0x50)
//! byte 2      version (0 = GeoPackage Binary version 1)
//! byte 3      flags: [R R X E E E B] — bit 0 B: header/envelope byte order
//!             (0 = big-endian, 1 = little-endian); bits 1–3 EEE: envelope
//!             indicator; bit 4: empty-geometry flag; bit 5 X: extended GPB;
//!             bits 6–7 R: reserved (0)
//! byte 4..8   srs_id (i32, byte order per flag B)
//! byte 8..    envelope: 0, 4, 6, or 8 doubles (byte order per flag B)
//! then        ISO WKB geometry (its own embedded byte-order markers)
//! ```
//!
//! This module parses and encodes the header only; the WKB body is opaque here
//! (the `geopackage` crate delegates it to georust `wkb`).

/// Byte order of the GPB header integers and envelope doubles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteOrder {
    /// Big-endian (flags bit 0 = 0).
    Big,
    /// Little-endian (flags bit 0 = 1).
    Little,
}

/// The GPB envelope. Values are `[min, max]` pairs per dimension, in the
/// order defined by the spec: x, y, then z and/or m.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Envelope {
    /// No envelope (indicator 0).
    None,
    /// `[minx, maxx, miny, maxy]` (indicator 1).
    Xy([f64; 4]),
    /// `[minx, maxx, miny, maxy, minz, maxz]` (indicator 2).
    Xyz([f64; 6]),
    /// `[minx, maxx, miny, maxy, minm, maxm]` (indicator 3).
    Xym([f64; 6]),
    /// `[minx, maxx, miny, maxy, minz, maxz, minm, maxm]` (indicator 4).
    Xyzm([f64; 8]),
}

impl Envelope {
    /// The envelope contents indicator code (0–4) for the flags byte.
    pub fn indicator(&self) -> u8 {
        match self {
            Envelope::None => 0,
            Envelope::Xy(_) => 1,
            Envelope::Xyz(_) => 2,
            Envelope::Xym(_) => 3,
            Envelope::Xyzm(_) => 4,
        }
    }

    fn n_doubles(indicator: u8) -> Option<usize> {
        match indicator {
            0 => Some(0),
            1 => Some(4),
            2 | 3 => Some(6),
            4 => Some(8),
            _ => None,
        }
    }

    /// The envelope values as a slice in spec order (empty for `None`).
    pub fn values(&self) -> &[f64] {
        match self {
            Envelope::None => &[],
            Envelope::Xy(v) => v,
            Envelope::Xyz(v) | Envelope::Xym(v) => v,
            Envelope::Xyzm(v) => v,
        }
    }

    /// `(minx, maxx, miny, maxy)` if an envelope is present.
    pub fn xy_bounds(&self) -> Option<(f64, f64, f64, f64)> {
        let v = self.values();
        (v.len() >= 4).then(|| (v[0], v[1], v[2], v[3]))
    }
}

/// A parsed GPB header.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GpbHeader {
    /// Spatial reference system identifier.
    pub srs_id: i32,
    /// Optional geometry envelope.
    pub envelope: Envelope,
    /// Empty-geometry flag (flags bit 4).
    pub empty: bool,
    /// Extended GeoPackage Binary flag (flags bit 5).
    pub extended: bool,
    /// Byte order the header was encoded with.
    pub byte_order: ByteOrder,
}

/// Errors from GPB header parsing.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum GpbError {
    /// The blob is shorter than the declared header.
    #[error("GPB blob truncated: need {expected} bytes, have {actual}")]
    Truncated {
        /// Bytes required by the declared header.
        expected: usize,
        /// Bytes actually present.
        actual: usize,
    },
    /// The first two bytes are not "GP".
    #[error("bad GPB magic: {0:#04x} {1:#04x}")]
    BadMagic(u8, u8),
    /// Unsupported GPB version byte.
    #[error("unsupported GPB version {0} (only version 0 is defined)")]
    UnsupportedVersion(u8),
    /// Envelope contents indicator 5–7 (invalid per spec).
    #[error("invalid envelope contents indicator {0}")]
    InvalidEnvelopeIndicator(u8),
}

const MAGIC: [u8; 2] = [0x47, 0x50]; // "GP"
const HEADER_BASE_LEN: usize = 8;

/// Parse a GPB header from `blob`.
///
/// Returns the header and the byte offset at which the ISO WKB body starts.
/// Reserved flag bits (6–7) are ignored on read; [`encode_header`] always
/// writes them as zero.
pub fn parse_header(blob: &[u8]) -> Result<(GpbHeader, usize), GpbError> {
    if blob.len() < HEADER_BASE_LEN {
        return Err(GpbError::Truncated {
            expected: HEADER_BASE_LEN,
            actual: blob.len(),
        });
    }
    if blob[0..2] != MAGIC {
        return Err(GpbError::BadMagic(blob[0], blob[1]));
    }
    if blob[2] != 0 {
        return Err(GpbError::UnsupportedVersion(blob[2]));
    }
    let flags = blob[3];
    let byte_order = if flags & 0b1 == 1 {
        ByteOrder::Little
    } else {
        ByteOrder::Big
    };
    let indicator = (flags >> 1) & 0b111;
    let empty = flags & 0b1_0000 != 0;
    let extended = flags & 0b10_0000 != 0;

    let n = Envelope::n_doubles(indicator).ok_or(GpbError::InvalidEnvelopeIndicator(indicator))?;
    let body_offset = HEADER_BASE_LEN + n * 8;
    if blob.len() < body_offset {
        return Err(GpbError::Truncated {
            expected: body_offset,
            actual: blob.len(),
        });
    }

    let srs_bytes: [u8; 4] = blob[4..8].try_into().expect("length checked");
    let srs_id = match byte_order {
        ByteOrder::Big => i32::from_be_bytes(srs_bytes),
        ByteOrder::Little => i32::from_le_bytes(srs_bytes),
    };

    let mut vals = [0f64; 8];
    for (i, chunk) in blob[8..body_offset].chunks_exact(8).enumerate() {
        let b: [u8; 8] = chunk.try_into().expect("chunks_exact");
        vals[i] = match byte_order {
            ByteOrder::Big => f64::from_be_bytes(b),
            ByteOrder::Little => f64::from_le_bytes(b),
        };
    }
    let envelope = match indicator {
        0 => Envelope::None,
        1 => Envelope::Xy(vals[0..4].try_into().expect("size")),
        2 => Envelope::Xyz(vals[0..6].try_into().expect("size")),
        3 => Envelope::Xym(vals[0..6].try_into().expect("size")),
        4 => Envelope::Xyzm(vals),
        _ => unreachable!("indicator validated above"),
    };

    Ok((
        GpbHeader {
            srs_id,
            envelope,
            empty,
            extended,
            byte_order,
        },
        body_offset,
    ))
}

/// Encode a GPB header (always little-endian, version 0, reserved bits zero).
///
/// Concatenate the result with an ISO WKB body to form a complete GPB blob.
pub fn encode_header(srs_id: i32, envelope: &Envelope, empty: bool, extended: bool) -> Vec<u8> {
    let vals = envelope.values();
    let mut out = Vec::with_capacity(HEADER_BASE_LEN + vals.len() * 8);
    out.extend_from_slice(&MAGIC);
    out.push(0); // version
    let mut flags = 0b1u8; // little-endian
    flags |= envelope.indicator() << 1;
    if empty {
        flags |= 0b1_0000;
    }
    if extended {
        flags |= 0b10_0000;
    }
    out.push(flags);
    out.extend_from_slice(&srs_id.to_le_bytes());
    for v in vals {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(srs_id: i32, envelope: Envelope, empty: bool, extended: bool) {
        let buf = encode_header(srs_id, &envelope, empty, extended);
        let (h, off) = parse_header(&buf).unwrap();
        assert_eq!(off, buf.len());
        assert_eq!(h.srs_id, srs_id);
        assert_eq!(h.envelope, envelope);
        assert_eq!(h.empty, empty);
        assert_eq!(h.extended, extended);
        assert_eq!(h.byte_order, ByteOrder::Little);
    }

    #[test]
    fn roundtrips() {
        roundtrip(4326, Envelope::None, false, false);
        roundtrip(-1, Envelope::Xy([1.0, 2.0, 3.0, 4.0]), false, false);
        roundtrip(
            0,
            Envelope::Xyz([1.0, 2.0, 3.0, 4.0, -5.0, 5.0]),
            false,
            false,
        );
        roundtrip(
            3857,
            Envelope::Xym([1.0, 2.0, 3.0, 4.0, 0.0, 9.0]),
            false,
            true,
        );
        roundtrip(
            i32::MIN,
            Envelope::Xyzm([1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]),
            true,
            false,
        );
    }

    #[test]
    fn parses_big_endian() {
        // Hand-built BE header: flags 0b0000_0010 (BE, indicator 1)
        let mut buf = vec![0x47, 0x50, 0x00, 0b0000_0010];
        buf.extend_from_slice(&4326i32.to_be_bytes());
        for v in [1.0f64, 2.0, 3.0, 4.0] {
            buf.extend_from_slice(&v.to_be_bytes());
        }
        let (h, off) = parse_header(&buf).unwrap();
        assert_eq!(h.byte_order, ByteOrder::Big);
        assert_eq!(h.srs_id, 4326);
        assert_eq!(h.envelope, Envelope::Xy([1.0, 2.0, 3.0, 4.0]));
        assert_eq!(off, 40);
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(
            parse_header(b"GX\x00\x01aaaa"),
            Err(GpbError::BadMagic(0x47, 0x58))
        );
        assert_eq!(
            parse_header(b"GP\x01\x01aaaa"),
            Err(GpbError::UnsupportedVersion(1))
        );
        // indicator 5 is invalid
        assert_eq!(
            parse_header(&[0x47, 0x50, 0x00, 5 << 1, 0, 0, 0, 0]),
            Err(GpbError::InvalidEnvelopeIndicator(5))
        );
        // declared XY envelope but truncated
        assert!(matches!(
            parse_header(&[0x47, 0x50, 0x00, 0b0000_0011, 0, 0, 0, 0, 1, 2, 3]),
            Err(GpbError::Truncated {
                expected: 40,
                actual: 11
            })
        ));
        assert!(matches!(
            parse_header(b"GP"),
            Err(GpbError::Truncated { .. })
        ));
    }

    #[test]
    fn reserved_bits_tolerated_on_read() {
        let mut buf = encode_header(4326, &Envelope::None, false, false);
        buf[3] |= 0b1100_0000;
        assert!(parse_header(&buf).is_ok());
    }
}
