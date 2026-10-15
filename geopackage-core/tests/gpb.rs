//! Property tests over the GeoPackage Binary header codec.
//!
//! The headers are drawn from the whole of their domain: any `srs_id`, every
//! envelope kind with any `f64` values (NaN and the infinities included), and
//! both flags. Big-endian headers, which this crate reads but never writes,
//! are built by the test's own encoder. Near-valid headers are built by
//! breaking one field of a valid one, so that each documented error is reached.

use geopackage_core::gpb::{
    ByteOrder, Envelope, GpbError, GpbHeader, body_offset, encode_header, encode_header_into,
    header_len, parse_header,
};
use hegel::generators;

fn draw_any_f64(tc: &hegel::TestCase) -> f64 {
    tc.draw(generators::floats::<f64>())
}

/// Draws an envelope of any kind, with any values.
fn draw_envelope(tc: &hegel::TestCase) -> Envelope {
    let values = || -> [f64; 8] { std::array::from_fn(|_| draw_any_f64(tc)) };
    let v = values();
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(4)) {
        0 => Envelope::None,
        1 => Envelope::Xy([v[0], v[1], v[2], v[3]]),
        2 => Envelope::Xyz([v[0], v[1], v[2], v[3], v[4], v[5]]),
        3 => Envelope::Xym([v[0], v[1], v[2], v[3], v[4], v[5]]),
        _ => Envelope::Xyzm(v),
    }
}

/// The kind of an envelope and the bits of its values, so that two envelopes
/// compare exactly, NaN included.
fn envelope_bits(envelope: &Envelope) -> (u8, Vec<u64>) {
    (
        envelope.indicator(),
        envelope.values().iter().map(|v| v.to_bits()).collect(),
    )
}

/// A header compared by `envelope_bits`, with the remaining fields as they are.
fn header_bits(header: &GpbHeader) -> (i32, (u8, Vec<u64>), bool, bool, ByteOrder) {
    (
        header.srs_id,
        envelope_bits(&header.envelope),
        header.empty,
        header.extended,
        header.byte_order,
    )
}

/// Encodes a header in the given byte order, independently of the crate's
/// encoder. `reserved` sets flag bits 6 and 7, which a reader ignores.
fn encode_header_by_hand(
    srs_id: i32,
    envelope: &Envelope,
    empty: bool,
    extended: bool,
    byte_order: ByteOrder,
    reserved: u8,
) -> Vec<u8> {
    let little = matches!(byte_order, ByteOrder::Little);
    let mut flags = u8::from(little) | (envelope.indicator() << 1);
    flags |= u8::from(empty) << 4;
    flags |= u8::from(extended) << 5;
    flags |= (reserved & 0b11) << 6;
    let mut out = vec![b'G', b'P', 0, flags];
    if little {
        out.extend_from_slice(&srs_id.to_le_bytes());
    } else {
        out.extend_from_slice(&srs_id.to_be_bytes());
    }
    for value in envelope.values() {
        if little {
            out.extend_from_slice(&value.to_le_bytes());
        } else {
            out.extend_from_slice(&value.to_be_bytes());
        }
    }
    out
}

/// `encode_header` writes a little-endian header that `parse_header` reads
/// back exactly, with the body starting at `header_len` bytes, whatever
/// follows the header.
#[hegel::test]
fn an_encoded_header_parses_back(tc: hegel::TestCase) {
    let srs_id = tc.draw(generators::integers::<i32>());
    let envelope = draw_envelope(&tc);
    let (empty, extended) = (
        tc.draw(generators::booleans()),
        tc.draw(generators::booleans()),
    );
    let mut blob = encode_header(srs_id, &envelope, empty, extended);
    assert_eq!(blob.len(), header_len(&envelope));
    blob.extend(tc.draw(generators::vecs(generators::integers::<u8>()).max_size(32)));

    let (header, offset) = parse_header(&blob).unwrap();
    assert_eq!(
        header_bits(&header),
        header_bits(&GpbHeader {
            srs_id,
            envelope,
            empty,
            extended,
            byte_order: ByteOrder::Little,
        })
    );
    assert_eq!(offset, header_len(&envelope));
}

/// `encode_header_into` appends to a buffer exactly the bytes that
/// `encode_header` returns, and leaves what the buffer held before it.
#[hegel::test]
fn encode_header_into_appends_what_encode_header_returns(tc: hegel::TestCase) {
    let srs_id = tc.draw(generators::integers::<i32>());
    let envelope = draw_envelope(&tc);
    let (empty, extended) = (
        tc.draw(generators::booleans()),
        tc.draw(generators::booleans()),
    );
    let prefix = tc.draw(generators::vecs(generators::integers::<u8>()).max_size(16));
    let mut buffer = prefix.clone();
    encode_header_into(&mut buffer, srs_id, &envelope, empty, extended);
    let mut expected = prefix;
    expected.extend(encode_header(srs_id, &envelope, empty, extended));
    assert_eq!(buffer, expected);
}

