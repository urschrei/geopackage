//! Property tests over tile matrix arithmetic: locating a position, the XYZ
//! conversions, row flipping, ladders, and the grid checks.
//!
//! `tile_range` is not tested here on its own: the `geopackage` crate's
//! `a_box_query_returns_the_tiles_it_touches` checks it, through
//! `cursor_in`, against `tile_bounds` for boxes with edges from the whole
//! `f64` domain.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geopackage_core::tiles::{TileCoord, TileError, TileMatrix, TileMatrixSet, ZoomLadder};
use hegel::generators;

/// Draws two distinct finite values in `[-1e9, 1e9]`, in ascending order.
fn draw_edges(tc: &hegel::TestCase) -> (f64, f64) {
    let edge = || tc.draw(generators::floats::<f64>().min_value(-1e9).max_value(1e9));
    let (a, b) = (edge(), edge());
    tc.assume(a < b || b < a);
    (a.min(b), a.max(b))
}

/// Draws a matrix set and one level of a ladder over it, whose grid spans the
/// extent (Requirement 45).
fn draw_level(tc: &hegel::TestCase) -> (TileMatrixSet, TileMatrix) {
    let (min_x, max_x) = draw_edges(tc);
    let (min_y, max_y) = draw_edges(tc);
    let set = TileMatrixSet::new(0, min_x, min_y, max_x, max_y);
    let side = || tc.draw(generators::integers::<i64>().min_value(1).max_value(65_535));
    let base = || tc.draw(generators::integers::<i64>().min_value(1).max_value(5));
    let zoom = tc.draw(generators::integers::<i64>().min_value(0).max_value(4));
    let matrices = set
        .ladder(
            ZoomLadder::new(zoom, zoom)
                .base_grid(base(), base())
                .tile_size(side(), side()),
        )
        .unwrap();
    (set, *matrices.first().unwrap())
}

/// Draws a coordinate on one axis: any `f64`, a tile edge, or a value in or
/// near the extent.
fn draw_position(tc: &hegel::TestCase, edges: &[f64], low: f64, high: f64) -> f64 {
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(2)) {
        0 => tc.draw(generators::floats::<f64>()),
        1 => tc.draw(generators::sampled_from(edges.to_vec())),
        _ => {
            let margin = (high - low) / 8.0;
            tc.draw(
                generators::floats::<f64>()
                    .min_value(low - margin)
                    .max_value(high + margin),
            )
        }
    }
}

/// `tile_at` returns a tile exactly for a position inside the extent, and
/// that tile's bounds contain the position. A position on an edge that two
/// tiles share belongs to the tile east or south of it.
#[hegel::test]
fn tile_at_returns_the_tile_whose_bounds_contain_the_position(tc: hegel::TestCase) {
    let (set, matrix) = draw_level(&tc);
    let columns: Vec<f64> = (0..=matrix.matrix_width)
        .map(|c| set.tile_bounds(&matrix, c, 0).min_x)
        .chain([set.max_x])
        .collect();
    let rows: Vec<f64> = (0..=matrix.matrix_height)
        .map(|r| set.tile_bounds(&matrix, 0, r).max_y)
        .chain([set.min_y])
        .collect();
    let x = draw_position(&tc, &columns, set.min_x, set.max_x);
    let y = draw_position(&tc, &rows, set.min_y, set.max_y);

    let inside = (set.min_x..=set.max_x).contains(&x) && (set.min_y..=set.max_y).contains(&y);
    let Some((column, row)) = set.tile_at(&matrix, x, y) else {
        assert!(!inside, "({x}, {y}) is inside the extent");
        return;
    };
    assert!(inside, "({x}, {y}) is outside the extent");
    assert!(matrix.contains(column, row));
    let bounds = set.tile_bounds(&matrix, column, row);
    assert!(
        bounds.min_x <= x && x <= bounds.max_x && bounds.min_y <= y && y <= bounds.max_y,
        "({x}, {y}) is outside tile ({column}, {row}) at {bounds:?}"
    );
    assert!(
        x < bounds.max_x || column == matrix.matrix_width - 1,
        "({x}, {y}) is on the east edge of ({column}, {row})"
    );
    assert!(
        y > bounds.min_y || row == matrix.matrix_height - 1,
        "({x}, {y}) is on the south edge of ({column}, {row})"
    );
}

