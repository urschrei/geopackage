//! Handle construction cost, against the per-call work it would be repeated
//! for (M5 phase 9).
//!
//! `Layer<'a>` and `TilePyramid<'a>` borrow the `GeoPackage` they came from, so
//! a C ABI handle cannot hold one: the borrow is exactly the use-after-free the
//! lifetime prevents. The alternative to making them owned is for the FFI to
//! hold the parent and rebuild the handle on every call, which is what this
//! measures the cost of.
//!
//! Construction is a near-constant cost (a `gpkg_contents` lookup, a table-name
//! resolution, a `PRAGMA table_info`, a `gpkg_data_columns` query and a
//! `Vec<Column>` clone), so the figure that decides anything is not
//! construction on its own but its ratio against the call it would wrap. The
//! groups below are therefore paired: what a rebuild costs, and what the
//! operations cost that a rebuild would precede.
//!
//! `features_in` is measured at four result sizes because the ratio moves with
//! them: a query returning a thousand rows amortises a rebuild that a query
//! returning one does not, and small result sets are the ordinary case for an
//! interactive map or a feature-info request.
//!
//! `construct/arc_clone_and_drop` is the other side of the trade: what an owned
//! handle would add per construction, if `Layer` and `TilePyramid` held an
//! `Arc<GeoPackage>` instead of a reference.
//!
//! Wall-clock figures, and most of the wall clock is SQLite.
//!
//! Run: `cargo bench -p geopackage --bench handles`.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use geo_types::Point;
use geopackage::core::tiles::{TileCoord, TileMatrixSet, ZoomLadder};
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{
    BoundingBox, ColumnSpec, GeoPackage, GeometrySpec, NewFeature, TableSchemaBuilder,
    TilePyramidBuilder, Value,
};

/// Rows in the feature layer the query group runs against.
const ROWS: i32 = 100_000;

/// Payload bytes per tile, matching the `tiles` bench so the figures compare.
const TILE_BYTES: usize = 4096;

/// Deepest zoom of the benchmark pyramid, and the side of its populated grid.
const TILE_ZOOM: i64 = 7;
const TILE_GRID: i64 = 64;

/// A 256-pixel PNG header padded to [`TILE_BYTES`].
fn tile_payload() -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&13_u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&256_u32.to_be_bytes());
    bytes.extend_from_slice(&256_u32.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    bytes.resize(TILE_BYTES, 0x5A);
    bytes
}

/// A feature file with a narrow and a wide layer, and [`ROWS`] points spread
/// over a unit square so a bounding box of a given area returns a predictable
/// share of them.
fn feature_file(path: std::path::PathBuf) -> GeoPackage {
    let gpkg = GeoPackage::create(path).expect("create gpkg");

    let narrow = TableSchemaBuilder::new("narrow")
        .column(ColumnSpec::new("name", ColumnType::Text(None)))
        .geometry(GeometrySpec::new(GeometryType::Point, 4326));
    gpkg.create_layer(&narrow).expect("create narrow layer");

    // 54 columns, the shape of the README's `admin` dataset. Construction cost
    // scales with column count, so the wide case is what an introspection-heavy
    // consumer pays.
    let mut wide = TableSchemaBuilder::new("wide");
    for i in 0..52 {
        wide = wide.column(ColumnSpec::new(
            format!("c{i}"),
            if i % 2 == 0 {
                ColumnType::Text(None)
            } else {
                ColumnType::Double
            },
        ));
    }
    gpkg.create_layer(&wide.geometry(GeometrySpec::new(GeometryType::Point, 4326)))
        .expect("create wide layer");

    gpkg.layer("narrow")
        .expect("open narrow layer")
        .write_all(
            (0..ROWS)
                .map(|i| {
                    NewFeature::new(
                        Point::new(f64::from(i % 1000) / 1000.0, f64::from(i / 1000) / 100.0),
                        vec![Value::Text("p".into())],
                    )
                })
                .collect::<Vec<_>>(),
            10_000,
        )
        .expect("write points");
    gpkg
}

