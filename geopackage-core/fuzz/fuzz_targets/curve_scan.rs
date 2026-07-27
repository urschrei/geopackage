//! Fuzz the non-linear geometry walker: reading arbitrary bytes as an ISO WKB
//! body, and the arc primitive over arbitrary coordinates, must never panic.
//!
//! This is the parser with the most exposure per line in the workspace. Unlike
//! the rest of the read path it does not go through the `wkb` crate: it walks
//! the WKB structure itself, doing its own bounds arithmetic over lengths and
//! counts taken from the input. It is reachable from any file this crate opens,
//! not only ones it wrote, because `ST_MinX` and the `features_in` re-filter
//! both route non-linear bodies through it.
//!
//! Unlike `gpb_geometry`, this target should stay clean: the walker allocates
//! nothing, so an enormous declared count fails on the first read past the end
//! of the buffer rather than reserving for it. That is the property the `wkb`
//! reader lacks (see the known issue on `gpb_geometry`), so a memory blow-up
//! here would be our bug rather than an upstream one.

#![no_main]

use geopackage_core::curve;
use libfuzzer_sys::fuzz_target;

/// The `index`-th little-endian `f64` of `data`, if it has that many.
fn f64_at(data: &[u8], index: usize) -> Option<f64> {
    let start = index.checked_mul(8)?;
    let end = start.checked_add(8)?;
    let bytes: [u8; 8] = data.get(start..end)?.try_into().ok()?;
    Some(f64::from_le_bytes(bytes))
}

/// Whether a control point contributes to a bounding box: both ordinates
/// finite, matching the walker's non-finite policy.
fn contributes(point: [f64; 2]) -> bool {
    let [x, y] = point;
    x.is_finite() && y.is_finite()
}

fuzz_target!(|data: &[u8]| {
    // --- the WKB walk ---
    if let Ok(scan) = curve::scan(data) {
        // The reported extent is inside the input it was read from.
        assert!(scan.len <= data.len());

        // Emptiness and the two envelope forms are one fact in three places.
        assert_eq!(scan.empty, scan.xy_envelope.is_none());
        assert_eq!(
            scan.empty,
            scan.envelope == geopackage_core::gpb::Envelope::None
        );

        if let Some([min_x, max_x, min_y, max_y]) = scan.xy_envelope {
            assert!(min_x <= max_x && min_y <= max_y);
            assert!(min_x.is_finite() && max_x.is_finite());
            assert!(min_y.is_finite() && max_y.is_finite());
            // The header form must carry the same bounds.
            let (hx0, hx1, hy0, hy1) = scan
                .envelope
                .xy_bounds()
                .expect("a non-empty scan has envelope bounds");
            assert_eq!([hx0, hx1, hy0, hy1], [min_x, max_x, min_y, max_y]);
        }

        // The convenience entry point is the same walk with less kept.
        assert_eq!(curve::xy_envelope(data).ok().flatten(), scan.xy_envelope);

        // Trailing bytes beyond the geometry change nothing about it, so
        // truncating to the reported extent is the same read.
        if let Some(exact) = data.get(..scan.len) {
            let again = curve::scan(exact).expect("the geometry's own bytes still read");
            assert_eq!(again.xy_envelope, scan.xy_envelope);
            assert_eq!(again.dimensions, scan.dimensions);
            assert_eq!(again.len, scan.len);
        }
    } else {
        // A rejected body is rejected by both entry points.
        assert!(curve::xy_envelope(data).is_err());
    }

    // --- the arc primitive ---
    //
    // Fed raw bit patterns rather than plausible coordinates, so NaN, the
    // infinities, subnormals and huge magnitudes all arrive.
    if let (Some(x0), Some(y0), Some(x1), Some(y1), Some(x2), Some(y2)) = (
        f64_at(data, 0),
        f64_at(data, 1),
        f64_at(data, 2),
        f64_at(data, 3),
        f64_at(data, 4),
        f64_at(data, 5),
    ) {
        let p0 = [x0, y0];
        let p1 = [x1, y1];
        let p2 = [x2, y2];
        let envelope = curve::arc_envelope(p0, p1, p2);

        let any_finite = [p0, p1, p2].into_iter().any(contributes);
        assert_eq!(envelope.is_some(), any_finite);

        if let Some([min_x, max_x, min_y, max_y]) = envelope {
            assert!(min_x <= max_x && min_y <= max_y);
            assert!(min_x.is_finite() && max_x.is_finite());
            assert!(min_y.is_finite() && max_y.is_finite());

            // Every control point that counts is inside the box. The arc's own
            // extremes can only widen it further, so this is the floor rather
            // than the whole guarantee.
            for point in [p0, p1, p2] {
                if contributes(point) {
                    let [x, y] = point;
                    assert!(x >= min_x && x <= max_x, "control point x {x} outside box");
                    assert!(y >= min_y && y <= max_y, "control point y {y} outside box");
                }
            }
        }
    }
});
