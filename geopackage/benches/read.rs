//! Read-path throughput benchmarks (M2 acceptance criterion 4).
//!
//! Two point layers of the same data are built once (kept alive for the whole
//! run): one with a 1.4 spatial index, one without. The benchmarks then measure
//!
//! - `full_scan`:            [`geopackage::Layer::features`] over every row.
//! - `features_in/index_small`: a small bounding-box query served by the RTree.
//! - `features_in/scan_small`:  the same box answered by a full-scan filter
//!   (no index), the honest baseline the index is measured against.
//! - `features_in/index_full`:  a box covering the whole layer, served by the
//!   RTree (index traversal plus materialising every row).
//!
//! Row count defaults to 1,000,000; override with `GPKG_BENCH_ROWS`.
//!
//! Run: `cargo bench -p geopackage --bench read`.

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, SamplingMode, Throughput, criterion_group, criterion_main};
use geo_types::{Geometry, Point};
use geopackage::core::types::GeometryType;
use geopackage::{BoundingBox, GeoPackage, GeometrySpec, NewFeature, TableSchemaBuilder};

fn rows() -> usize {
    std::env::var("GPKG_BENCH_ROWS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000_000)
}

/// Deterministic coordinate for row `i` over the WGS 84 domain (matches the
/// write bench).
fn coord(i: usize) -> (f64, f64) {
    let f = i as f64;
    let x = (f * 0.618_033_988_75).rem_euclid(360.0) - 180.0;
    let y = (f * 0.314_159_265_36).rem_euclid(180.0) - 90.0;
    (x, y)
}

/// Build a point GeoPackage of `n` rows, optionally with a spatial index, then
/// close it and reopen it for reading. The returned `TempDir` must outlive the
/// handle.
///
/// The reopen is deliberate. Querying through the same connection that built
/// the index measures a connection warmed as a side effect of index
/// construction rather than the query itself: with the pre-0.2 gate, the
/// build's whole-database `PRAGMA integrity_check` read every page and left
/// index queries roughly 4x faster than on a freshly opened file, for a
/// byte-identical index. Reopening measures what a caller querying an existing
/// `.gpkg` actually gets, and makes the figures independent of how the fixture
/// was built.
fn build(n: usize, indexed: bool) -> (tempfile::TempDir, GeoPackage) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("read.gpkg");
    {
        let gpkg = GeoPackage::create(&path).expect("create gpkg");
        let builder = TableSchemaBuilder::new("pts")
            .geometry(GeometrySpec::new(GeometryType::Point, 4326))
            // The `indexed` argument decides, not the default.
            .spatial_index(false);
        let layer = gpkg.create_layer(&builder).expect("create layer");
        let features: Vec<NewFeature<Geometry<f64>>> = (0..n)
            .map(|i| {
                let (x, y) = coord(i);
                NewFeature::new(Geometry::Point(Point::new(x, y)), Vec::new())
            })
            .collect();
        layer.write_all(features, 0).expect("write_all");
        if indexed {
            layer.create_spatial_index().expect("create spatial index");
        }
        gpkg.close().expect("close gpkg");
    }
    let gpkg = GeoPackage::open(&path).expect("reopen gpkg");
    (dir, gpkg)
}

fn bench_reads(c: &mut Criterion) {
    let n = rows();
    let (_indexed_dir, indexed) = build(n, true);
    let (_plain_dir, plain) = build(n, false);

    // A box covering ~0.25% of the domain (5% of each axis about the origin).
    let small = BoundingBox::new(-9.0, -4.5, 9.0, 4.5);
    // A box covering the whole domain: every row matches.
    let full = BoundingBox::new(-200.0, -100.0, 200.0, 100.0);

    let n64 = u64::try_from(n).expect("row count fits u64");

    let mut group = c.benchmark_group("read");
    group.sample_size(10);
    group.sampling_mode(SamplingMode::Flat);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(10));

    group.throughput(Throughput::Elements(n64));
    group.bench_function("full_scan", |b| {
        let layer = plain.layer("pts").expect("layer");
        b.iter(|| {
            let features = layer.features().expect("features");
            black_box(features.count())
        });
    });
    group.bench_function("features_in/index_full", |b| {
        let layer = indexed.layer("pts").expect("layer");
        assert!(layer.has_spatial_index().expect("index present"));
        b.iter(|| {
            let features = layer.features_in(full).expect("features_in");
            black_box(features.count())
        });
    });

    // The small-box queries return a subset; throughput is per candidate scanned
    // (the whole layer for the scan form, the index result for the index form),
    // so report elements = n for a comparable elements/second figure.
    group.bench_function("features_in/index_small", |b| {
        let layer = indexed.layer("pts").expect("layer");
        assert!(layer.has_spatial_index().expect("index present"));
        b.iter(|| {
            let features = layer.features_in(small).expect("features_in");
            black_box(features.count())
        });
    });
    group.bench_function("features_in/scan_small", |b| {
        let layer = plain.layer("pts").expect("layer");
        assert!(!layer.has_spatial_index().expect("no index"));
        b.iter(|| {
            let features = layer.features_in(small).expect("features_in");
            black_box(features.count())
        });
    });

    group.finish();
}

criterion_group!(benches, bench_reads);
criterion_main!(benches);
