//! Property tests over the C tile API: pyramid creation, writes, reads and
//! cursors, against the Rust API.
//!
//! Each case creates a pyramid through the C API and an identical twin
//! through the Rust API, applies the same drawn operations to both, and
//! compares each outcome.
//!
//! One `unsafe` block per call, as the rest of this crate's tests do.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::ffi::{CStr, CString};
use std::path::Path;

use geopackage::core::tiles::{TileCoord, TileMatrixSet, ZoomLadder};
use geopackage::{BoundingBox, GeoPackage, TilePyramid, TilePyramidBuilder};
use geopackage_ffi::{
    Status, gpkg_bytes_free, gpkg_close, gpkg_create, gpkg_error_clear, gpkg_error_t, gpkg_t,
    gpkg_tile_cursor_free, gpkg_tile_cursor_next, gpkg_tile_cursor_t, gpkg_tiles_count,
    gpkg_tiles_count_at, gpkg_tiles_create, gpkg_tiles_cursor, gpkg_tiles_cursor_at,
    gpkg_tiles_cursor_in, gpkg_tiles_delete, gpkg_tiles_extent, gpkg_tiles_free, gpkg_tiles_get,
    gpkg_tiles_get_into, gpkg_tiles_has, gpkg_tiles_matrix_at, gpkg_tiles_put, gpkg_tiles_t,
    gpkg_tiles_zoom_level_count,
};
use hegel::generators;

/// An error slot holding a value no call writes on success.
fn sentinel() -> gpkg_error_t {
    gpkg_error_t {
        code: Status::Other,
        message: std::ptr::null_mut(),
    }
}

/// Checks the error protocol for a call that returned `status`: a success,
/// or a failure the documentation says sets no message, leaves the slot as it
/// was; any other failure fills it with the same status and a message.
fn check(status: Status, error: &mut gpkg_error_t, silent: bool) -> Status {
    if status == Status::Ok || silent {
        assert_eq!(error.code, Status::Other, "{status:?} wrote the slot");
        assert!(error.message.is_null(), "{status:?} wrote the slot");
        return status;
    }
    assert!(!error.message.is_null(), "a failure without a message");
    // SAFETY: the library filled this in and has not freed it.
    let message = unsafe { CStr::from_ptr(error.message) }
        .to_string_lossy()
        .into_owned();
    assert_eq!(error.code, status, "the slot disagrees: {message}");
    // SAFETY: a slot this library filled in.
    unsafe { gpkg_error_clear(error) };
    *error = sentinel();
    status
}

fn status_of<T>(result: &geopackage::Result<T>) -> Status {
    result.as_ref().map_or_else(Status::from, |_| Status::Ok)
}

/// A PNG header of the given size.
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&13_u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    bytes
}

/// A WebP header of the given size.
fn webp(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::from(*b"RIFF");
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(b"WEBPVP8X");
    bytes.extend_from_slice(&10_u32.to_le_bytes());
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    bytes.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
    bytes.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
    bytes
}

/// The arguments of `gpkg_tiles_create`, as drawn.
#[derive(Debug, Clone, Copy)]
struct Create {
    srs_id: i32,
    extent: (f64, f64, f64, f64),
    zooms: (i64, i64),
    base: (i64, i64),
    tile: (i64, i64),
}

impl Create {
    /// The ladder the documentation describes: zero for both values of a
    /// pair takes the default.
    fn ladder(self) -> ZoomLadder {
        let mut ladder = ZoomLadder::new(self.zooms.0, self.zooms.1);
        if self.base != (0, 0) {
            ladder = ladder.base_grid(self.base.0, self.base.1);
        }
        if self.tile != (0, 0) {
            ladder = ladder.tile_size(self.tile.0, self.tile.1);
        }
        ladder
    }

    fn set(self) -> TileMatrixSet {
        let (min_x, min_y, max_x, max_y) = self.extent;
        TileMatrixSet::new(self.srs_id, min_x, min_y, max_x, max_y)
    }
}

