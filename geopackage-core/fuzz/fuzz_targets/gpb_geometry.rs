//! Fuzz the GPB geometry wrapper: parsing arbitrary bytes as a `GpbGeometry`,
//! then walking the body for its envelope and emptiness, must never panic.
//!
//! When parsing succeeds the geometry is a type the `wkb` crate could read, so
//! the envelope traversal and the declared-type helpers must all terminate
//! without panicking on any input. `wkb_geometry_type` is also exercised
//! directly on the raw body, including curve-type bodies the reader rejects.
//!
//! Known issue (not a wrapper bug): `wkb` 0.9.2's reader pre-allocates from an
//! untrusted element count (`Vec::with_capacity(num_geometries)` /
//! `num_rings`), so a body declaring an enormous count drives an out-of-memory
//! inside `GpbGeometry::parse` before any assertion here runs. This target
//! surfaces that OOM class deliberately; the fix is upstream in `wkb` (see the
//! roadmap item in `roadmap/03-m1-read-path.md`). Our own code never panics.

#![no_main]

use geopackage_core::GeometryType;
use geopackage_core::geometry::{self, GpbGeometry};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(geom) = GpbGeometry::parse(data) {
        // Envelope traversal and emptiness must not panic on any parsed body.
        let envelope = geom.xy_envelope();
        let empty = geom.is_empty();
        if let Some([min_x, max_x, min_y, max_y]) = envelope {
            // A finite envelope implies the header empty flag decides emptiness.
            assert_eq!(empty, geom.header().empty);
            assert!(min_x <= max_x && min_y <= max_y);
        } else {
            // No finite coordinate: the geometry is empty.
            assert!(empty);
        }

        // Declared-type helpers must not panic.
        let declared = geom.geometry_type();
        assert!(geom.matches_declared(declared));
        assert!(geom.matches_declared(GeometryType::Geometry));

        // The raw type-code reader must agree with the parsed reader for the
        // readable (non-curve) types, and never panic.
        if let Ok(raw) = geometry::wkb_geometry_type(geom.wkb_body()) {
            assert_eq!(raw, declared);
        }

        // Conversion to geo-types must not panic (empty points return None).
        let _ = geom.to_geo();
    }

    // The raw type-code reader over arbitrary bytes must also never panic.
    let _ = geometry::wkb_geometry_type(data);
});
