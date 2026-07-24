//! Fuzz the GPB header parser: must never panic on arbitrary input, and any
//! successfully parsed header must round-trip through the encoder.

#![no_main]

use geopackage_core::gpb::{encode_header, parse_header};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok((header, offset)) = parse_header(data) {
        assert!(offset <= data.len());
        // Re-encode (canonical LE) and re-parse: parsed values must survive.
        let reencoded = encode_header(
            header.srs_id,
            &header.envelope,
            header.empty,
            header.extended,
        );
        let (h2, off2) = parse_header(&reencoded).expect("re-encoded header must parse");
        assert_eq!(off2, reencoded.len());
        assert_eq!(h2.srs_id, header.srs_id);
        assert_eq!(h2.empty, header.empty);
        assert_eq!(h2.extended, header.extended);
        // NaN-safe envelope comparison via bit patterns.
        let bits = |e: &geopackage_core::Envelope| {
            e.values().iter().map(|v| v.to_bits()).collect::<Vec<_>>()
        };
        assert_eq!(bits(&h2.envelope), bits(&header.envelope));
        assert_eq!(h2.envelope.indicator(), header.envelope.indicator());
    }
});