/// Draws creation arguments, mostly valid, sometimes with an unknown SRS, an
/// inverted extent or zoom range, or one value of a pair zero.
fn draw_create(tc: &hegel::TestCase) -> Create {
    let small = |tc: &hegel::TestCase, low: i64, high: i64| {
        tc.draw(generators::integers::<i64>().min_value(low).max_value(high))
    };
    let min_zoom = small(tc, -1, 2);
    let pair =
        |tc: &hegel::TestCase, values: Vec<(i64, i64)>| tc.draw(generators::sampled_from(values));
    Create {
        srs_id: tc.draw(generators::sampled_from(vec![4326, 4326, 4326, 3857])),
        extent: tc.draw(generators::sampled_from(vec![
            (0.0, 0.0, 16.0, 16.0),
            (-180.0, -90.0, 180.0, 90.0),
            (5.0, 0.0, 1.0, 16.0),
        ])),
        zooms: (min_zoom, min_zoom + small(tc, -1, 2)),
        base: pair(tc, vec![(0, 0), (1, 1), (2, 1), (0, 1)]),
        tile: pair(tc, vec![(0, 0), (16, 16), (16, 8), (8, 0)]),
    }
}

/// Creates a pyramid named `p` through the C API.
fn create_c(gpkg: *const gpkg_t, args: Create, error: &mut gpkg_error_t) -> *mut gpkg_tiles_t {
    let name = CString::new("p").unwrap();
    let (min_x, min_y, max_x, max_y) = args.extent;
    // SAFETY: a live container, a NUL-terminated name and a writable slot.
    unsafe {
        gpkg_tiles_create(
            gpkg,
            name.as_ptr(),
            args.srs_id,
            min_x,
            min_y,
            max_x,
            max_y,
            args.zooms.0,
            args.zooms.1,
            args.base.0,
            args.base.1,
            args.tile.0,
            args.tile.1,
            error,
        )
    }
}

/// Creates the twin through the Rust API, or returns the status the C API
/// documents for arguments the ladder rejects.
fn create_rust(gpkg: &GeoPackage, args: Create) -> Result<TilePyramid<'_>, Status> {
    let set = args.set();
    let matrices = set
        .ladder(args.ladder())
        .map_err(|_ladder| Status::InvalidArgument)?;
    gpkg.create_tile_pyramid(&TilePyramidBuilder::new("p", set).matrices(matrices))
        .map_err(|error| Status::from(&error))
}

fn open_c() -> (*mut gpkg_t, gpkg_error_t) {
    let mut error = sentinel();
    let path = CString::new(":memory:").unwrap();
    // SAFETY: a NUL-terminated path and a writable error slot.
    let gpkg = unsafe { gpkg_create(path.as_ptr(), &raw mut error) };
    assert!(!gpkg.is_null());
    (gpkg, error)
}

/// What the C API reports of a pyramid's grid: each zoom level's numbers,
/// and the extent and SRS.
type Grid = (Vec<(i64, i64, i64, i64, i64)>, (f64, f64, f64, f64, i32));

fn grid_c(tiles: *const gpkg_tiles_t, error: &mut gpkg_error_t) -> Grid {
    let mut count = 0;
    // SAFETY: a live tiles handle and a writable out-parameter.
    let status = unsafe { gpkg_tiles_zoom_level_count(tiles, &raw mut count, error) };
    assert_eq!(check(status, error, false), Status::Ok);
    let levels = (0..count)
        .map(|index| {
            let mut level = (0, 0, 0, 0, 0);
            // SAFETY: a live tiles handle, an index below the count, and
            // writable out-parameters.
            let status = unsafe {
                gpkg_tiles_matrix_at(
                    tiles,
                    index,
                    &raw mut level.0,
                    &raw mut level.1,
                    &raw mut level.2,
                    &raw mut level.3,
                    &raw mut level.4,
                    error,
                )
            };
            assert_eq!(check(status, error, false), Status::Ok);
            level
        })
        .collect();
    let mut extent = (0.0, 0.0, 0.0, 0.0, 0);
    // SAFETY: a live tiles handle and writable out-parameters.
    let status = unsafe {
        gpkg_tiles_extent(
            tiles,
            &raw mut extent.0,
            &raw mut extent.1,
            &raw mut extent.2,
            &raw mut extent.3,
            &raw mut extent.4,
            error,
        )
    };
    assert_eq!(check(status, error, false), Status::Ok);
    (levels, extent)
}

