//! Property tests over tile pyramids: the write/read round trip, the order a
//! scan yields tiles in, and the tiles a bounding-box query selects.
//!
//! Each property is checked against an oracle built from the drawn values
//! themselves rather than from the code under test: the expected payloads come
//! from the map the generator filled, and the expected addresses of a box query
//! from the tile index rectangle the box was derived from. A bug in the
//! addressing arithmetic therefore cannot hide behind itself, which matters
//! most for the row axis, where GeoPackage counts south from the top and half
//! the world's tooling counts north from the bottom.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::collections::BTreeMap;

use geopackage::core::tiles::{TileCoord, TileMatrix, TileMatrixSet, ZoomLadder};
use geopackage::{BoundingBox, GeoPackage, TilePyramid, TilePyramidBuilder};
use hegel::generators;

/// A PNG header of the given size, followed by `tag` so that two tiles of the
/// same size are distinguishable. Only the header is ever read.
fn png(width: i64, height: i64, tag: u64) -> Vec<u8> {
    let (width, height) = (
        u32::try_from(width).unwrap(),
        u32::try_from(height).unwrap(),
    );
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&13_u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    bytes.extend_from_slice(&tag.to_le_bytes());
    bytes
}

/// A pyramid over a drawn extent, with a drawn number of levels, base grid and
/// tile size. Every shape this draws is a conforming one, so creation is
/// expected to succeed.
fn draw_pyramid(tc: &hegel::TestCase) -> (TileMatrixSet, Vec<TileMatrix>) {
    let min_x = tc.draw(generators::floats::<f64>().min_value(-180.0).max_value(0.0));
    let min_y = tc.draw(generators::floats::<f64>().min_value(-90.0).max_value(0.0));
    let width = tc.draw(generators::floats::<f64>().min_value(10.0).max_value(360.0));
    let height = tc.draw(generators::floats::<f64>().min_value(10.0).max_value(180.0));
    let levels = tc.draw(generators::integers::<i64>().min_value(1).max_value(4));
    let columns = tc.draw(generators::integers::<i64>().min_value(1).max_value(2));
    let rows = tc.draw(generators::integers::<i64>().min_value(1).max_value(2));
    let tile_side = if tc.draw(generators::booleans()) {
        64
    } else {
        256
    };

    // srs_id 0 is one of the three rows every GeoPackage is created with, so no
    // registration is needed and the drawn extent needs no CRS to make sense.
    let matrix_set = TileMatrixSet::new(0, min_x, min_y, min_x + width, min_y + height);
    let matrices = matrix_set
        .ladder(
            ZoomLadder::new(0, levels - 1)
                .base_grid(columns, rows)
                .tile_size(tile_side, tile_side),
        )
        .unwrap();
    (matrix_set, matrices)
}

/// The addresses a scan yields, in order.
fn scanned(cursor: &mut geopackage::TileCursor<'_>) -> Vec<(i64, i64, i64)> {
    let mut stream = cursor.tiles().unwrap();
    let mut out = Vec::new();
    while let Some(tile) = stream.next().unwrap() {
        let coord = tile.coord();
        out.push((coord.zoom_level, coord.column, coord.row));
    }
    out
}

/// Fill every tile of one zoom level.
fn fill_level(pyramid: &TilePyramid<'_>, matrix: &TileMatrix) {
    let tiles: Vec<(TileCoord, Vec<u8>)> = (0..matrix.matrix_height)
        .flat_map(|row| {
            (0..matrix.matrix_width).map(move |column| {
                (
                    TileCoord::new(matrix.zoom_level, column, row),
                    png(matrix.tile_width, matrix.tile_height, 0),
                )
            })
        })
        .collect();
    pyramid.write_all(tiles, 0).unwrap();
}

