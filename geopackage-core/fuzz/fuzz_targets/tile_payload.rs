//! Fuzz the tile payload probe, the coverage TIFF profile check and the tile
//! matrix rules: none may panic on arbitrary input, a probe that succeeds must
//! agree with the size check built from its own answer, and the two readers of
//! a TIFF header must agree with each other.
//!
//! A tile payload is the one part of a GeoPackage this crate reads without
//! having written it and without a schema to constrain it: whatever bytes a
//! `tile_data` column contains are handed to `probe` as they are, and a
//! coverage payload reaches `coverage_tiff` the same way.

#![no_main]

use geopackage_core::coverage;
use geopackage_core::tiles::{self, TileFormat, TileMatrix, TileMatrixSet};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The profile check reads the same header the probe does, and refuses or
    // accepts without panicking whatever the bytes are.
    let coverage = coverage::coverage_tiff(data);
    let probed = tiles::probe(data);

    // A payload the coverage profile accepts is a TIFF the probe also reads,
    // at the size the profile read from the same IFD. Two readers of one
    // header disagreeing is the bug this assertion exists to catch: it would
    // mean a conforming coverage tile that the tile size check refuses. The
    // probe delegates TIFF to the profile's own walker, which is what makes
    // the two agree by construction rather than by luck.
    if let Ok(coverage) = &coverage {
        let payload = probed
            .as_ref()
            .expect("a payload the coverage profile accepts is a readable TIFF");
        assert_eq!(payload.format, TileFormat::Tiff);
        assert_eq!(
            (payload.width, payload.height),
            (coverage.width, coverage.height)
        );
    }

    if let Ok(payload) = probed {
        assert!(payload.width >= 0 && payload.height >= 0);
        // A matrix declaring exactly what the payload reports must accept it,
        // and one declaring a different width must not.
        let matching = TileMatrix::new(0, 1, 1, payload.width, payload.height, 1.0, 1.0);
        matching.check_payload(&payload).expect("its own size fits");
        let mismatched = TileMatrix::new(
            0,
            1,
            1,
            payload.width.wrapping_add(1),
            payload.height,
            1.0,
            1.0,
        );
        assert!(mismatched.check_payload(&payload).is_err());
    }

    // The same bytes, read as a tile matrix: the rules must reject or accept
    // without panicking, whatever the values are.
    if data.len() >= 56 {
        let value = |offset: usize| {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&data[offset..offset + 8]);
            bytes
        };
        let matrix_set = TileMatrixSet::new(
            0,
            f64::from_le_bytes(value(0)),
            f64::from_le_bytes(value(8)),
            f64::from_le_bytes(value(16)),
            f64::from_le_bytes(value(24)),
        );
        let matrix = TileMatrix::new(
            i64::from_le_bytes(value(32)),
            i64::from_le_bytes(value(40)),
            i64::from_le_bytes(value(48)),
            256,
            256,
            f64::from_le_bytes(value(8)),
            f64::from_le_bytes(value(16)),
        );
        let _ = matrix_set.validate(&[matrix]);
        let _ = tiles::is_power_of_two_ladder(&[matrix]);
        let _ = matrix_set.tile_bounds(&matrix, matrix.matrix_width, matrix.matrix_height);
        let _ = matrix_set.tile_at(&matrix, matrix_set.min_x, matrix_set.max_y);
        let _ = matrix_set.tile_range(
            &matrix,
            matrix_set.min_x,
            matrix_set.min_y,
            matrix_set.max_x,
            matrix_set.max_y,
        );
    }
});