fn grid_rust(pyramid: &TilePyramid<'_>) -> Grid {
    let levels = pyramid
        .matrices()
        .iter()
        .map(|m| {
            (
                m.zoom_level,
                m.matrix_width,
                m.matrix_height,
                m.tile_width,
                m.tile_height,
            )
        })
        .collect();
    let set = pyramid.matrix_set();
    (
        levels,
        (set.min_x, set.min_y, set.max_x, set.max_y, set.srs_id),
    )
}

/// `gpkg_tiles_create` succeeds exactly when the Rust API creates the same
/// pyramid, fails with the same status otherwise, and reports the grid the
/// Rust API stores.
#[hegel::test]
fn creation_agrees_with_the_rust_api(tc: hegel::TestCase) {
    let args = draw_create(&tc);
    let (gpkg, mut error) = open_c();
    let tiles = create_c(gpkg, args, &mut error);
    let status = if tiles.is_null() {
        error.code
    } else {
        Status::Ok
    };
    check(status, &mut error, false);
    let rust = GeoPackage::create(Path::new(":memory:")).unwrap();
    let twin = create_rust(&rust, args);
    tc.event(format!("{status:?}"));
    match &twin {
        Ok(pyramid) => {
            assert_eq!(status, Status::Ok, "{args:?}");
            assert_eq!(grid_c(tiles, &mut error), grid_rust(pyramid), "{args:?}");
        }
        Err(expected) => assert_eq!(status, *expected, "{args:?}"),
    }
    // SAFETY: a tiles handle or NULL, freed once.
    unsafe { gpkg_tiles_free(tiles) };
    // SAFETY: a live container with no children.
    let status = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(check(status, &mut error, false), Status::Ok);
}

/// One operation on a pyramid, as drawn.
#[derive(Debug)]
enum Op {
    Put(TileCoord, Vec<u8>),
    Get(TileCoord),
    /// A read into a buffer of the given capacity, or with a NULL buffer.
    GetInto(TileCoord, Option<usize>),
    Has(TileCoord),
    Delete(TileCoord),
    Count,
    CountAt(i64),
    Cursor,
    CursorAt(i64),
    CursorIn(i64, (f64, f64, f64, f64)),
}

fn draw_coord(tc: &hegel::TestCase, pyramid: &TilePyramid<'_>) -> TileCoord {
    let zooms = pyramid.zoom_levels();
    let first = zooms.first().copied().unwrap_or(0);
    let last = zooms.last().copied().unwrap_or(0);
    // Usually an address in the grid of a declared level; otherwise one
    // level or one tile beyond.
    let inside = tc.draw(generators::weighted_booleans(0.7));
    let (low, high) = if inside {
        (first, last)
    } else {
        (first - 1, last + 1)
    };
    let zoom = tc.draw(generators::integers::<i64>().min_value(low).max_value(high));
    let (width, height) = pyramid
        .matrix(zoom)
        .map_or((1, 1), |m| (m.matrix_width, m.matrix_height));
    let (low, extra) = if inside { (0, 1) } else { (-1, 0) };
    let column = tc.draw(
        generators::integers::<i64>()
            .min_value(low)
            .max_value(width - extra),
    );
    let row = tc.draw(
        generators::integers::<i64>()
            .min_value(low)
            .max_value(height - extra),
    );
    TileCoord::new(zoom, column, row)
}

/// Draws a payload: an image of the zoom level's tile size or of another
/// size, in PNG or WebP, or bytes that are not an image.
fn draw_payload(tc: &hegel::TestCase, pyramid: &TilePyramid<'_>, zoom: i64) -> Vec<u8> {
    let (width, height) = pyramid.matrix(zoom).map_or((16, 16), |m| {
        (
            u32::try_from(m.tile_width).unwrap(),
            u32::try_from(m.tile_height).unwrap(),
        )
    });
    let (width, height) = if tc.draw(generators::weighted_booleans(0.8)) {
        (width, height)
    } else {
        (width + 1, height)
    };
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(3)) {
        0 | 1 => png(width, height),
        2 => webp(width, height),
        _ => tc.draw(generators::vecs(generators::integers::<u8>()).max_size(16)),
    }
}

