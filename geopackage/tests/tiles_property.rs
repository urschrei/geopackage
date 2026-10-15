//! Property tests over tile pyramids: creation, the writer and its batch path,
//! the point reads, the three scans, and the counts.
//!
//! Each property is checked against an oracle built from the drawn values
//! themselves rather than from the code under test: the expected payloads come
//! from a model of the writes that should succeed, and the expected addresses
//! of a box query from the bounds of each stored tile. A bug in the addressing
//! arithmetic therefore cannot hide behind itself, which matters most for the
//! row axis, where GeoPackage counts south from the top and half the world's
//! tooling counts north from the bottom.
//!
//! Every container is SQLite's private per-connection in-memory database (the
//! `:memory:` filename), for the reason `in_memory_with_points` in
//! `spatial_index.rs` gives: a property builds a container per case, and on
//! Windows CI creating a file and cycling a rollback journal per transaction
//! costs orders of magnitude more than on macOS or Linux. That is what tripped
//! hegel's `TooSlow` health check on `features_in` (issue #44). These
//! properties are about tile addressing and payload storage, neither of which
//! depends on the container being a file; `tiles.rs` writes real ones.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::collections::BTreeMap;
use std::path::Path;

use geopackage::core::tiles::{
    TileBounds, TileCoord, TileError, TileMatrix, TileMatrixSet, ZoomLadder, is_power_of_two_ladder,
};
use geopackage::{BoundingBox, Error, GeoPackage, TilePyramid, TilePyramidBuilder};
use hegel::generators;

/// A tile address as `(zoom_level, column, row)`.
type Address = (i64, i64, i64);

/// The tiles a pyramid should contain, by address.
type Model = BTreeMap<Address, Vec<u8>>;

fn address(coord: TileCoord) -> Address {
    (coord.zoom_level, coord.column, coord.row)
}

fn in_memory() -> GeoPackage {
    GeoPackage::create(Path::new(":memory:")).unwrap()
}

// --- payloads -----------------------------------------------------------------

/// A PNG header of the given size, followed by `tag` so that two tiles of the
/// same size are distinguishable. Only the header is ever read.
fn png(width: u16, height: u16, tag: u64) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&13_u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&u32::from(width).to_be_bytes());
    bytes.extend_from_slice(&u32::from(height).to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    bytes.extend_from_slice(&tag.to_le_bytes());
    bytes
}

/// A baseline JPEG header (start of image and a `SOF0` segment) of the given
/// size, followed by `tag`.
fn jpeg(width: u16, height: u16, tag: u64) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08];
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
    bytes.extend_from_slice(&tag.to_le_bytes());
    bytes
}

/// A WebP container in its extended (`VP8X`) form, whose dimensions are stored
/// one less than the real size, followed by `tag`.
fn webp(width: u16, height: u16, tag: u64) -> Vec<u8> {
    let mut bytes = Vec::from(*b"RIFF");
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(b"WEBP");
    bytes.extend_from_slice(b"VP8X");
    bytes.extend_from_slice(&10_u32.to_le_bytes());
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    bytes.extend_from_slice(&(u32::from(width) - 1).to_le_bytes()[..3]);
    bytes.extend_from_slice(&(u32::from(height) - 1).to_le_bytes()[..3]);
    bytes.extend_from_slice(&tag.to_le_bytes());
    bytes
}

/// A GIF header of the given size: an image that no tile table may hold.
fn gif(width: u16, height: u16) -> Vec<u8> {
    let mut bytes = b"GIF89a".to_vec();
    bytes.extend_from_slice(&width.to_le_bytes());
    bytes.extend_from_slice(&height.to_le_bytes());
    bytes.extend_from_slice(&[0, 0, 0]);
    bytes
}

/// A little-endian baseline TIFF header with one IFD naming the image size:
/// a coverage payload, which a plain tile pyramid may not hold.
fn tiff(width: u16, height: u16) -> Vec<u8> {
    let mut bytes = Vec::from(*b"II");
    bytes.extend_from_slice(&42_u16.to_le_bytes());
    bytes.extend_from_slice(&8_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    for (tag, value) in [(256_u16, width), (257_u16, height)] {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&3_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&u32::from(value).to_le_bytes());
    }
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes
}

/// The pixel size of a zoom level, as the `u16` every payload builder takes.
/// `draw_pyramid` keeps tile sizes in that range.
fn tile_size(matrix: &TileMatrix) -> (u16, u16) {
    (
        u16::try_from(matrix.tile_width).unwrap(),
        u16::try_from(matrix.tile_height).unwrap(),
    )
}

/// Draws a payload of the given size in one of the three formats a tile table
/// may hold.
fn draw_payload(tc: &hegel::TestCase, (width, height): (u16, u16), tag: u64) -> Vec<u8> {
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(2)) {
        0 => png(width, height, tag),
        1 => jpeg(width, height, tag),
        _ => webp(width, height, tag),
    }
}

fn is_webp(payload: &[u8]) -> bool {
    payload.starts_with(b"RIFF")
}

// --- pyramids -----------------------------------------------------------------

