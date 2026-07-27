//! Tile throughput benchmarks (M4 acceptance criterion 3).
//!
//! A pyramid is built once and reopened, then measured three ways, because a
//! tile store is asked for tiles in three different shapes:
//!
//! - `get_tile/random`:     one tile at a time, by address, in a scattered
//!   order, which is what a tile server does.
//! - `get_tile_into/random`: the same, into a buffer the caller reuses, which
//!   is the difference an owned return makes.
//! - `scan`:                every tile in matrix order through the lending
//!   cursor, which is what a copy or an export does.
//! - `write_all`:           the batch write, tiles per second.
//!
//! Throughput is in tiles per second, so the figures compare directly against
//! `scripts/compare_gdal_tiles.sh`. Payloads are a fixed 4 KiB, which is a
//! plausible size for a 256-pixel PNG basemap tile and keeps the measurement
//! about the container rather than about the payload.
//!
//! These are wall-clock figures, and most of the wall clock here is SQLite.
//! Allocation behaviour on these paths needs a DHAT harness instead (issue
//! #31): a criterion figure will not move usefully when an allocation is
//! removed from a path this I/O-heavy.
//!
//! Zoom levels default to 6 (a 64 by 64 grid at the deepest, 5,461 tiles in
//! all); override with `GPKG_BENCH_ZOOM`.
//!
//! Run: `cargo bench -p geopackage --bench tiles`.

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use geopackage::core::tiles::{TileCoord, TileMatrixSet, ZoomLadder};
use geopackage::{GeoPackage, TilePyramid, TilePyramidBuilder};

/// Payload bytes per tile: a PNG header followed by filler, which the write
/// path probes and the read path returns untouched.
const TILE_BYTES: usize = 4096;

fn max_zoom() -> i64 {
    std::env::var("GPKG_BENCH_ZOOM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6)
}

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

/// Every address of a full web mercator ladder up to `max_zoom`.
fn addresses(max_zoom: i64) -> Vec<TileCoord> {
    let mut out = Vec::new();
    for zoom_level in 0..=max_zoom {
        let side = 1_i64 << zoom_level;
        for row in 0..side {
            for column in 0..side {
                out.push(TileCoord::new(zoom_level, column, row));
            }
        }
    }
    out
}

/// A scattered walk over the addresses, so the reads do not run in key order.
fn scattered(coords: &[TileCoord]) -> Vec<TileCoord> {
    // A stride coprime with most lengths: deterministic, and enough to defeat
    // the sequential locality of a rowid scan.
    let stride = 7919;
    (0..coords.len())
        .filter_map(|i| coords.get((i * stride) % coords.len()).copied())
        .collect()
}

/// Build a full pyramid, close it, and reopen it for reading: the same
/// reopen-before-measuring rule the read bench documents.
fn build(max_zoom: i64) -> (tempfile::TempDir, GeoPackage, Vec<TileCoord>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("tiles.gpkg");
    let coords = addresses(max_zoom);
    {
        let gpkg = GeoPackage::create(&path).expect("create gpkg");
        gpkg.add_epsg_srs(3857).expect("register 3857");
        let matrix_set = TileMatrixSet::web_mercator_quad();
        let matrices = matrix_set
            .ladder(ZoomLadder::new(0, max_zoom))
            .expect("ladder");
        let pyramid = gpkg
            .create_tile_pyramid(&TilePyramidBuilder::new("basemap", matrix_set).matrices(matrices))
            .expect("create pyramid");
        let payload = tile_payload();
        let tiles: Vec<(TileCoord, &[u8])> = coords
            .iter()
            .map(|coord| (*coord, payload.as_slice()))
            .collect();
        pyramid.write_all(tiles, 0).expect("write tiles");
        gpkg.close().expect("close");
    }
    let gpkg = GeoPackage::open_read_only(&path).expect("reopen");
    (dir, gpkg, coords)
}

fn bench_reads(c: &mut Criterion) {
    let max_zoom = max_zoom();
    let (_dir, gpkg, coords) = build(max_zoom);
    let pyramid: TilePyramid<'_> = gpkg.tiles("basemap").expect("open pyramid");
    let order = scattered(&coords);

    let mut group = c.benchmark_group("tiles");
    group.measurement_time(Duration::from_secs(10));
    group.throughput(Throughput::Elements(order.len() as u64));

    group.bench_function("get_tile/random", |b| {
        b.iter(|| {
            let mut bytes = 0usize;
            for coord in &order {
                bytes += pyramid
                    .get_tile(*coord)
                    .expect("read")
                    .map_or(0, |tile| tile.len());
            }
            black_box(bytes)
        });
    });

    group.bench_function("get_tile_into/random", |b| {
        b.iter(|| {
            // One buffer for the whole pass, which is the point of the call.
            let mut buffer = Vec::with_capacity(TILE_BYTES);
            let mut bytes = 0usize;
            for coord in &order {
                if pyramid.get_tile_into(*coord, &mut buffer).expect("read") {
                    bytes += buffer.len();
                }
            }
            black_box(bytes)
        });
    });

    group.bench_function("scan", |b| {
        b.iter(|| {
            let mut cursor = pyramid.cursor().expect("cursor");
            let mut stream = cursor.tiles().expect("stream");
            let mut bytes = 0usize;
            while let Some(tile) = stream.next().expect("next") {
                // Borrowed: the payload is never copied out of the row.
                bytes += tile.data().len();
            }
            black_box(bytes)
        });
    });

    group.finish();
}

fn bench_writes(c: &mut Criterion) {
    let max_zoom = max_zoom();
    let coords = addresses(max_zoom);
    let payload = tile_payload();

    let mut group = c.benchmark_group("tiles");
    group.measurement_time(Duration::from_secs(10));
    group.throughput(Throughput::Elements(coords.len() as u64));
    group.bench_function("write_all", |b| {
        b.iter_batched(
            || {
                let dir = tempfile::tempdir().expect("tempdir");
                let path = dir.path().join("write.gpkg");
                let gpkg = GeoPackage::create(&path).expect("create");
                gpkg.add_epsg_srs(3857).expect("register 3857");
                let matrix_set = TileMatrixSet::web_mercator_quad();
                let matrices = matrix_set
                    .ladder(ZoomLadder::new(0, max_zoom))
                    .expect("ladder");
                gpkg.create_tile_pyramid(
                    &TilePyramidBuilder::new("basemap", matrix_set).matrices(matrices),
                )
                .expect("create pyramid");
                (dir, gpkg)
            },
            |(dir, gpkg)| {
                let pyramid = gpkg.tiles("basemap").expect("open pyramid");
                let tiles: Vec<(TileCoord, &[u8])> = coords
                    .iter()
                    .map(|coord| (*coord, payload.as_slice()))
                    .collect();
                pyramid.write_all(tiles, 0).expect("write");
                drop(gpkg);
                drop(dir);
            },
            criterion::BatchSize::PerIteration,
        );
    });
    group.finish();
}

criterion_group!(benches, bench_reads, bench_writes);
criterion_main!(benches);
