//! Columnar against row-based reading (M3 criterion 1), over two workload
//! shapes.
//!
//! # Why two shapes
//!
//! What a columnar reader saves is mostly per-row, per-attribute handling, so
//! the answer depends on how much of a row is attributes and how much is
//! geometry. Measuring one shape and generalising is how a benchmark misleads.
//!
//! - `points_9attr`: point geometry, nine attributes. Attribute-heavy relative
//!   to its geometry, so it is close to the best case for a columnar reader.
//! - `polygons_13attr`: an 11-vertex footprint-like polygon, thirteen
//!   attributes. This is the shape GDAL's published figures come from (3.2M
//!   building footprints of 13 attributes, roadmap 05-m3 slide 10), so our
//!   ratios are comparable to theirs rather than to a workload of our choosing.
//!
//! # What is measured, per shape
//!
//! - `row/features`: [`geopackage::Layer::features`], which materialises the
//!   whole result set.
//! - `row/cursor`: [`geopackage::Layer::cursor`], which streams a row at a time
//!   and is the faster of the two row APIs. **This is the baseline**, because
//!   comparing against the slower of our own two paths would flatter the result.
//! - `sqlite/step_only`: stepping the same query, touching nothing. What SQLite
//!   charges to walk the rows at all.
//! - `sqlite/step_and_fetch`: stepping and fetching every value, building
//!   nothing. The gap above `step_only` is per-value accessor dispatch.
//! - `arrow/read_arrow`: the columnar path at the default batch size. The gap
//!   above `step_and_fetch` is array building.
//!
//! The two floors are what make the result actionable rather than a bare ratio:
//! they say how much of the time any Arrow-side work could ever remove, which is
//! what decides whether GDAL's aggregate-function technique is worth reaching
//! for (roadmap benchmarks/2026-07-25-gdal-arrow-techniques.md).
//!
//! All measurements read every row and every column of the same layer, from the
//! same file, on one thread.
//!
//! Row count defaults to 200,000; override with `GPKG_BENCH_ROWS`.
//!
//! Run: `cargo bench -p geopackage --features arrow --bench arrow`.

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, SamplingMode, Throughput, criterion_group, criterion_main};
use geo_types::{Geometry, LineString, Point, Polygon};
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

/// One of the four category strings, indexed without panicking.
fn category(i: usize) -> String {
    ["alpha", "beta", "gamma", "delta"]
        .get(i % 4)
        .copied()
        .unwrap_or("alpha")
        .to_owned()
}

/// A small closed ring of 11 vertices about `(x, y)`, in the shape of a
/// building footprint rather than a rectangle, so the WKB is representative of
/// the polygons GDAL measured.
fn footprint(x: f64, y: f64) -> Geometry<f64> {
    const VERTICES: usize = 10;
    let mut points: Vec<(f64, f64)> = (0..VERTICES)
        .map(|v| {
            let angle = (v as f64) * std::f64::consts::TAU / (VERTICES as f64);
            // A slightly irregular radius, so no two edges are identical.
            let radius = 0.000_5 + 0.000_2 * ((v % 3) as f64);
            (x + radius * angle.cos(), y + radius * angle.sin())
        })
        .collect();
    if let Some(&first) = points.first() {
        points.push(first);
    }
    Geometry::Polygon(Polygon::new(LineString::from(points), Vec::new()))
}

/// Build a layer of `n` rows, then close and reopen it, so the measurement is of
/// a file a caller opened rather than of a connection warmed by having just
/// written it (the same reason the `read` bench reopens).
fn build(
    n: usize,
    file: &str,
    columns: &[(&str, ColumnType)],
    geometry_type: GeometryType,
    mut row: impl FnMut(usize) -> (Geometry<f64>, Vec<Value>),
) -> (tempfile::TempDir, GeoPackage) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(file);
    {
        let gpkg = GeoPackage::create(&path).expect("create gpkg");
        let mut builder = TableSchemaBuilder::new("features");
        for (name, column_type) in columns {
            builder = builder.column(ColumnSpec::new(*name, column_type.clone()));
        }
        let builder = builder.geometry(GeometrySpec::new(geometry_type, 4326));
        let layer = gpkg.create_layer(&builder).expect("create layer");

        let features: Vec<NewFeature<Geometry<f64>>> = (0..n)
            .map(|i| {
                let (geometry, values) = row(i);
                NewFeature::new(geometry, values)
            })
            .collect();
        layer.write_all(features, 0).expect("write_all");
        gpkg.close().expect("close gpkg");
    }
    let gpkg = GeoPackage::open(&path).expect("reopen gpkg");
    (dir, gpkg)
}

