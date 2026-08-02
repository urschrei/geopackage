//! Measurement tool for the tile read paths, driven by
//! `scripts/compare_gdal_tiles.sh`.
//!
//! Subcommands:
//!
//! - `fixture <file> <max_zoom>`: write a full web mercator pyramid of
//!   4 KiB tiles from zoom 0 to `max_zoom`.
//! - `noop <file>`: open and close, the startup floor to subtract from a timed
//!   read.
//! - `scan <file>`: stream every tile through the lending cursor.
//! - `random <file>`: read every tile by address, in a scattered order.
//!
//! Every subcommand prints `<key>=<value>` lines, including `elapsed_ms`
//! measured inside the process, so the script can report both internal time and
//! externally observed wall time.
//!
//! What these figures cover, and what they do not: this crate retrieves stored
//! tile bytes, and GDAL retrieves pixels, because it decodes the payload and
//! this crate cannot. That is a difference in what the two implementations do,
//! a scope decision recorded in `roadmap/06-m4-tiles.md`, not a
//! difference in how fast they do the same thing. The two timings are kept
//! side by side because a caller serving tiles needs both numbers, and the
//! write-up states which operation each one is.

#![expect(
    clippy::print_stdout,
    clippy::unwrap_used,
    clippy::panic,
    reason = "a measurement binary: printing key=value lines is its whole output, and an unmet precondition should stop it"
)]

use std::time::Instant;

use geopackage::core::tiles::{TileCoord, TileMatrixSet, ZoomLadder};
use geopackage::{GeoPackage, TilePyramid, TilePyramidBuilder};

const TILE_BYTES: usize = 4096;

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

/// A scattered walk over the addresses, defeating the locality of a rowid scan.
fn scattered(coords: &[TileCoord]) -> Vec<TileCoord> {
    let stride = 7919;
    (0..coords.len())
        .filter_map(|i| coords.get((i * stride) % coords.len()).copied())
        .collect()
}

fn write_fixture(path: &str, max_zoom: i64) {
    let gpkg = GeoPackage::create(path).unwrap();
    gpkg.add_epsg_srs(3857).unwrap();
    let matrix_set = TileMatrixSet::web_mercator_quad();
    let matrices = matrix_set.ladder(ZoomLadder::new(0, max_zoom)).unwrap();
    let pyramid = gpkg
        .create_tile_pyramid(&TilePyramidBuilder::new("tiles", matrix_set).matrices(matrices))
        .unwrap();
    let payload = tile_payload();
    let coords = addresses(max_zoom);
    let tiles: Vec<(TileCoord, &[u8])> = coords
        .iter()
        .map(|coord| (*coord, payload.as_slice()))
        .collect();
    let started = Instant::now();
    pyramid.write_all(tiles, 0).unwrap();
    let elapsed = started.elapsed();
    gpkg.close().unwrap();
    println!("tiles={}", coords.len());
    println!("tile_bytes={TILE_BYTES}");
    println!("elapsed_ms={:.3}", elapsed.as_secs_f64() * 1000.0);
}

/// Every tile, streamed. Returns the bytes seen so the read cannot be elided.
fn scan(pyramid: &TilePyramid<'_>) -> (usize, usize) {
    let mut cursor = pyramid.cursor().unwrap();
    let mut stream = cursor.tiles().unwrap();
    let (mut tiles, mut bytes) = (0, 0);
    while let Some(tile) = stream.next().unwrap() {
        tiles += 1;
        bytes += tile.data().len();
    }
    (tiles, bytes)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(String::as_str).unwrap_or("");
    let file = args.get(2).cloned().unwrap_or_default();

    match command {
        "fixture" => {
            let max_zoom: i64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(6);
            write_fixture(&file, max_zoom);
        }
        "noop" => {
            let started = Instant::now();
            let gpkg = GeoPackage::open_read_only(&file).unwrap();
            let pyramid = gpkg.tiles("tiles").unwrap();
            let elapsed = started.elapsed();
            println!("zoom_levels={}", pyramid.zoom_levels().len());
            println!("elapsed_ms={:.3}", elapsed.as_secs_f64() * 1000.0);
        }
        "scan" => {
            let gpkg = GeoPackage::open_read_only(&file).unwrap();
            let pyramid = gpkg.tiles("tiles").unwrap();
            let started = Instant::now();
            let (tiles, bytes) = scan(&pyramid);
            let elapsed = started.elapsed();
            println!("tiles={tiles}");
            println!("bytes={bytes}");
            println!("elapsed_ms={:.3}", elapsed.as_secs_f64() * 1000.0);
            println!(
                "tiles_per_sec={:.0}",
                tiles as f64 / elapsed.as_secs_f64().max(f64::MIN_POSITIVE)
            );
        }
        "random" => {
            let gpkg = GeoPackage::open_read_only(&file).unwrap();
            let pyramid = gpkg.tiles("tiles").unwrap();
            let order = scattered(&addresses(
                pyramid.zoom_levels().last().copied().unwrap_or(0),
            ));
            let started = Instant::now();
            let mut bytes = 0usize;
            for coord in &order {
                bytes += pyramid.get_tile(*coord).unwrap().map_or(0, |t| t.len());
            }
            let elapsed = started.elapsed();
            println!("tiles={}", order.len());
            println!("bytes={bytes}");
            println!("elapsed_ms={:.3}", elapsed.as_secs_f64() * 1000.0);
            println!(
                "tiles_per_sec={:.0}",
                order.len() as f64 / elapsed.as_secs_f64().max(f64::MIN_POSITIVE)
            );
        }
        other => {
            panic!("unknown subcommand {other:?}: expected fixture, noop, scan or random");
        }
    }
}