/// Draws two distinct finite values in `[-1e9, 1e9]`, in ascending order.
fn draw_edges(tc: &hegel::TestCase) -> (f64, f64) {
    let edge = || tc.draw(generators::floats::<f64>().min_value(-1e9).max_value(1e9));
    let (a, b) = (edge(), edge());
    tc.assume(a < b || b < a);
    (a.min(b), a.max(b))
}

/// Draws a pyramid: an extent, and either a power-of-two ladder or an
/// arrangement of zoom levels that needs `gpkg_zoom_other`. Returns the matrix
/// set, the matrices, and whether to allow zoom-other.
///
/// The tile size is at most 65535 pixels a side, the largest a JPEG header can
/// declare. The base grid is at most four tiles a side, and there are at most
/// four levels, so a case has at most 8192 addresses at its top level.
fn draw_pyramid(tc: &hegel::TestCase) -> (TileMatrixSet, Vec<TileMatrix>, bool) {
    let (min_x, max_x) = draw_edges(tc);
    let (min_y, max_y) = draw_edges(tc);
    let matrix_set = TileMatrixSet::new(0, min_x, min_y, max_x, max_y);
    let side = || {
        tc.draw(
            generators::integers::<i64>()
                .min_value(1)
                .max_value(i64::from(u16::MAX)),
        )
    };
    let (tile_width, tile_height) = (side(), side());
    let levels = tc.draw(generators::integers::<i64>().min_value(1).max_value(4));
    let min_zoom = tc.draw(generators::integers::<i64>().min_value(0).max_value(30));
    let base = || tc.draw(generators::integers::<i64>().min_value(1).max_value(4));
    let (mut columns, mut rows) = (base(), base());

    if tc.draw(generators::booleans()) {
        let matrices = matrix_set
            .ladder(
                ZoomLadder::new(min_zoom, min_zoom + levels - 1)
                    .base_grid(columns, rows)
                    .tile_size(tile_width, tile_height),
            )
            .unwrap();
        return (matrix_set, matrices, tc.draw(generators::booleans()));
    }

    // Zoom levels with gaps, and grids that grow by any amount, so that the
    // pixel size descends (Requirement 53) by factors other than two.
    let step = |max| tc.draw(generators::integers::<i64>().min_value(1).max_value(max));
    let mut zoom_level = min_zoom;
    let mut matrices = Vec::new();
    for _ in 0..levels {
        matrices.push(TileMatrix::new(
            zoom_level,
            columns,
            rows,
            tile_width,
            tile_height,
            matrix_set.width() / (columns * tile_width) as f64,
            matrix_set.height() / (rows * tile_height) as f64,
        ));
        zoom_level += step(3);
        columns += step(4);
        rows += step(4);
    }
    (matrix_set, matrices, true)
}

/// Creates the drawn pyramid as `basemap`. A pyramid that creation rejects
/// for breaking a consistency rule (an extent too narrow for its magnitude,
/// say) is outside these properties' domain and the case is discarded;
/// `creation_accepts_exactly_the_consistent_pyramids` covers that boundary.
fn create<'a>(
    tc: &hegel::TestCase,
    gpkg: &'a GeoPackage,
    (matrix_set, matrices, allow_zoom_other): &(TileMatrixSet, Vec<TileMatrix>, bool),
) -> TilePyramid<'a> {
    let result = gpkg.create_tile_pyramid(
        &TilePyramidBuilder::new("basemap", *matrix_set)
            .matrices(matrices.clone())
            .allow_zoom_other(*allow_zoom_other),
    );
    tc.assume(!matches!(result, Err(Error::Tile(_))));
    result.unwrap()
}

/// Draws one of the pyramid's zoom levels.
fn draw_matrix(tc: &hegel::TestCase, matrices: &[TileMatrix]) -> TileMatrix {
    tc.draw(generators::sampled_from(matrices.to_vec()))
}

/// Draws an address inside one zoom level's grid.
fn draw_inside(tc: &hegel::TestCase, matrix: &TileMatrix) -> TileCoord {
    TileCoord::new(
        matrix.zoom_level,
        tc.draw(
            generators::integers::<i64>()
                .min_value(0)
                .max_value(matrix.matrix_width - 1),
        ),
        tc.draw(
            generators::integers::<i64>()
                .min_value(0)
                .max_value(matrix.matrix_height - 1),
        ),
    )
}

/// Draws a zoom level the pyramid does not declare.
fn draw_undeclared_zoom(tc: &hegel::TestCase, matrices: &[TileMatrix]) -> i64 {
    let zoom_level = tc.draw(generators::integers::<i64>());
    tc.assume(matrices.iter().all(|m| m.zoom_level != zoom_level));
    zoom_level
}

/// Draws an address anywhere: in a grid, outside one, or at a zoom level the
/// pyramid does not declare.
fn draw_any_address(tc: &hegel::TestCase, matrices: &[TileMatrix]) -> TileCoord {
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(2)) {
        0 => draw_inside(tc, &draw_matrix(tc, matrices)),
        1 => TileCoord::new(
            draw_matrix(tc, matrices).zoom_level,
            tc.draw(generators::integers::<i64>()),
            tc.draw(generators::integers::<i64>()),
        ),
        _ => TileCoord::new(
            tc.draw(generators::integers::<i64>()),
            tc.draw(generators::integers::<i64>()),
            tc.draw(generators::integers::<i64>()),
        ),
    }
}

