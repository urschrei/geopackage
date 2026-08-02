//! Whether a threaded bounding-box-filtered columnar read would pay.
//!
//! M3 designed the threaded filtered read before building it: one thread
//! walks the RTree and hands candidate id blocks to workers, so the scan
//! happens once and no feature is returned twice. It also recorded the open
//! question, whether filtered results are typically large enough for any of
//! that to pay, given that fetching an arbitrary id list is index lookups
//! rather than the rowid range scans the unfiltered workers get. This
//! benchmark answers it with a measurement instead of a design argument.
//!
//! # What is measured
//!
//! Over one layer of diagonal points with two attributes, at bounding boxes
//! selecting about 1%, 10%, 50% and 100% of the rows:
//!
//! - `candidates_only`: stepping the RTree subquery and counting ids,
//!   touching no feature. Under the proposed design this work stays on one
//!   thread, so it is the serial floor: by Amdahl's argument, however many
//!   workers convert candidates, the filtered read cannot finish faster than
//!   this.
//! - `read_arrow_in`: the whole single-threaded filtered read as shipped,
//!   batches drained and row counts summed.
//!
//! For scale, two unfiltered references over the same layer:
//!
//! - `full/sequential`: `read_arrow` at one thread.
//! - `full/threaded`: `read_arrow` at four, the speedup the threaded
//!   machinery buys where its assumptions hold. If the filtered read's gap
//!   above its own serial floor is small relative to this, threading the
//!   filtered read has little to win.
//!
//! Row count defaults to 200,000; override with `GPKG_BENCH_ROWS`.
//!
//! Run: `cargo bench -p geopackage --features arrow --bench filtered`.

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, SamplingMode, criterion_group, criterion_main};
use geo_types::Point;
use geopackage::arrow::ArrowReadOptions;
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{
    BoundingBox, ColumnSpec, GeoPackage, GeometrySpec, NewFeature, TableSchemaBuilder, Value,
};

fn rows() -> i64 {
    std::env::var("GPKG_BENCH_ROWS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(200_000)
}

/// A layer of `count` points on a diagonal, spatially indexed, in a temp file.
fn build(count: i64) -> (tempfile::TempDir, GeoPackage) {
    let dir = tempfile::tempdir().expect("tempdir");
    let gpkg = GeoPackage::create(dir.path().join("bench.gpkg")).expect("create");
    gpkg.add_epsg_srs(4326).expect("srs");
    gpkg.create_layer(
        &TableSchemaBuilder::new("pts")
            .column(ColumnSpec::new("name", ColumnType::Text(None)))
            .column(ColumnSpec::new("rank", ColumnType::Integer))
            .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
    )
    .expect("layer");
    let layer = gpkg.layer("pts").expect("open");
    layer
        .write_all(
            (0..count).map(|i| {
                // The diagonal is scaled into a unit square so a bounding box
                // over `[0, f]` selects fraction `f` of the rows whatever the
                // row count.
                let t = i as f64 / count as f64;
                NewFeature::new(
                    Point::new(t, t),
                    vec![Value::Text(format!("p{i}")), Value::Integer(i % 10)],
                )
            }),
            10_000,
        )
        .expect("write");
    assert!(layer.has_spatial_index().expect("status"));
    (dir, gpkg)
}

/// A box selecting about `fraction` of the diagonal.
fn bbox(fraction: f64) -> BoundingBox {
    BoundingBox::new(0.0, 0.0, fraction, fraction)
}

fn drain(gpkg: &GeoPackage, bbox: BoundingBox) -> usize {
    let layer = gpkg.layer("pts").expect("open");
    layer
        .read_arrow_in(bbox, ArrowReadOptions::default())
        .expect("read")
        .map(|batch| batch.expect("batch").num_rows())
        .sum()
}

/// The RTree candidate scan alone: the work the proposed design cannot
/// parallelise. Widened exactly as the read widens.
fn candidates(gpkg: &GeoPackage, bbox: BoundingBox) -> i64 {
    gpkg.connection()
        .query_row(
            "SELECT count(id) FROM rtree_pts_geom \
             WHERE minx <= ?1 AND maxx >= ?2 AND miny <= ?3 AND maxy >= ?4",
            [bbox.max_x, bbox.min_x, bbox.max_y, bbox.min_y],
            |row| row.get(0),
        )
        .expect("scan")
}

fn bench(criterion: &mut Criterion) {
    let count = rows();
    let (_dir, gpkg) = build(count);

    let mut group = criterion.benchmark_group("filtered");
    group
        .sampling_mode(SamplingMode::Flat)
        .sample_size(10)
        .measurement_time(Duration::from_secs(5));

    for (label, fraction) in [
        ("1pct", 0.01),
        ("10pct", 0.1),
        ("50pct", 0.5),
        ("100pct", 1.0),
    ] {
        group.bench_function(format!("candidates_only/{label}"), |bencher| {
            bencher.iter(|| black_box(candidates(&gpkg, bbox(fraction))));
        });
        group.bench_function(format!("read_arrow_in/{label}"), |bencher| {
            bencher.iter(|| black_box(drain(&gpkg, bbox(fraction))));
        });
    }

    group.bench_function("full/sequential", |bencher| {
        bencher.iter(|| {
            let layer = gpkg.layer("pts").expect("open");
            let total: usize = layer
                .read_arrow(ArrowReadOptions::default().with_threads(1))
                .expect("read")
                .map(|batch| batch.expect("batch").num_rows())
                .sum();
            black_box(total)
        });
    });
    group.bench_function("full/threaded", |bencher| {
        bencher.iter(|| {
            let layer = gpkg.layer("pts").expect("open");
            let total: usize = layer
                .read_arrow(ArrowReadOptions::default().with_threads(4))
                .expect("read")
                .map(|batch| batch.expect("batch").num_rows())
                .sum();
            black_box(total)
        });
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
