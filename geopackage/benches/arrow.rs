//! Columnar against row-based reading: M3 acceptance criterion 1.
//!
//! The criterion is that `read_arrow` on one thread reads a layer at least 3x
//! faster than this crate's own row-based full scan of the same file. That is
//! the ratio GDAL reports for its GeoPackage driver (6.6 s to 2.2 s, roadmap
//! 05-m3), and it is the number that fails loudly if the columnar path ever ends
//! up layered over the row path.
//!
//! Three benchmarks over one file:
//!
//! - `row/features`: [`geopackage::Layer::features`], which materialises the
//!   whole result set.
//! - `row/cursor`: [`geopackage::Layer::cursor`], which streams a row at a time
//!   and is the faster of the two row APIs. **This is the baseline the criterion
//!   is measured against**, because comparing against the slower of our own two
//!   paths would flatter the result.
//! - `arrow/read_arrow`: the columnar path at the default batch size.
//!
//! All three read every row and every column of the same layer, from the same
//! file, on one thread. Like for like is the whole point: M2 had to withdraw a
//! GDAL-parity claim that rested on a figure which also included GDAL reading a
//! source file.
//!
//! The layer is deliberately not geometry-only. Most of what a columnar reader
//! saves is per-row attribute handling, so a layer with no attributes would
//! measure the wrong thing; GDAL's own figures come from 13 attributes per row.
//! This uses nine, of mixed type.
//!
//! Row count defaults to 200,000; override with `GPKG_BENCH_ROWS`.
//!
//! Run: `cargo bench -p geopackage --features arrow --bench arrow`.

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, SamplingMode, Throughput, criterion_group, criterion_main};
use geo_types::{Geometry, Point};
use geopackage::arrow::ArrowReadOptions;
use geopackage::core::datetime::DateTime;
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{ColumnSpec, GeoPackage, GeometrySpec, NewFeature, TableSchemaBuilder, Value};

fn rows() -> usize {
    std::env::var("GPKG_BENCH_ROWS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200_000)
}

/// Deterministic coordinate for row `i` over the WGS 84 domain (matches the
/// other read benches).
fn coord(i: usize) -> (f64, f64) {
    let f = i as f64;
    let x = (f * 0.618_033_988_75).rem_euclid(360.0) - 180.0;
    let y = (f * 0.314_159_265_36).rem_euclid(180.0) - 90.0;
    (x, y)
}

/// Build a point layer of `n` rows with nine attributes, then close and reopen
/// it, so the measurement is of a file a caller opened rather than of a
/// connection warmed by having just written it (the same reason the `read`
/// bench reopens).
fn build(n: usize) -> (tempfile::TempDir, GeoPackage) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("arrow.gpkg");
    {
        let gpkg = GeoPackage::create(&path).expect("create gpkg");
        let builder = TableSchemaBuilder::new("pts")
            .column(ColumnSpec::new("name", ColumnType::Text(None)))
            .column(ColumnSpec::new("category", ColumnType::Text(None)))
            .column(ColumnSpec::new("count", ColumnType::Integer))
            .column(ColumnSpec::new("code", ColumnType::MediumInt))
            .column(ColumnSpec::new("ratio", ColumnType::Double))
            .column(ColumnSpec::new("height", ColumnType::Float))
            .column(ColumnSpec::new("active", ColumnType::Boolean))
            .column(ColumnSpec::new("seen", ColumnType::DateTime))
            .column(ColumnSpec::new("payload", ColumnType::Blob(None)))
            .geometry(GeometrySpec::new(GeometryType::Point, 4326));
        let layer = gpkg.create_layer(&builder).expect("create layer");

        let stamp = DateTime::parse_strict("2026-07-24T12:34:56.789Z").expect("datetime");
        let features: Vec<NewFeature<Geometry<f64>>> = (0..n)
            .map(|i| {
                let (x, y) = coord(i);
                let f = i as f64;
                NewFeature::new(
                    Geometry::Point(Point::new(x, y)),
                    vec![
                        Value::Text(format!("feature number {i}")),
                        Value::Text(
                            ["alpha", "beta", "gamma", "delta"]
                                .get(i % 4)
                                .copied()
                                .unwrap_or("alpha")
                                .to_owned(),
                        ),
                        Value::Integer(i as i64),
                        Value::Integer((i % 30_000) as i64),
                        Value::Float(f * 1.5),
                        Value::Float(f * 0.25),
                        Value::Boolean(i % 2 == 0),
                        Value::DateTime(stamp),
                        Value::Blob(vec![0xde, 0xad, 0xbe, 0xef]),
                    ],
                )
            })
            .collect();
        layer.write_all(features, 0).expect("write_all");
        gpkg.close().expect("close gpkg");
    }
    let gpkg = GeoPackage::open(&path).expect("reopen gpkg");
    (dir, gpkg)
}

fn bench_columnar_vs_row(c: &mut Criterion) {
    let n = rows();
    let (_dir, gpkg) = build(n);
    let n64 = u64::try_from(n).expect("row count fits u64");

    let mut group = c.benchmark_group("arrow_vs_row");
    group.sample_size(10);
    group.sampling_mode(SamplingMode::Flat);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(10));
    group.throughput(Throughput::Elements(n64));

    group.bench_function("row/features", |b| {
        let layer = gpkg.layer("pts").expect("layer");
        b.iter(|| {
            let features = layer.features().expect("features");
            black_box(features.count())
        });
    });

    group.bench_function("row/cursor", |b| {
        let layer = gpkg.layer("pts").expect("layer");
        b.iter(|| {
            let mut cursor = layer.cursor().expect("cursor");
            let features = cursor.features().expect("features");
            black_box(features.count())
        });
    });

    // Two floors, to attribute the remaining time. `step_only` is what SQLite
    // charges to walk the rows at all; `step_and_fetch` adds the per-value
    // accessor dispatch. Whatever `read_arrow` costs above `step_and_fetch` is
    // the array building, and whatever `step_and_fetch` costs is the part no
    // amount of work on the Arrow side can remove. This is the measurement that
    // decides whether GDAL's aggregate-function technique is worth reaching for
    // (roadmap benchmarks/2026-07-25-gdal-arrow-techniques.md).
    let sql = "SELECT fid, name, category, count, code, ratio, height, active, seen, payload, geom \
               FROM pts";

    group.bench_function("sqlite/step_only", |b| {
        let conn = gpkg.connection();
        b.iter(|| {
            let mut stmt = conn.prepare_cached(sql).expect("prepare");
            let mut rows = stmt.query([]).expect("query");
            let mut total = 0usize;
            while rows.next().expect("step").is_some() {
                total += 1;
            }
            black_box(total)
        });
    });

    group.bench_function("sqlite/step_and_fetch", |b| {
        let conn = gpkg.connection();
        b.iter(|| {
            let mut stmt = conn.prepare_cached(sql).expect("prepare");
            let mut rows = stmt.query([]).expect("query");
            let mut total = 0usize;
            while let Some(row) = rows.next().expect("step") {
                for column in 0..11 {
                    black_box(row.get_ref(column).expect("get_ref"));
                }
                total += 1;
            }
            black_box(total)
        });
    });

    group.bench_function("arrow/read_arrow", |b| {
        let layer = gpkg.layer("pts").expect("layer");
        b.iter(|| {
            let batches = layer
                .read_arrow(ArrowReadOptions::default())
                .expect("read_arrow");
            let mut total = 0usize;
            for batch in batches {
                total += batch.expect("batch").num_rows();
            }
            black_box(total)
        });
    });

    group.finish();
}

criterion_group!(benches, bench_columnar_vs_row);
criterion_main!(benches);