/// On the web mercator quad, `xyz_to_tile` and `tile_to_xyz` are the
/// identity on every address in the grid, and reject one outside it; on any
/// other extent both report `NotAnXyzGrid`.
#[hegel::test]
fn xyz_addresses_round_trip_on_the_quad(tc: hegel::TestCase) {
    let quad = TileMatrixSet::web_mercator_quad();
    let zoom = tc.draw(generators::integers::<i64>().min_value(0).max_value(20));
    // The quad doubles from one tile at zoom 0, so its level `zoom` is the
    // last of a ladder from 0.
    let matrix = *quad
        .ladder(ZoomLadder::new(0, zoom))
        .unwrap()
        .last()
        .unwrap();
    let side = 1_i64 << zoom;
    let x = tc.draw(
        generators::integers::<i64>()
            .min_value(-2)
            .max_value(side + 1),
    );
    let y = tc.draw(
        generators::integers::<i64>()
            .min_value(-2)
            .max_value(side + 1),
    );

    if (0..side).contains(&x) && (0..side).contains(&y) {
        let coord = quad.xyz_to_tile(&matrix, zoom, x, y).unwrap();
        assert_eq!(coord, TileCoord::new(zoom, x, y));
        assert_eq!(quad.tile_to_xyz(&matrix, coord).unwrap(), (zoom, x, y));
    } else {
        assert!(matches!(
            quad.xyz_to_tile(&matrix, zoom, x, y),
            Err(TileError::CoordOutsideMatrix { .. })
        ));
    }

    let (other, other_matrix) = draw_level(&tc);
    assert!(matches!(
        other.xyz_to_tile(&other_matrix, other_matrix.zoom_level, 0, 0),
        Err(TileError::NotAnXyzGrid { .. })
    ));
}

/// `flip_row` maps the grid's rows onto themselves and is its own inverse.
#[hegel::test]
fn flip_row_is_an_involution_on_the_grid(tc: hegel::TestCase) {
    let (_, matrix) = draw_level(&tc);
    let row = tc.draw(
        generators::integers::<i64>()
            .min_value(0)
            .max_value(matrix.matrix_height - 1),
    );
    let flipped = matrix.flip_row(row);
    assert!((0..matrix.matrix_height).contains(&flipped));
    assert_eq!(matrix.flip_row(flipped), row);
}

/// A ladder over a valid extent is a consistent pyramid: one level per zoom,
/// each grid twice the one below, and the whole passes `validate`.
#[hegel::test]
fn a_ladder_is_a_consistent_pyramid(tc: hegel::TestCase) {
    let (min_x, max_x) = draw_edges(&tc);
    let (min_y, max_y) = draw_edges(&tc);
    let set = TileMatrixSet::new(0, min_x, min_y, max_x, max_y);
    let min_zoom = tc.draw(generators::integers::<i64>().min_value(0).max_value(20));
    let max_zoom = tc.draw(
        generators::integers::<i64>()
            .min_value(min_zoom)
            .max_value(min_zoom + 6),
    );
    let columns = tc.draw(generators::integers::<i64>().min_value(1).max_value(8));
    let rows = tc.draw(generators::integers::<i64>().min_value(1).max_value(8));
    let matrices = set
        .ladder(
            ZoomLadder::new(min_zoom, max_zoom)
                .base_grid(columns, rows)
                .tile_size(256, 256),
        )
        .unwrap();

    assert_eq!(matrices.len() as i64, max_zoom - min_zoom + 1);
    for (level, matrix) in (0_u32..).zip(&matrices) {
        assert_eq!(matrix.zoom_level, min_zoom + i64::from(level));
        assert_eq!(matrix.matrix_width, columns << level);
        assert_eq!(matrix.matrix_height, rows << level);
    }
    let checked = set.validate(&matrices);
    assert!(checked.is_ok(), "{checked:?}");
    assert!(geopackage_core::tiles::is_power_of_two_ladder(&matrices));
}

