//! Allocation profile of the bulk RTree build (`create_spatial_index`).
//!
//! Instruction-count and wall-clock benches answer "how long"; this one answers
//! "how many allocations", which is what the packer's per-node `Vec`s are about.
//! It runs the build under Valgrind's DHAT tool via Gungraun, so the reported
//! `Total blocks` (allocation count) and `Total bytes` are exact and
//! deterministic rather than sampled.
//!
//! The fixture (an unindexed point layer) is built in the `setup` expression, so
//! its allocations are not attributed to the measured region. Only the
//! `create_spatial_index_with(always_bulk)` call is measured: the `ST_*`/Rust
//! envelope scan, the packing in `packed::pack_into`, the shadow-table writes,
//! and the gate.
//!
//! Run: `cargo bench -p geopackage --bench alloc`.

#![allow(
    missing_docs,
    reason = "the gungraun benchmark macros expand to undocumented public items"
)]

use std::hint::black_box;

use geo_types::{Geometry, Point};
use geopackage::core::types::GeometryType;
use geopackage::{
    BulkIndexOptions, GeoPackage, GeometrySpec, NewFeature, TableSchemaBuilder,
};
use gungraun::{
    Dhat, LibraryBenchmarkConfig, library_benchmark, library_benchmark_group, main,
};

/// Deterministic, well-spread coordinate for row `i` (matches the other benches
/// so the tree built here is the same shape).
fn coord(i: usize) -> (f64, f64) {
    let f = i as f64;
    let x = (f * 0.618_033_988_75).rem_euclid(360.0) - 180.0;
    let y = (f * 0.314_159_265_36).rem_euclid(180.0) - 90.0;
    (x, y)
}

/// Write an unindexed point layer `pts` of `n` rows into a fresh temp-file
/// GeoPackage, and hand back the still-open handle (and the temp dir that must
/// outlive it). Runs in `setup`, so none of this is measured.
fn fixture(n: usize) -> (tempfile::TempDir, GeoPackage) {
    let dir = tempfile::tempdir().expect("tempdir");
    let gpkg = GeoPackage::create(dir.path().join("bench.gpkg")).expect("create gpkg");
    let builder = TableSchemaBuilder::new("pts".to_owned())
        .geometry(GeometrySpec::new(GeometryType::Point, 4326));
    {
        let layer = gpkg.create_layer(&builder).expect("create layer");
        let features: Vec<NewFeature<Geometry<f64>>> = (0..n)
            .map(|i| {
                let (x, y) = coord(i);
                NewFeature::new(Geometry::Point(Point::new(x, y)), Vec::new())
            })
            .collect();
        // Never bulk here: we want the rows present but the index absent, so the
        // measured call is a from-scratch bulk build over an existing table.
        layer
            .write_all_with(features, 0, BulkIndexOptions::never_bulk())
            .expect("write_all");
    }
    (dir, gpkg)
}

#[library_benchmark]
#[bench::points_100k(fixture(100_000))]
fn build_index(fixture: (tempfile::TempDir, GeoPackage)) {
    let (dir, gpkg) = fixture;
    let layer = gpkg.layer("pts").expect("open layer");
    layer
        .create_spatial_index_with(black_box(BulkIndexOptions::always_bulk()))
        .expect("build index");
    // Keep the temp dir alive across the measured build, then drop it here.
    drop(dir);
}

library_benchmark_group!(name = alloc; benchmarks = build_index);

main!(
    config = LibraryBenchmarkConfig::default().tool(Dhat::default());
    library_benchmark_groups = alloc
);