/// A web mercator pyramid whose deepest level is fully populated.
fn tile_file(path: std::path::PathBuf) -> (GeoPackage, Vec<TileCoord>) {
    let gpkg = GeoPackage::create(path).expect("create gpkg");
    gpkg.add_epsg_srs(3857).expect("register 3857");
    let matrix_set = TileMatrixSet::web_mercator_quad();
    let matrices = matrix_set
        .ladder(ZoomLadder::new(0, TILE_ZOOM))
        .expect("ladder");
    let pyramid = gpkg
        .create_tile_pyramid(&TilePyramidBuilder::new("basemap", matrix_set).matrices(matrices))
        .expect("create pyramid");

    let payload = tile_payload();
    let coords: Vec<TileCoord> = (0..TILE_GRID)
        .flat_map(|col| (0..TILE_GRID).map(move |row| TileCoord::new(TILE_ZOOM, col, row)))
        .collect();
    for coord in &coords {
        pyramid.put_tile(*coord, &payload).expect("put tile");
    }
    drop(pyramid);
    (gpkg, coords)
}

fn benchmark(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let gpkg = feature_file(dir.path().join("features.gpkg"));
    let layer = gpkg.layer("narrow").expect("open narrow layer");

    let mut construction = c.benchmark_group("construct");
    construction.measurement_time(Duration::from_secs(8));
    construction.bench_function("layer/narrow_4_columns", |b| {
        b.iter(|| black_box(gpkg.layer("narrow").expect("open narrow layer")));
    });
    construction.bench_function("layer/wide_54_columns", |b| {
        b.iter(|| black_box(gpkg.layer("wide").expect("open wide layer")));
    });
    // The other side of the trade: what owning would add per construction.
    let arc = Arc::new(0u8);
    construction.bench_function("arc_clone_and_drop", |b| {
        b.iter(|| black_box(Arc::clone(black_box(&arc))));
    });
    construction.finish();

    let mut cheap = c.benchmark_group("cheap_calls");
    cheap.measurement_time(Duration::from_secs(8));
    cheap.bench_function("layer/extent", |b| {
        b.iter(|| black_box(layer.extent().expect("extent")));
    });
    cheap.bench_function("layer/has_spatial_index", |b| {
        b.iter(|| black_box(layer.has_spatial_index().expect("index check")));
    });
    cheap.finish();

    // The ratio against a rebuild moves with result size, so all four are here
    // rather than one representative figure.
    let mut queries = c.benchmark_group("features_in");
    queries.measurement_time(Duration::from_secs(8));
    for (label, bbox) in [
        ("1_row", BoundingBox::new(0.0, 0.0, 0.0005, 0.005)),
        ("15_rows", BoundingBox::new(0.0, 0.0, 0.004, 0.02)),
        ("126_rows", BoundingBox::new(0.0, 0.0, 0.02, 0.05)),
        ("1071_rows", BoundingBox::new(0.0, 0.0, 0.05, 0.2)),
    ] {
        queries.bench_function(label, |b| {
            b.iter(|| black_box(layer.features_in(bbox).expect("query").count()));
        });
    }
    queries.finish();

    drop(layer);
    drop(gpkg);

    // Tiles: `get_tile` is the most call-heavy entry point in the crate, so it
    // is where a per-call rebuild costs most.
    let (tile_gpkg, coords) = tile_file(dir.path().join("tiles.gpkg"));
    let pyramid = tile_gpkg.tiles("basemap").expect("open pyramid");

    let mut tiles = c.benchmark_group("tiles");
    tiles.measurement_time(Duration::from_secs(8));
    tiles.bench_function("construct/pyramid", |b| {
        b.iter(|| black_box(tile_gpkg.tiles("basemap").expect("open pyramid")));
    });
    // Cycled rather than indexed: the addresses are visited in a scattered
    // order without the benchmark doing bounds checks of its own.
    let mut addresses = coords.iter().cycle();
    tiles.bench_function("get_tile", |b| {
        b.iter(|| {
            let coord = *addresses.next().expect("cycle over a non-empty slice");
            black_box(pyramid.get_tile(coord).expect("get_tile"))
        });
    });
    let mut buffer = Vec::with_capacity(TILE_BYTES * 2);
    let mut into_addresses = coords.iter().cycle();
    tiles.bench_function("get_tile_into", |b| {
        b.iter(|| {
            let coord = *into_addresses.next().expect("cycle over a non-empty slice");
            black_box(
                pyramid
                    .get_tile_into(coord, &mut buffer)
                    .expect("get_tile_into"),
            )
        });
    });
    tiles.finish();
}

criterion_group!(benches, benchmark);
criterion_main!(benches);
