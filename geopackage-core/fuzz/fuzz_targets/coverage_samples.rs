//! Fuzz the coverage sample decode and the value lookup over its result. They
//! must not panic on arbitrary input. A decode that succeeds must agree with
//! the header check, and every sample a lookup reads must be in the tile.
//!
//! The decoders read `tile_data` BLOBs that this crate did not write, as the
//! header check does in `tile_payload`, so they get the same treatment.

#![no_main]

use geopackage_core::coverage::{
    self, CoverageAncillary, Interpolation, SampleLookup, TileAncillary,
};
use geopackage_core::tiles::{TileCoord, TileMatrix, TileMatrixSet};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(samples) = coverage::decode_samples(data) else {
        return;
    };
    let header = coverage::coverage_payload(data).expect("a decoded payload has a valid header");
    let (width, height) = (samples.width(), samples.height());
    assert_eq!(
        (i64::try_from(width), i64::try_from(height)),
        (Ok(header.width()), Ok(header.height()))
    );
    assert_eq!(Some(samples.as_slice().len()), width.checked_mul(height));

    let mut ancillary = CoverageAncillary::new("fuzz", "float");
    ancillary.data_null = samples.as_slice().first().copied();
    let _ = samples.check_finite();
    if let Some(statistics) = samples.statistics(&ancillary, &TileAncillary::defaults()) {
        assert!(statistics.count > 0 && statistics.count <= samples.as_slice().len());
    }

    // One tile over an extent of one unit per sample, looked up at a position
    // taken from the same bytes, under each cell encoding.
    let (Ok(tile_width), Ok(tile_height)) = (i64::try_from(width), i64::try_from(height)) else {
        return;
    };
    let set = TileMatrixSet::new(0, 0.0, 0.0, width as f64, height as f64);
    let matrix = TileMatrix::new(0, 1, 1, tile_width, tile_height, 1.0, 1.0);
    let coordinate = |offset: usize| {
        data.get(offset..offset + 8)
            .map_or(0.5, |bytes| f64::from_le_bytes(bytes.try_into().unwrap()))
    };
    let (x, y) = (coordinate(0), coordinate(8));
    for encoding in [None, Some("grid-value-is-area"), Some("grid-value-is-corner")] {
        ancillary.grid_cell_encoding = encoding.map(str::to_owned);
        for interpolation in [Interpolation::Nearest, Interpolation::Bilinear] {
            let Some(lookup) = SampleLookup::new(&set, &matrix, &ancillary, x, y, interpolation)
            else {
                continue;
            };
            for sample in lookup.samples() {
                assert!(sample.weight > 0.0 && sample.weight <= 1.0, "{sample:?}");
                let (coord, column, row) = sample.tile(&matrix).expect("a positive tile size");
                assert_eq!(coord, TileCoord::new(0, 0, 0));
                assert!(column < width && row < height, "{sample:?}");
            }
            let _ = lookup.value(|sample| {
                let (_, column, row) = sample.tile(&matrix).expect("a positive tile size");
                Ok::<_, ()>(samples.value(column, row, &ancillary, &TileAncillary::defaults()))
            });
        }
    }
});