#[hegel::test]
fn written_tiles_read_back_and_scan_in_matrix_order(tc: hegel::TestCase) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    let (matrix_set, matrices) = draw_pyramid(&tc);
    let pyramid = gpkg
        .create_tile_pyramid(
            &TilePyramidBuilder::new("basemap", matrix_set).matrices(matrices.clone()),
        )
        .unwrap();

    // Draw addresses inside the grids, letting duplicates through: writing the
    // same address twice must replace rather than accumulate.
    let count = tc.draw(generators::integers::<usize>().min_value(0).max_value(20));
    let mut written: Vec<(TileCoord, Vec<u8>)> = Vec::with_capacity(count);
    let mut expected: BTreeMap<(i64, i64, i64), Vec<u8>> = BTreeMap::new();
    for tag in 0..count {
        let level = tc.draw(
            generators::integers::<usize>()
                .min_value(0)
                .max_value(matrices.len() - 1),
        );
        let matrix = matrices[level];
        let column = tc.draw(
            generators::integers::<i64>()
                .min_value(0)
                .max_value(matrix.matrix_width - 1),
        );
        let row = tc.draw(
            generators::integers::<i64>()
                .min_value(0)
                .max_value(matrix.matrix_height - 1),
        );
        let coord = TileCoord::new(matrix.zoom_level, column, row);
        let payload = png(matrix.tile_width, matrix.tile_height, tag as u64);
        expected.insert((coord.zoom_level, coord.column, coord.row), payload.clone());
        written.push((coord, payload));
    }
    let batch_size = tc.draw(generators::integers::<usize>().min_value(0).max_value(5));
    assert_eq!(pyramid.write_all(written, batch_size).unwrap(), count);

    // Every distinct address holds the last payload written to it.
    assert_eq!(pyramid.tile_count().unwrap(), expected.len() as i64);
    for ((zoom_level, column, row), payload) in &expected {
        let coord = TileCoord::new(*zoom_level, *column, *row);
        assert_eq!(
            pyramid.get_tile(coord).unwrap().as_deref(),
            Some(payload.as_slice())
        );
        assert!(pyramid.has_tile(coord).unwrap());
    }

    // A scan yields them in matrix order: zoom, then north to south, then west
    // to east. `BTreeMap` sorts by (zoom, column, row), so the oracle is
    // re-sorted into the order the scan promises rather than assumed.
    let mut oracle: Vec<(i64, i64, i64)> = expected.keys().copied().collect();
    oracle.sort_unstable_by_key(|(zoom_level, column, row)| (*zoom_level, *row, *column));
    assert_eq!(scanned(&mut pyramid.cursor().unwrap()), oracle);

    for matrix in &matrices {
        let at_level = oracle
            .iter()
            .filter(|(zoom_level, _, _)| *zoom_level == matrix.zoom_level)
            .count();
        assert_eq!(
            pyramid.tile_count_at(matrix.zoom_level).unwrap(),
            at_level as i64
        );
    }
}

#[hegel::test]
fn a_box_query_returns_the_tiles_it_covers(tc: hegel::TestCase) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    let (matrix_set, matrices) = draw_pyramid(&tc);
    let pyramid = gpkg
        .create_tile_pyramid(
            &TilePyramidBuilder::new("basemap", matrix_set).matrices(matrices.clone()),
        )
        .unwrap();

    let level = tc.draw(
        generators::integers::<usize>()
            .min_value(0)
            .max_value(matrices.len() - 1),
    );
    let matrix = matrices[level];
    fill_level(&pyramid, &matrix);

    // Draw a rectangle of tile indices, then a box strictly inside those
    // tiles: a quarter of a tile in from each edge, so no coordinate lands on
    // a tile boundary and the property is about the addressing, not about which
    // side of a tie a division falls.
    let min_column = tc.draw(
        generators::integers::<i64>()
            .min_value(0)
            .max_value(matrix.matrix_width - 1),
    );
    let max_column = tc.draw(
        generators::integers::<i64>()
            .min_value(min_column)
            .max_value(matrix.matrix_width - 1),
    );
    let min_row = tc.draw(
        generators::integers::<i64>()
            .min_value(0)
            .max_value(matrix.matrix_height - 1),
    );
    let max_row = tc.draw(
        generators::integers::<i64>()
            .min_value(min_row)
            .max_value(matrix.matrix_height - 1),
    );

    let north_west = matrix_set.tile_bounds(&matrix, min_column, min_row);
    let south_east = matrix_set.tile_bounds(&matrix, max_column, max_row);
    let inset_x = matrix.tile_span_x() / 4.0;
    let inset_y = matrix.tile_span_y() / 4.0;
    let bbox = BoundingBox::new(
        north_west.min_x + inset_x,
        south_east.min_y + inset_y,
        south_east.max_x - inset_x,
        north_west.max_y - inset_y,
    );

    let mut oracle: Vec<(i64, i64, i64)> = Vec::new();
    for row in min_row..=max_row {
        for column in min_column..=max_column {
            oracle.push((matrix.zoom_level, column, row));
        }
    }
    assert_eq!(
        scanned(&mut pyramid.cursor_in(matrix.zoom_level, bbox).unwrap()),
        oracle
    );
}