/// The point layer: nine attributes of mixed type.
fn build_points(n: usize) -> (tempfile::TempDir, GeoPackage, Vec<&'static str>) {
    let columns: Vec<(&str, ColumnType)> = vec![
        ("name", ColumnType::Text(None)),
        ("category", ColumnType::Text(None)),
        ("count", ColumnType::Integer),
        ("code", ColumnType::MediumInt),
        ("ratio", ColumnType::Double),
        ("height", ColumnType::Float),
        ("active", ColumnType::Boolean),
        ("seen", ColumnType::DateTime),
        ("payload", ColumnType::Blob(None)),
    ];
    let stamp = DateTime::parse_strict("2026-07-24T12:34:56.789Z").expect("datetime");
    let (dir, gpkg) = build(n, "points.gpkg", &columns, GeometryType::Point, move |i| {
        let (x, y) = coord(i);
        let f = i as f64;
        (
            Geometry::Point(Point::new(x, y)),
            vec![
                Value::Text(format!("feature number {i}")),
                Value::Text(category(i)),
                Value::Integer(i as i64),
                Value::Integer((i % 30_000) as i64),
                Value::Float(f * 1.5),
                Value::Float(f * 0.25),
                Value::Boolean(i % 2 == 0),
                Value::DateTime(stamp),
                Value::Blob(vec![0xde, 0xad, 0xbe, 0xef]),
            ],
        )
    });
    let mut names: Vec<&'static str> = vec!["fid"];
    names.extend(columns.iter().map(|(name, _)| *name));
    names.push("geom");
    (dir, gpkg, names)
}

/// The polygon layer: thirteen attributes, matching the shape of GDAL's
/// published benchmark.
fn build_polygons(n: usize) -> (tempfile::TempDir, GeoPackage, Vec<&'static str>) {
    let columns: Vec<(&str, ColumnType)> = vec![
        ("name", ColumnType::Text(None)),
        ("category", ColumnType::Text(None)),
        ("source", ColumnType::Text(None)),
        ("quality", ColumnType::Text(None)),
        ("count", ColumnType::Integer),
        ("code", ColumnType::MediumInt),
        ("floors", ColumnType::SmallInt),
        ("built_year", ColumnType::SmallInt),
        ("area", ColumnType::Double),
        ("height", ColumnType::Float),
        ("active", ColumnType::Boolean),
        ("surveyed", ColumnType::DateTime),
        ("payload", ColumnType::Blob(None)),
    ];
    let stamp = DateTime::parse_strict("2026-07-24T12:34:56.789Z").expect("datetime");
    let (dir, gpkg) = build(
        n,
        "polygons.gpkg",
        &columns,
        GeometryType::Polygon,
        move |i| {
            let (x, y) = coord(i);
            let f = i as f64;
            (
                footprint(x, y),
                vec![
                    Value::Text(format!("building {i}")),
                    Value::Text(category(i)),
                    Value::Text("survey".to_owned()),
                    Value::Text(category(i + 1)),
                    Value::Integer(i as i64),
                    Value::Integer((i % 30_000) as i64),
                    Value::Integer((i % 40) as i64),
                    Value::Integer(1900 + (i % 125) as i64),
                    Value::Float(f * 1.5),
                    Value::Float(f * 0.25),
                    Value::Boolean(i % 2 == 0),
                    Value::DateTime(stamp),
                    Value::Blob(vec![0xde, 0xad, 0xbe, 0xef]),
                ],
            )
        },
    );
    let mut names: Vec<&'static str> = vec!["fid"];
    names.extend(columns.iter().map(|(name, _)| *name));
    names.push("geom");
    (dir, gpkg, names)
}

/// Register the five measurements for one workload shape.
fn bench_shape(c: &mut Criterion, shape: &str, gpkg: &GeoPackage, names: &[&str], n: usize) {
    let n64 = u64::try_from(n).expect("row count fits u64");
    let column_count = names.len();
    let sql = format!(
        "SELECT {} FROM features",
        names
            .iter()
            .map(|name| format!("\"{name}\""))
            .collect::<Vec<_>>()
            .join(",")
    );

    let mut group = c.benchmark_group(shape);
    group.sample_size(10);
    group.sampling_mode(SamplingMode::Flat);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(10));
    group.throughput(Throughput::Elements(n64));

    group.bench_function("row/features", |b| {
        let layer = gpkg.layer("features").expect("layer");
        b.iter(|| {
            let features = layer.features().expect("features");
            black_box(features.count())
        });
    });

    group.bench_function("row/cursor", |b| {
        let layer = gpkg.layer("features").expect("layer");
        b.iter(|| {
            let mut cursor = layer.cursor().expect("cursor");
            let features = cursor.features().expect("features");
            black_box(features.count())
        });
    });

    group.bench_function("sqlite/step_only", |b| {
        let conn = gpkg.connection();
        b.iter(|| {
            let mut stmt = conn.prepare_cached(&sql).expect("prepare");
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
            let mut stmt = conn.prepare_cached(&sql).expect("prepare");
            let mut rows = stmt.query([]).expect("query");
            let mut total = 0usize;
            while let Some(row) = rows.next().expect("step") {
                for column in 0..column_count {
                    black_box(row.get_ref(column).expect("get_ref"));
                }
                total += 1;
            }
            black_box(total)
        });
    });

    group.bench_function("arrow/read_arrow", |b| {
        let layer = gpkg.layer("features").expect("layer");
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

fn bench_columnar_vs_row(c: &mut Criterion) {
    let n = rows();

    let (_points_dir, points, point_names) = build_points(n);
    bench_shape(c, "points_9attr", &points, &point_names, n);
    drop(points);

    let (_polygons_dir, polygons, polygon_names) = build_polygons(n);
    bench_shape(c, "polygons_13attr", &polygons, &polygon_names, n);
}

criterion_group!(benches, bench_columnar_vs_row);
criterion_main!(benches);