fn draw_op(tc: &hegel::TestCase, pyramid: &TilePyramid<'_>) -> Op {
    let coord = draw_coord(tc, pyramid);
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(9)) {
        0 | 1 => Op::Put(coord, draw_payload(tc, pyramid, coord.zoom_level)),
        2 => Op::Get(coord),
        3 => {
            let capacity = tc
                .draw(generators::booleans())
                .then(|| tc.draw(generators::integers::<usize>().min_value(0).max_value(64)));
            Op::GetInto(coord, capacity)
        }
        4 => Op::Has(coord),
        5 => Op::Delete(coord),
        6 => {
            if tc.draw(generators::booleans()) {
                Op::Count
            } else {
                Op::CountAt(coord.zoom_level)
            }
        }
        7 => Op::Cursor,
        8 => Op::CursorAt(coord.zoom_level),
        _ => {
            let set = pyramid.matrix_set();
            let span = |tc: &hegel::TestCase, low: f64, high: f64| {
                let a = tc.draw(generators::floats::<f64>().min_value(low).max_value(high));
                let b = tc.draw(generators::floats::<f64>().min_value(low).max_value(high));
                (a.min(b), a.max(b))
            };
            let (min_x, max_x) = span(tc, set.min_x - 1.0, set.max_x + 1.0);
            let (min_y, max_y) = span(tc, set.min_y - 1.0, set.max_y + 1.0);
            Op::CursorIn(coord.zoom_level, (min_x, min_y, max_x, max_y))
        }
    }
}

/// A tile a cursor returned: its zoom level, column, row and bytes.
type Stored = (i64, i64, i64, Vec<u8>);

/// What an operation reported: its status, and any value it returned.
#[derive(Debug, PartialEq)]
enum Outcome {
    Status(Status),
    Bytes(Status, Option<Vec<u8>>),
    /// A read into a buffer: the status, the reported length, and the bytes
    /// copied.
    Into(Status, usize, Option<Vec<u8>>),
    Flag(Status, Option<bool>),
    Count(Status, Option<i64>),
    Tiles(Status, Option<Vec<Stored>>),
}

/// Drains a C cursor, or reports the status of a failed open.
fn drain_c(cursor: *mut gpkg_tile_cursor_t, error: &mut gpkg_error_t) -> Outcome {
    let status = if cursor.is_null() {
        error.code
    } else {
        Status::Ok
    };
    if check(status, error, false) != Status::Ok {
        return Outcome::Tiles(status, None);
    }
    let mut tiles = Vec::new();
    loop {
        let (mut zoom, mut column, mut row) = (0, 0, 0);
        let mut data = std::ptr::null();
        let mut len = 0;
        // SAFETY: a live cursor and writable out-parameters.
        let status = unsafe {
            gpkg_tile_cursor_next(
                cursor,
                &raw mut zoom,
                &raw mut column,
                &raw mut row,
                &raw mut data,
                &raw mut len,
                error,
            )
        };
        assert_eq!(check(status, error, false), Status::Ok);
        if data.is_null() {
            break;
        }
        // SAFETY: the cursor lends `len` bytes until its next call.
        let bytes = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
        tiles.push((zoom, column, row, bytes));
    }
    // SAFETY: a live cursor, freed once.
    unsafe { gpkg_tile_cursor_free(cursor) };
    Outcome::Tiles(Status::Ok, Some(tiles))
}

fn drain_rust(cursor: geopackage::Result<geopackage::TileCursor<'_>>) -> Outcome {
    let mut cursor = match cursor {
        Ok(cursor) => cursor,
        Err(error) => return Outcome::Tiles(Status::from(&error), None),
    };
    let mut tiles = Vec::new();
    let mut stream = cursor.tiles().unwrap();
    while let Some(tile) = stream.next().unwrap() {
        let coord = tile.coord();
        tiles.push((coord.zoom_level, coord.column, coord.row, tile.to_vec()));
    }
    Outcome::Tiles(Status::Ok, Some(tiles))
}