/// A header in either byte order, with the reserved flag bits set or not,
/// parses to the values it was built from.
#[hegel::test]
fn a_header_in_either_byte_order_parses(tc: hegel::TestCase) {
    let srs_id = tc.draw(generators::integers::<i32>());
    let envelope = draw_envelope(&tc);
    let (empty, extended) = (
        tc.draw(generators::booleans()),
        tc.draw(generators::booleans()),
    );
    let byte_order = tc.draw(generators::sampled_from(vec![
        ByteOrder::Big,
        ByteOrder::Little,
    ]));
    let reserved = tc.draw(generators::integers::<u8>().min_value(0).max_value(3));
    let blob = encode_header_by_hand(srs_id, &envelope, empty, extended, byte_order, reserved);

    let (header, offset) = parse_header(&blob).unwrap();
    assert_eq!(
        header_bits(&header),
        header_bits(&GpbHeader {
            srs_id,
            envelope,
            empty,
            extended,
            byte_order,
        })
    );
    assert_eq!(offset, blob.len());
}

/// What a drawn near-valid header breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Defect {
    Magic,
    Version,
    Indicator,
    Truncated,
}

/// A header with one field broken has the error its defect documents.
#[hegel::test]
fn a_broken_header_has_its_documented_error(tc: hegel::TestCase) {
    let envelope = draw_envelope(&tc);
    let byte_order = tc.draw(generators::sampled_from(vec![
        ByteOrder::Big,
        ByteOrder::Little,
    ]));
    let mut blob = encode_header_by_hand(
        tc.draw(generators::integers::<i32>()),
        &envelope,
        tc.draw(generators::booleans()),
        tc.draw(generators::booleans()),
        byte_order,
        0,
    );
    let defect = tc.draw(generators::sampled_from(vec![
        Defect::Magic,
        Defect::Version,
        Defect::Indicator,
        Defect::Truncated,
    ]));
    let expected = match defect {
        Defect::Magic => {
            let magic = [
                tc.draw(generators::integers::<u8>()),
                tc.draw(generators::integers::<u8>()),
            ];
            tc.assume(magic != *b"GP");
            blob[..2].copy_from_slice(&magic);
            GpbError::BadMagic(magic[0], magic[1])
        }
        Defect::Version => {
            let version = tc.draw(generators::integers::<u8>().min_value(1));
            blob[2] = version;
            GpbError::UnsupportedVersion(version)
        }
        Defect::Indicator => {
            let indicator = tc.draw(generators::integers::<u8>().min_value(5).max_value(7));
            blob[3] = (blob[3] & !0b1110) | (indicator << 1);
            GpbError::InvalidEnvelopeIndicator(indicator)
        }
        Defect::Truncated => {
            let full = blob.len();
            let len = tc.draw(
                generators::integers::<usize>()
                    .min_value(0)
                    .max_value(full - 1),
            );
            blob.truncate(len);
            // Fewer than eight bytes is short of the fixed header; otherwise
            // the envelope the flags declare does not fit.
            GpbError::Truncated {
                expected: if len < 8 { 8 } else { full },
                actual: len,
            }
        }
    };
    assert_eq!(parse_header(&blob).unwrap_err(), expected, "{defect:?}");
}

/// Draws bytes: a valid header with a body, one with a single byte changed or
/// cut short, or any bytes at all.
fn draw_header_like_bytes(tc: &hegel::TestCase) -> Vec<u8> {
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(2)) {
        0 => tc.draw(generators::vecs(generators::integers::<u8>()).max_size(80)),
        kind => {
            let byte_order = tc.draw(generators::sampled_from(vec![
                ByteOrder::Big,
                ByteOrder::Little,
            ]));
            let mut blob = encode_header_by_hand(
                tc.draw(generators::integers::<i32>()),
                &draw_envelope(tc),
                tc.draw(generators::booleans()),
                tc.draw(generators::booleans()),
                byte_order,
                tc.draw(generators::integers::<u8>().min_value(0).max_value(3)),
            );
            if kind == 1 {
                let at = tc.draw(
                    generators::integers::<usize>()
                        .min_value(0)
                        .max_value(blob.len() - 1),
                );
                if let Some(byte) = blob.get_mut(at) {
                    *byte = tc.draw(generators::integers::<u8>());
                }
            }
            let len = tc.draw(
                generators::integers::<usize>()
                    .min_value(0)
                    .max_value(blob.len() + 8),
            );
            blob.resize(len, 0);
            blob
        }
    }
}

/// `body_offset` agrees with `parse_header` on any bytes: the same offset when
/// the header parses, and the same error when it does not.
#[hegel::test]
fn body_offset_agrees_with_parse_header(tc: hegel::TestCase) {
    let blob = draw_header_like_bytes(&tc);
    assert_eq!(
        body_offset(&blob),
        parse_header(&blob).map(|(_, offset)| offset)
    );
}