// --- reading the stored state -------------------------------------------------

/// Every row of `basemap`, read with SQL rather than through the pyramid.
fn stored(gpkg: &GeoPackage) -> Model {
    gpkg.connection()
        .prepare("SELECT zoom_level, tile_column, tile_row, tile_data FROM basemap")
        .unwrap()
        .query_map([], |row| {
            Ok(((row.get(0)?, row.get(1)?, row.get(2)?), row.get(3)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// The addresses and payloads a scan yields, in order.
fn scanned(cursor: &mut geopackage::TileCursor<'_>) -> Vec<(Address, Vec<u8>)> {
    let mut stream = cursor.tiles().unwrap();
    let mut out = Vec::new();
    while let Some(tile) = stream.next().unwrap() {
        out.push((address(tile.coord()), tile.to_vec()));
    }
    out
}

/// The model's tiles in matrix order: zoom, then north to south, then west to
/// east. `BTreeMap` sorts by (zoom, column, row), so the order is imposed here
/// rather than assumed.
fn in_matrix_order(tiles: impl IntoIterator<Item = (Address, Vec<u8>)>) -> Vec<(Address, Vec<u8>)> {
    let mut tiles: Vec<(Address, Vec<u8>)> = tiles.into_iter().collect();
    tiles.sort_unstable_by_key(|((zoom_level, column, row), _)| (*zoom_level, *row, *column));
    tiles
}

/// Writes a drawn set of valid tiles in one `write_all`, deletes a drawn subset
/// of them, and returns the model of what remains.
fn draw_state(tc: &hegel::TestCase, pyramid: &TilePyramid<'_>, matrices: &[TileMatrix]) -> Model {
    let count = tc.draw(generators::integers::<u64>().min_value(0).max_value(24));
    let mut model = Model::new();
    let mut tiles = Vec::new();
    for tag in 0..count {
        let matrix = draw_matrix(tc, matrices);
        let coord = draw_inside(tc, &matrix);
        let payload = draw_payload(tc, tile_size(&matrix), tag);
        model.insert(address(coord), payload.clone());
        tiles.push((coord, payload));
    }
    pyramid.write_all(tiles, 0).unwrap();
    let written: Vec<Address> = model.keys().copied().collect();
    for (zoom_level, column, row) in written {
        if tc.draw(generators::weighted_booleans(0.25)) {
            assert!(
                pyramid
                    .delete_tile(TileCoord::new(zoom_level, column, row))
                    .unwrap()
            );
            model.remove(&(zoom_level, column, row));
        }
    }
    model
}

// --- creation -----------------------------------------------------------------

/// Draws a matrix set extent from the whole `f64` domain, NaN and the
/// infinities included, or a finite one that a valid pyramid could have.
fn draw_any_extent(tc: &hegel::TestCase) -> TileMatrixSet {
    if tc.draw(generators::booleans()) {
        let edge = || tc.draw(generators::floats::<f64>());
        TileMatrixSet::new(0, edge(), edge(), edge(), edge())
    } else {
        let (min_x, max_x) = draw_edges(tc);
        let (min_y, max_y) = draw_edges(tc);
        TileMatrixSet::new(0, min_x, min_y, max_x, max_y)
    }
}

/// Draws up to four matrices with every field from a domain that includes the
/// values each consistency rule rejects: negative and duplicate zoom levels,
/// non-positive dimensions, and pixel sizes that are non-positive, non-finite,
/// not descending, or that do not span the extent. Each pixel size is usually
/// the one that spans the extent, so that a useful share of cases is valid.
fn draw_any_matrices(tc: &hegel::TestCase, matrix_set: &TileMatrixSet) -> Vec<TileMatrix> {
    let count = tc.draw(generators::integers::<usize>().min_value(0).max_value(4));
    (0..count)
        .map(|_| {
            let zoom_level = tc.draw(generators::integers::<i64>().min_value(-1).max_value(6));
            let dimension = || tc.draw(generators::integers::<i64>().min_value(-1).max_value(64));
            let (matrix_width, matrix_height) = (dimension(), dimension());
            let (tile_width, tile_height) = (dimension(), dimension());
            let pixel_size = |extent: f64, cells: i64| {
                if tc.draw(generators::weighted_booleans(0.8)) {
                    extent / cells as f64
                } else {
                    tc.draw(generators::floats::<f64>())
                }
            };
            TileMatrix::new(
                zoom_level,
                matrix_width,
                matrix_height,
                tile_width,
                tile_height,
                pixel_size(matrix_set.width(), matrix_width * tile_width),
                pixel_size(matrix_set.height(), matrix_height * tile_height),
            )
        })
        .collect()
}

/// `create_tile_pyramid` accepts exactly the pyramids that
/// `TileMatrixSet::validate` accepts, rejects the others with the same
/// `TileError`, and among the valid ones requires `allow_zoom_other` for the
/// pyramids that `is_power_of_two_ladder` does not recognise.
#[hegel::test]
fn creation_accepts_exactly_the_consistent_pyramids(tc: hegel::TestCase) {
    let gpkg = in_memory();
    // Half the cases come from the generator of valid pyramids, which is what
    // reaches the zoom-other check.
    let (matrix_set, matrices) = if tc.draw(generators::booleans()) {
        let (matrix_set, matrices, _) = draw_pyramid(&tc);
        (matrix_set, matrices)
    } else {
        let matrix_set = draw_any_extent(&tc);
        let matrices = draw_any_matrices(&tc, &matrix_set);
        (matrix_set, matrices)
    };
    let allow_zoom_other = tc.draw(generators::booleans());
    let result = gpkg.create_tile_pyramid(
        &TilePyramidBuilder::new("basemap", matrix_set)
            .matrices(matrices.clone())
            .allow_zoom_other(allow_zoom_other),
    );

    match matrix_set.validate(&matrices) {
        Err(expected) => match result {
            Err(Error::Tile(actual)) => {
                tc.event("inconsistent");
                assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
            }
            other => panic!(
                "expected {expected:?}, got {:?}",
                other.map(|pyramid| pyramid.table_name().to_owned())
            ),
        },
        Ok(()) if !is_power_of_two_ladder(&matrices) && !allow_zoom_other => {
            tc.event("zoom-other not allowed");
            assert!(
                matches!(result, Err(Error::ZoomOtherNotEnabled { .. })),
                "{:?}",
                result.map(|pyramid| pyramid.table_name().to_owned())
            );
        }
        Ok(()) => {
            tc.event("created");
            result.unwrap();
        }
    }
}

/// A created pyramid reads back with the matrix set and matrices it was
/// created with, and `gpkg_zoom_other` is registered exactly when the
/// matrices are not a power-of-two ladder.
#[hegel::test]
fn a_created_pyramid_reads_back_as_written(tc: hegel::TestCase) {
    let gpkg = in_memory();
    // Half the cases come from the generator of valid pyramids, so that
    // discarding the rejected ones leaves enough valid cases.
    let (matrix_set, matrices) = if tc.draw(generators::booleans()) {
        let (matrix_set, matrices, _) = draw_pyramid(&tc);
        (matrix_set, matrices)
    } else {
        let matrix_set = draw_any_extent(&tc);
        let matrices = draw_any_matrices(&tc, &matrix_set);
        (matrix_set, matrices)
    };
    let result = gpkg.create_tile_pyramid(
        &TilePyramidBuilder::new("basemap", matrix_set)
            .matrices(matrices.clone())
            .allow_zoom_other(true),
    );
    tc.assume(!matches!(result, Err(Error::Tile(_))));
    drop(result.unwrap());

    let pyramid = gpkg.tiles("basemap").unwrap();
    assert_eq!(*pyramid.matrix_set(), matrix_set);
    let mut sorted = matrices.clone();
    sorted.sort_unstable_by_key(|matrix| matrix.zoom_level);
    assert_eq!(pyramid.matrices(), sorted.as_slice());
    let registered =
        gpkg.extensions().unwrap().iter().any(|row| {
            row.name == "gpkg_zoom_other" && row.table_name.as_deref() == Some("basemap")
        });
    assert_eq!(registered, !is_power_of_two_ladder(&matrices));
}

// --- the writer ---------------------------------------------------------------

/// What is wrong with a drawn put, if anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Defect {
    None,
    UnknownZoomLevel,
    OutsideGrid,
    WrongSize,
    /// An image of the right size in a format no tile table may hold.
    FormatNotAllowed,
    /// Bytes drawn from the whole domain.
    AnyBytes,
}

#[derive(Debug)]
enum Op {
    Put {
        coord: TileCoord,
        data: Vec<u8>,
        defect: Defect,
    },
    Delete {
        coord: TileCoord,
    },
}

/// Draws a payload of the given size whose size differs from `size`.
fn draw_wrong_size(tc: &hegel::TestCase, size: (u16, u16), tag: u64) -> Vec<u8> {
    let side = || tc.draw(generators::integers::<u16>().min_value(1));
    let wrong = (side(), side());
    tc.assume(wrong != size);
    draw_payload(tc, wrong, tag)
}

/// Draws a put. Half the puts are valid, so that the model stores, rewrites
/// and deletes tiles often; with `defective` set, none is.
fn draw_put(tc: &hegel::TestCase, matrices: &[TileMatrix], tag: u64, defective: bool) -> Op {
    let defect = if !defective && tc.draw(generators::booleans()) {
        Defect::None
    } else {
        tc.draw(generators::sampled_from(vec![
            Defect::UnknownZoomLevel,
            Defect::OutsideGrid,
            Defect::WrongSize,
            Defect::FormatNotAllowed,
            Defect::AnyBytes,
        ]))
    };
    tc.event(format!("put with defect {defect:?}"));
    let matrix = draw_matrix(tc, matrices);
    let size = tile_size(&matrix);
    let coord = match defect {
        Defect::UnknownZoomLevel => TileCoord::new(draw_undeclared_zoom(tc, matrices), 0, 0),
        Defect::OutsideGrid => {
            let column = tc.draw(generators::integers::<i64>());
            let row = tc.draw(generators::integers::<i64>());
            tc.assume(!matrix.contains(column, row));
            TileCoord::new(matrix.zoom_level, column, row)
        }
        _ => draw_inside(tc, &matrix),
    };
    let data = match defect {
        Defect::WrongSize => draw_wrong_size(tc, size, tag),
        Defect::FormatNotAllowed => {
            if tc.draw(generators::booleans()) {
                gif(size.0, size.1)
            } else {
                tiff(size.0, size.1)
            }
        }
        Defect::AnyBytes => tc.draw(generators::vecs(generators::integers::<u8>()).max_size(64)),
        _ => draw_payload(tc, size, tag),
    };
    Op::Put {
        coord,
        data,
        defect,
    }
}

fn draw_op(tc: &hegel::TestCase, matrices: &[TileMatrix], tag: u64) -> Op {
    if tc.draw(generators::booleans()) {
        draw_put(tc, matrices, tag, false)
    } else if tc.draw(generators::weighted_booleans(0.9)) {
        Op::Delete {
            coord: draw_inside(tc, &draw_matrix(tc, matrices)),
        }
    } else {
        Op::Delete {
            coord: draw_any_address(tc, matrices),
        }
    }
}

/// Returns `true` if `result` is the documented outcome of a put with `defect`.
fn put_outcome_is_documented(defect: Defect, result: &geopackage::Result<()>) -> bool {
    match defect {
        Defect::None => result.is_ok(),
        Defect::UnknownZoomLevel => matches!(result, Err(Error::UnknownZoomLevel { .. })),
        Defect::OutsideGrid => matches!(
            result,
            Err(Error::Tile(TileError::CoordOutsideMatrix { .. }))
        ),
        Defect::WrongSize => matches!(
            result,
            Err(Error::Tile(TileError::PayloadSizeMismatch { .. }))
        ),
        Defect::FormatNotAllowed => matches!(result, Err(Error::TileFormatNotAllowed { .. })),
        // Bytes from the whole domain may be no image, an image of a format no
        // tile table may hold, or one of the wrong size. Each is a documented
        // rejection.
        Defect::AnyBytes => matches!(
            result,
            Err(Error::Tile(
                TileError::UnreadablePayload { .. } | TileError::PayloadSizeMismatch { .. }
            ) | Error::TileFormatNotAllowed { .. })
        ),
    }
}

/// Returns `true` if `gpkg_webp` is registered for `basemap`.
fn webp_registered(gpkg: &GeoPackage) -> bool {
    gpkg.extensions().unwrap().iter().any(|row| {
        row.name == "gpkg_webp"
            && row.table_name.as_deref() == Some("basemap")
            && row.column_name.as_deref() == Some("tile_data")
    })
}

/// What a session step reports to the property that runs it.
enum Step<'a> {
    Put {
        coord: TileCoord,
        data: &'a [u8],
        defect: Defect,
        result: &'a geopackage::Result<()>,
    },
    Delete {
        coord: TileCoord,
        result: &'a geopackage::Result<bool>,
    },
    /// The session's writes are durable: a writer committed, or a single-tile
    /// call returned.
    Committed,
    /// A writer was dropped without a commit.
    RolledBack,
}

/// Runs between one and four drawn sessions against `pyramid`, and calls
/// `check` after each operation and at each commit or rollback. A session is
/// either a `TileWriter` that commits or is dropped, or a run of
/// `put_tile` and `delete_tile` calls, each in its own transaction.
fn run_sessions(
    tc: &hegel::TestCase,
    pyramid: &TilePyramid<'_>,
    matrices: &[TileMatrix],
    mut check: impl FnMut(Step<'_>),
) {
    let mut tag = 0;
    let sessions = tc.draw(generators::integers::<usize>().min_value(1).max_value(4));
    for _ in 0..sessions {
        let ops = tc.draw(generators::integers::<usize>().min_value(0).max_value(8));
        let mut writer = if tc.draw(generators::booleans()) {
            Some(pyramid.writer().unwrap())
        } else {
            None
        };
        for _ in 0..ops {
            tag += 1;
            match draw_op(tc, matrices, tag) {
                Op::Put {
                    coord,
                    data,
                    defect,
                } => {
                    let result = match writer.as_mut() {
                        Some(writer) => writer.put(coord, &data),
                        None => pyramid.put_tile(coord, &data),
                    };
                    check(Step::Put {
                        coord,
                        data: &data,
                        defect,
                        result: &result,
                    });
                }
                Op::Delete { coord } => {
                    let result = match writer.as_mut() {
                        Some(writer) => writer.delete(coord),
                        None => pyramid.delete_tile(coord),
                    };
                    check(Step::Delete {
                        coord,
                        result: &result,
                    });
                }
            }
            if writer.is_none() {
                check(Step::Committed);
            }
        }
        if let Some(writer) = writer {
            if tc.draw(generators::booleans()) {
                writer.commit().unwrap();
                check(Step::Committed);
            } else {
                drop(writer);
                check(Step::RolledBack);
            }
        }
    }
}

/// Every put has the outcome its defect documents, every delete reports
/// whether a tile was there, and at each commit or rollback the table holds
/// exactly the tiles of the writes that were committed.
#[hegel::test]
fn writer_sessions_match_a_model(tc: hegel::TestCase) {
    let gpkg = in_memory();
    let drawn = draw_pyramid(&tc);
    let pyramid = create(&tc, &gpkg, &drawn);
    let matrices = drawn.1;

    let mut committed = Model::new();
    let mut pending = Model::new();
    run_sessions(&tc, &pyramid, &matrices, |step| match step {
        Step::Put {
            coord,
            data,
            defect,
            result,
        } => {
            assert!(
                put_outcome_is_documented(defect, result),
                "a put with defect {defect:?} returned {result:?}"
            );
            if defect == Defect::None {
                pending.insert(address(coord), data.to_vec());
            }
        }
        Step::Delete { coord, result } => {
            let existed = pending.remove(&address(coord)).is_some();
            assert_eq!(result.as_ref().ok(), Some(&existed), "delete at {coord:?}");
        }
        Step::Committed => {
            committed = pending.clone();
            assert_eq!(stored(&gpkg), committed, "after a commit");
        }
        Step::RolledBack => {
            pending = committed.clone();
            assert_eq!(stored(&gpkg), committed, "after a rollback");
        }
    });
}

/// `gpkg_webp` is registered for the pyramid exactly when a WebP tile has been
/// committed to it, and stays registered after that tile is deleted.
#[hegel::test]
fn gpkg_webp_is_registered_once_a_webp_tile_is_committed(tc: hegel::TestCase) {
    let gpkg = in_memory();
    let drawn = draw_pyramid(&tc);
    let pyramid = create(&tc, &gpkg, &drawn);
    let matrices = drawn.1;

    let mut committed = false;
    let mut pending = false;
    run_sessions(&tc, &pyramid, &matrices, |step| match step {
        Step::Put {
            data,
            defect: Defect::None,
            ..
        } if is_webp(data) => pending = true,
        Step::Put { .. } | Step::Delete { .. } => {}
        Step::Committed => {
            committed = pending;
            assert_eq!(webp_registered(&gpkg), committed, "after a commit");
        }
        Step::RolledBack => {
            pending = committed;
            assert_eq!(webp_registered(&gpkg), committed, "after a rollback");
        }
    });
}

/// `write_all` returns the number of tiles it wrote. When a tile fails, the
/// batches before the failing one stay written and the failing batch is rolled
/// back; with `batch_size` 0 the whole call is one batch.
#[hegel::test]
fn write_all_keeps_the_batches_before_a_failure(tc: hegel::TestCase) {
    let gpkg = in_memory();
    let drawn = draw_pyramid(&tc);
    let pyramid = create(&tc, &gpkg, &drawn);
    let matrices = drawn.1;
    let mut model = draw_state(&tc, &pyramid, &matrices);

    let count = tc.draw(generators::integers::<usize>().min_value(0).max_value(20));
    let failing = if count > 0 && tc.draw(generators::booleans()) {
        Some(
            tc.draw(
                generators::integers::<usize>()
                    .min_value(0)
                    .max_value(count - 1),
            ),
        )
    } else {
        None
    };
    let mut tiles: Vec<(TileCoord, Vec<u8>)> = Vec::with_capacity(count);
    for index in 0..count {
        let tag = 1_000 + index as u64;
        let op = if failing == Some(index) {
            draw_put(&tc, &matrices, tag, true)
        } else {
            let matrix = draw_matrix(&tc, &matrices);
            Op::Put {
                coord: draw_inside(&tc, &matrix),
                data: draw_payload(&tc, tile_size(&matrix), tag),
                defect: Defect::None,
            }
        };
        if let Op::Put { coord, data, .. } = op {
            tiles.push((coord, data));
        }
    }
    let batch_size = tc.draw(generators::integers::<usize>().min_value(0).max_value(6));

    let durable = match (failing, batch_size) {
        (None, _) => count,
        (Some(_), 0) => 0,
        (Some(index), size) => index / size * size,
    };
    for (coord, data) in tiles.iter().take(durable) {
        model.insert(address(*coord), data.clone());
    }
    let result = pyramid.write_all(tiles, batch_size);
    match failing {
        None => assert_eq!(result.unwrap(), count),
        Some(_) => drop(result.unwrap_err()),
    }
    assert_eq!(stored(&gpkg), model);
}

// --- reads --------------------------------------------------------------------

/// `get_tile`, `get_tile_into` and `has_tile` agree with the stored tiles at
/// any address: one with a tile, one in a grid without a tile, one outside a
/// grid, and one at an undeclared zoom level. `get_tile_into` leaves the
/// buffer untouched where there is no tile.
#[hegel::test]
fn point_reads_agree_with_the_stored_tiles(tc: hegel::TestCase) {
    let gpkg = in_memory();
    let drawn = draw_pyramid(&tc);
    let pyramid = create(&tc, &gpkg, &drawn);
    let matrices = drawn.1;
    let model = draw_state(&tc, &pyramid, &matrices);

    let queries = tc.draw(generators::integers::<usize>().min_value(1).max_value(8));
    for _ in 0..queries {
        let coord = if !model.is_empty() && tc.draw(generators::booleans()) {
            let keys: Vec<Address> = model.keys().copied().collect();
            let (zoom_level, column, row) = tc.draw(generators::sampled_from(keys));
            TileCoord::new(zoom_level, column, row)
        } else {
            draw_any_address(&tc, &matrices)
        };
        let expected = model.get(&address(coord));

        assert_eq!(
            pyramid.get_tile(coord).unwrap().as_ref(),
            expected,
            "{coord:?}"
        );
        assert_eq!(
            pyramid.has_tile(coord).unwrap(),
            expected.is_some(),
            "{coord:?}"
        );
        let sentinel = b"untouched".to_vec();
        let mut buffer = sentinel.clone();
        let found = pyramid.get_tile_into(coord, &mut buffer).unwrap();
        assert_eq!(found, expected.is_some(), "{coord:?}");
        assert_eq!(&buffer, expected.unwrap_or(&sentinel), "{coord:?}");
    }
}

/// A scan of the whole pyramid yields every stored tile, with its payload, in
/// matrix order.
#[hegel::test]
fn a_scan_yields_every_tile_in_matrix_order(tc: hegel::TestCase) {
    let gpkg = in_memory();
    let drawn = draw_pyramid(&tc);
    let pyramid = create(&tc, &gpkg, &drawn);
    let model = draw_state(&tc, &pyramid, &drawn.1);

    assert_eq!(
        scanned(&mut pyramid.cursor().unwrap()),
        in_matrix_order(model)
    );
}

/// A scan of one zoom level yields that level's stored tiles, with their
/// payloads, in matrix order. A zoom level the pyramid does not declare yields
/// nothing.
#[hegel::test]
fn a_zoom_scan_yields_that_levels_tiles_in_matrix_order(tc: hegel::TestCase) {
    let gpkg = in_memory();
    let drawn = draw_pyramid(&tc);
    let pyramid = create(&tc, &gpkg, &drawn);
    let matrices = drawn.1;
    let model = draw_state(&tc, &pyramid, &matrices);

    let zoom_level = if tc.draw(generators::booleans()) {
        draw_matrix(&tc, &matrices).zoom_level
    } else {
        tc.draw(generators::integers::<i64>())
    };
    let expected = in_matrix_order(
        model
            .into_iter()
            .filter(|((zoom, _, _), _)| *zoom == zoom_level),
    );
    assert_eq!(
        scanned(&mut pyramid.cursor_at(zoom_level).unwrap()),
        expected
    );
}

/// `tile_count` is the number of stored tiles, and `tile_count_at` the number
/// at any zoom level, declared or not.
#[hegel::test]
fn tile_counts_match_the_stored_tiles(tc: hegel::TestCase) {
    let gpkg = in_memory();
    let drawn = draw_pyramid(&tc);
    let pyramid = create(&tc, &gpkg, &drawn);
    let matrices = drawn.1;
    let model = draw_state(&tc, &pyramid, &matrices);

    assert_eq!(pyramid.tile_count().unwrap(), model.len() as i64);
    let zoom_level = if tc.draw(generators::booleans()) {
        draw_matrix(&tc, &matrices).zoom_level
    } else {
        tc.draw(generators::integers::<i64>())
    };
    let at_level = model
        .keys()
        .filter(|(zoom, _, _)| *zoom == zoom_level)
        .count();
    assert_eq!(
        pyramid.tile_count_at(zoom_level).unwrap(),
        at_level as i64,
        "zoom level {zoom_level}"
    );
}

/// A box strictly inside a rectangle of tiles, a quarter of a tile in from
/// each edge, selects exactly the stored tiles of that rectangle.
///
/// No coordinate lands on a tile boundary, so this property is about the
/// addressing, not about which side of a tie a division falls.
/// `a_box_query_returns_the_tiles_it_touches` covers the boundaries.
#[hegel::test]
fn a_box_query_returns_the_tiles_it_covers(tc: hegel::TestCase) {
    let gpkg = in_memory();
    let drawn = draw_pyramid(&tc);
    let pyramid = create(&tc, &gpkg, &drawn);
    let (matrix_set, matrices, _) = drawn;
    let model = draw_state(&tc, &pyramid, &matrices);

    let matrix = draw_matrix(&tc, &matrices);
    let column = |tc: &hegel::TestCase, min: i64| {
        tc.draw(
            generators::integers::<i64>()
                .min_value(min)
                .max_value(matrix.matrix_width - 1),
        )
    };
    let row = |tc: &hegel::TestCase, min: i64| {
        tc.draw(
            generators::integers::<i64>()
                .min_value(min)
                .max_value(matrix.matrix_height - 1),
        )
    };
    let min_column = column(&tc, 0);
    let max_column = column(&tc, min_column);
    let min_row = row(&tc, 0);
    let max_row = row(&tc, min_row);

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

    let expected = in_matrix_order(model.into_iter().filter(|((zoom, c, r), _)| {
        *zoom == matrix.zoom_level
            && (min_column..=max_column).contains(c)
            && (min_row..=max_row).contains(r)
    }));
    assert_eq!(
        scanned(&mut pyramid.cursor_in(matrix.zoom_level, bbox).unwrap()),
        expected
    );
}

/// Draws a box edge on one axis: any `f64`, NaN and the infinities included;
/// a tile boundary of the grid, from one tile before it to one tile past it; or
/// a value in or near the extent.
fn draw_box_edge(tc: &hegel::TestCase, boundaries: &[f64], low: f64, high: f64) -> f64 {
    if tc.draw(generators::weighted_booleans(0.1)) {
        return tc.draw(generators::floats::<f64>());
    }
    if tc.draw(generators::booleans()) {
        tc.draw(generators::sampled_from(boundaries.to_vec()))
    } else {
        let margin = (high - low) / 4.0;
        tc.draw(
            generators::floats::<f64>()
                .min_value(low - margin)
                .max_value(high + margin),
        )
    }
}

/// Draws the two edges of a box on one axis, usually in order. One time in
/// ten they are left as drawn, so that a minimum above its maximum also
/// occurs.
fn draw_box_axis(tc: &hegel::TestCase, boundaries: &[f64], low: f64, high: f64) -> (f64, f64) {
    let (a, b) = (
        draw_box_edge(tc, boundaries, low, high),
        draw_box_edge(tc, boundaries, low, high),
    );
    if b < a && tc.draw(generators::weighted_booleans(0.9)) {
        (b, a)
    } else {
        (a, b)
    }
}

/// Returns `true` if the box, as the set of points `(x, y)` with
/// `min_x <= x <= max_x` and `min_y <= y <= max_y`, shares a point with the
/// tile. A box with a NaN edge, or with a minimum above its maximum, contains
/// no point.
fn box_touches(bbox: BoundingBox, tile: &TileBounds) -> bool {
    bbox.min_x <= bbox.max_x
        && bbox.min_y <= bbox.max_y
        && tile.min_x <= bbox.max_x
        && tile.max_x >= bbox.min_x
        && tile.min_y <= bbox.max_y
        && tile.max_y >= bbox.min_y
}

/// A box query at a declared zoom level yields, in matrix order, the stored
/// tiles at that level whose bounds share a point with the box, edges
/// included, if the box shares a point with the extent; a box that misses the
/// extent yields no tiles, as `cursor_in` documents. The extent condition is
/// separate because a tile's computed bounds can reach past the extent by a
/// rounding error. At an undeclared zoom level the query is an
/// `UnknownZoomLevel` error.
#[hegel::test]
fn a_box_query_returns_the_tiles_it_touches(tc: hegel::TestCase) {
    let gpkg = in_memory();
    let drawn = draw_pyramid(&tc);
    let pyramid = create(&tc, &gpkg, &drawn);
    let (matrix_set, matrices, _) = drawn;
    let model = draw_state(&tc, &pyramid, &matrices);

    let matrix = draw_matrix(&tc, &matrices);
    let columns: Vec<f64> = (-1..=matrix.matrix_width + 1)
        .map(|column| matrix_set.tile_bounds(&matrix, column, 0).min_x)
        .collect();
    let rows: Vec<f64> = (-1..=matrix.matrix_height + 1)
        .map(|row| matrix_set.tile_bounds(&matrix, 0, row).max_y)
        .collect();
    let (min_x, max_x) = draw_box_axis(&tc, &columns, matrix_set.min_x, matrix_set.max_x);
    let (min_y, max_y) = draw_box_axis(&tc, &rows, matrix_set.min_y, matrix_set.max_y);
    let bbox = BoundingBox::new(min_x, min_y, max_x, max_y);

    if tc.draw(generators::weighted_booleans(0.1)) {
        let zoom_level = draw_undeclared_zoom(&tc, &matrices);
        assert!(matches!(
            pyramid.cursor_in(zoom_level, bbox),
            Err(Error::UnknownZoomLevel { .. })
        ));
        return;
    }

    let extent = TileBounds {
        min_x: matrix_set.min_x,
        min_y: matrix_set.min_y,
        max_x: matrix_set.max_x,
        max_y: matrix_set.max_y,
    };
    let expected = in_matrix_order(model.into_iter().filter(|((zoom, column, row), _)| {
        *zoom == matrix.zoom_level
            && box_touches(bbox, &extent)
            && box_touches(bbox, &matrix_set.tile_bounds(&matrix, *column, *row))
    }));
    tc.event(if expected.is_empty() {
        "the box selects no stored tile"
    } else {
        "the box selects a stored tile"
    });
    assert_eq!(
        scanned(&mut pyramid.cursor_in(matrix.zoom_level, bbox).unwrap()),
        expected,
        "{bbox:?}"
    );
}