fn apply_c(tiles: *const gpkg_tiles_t, op: &Op, error: &mut gpkg_error_t) -> Outcome {
    match op {
        Op::Put(coord, data) => {
            // SAFETY: a live tiles handle and `data` readable for its length.
            let status = unsafe {
                gpkg_tiles_put(
                    tiles,
                    coord.zoom_level,
                    coord.column,
                    coord.row,
                    data.as_ptr(),
                    data.len(),
                    error,
                )
            };
            Outcome::Status(check(status, error, false))
        }
        Op::Get(coord) => {
            let mut data = std::ptr::null_mut();
            let mut len = 0;
            // SAFETY: a live tiles handle and writable out-parameters.
            let status = unsafe {
                gpkg_tiles_get(
                    tiles,
                    coord.zoom_level,
                    coord.column,
                    coord.row,
                    &raw mut data,
                    &raw mut len,
                    error,
                )
            };
            let status = check(status, error, status == Status::NotFound);
            if status != Status::Ok {
                assert!(data.is_null() && len == 0, "{status:?} returned bytes");
                return Outcome::Bytes(status, None);
            }
            // SAFETY: an allocation of `len` bytes this call returned.
            let bytes = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
            // SAFETY: freed once, with the length this call wrote.
            unsafe { gpkg_bytes_free(data, len) };
            Outcome::Bytes(status, Some(bytes))
        }
        Op::GetInto(coord, capacity) => {
            let mut buffer = vec![0xA5_u8; capacity.unwrap_or(0)];
            let pointer = if capacity.is_some() {
                buffer.as_mut_ptr()
            } else {
                std::ptr::null_mut()
            };
            let mut len = usize::MAX;
            // SAFETY: a live tiles handle, a buffer writable for its
            // capacity or NULL with capacity zero, and writable
            // out-parameters.
            let status = unsafe {
                gpkg_tiles_get_into(
                    tiles,
                    coord.zoom_level,
                    coord.column,
                    coord.row,
                    pointer,
                    buffer.len(),
                    &raw mut len,
                    error,
                )
            };
            let status = check(status, error, status == Status::NotFound);
            let copied = (status == Status::Ok)
                .then(|| buffer.get(..len).map(<[u8]>::to_vec))
                .flatten();
            Outcome::Into(status, len, copied)
        }
        Op::Has(coord) => {
            let mut out = false;
            // SAFETY: a live tiles handle and a writable out-parameter.
            let status = unsafe {
                gpkg_tiles_has(
                    tiles,
                    coord.zoom_level,
                    coord.column,
                    coord.row,
                    &raw mut out,
                    error,
                )
            };
            let status = check(status, error, false);
            Outcome::Flag(status, (status == Status::Ok).then_some(out))
        }
        Op::Delete(coord) => {
            let mut out = false;
            // SAFETY: as above.
            let status = unsafe {
                gpkg_tiles_delete(
                    tiles,
                    coord.zoom_level,
                    coord.column,
                    coord.row,
                    &raw mut out,
                    error,
                )
            };
            let status = check(status, error, false);
            Outcome::Flag(status, (status == Status::Ok).then_some(out))
        }
        Op::Count => {
            let mut out = 0;
            // SAFETY: as above.
            let status = unsafe { gpkg_tiles_count(tiles, &raw mut out, error) };
            let status = check(status, error, false);
            Outcome::Count(status, (status == Status::Ok).then_some(out))
        }
        Op::CountAt(zoom) => {
            let mut out = 0;
            // SAFETY: as above.
            let status = unsafe { gpkg_tiles_count_at(tiles, *zoom, &raw mut out, error) };
            let status = check(status, error, false);
            Outcome::Count(status, (status == Status::Ok).then_some(out))
        }
        // SAFETY: a live tiles handle; the cursor is drained and freed.
        Op::Cursor => drain_c(unsafe { gpkg_tiles_cursor(tiles, error) }, error),
        // SAFETY: as above.
        Op::CursorAt(zoom) => drain_c(unsafe { gpkg_tiles_cursor_at(tiles, *zoom, error) }, error),
        Op::CursorIn(zoom, (min_x, min_y, max_x, max_y)) => {
            // SAFETY: as above.
            let cursor = unsafe {
                gpkg_tiles_cursor_in(tiles, *zoom, *min_x, *min_y, *max_x, *max_y, error)
            };
            drain_c(cursor, error)
        }
    }
}