/// `ladder` reports each documented error: an extent no tile can be measured
/// against, a non-positive base grid or tile size, and a zoom range that is
/// empty, negative, or wide enough to overflow the grid.
#[hegel::test]
fn a_ladder_rejects_what_it_documents(tc: hegel::TestCase) {
    // A valid extent three times in four, so that the later checks are reached.
    let set = if tc.draw(generators::weighted_booleans(0.25)) {
        let any = || tc.draw(generators::floats::<f64>());
        TileMatrixSet::new(0, any(), any(), any(), any())
    } else {
        let (min_x, max_x) = draw_edges(&tc);
        let (min_y, max_y) = draw_edges(&tc);
        TileMatrixSet::new(0, min_x, min_y, max_x, max_y)
    };
    let min_zoom = tc.draw(generators::integers::<i64>().min_value(-2).max_value(8));
    let max_zoom = min_zoom + tc.draw(generators::integers::<i64>().min_value(-2).max_value(70));
    let dimension = || tc.draw(generators::integers::<i64>().min_value(-1).max_value(4));
    let (columns, rows, width, height) = (dimension(), dimension(), dimension(), dimension());
    let result = set.ladder(
        ZoomLadder::new(min_zoom, max_zoom)
            .base_grid(columns, rows)
            .tile_size(width, height),
    );

    let extent_ok = [set.min_x, set.min_y, set.max_x, set.max_y]
        .iter()
        .all(|v| v.is_finite())
        && set.min_x < set.max_x
        && set.min_y < set.max_y;
    let range_ok = min_zoom >= 0 && max_zoom >= min_zoom;
    let dimensions_ok = [columns, rows, width, height].iter().all(|&d| d > 0);
    // The grid doubles once per level after the first, and must stay an i64.
    let fits = |base: i64| {
        u32::try_from(max_zoom - min_zoom).is_ok_and(|levels| {
            levels < 63
                && base
                    .checked_mul(1_i64 << levels)
                    .is_some_and(|top| top.checked_mul(2).is_some())
        })
    };
    if !extent_ok {
        tc.event("invalid extent");
        assert!(
            matches!(result, Err(TileError::InvalidExtent { .. })),
            "{result:?}"
        );
    } else if !range_ok {
        tc.event("invalid zoom range");
        assert!(
            matches!(result, Err(TileError::InvalidZoomRange { .. })),
            "{result:?}"
        );
    } else if !dimensions_ok {
        tc.event("non-positive dimension");
        assert!(
            matches!(result, Err(TileError::NonPositiveDimension { .. })),
            "{result:?}"
        );
    } else if !(fits(columns) && fits(rows)) {
        tc.event("overflowing grid");
        assert!(
            matches!(result, Err(TileError::InvalidZoomRange { .. })),
            "{result:?}"
        );
    } else {
        tc.event("valid");
        assert!(result.is_ok(), "{result:?}");
    }
}

/// `check_contains` agrees with `contains` on any address.
#[hegel::test]
fn check_contains_agrees_with_contains(tc: hegel::TestCase) {
    let (_, matrix) = draw_level(&tc);
    let column = tc.draw(generators::integers::<i64>());
    let row = tc.draw(generators::integers::<i64>());
    let small = |n: i64| tc.draw(generators::integers::<i64>().min_value(-1).max_value(n));
    let (column, row) = if tc.draw(generators::booleans()) {
        (small(matrix.matrix_width), small(matrix.matrix_height))
    } else {
        (column, row)
    };
    let inside =
        (0..matrix.matrix_width).contains(&column) && (0..matrix.matrix_height).contains(&row);
    assert_eq!(matrix.contains(column, row), inside);
    assert_eq!(matrix.check_contains(column, row).is_ok(), inside);
}