fn apply_rust(pyramid: &TilePyramid<'_>, op: &Op) -> Outcome {
    match op {
        Op::Put(coord, data) => Outcome::Status(status_of(&pyramid.put_tile(*coord, data))),
        Op::Get(coord) => match pyramid.get_tile(*coord) {
            Ok(Some(bytes)) => Outcome::Bytes(Status::Ok, Some(bytes)),
            Ok(None) => Outcome::Bytes(Status::NotFound, None),
            Err(error) => Outcome::Bytes(Status::from(&error), None),
        },
        Op::GetInto(coord, capacity) => {
            let capacity = capacity.unwrap_or(0);
            match pyramid.get_tile(*coord) {
                Ok(Some(bytes)) if bytes.len() <= capacity => {
                    Outcome::Into(Status::Ok, bytes.len(), Some(bytes))
                }
                Ok(Some(bytes)) => Outcome::Into(Status::InvalidArgument, bytes.len(), None),
                Ok(None) => Outcome::Into(Status::NotFound, 0, None),
                Err(error) => Outcome::Into(Status::from(&error), usize::MAX, None),
            }
        }
        Op::Has(coord) => {
            let result = pyramid.has_tile(*coord);
            Outcome::Flag(status_of(&result), result.ok())
        }
        Op::Delete(coord) => {
            let result = pyramid.delete_tile(*coord);
            Outcome::Flag(status_of(&result), result.ok())
        }
        Op::Count => {
            let result = pyramid.tile_count();
            Outcome::Count(status_of(&result), result.ok())
        }
        Op::CountAt(zoom) => {
            let result = pyramid.tile_count_at(*zoom);
            Outcome::Count(status_of(&result), result.ok())
        }
        Op::Cursor => drain_rust(pyramid.cursor()),
        Op::CursorAt(zoom) => drain_rust(pyramid.cursor_at(*zoom)),
        Op::CursorIn(zoom, (min_x, min_y, max_x, max_y)) => {
            drain_rust(pyramid.cursor_in(*zoom, BoundingBox::new(*min_x, *min_y, *max_x, *max_y)))
        }
    }
}

/// Any operations on a pyramid give the same outcome through the C API as
/// through the Rust API, and every failure follows the error protocol, apart
/// from the documented silent `NOT_FOUND` of the two reads.
#[hegel::test]
fn operations_agree_with_the_rust_api(tc: hegel::TestCase) {
    let mut args = draw_create(&tc);
    // Creation is the other property's subject; this one needs a pyramid.
    args.srs_id = 4326;
    args.extent = (0.0, 0.0, 16.0, 16.0);
    args.zooms = (args.zooms.0.max(0), args.zooms.0.max(0) + 1);
    if args.base.0 == 0 || args.base.1 == 0 {
        args.base = (0, 0);
    }
    if args.tile.0 == 0 || args.tile.1 == 0 {
        args.tile = (16, 16);
    }
    let (gpkg, mut error) = open_c();
    let tiles = create_c(gpkg, args, &mut error);
    assert!(!tiles.is_null(), "{args:?}");
    let rust = GeoPackage::create(Path::new(":memory:")).unwrap();
    let twin = create_rust(&rust, args).unwrap();

    let steps = tc.draw(generators::integers::<usize>().min_value(1).max_value(12));
    for _ in 0..steps {
        let op = draw_op(&tc, &twin);
        let c = apply_c(tiles, &op, &mut error);
        let r = apply_rust(&twin, &op);
        tc.event(format!("{c:?}").split('(').next().unwrap());
        if let Outcome::Status(status) = &c {
            tc.event(format!("put {status:?}"));
        }
        assert_eq!(c, r, "{op:?}");
    }
    // SAFETY: a live tiles handle, freed once.
    unsafe { gpkg_tiles_free(tiles) };
    // SAFETY: a live container with no children.
    let status = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(check(status, &mut error, false), Status::Ok);
}

/// `gpkg_tiles_create_web_mercator` agrees with the Rust API for any zoom
/// range, and the pyramid list and names the C API reports are the ones the
/// Rust API reads.
#[hegel::test]
fn web_mercator_creation_and_names_agree_with_the_rust_api(tc: hegel::TestCase) {
    let (gpkg, mut error) = open_c();
    let rust = GeoPackage::create(Path::new(":memory:")).unwrap();
    // The documentation asks for EPSG:3857 to be registered first.
    if tc.draw(generators::weighted_booleans(0.8)) {
        // SAFETY: a live container and a writable error slot.
        let status = unsafe { geopackage_ffi::gpkg_add_epsg_srs(gpkg, 3857, &raw mut error) };
        assert_eq!(check(status, &mut error, false), Status::Ok);
        rust.add_epsg_srs(3857).unwrap();
    }
    let pyramids = tc.draw(generators::integers::<usize>().min_value(0).max_value(3));
    for index in 0..pyramids {
        let name = format!("p{index}");
        let min_zoom = tc.draw(generators::integers::<i64>().min_value(-1).max_value(3));
        let max_zoom = min_zoom + tc.draw(generators::integers::<i64>().min_value(-1).max_value(2));
        let c_name = CString::new(name.clone()).unwrap();
        // SAFETY: a live container, a NUL-terminated name and a writable
        // error slot.
        let tiles = unsafe {
            geopackage_ffi::gpkg_tiles_create_web_mercator(
                gpkg,
                c_name.as_ptr(),
                min_zoom,
                max_zoom,
                &raw mut error,
            )
        };
        let status = if tiles.is_null() {
            error.code
        } else {
            Status::Ok
        };
        check(status, &mut error, false);
        let set = TileMatrixSet::web_mercator_quad();
        let twin = match set.ladder(ZoomLadder::new(min_zoom, max_zoom)) {
            Err(_ladder) => Err(Status::InvalidArgument),
            Ok(matrices) => rust
                .create_tile_pyramid(&TilePyramidBuilder::new(&name, set).matrices(matrices))
                .map_err(|e| Status::from(&e)),
        };
        tc.event(format!("{status:?}"));
        match &twin {
            Ok(pyramid) => {
                assert_eq!(status, Status::Ok, "{min_zoom}..{max_zoom}");
                assert_eq!(grid_c(tiles, &mut error), grid_rust(pyramid));
                // SAFETY: a live tiles handle.
                let reported = unsafe { geopackage_ffi::gpkg_tiles_name(tiles, &raw mut error) };
                assert!(!reported.is_null());
                // SAFETY: a string this library returned.
                let reported_name = unsafe { CStr::from_ptr(reported) }
                    .to_str()
                    .unwrap()
                    .to_owned();
                // SAFETY: freed once.
                unsafe { geopackage_ffi::gpkg_string_free(reported) };
                assert_eq!(reported_name, name);
            }
            Err(expected) => assert_eq!(status, *expected, "{min_zoom}..{max_zoom}"),
        }
        // SAFETY: a tiles handle or NULL, freed once.
        unsafe { gpkg_tiles_free(tiles) };
    }
    let names: Vec<String> = rust
        .tile_pyramids()
        .unwrap()
        .iter()
        .map(|pyramid| pyramid.table_name().to_owned())
        .collect();
    let mut count = usize::MAX;
    // SAFETY: a live container and a writable out-parameter.
    let status =
        unsafe { geopackage_ffi::gpkg_tiles_names_count(gpkg, &raw mut count, &raw mut error) };
    assert_eq!(check(status, &mut error, false), Status::Ok);
    assert_eq!(count, names.len());
    for index in 0..=count {
        // SAFETY: a live container.
        let name = unsafe { geopackage_ffi::gpkg_tiles_name_at(gpkg, index, &raw mut error) };
        match names.get(index) {
            Some(expected) => {
                assert!(!name.is_null());
                check(Status::Ok, &mut error, false);
                // SAFETY: a string this library returned.
                assert_eq!(unsafe { CStr::from_ptr(name) }.to_str().unwrap(), expected);
                // SAFETY: freed once.
                unsafe { geopackage_ffi::gpkg_string_free(name) };
            }
            None => {
                assert!(name.is_null());
                assert_eq!(check(error.code, &mut error, false), Status::NotFound);
            }
        }
    }
    // SAFETY: a live container with no children.
    let status = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(check(status, &mut error, false), Status::Ok);
}
