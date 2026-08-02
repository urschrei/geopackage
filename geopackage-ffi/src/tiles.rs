//! `gpkg_tiles_t`: a handle onto one tile pyramid.
//!
//! A pyramid is pre-rendered raster tiles, addressed by zoom level, column and
//! row. [`gpkg_tiles_open`] takes one by name, the same `gpkg_contents` table
//! name a GeoPackage tool would show, and [`gpkg_tiles_free`] releases it.
//! [`gpkg_tiles_names_count`] and [`gpkg_tiles_name_at`] enumerate the file's
//! pyramids for a caller that does not already know the name.
//!
//! Rows count from the **top** of the pyramid's extent downwards, as WMTS and
//! XYZ do and TMS does not, and both indices are relative to that extent rather
//! than to a global grid.
//!
//! **No image is decoded anywhere here.** A tile goes out as the bytes the file
//! stores and comes in as the bytes the caller supplies. Writing checks the
//! address against the zoom level's grid and the payload's header against the
//! declared tile size, which is a header read rather than a decode; turning
//! tiles into pixels needs an image library on top of this one.
//!
//! # Walking a pyramid
//!
//! ```c
//! gpkg_tiles_t *tiles = gpkg_tiles_open(gpkg, "basemap", &error);
//! if (!tiles) {
//!     return fail("gpkg_tiles_open", &error);
//! }
//!
//! double min_x, min_y, max_x, max_y;
//! int32_t srs_id = 0;
//! gpkg_tiles_extent(tiles, &min_x, &min_y, &max_x, &max_y, &srs_id, &error);
//! printf("extent %f %f %f %f in srs %d\n", min_x, min_y, max_x, max_y,
//!        (int)srs_id);
//!
//! // Zoom levels are walked by position in the declared ladder, not by zoom
//! // number, so a pyramid that starts at zoom 7 needs no guessing.
//! size_t levels = 0;
//! gpkg_tiles_zoom_level_count(tiles, &levels, &error);
//! for (size_t i = 0; i < levels; i++) {
//!     int64_t zoom = 0, width = 0, height = 0;
//!     gpkg_tiles_matrix_at(tiles, i, &zoom, &width, &height, NULL, NULL, &error);
//!
//!     int64_t count = 0;
//!     gpkg_tiles_count_at(tiles, zoom, &count, &error);
//!     printf("zoom %lld: %lldx%lld grid, %lld tiles\n", (long long)zoom,
//!            (long long)width, (long long)height, (long long)count);
//! }
//!
//! gpkg_tiles_free(tiles);
//! ```
//!
//! # Two readers
//!
//! [`gpkg_tiles_get`] allocates a buffer and hands it over;
//! [`gpkg_tiles_get_into`] copies into a buffer the caller already has. They
//! return the same bytes, and the difference is who owns the memory.
//!
//! An allocation that crosses the boundary has to come back with its length,
//! since that is what [`gpkg_bytes_free`] reconstructs it from, and a wrong
//! length there is undefined behaviour. Nothing crosses in the other reader,
//! so there is no length to keep and nothing to get wrong; it also spares an
//! allocation per tile, which tells for a caller reading many.
//!
//! ```c
//! // One buffer, reused across every tile: the reader that owns nothing.
//! unsigned char buffer[64 * 1024];
//! size_t len = 0;
//! gpkg_status status =
//!     gpkg_tiles_get_into(tiles, 1, 0, 0, buffer, sizeof(buffer), &len, &error);
//!
//! if (status == GPKG_STATUS_INVALID_ARGUMENT) {
//!     // Too small: `len` says how much room the tile needs, so a caller that
//!     // cannot size its buffer in advance calls again with that much.
//!     gpkg_error_clear(&error);
//!     unsigned char *bigger = malloc(len);
//!     status = gpkg_tiles_get_into(tiles, 1, 0, 0, bigger, len, &len, &error);
//!     // ... use bigger, then free it ...
//!     free(bigger);
//! } else if (status == GPKG_STATUS_NOT_FOUND) {
//!     // No tile at that address, which a sparse pyramid is entitled to. No
//!     // message is set for it, so there is nothing to clear.
//! } else if (status != GPKG_STATUS_OK) {
//!     return fail("gpkg_tiles_get_into", &error);
//! }
//! ```
//!
//! ```c
//! // The owning reader, for a caller that would rather not size a buffer.
//! uint8_t *data = NULL;
//! size_t len = 0;
//! if (gpkg_tiles_get(tiles, 1, 0, 0, &data, &len, &error) == GPKG_STATUS_OK) {
//!     fwrite(data, 1, len, out);
//!     gpkg_bytes_free(data, len);   // the same len the call wrote
//! }
//! ```
//!
//! # Writing
//!
//! ```c
//! if (gpkg_tiles_put(tiles, 1, 0, 0, png, png_len, &error) != GPKG_STATUS_OK) {
//!     return fail("gpkg_tiles_put", &error);
//! }
//! ```
//!
//! The payload is copied into the file during the call, so the caller's buffer
//! is its own again on return. An address outside the zoom level's grid, or a
//! payload whose header says it is a different pixel size from the one the
//! level declares, is rejected rather than stored.
//!
//! # Why a pyramid handle is held rather than rebuilt
//!
//! [`gpkg_tiles_get`] measures about 5 microseconds. Rebuilding a borrowed
//! pyramid handle inside every call costs about 40 on top of that, which would
//! make a tile read from C roughly eight times slower than the same read from
//! Rust. So the handle keeps the pyramid, and [`crate::handle`] is where the
//! borrow that implies is accounted for.

use std::ffi::c_char;

use geopackage::core::tiles::{TileCoord, TileMatrixSet, ZoomLadder};
use geopackage::{BoundingBox, TilePyramidBuilder};

use crate::error::{Status, gpkg_error_t, set_error, set_library_error};
use crate::handle::{Container, CursorScan, TileCursorHandle, TilesHandle};
use crate::util::{borrow_str, out_string};

/// A tile pyramid of an open GeoPackage. Opaque; created by `gpkg_tiles_open`
/// and destroyed by `gpkg_tiles_free`.
#[expect(
    non_camel_case_types,
    reason = "the C name is the type's name; cbindgen emits it verbatim"
)]
pub type gpkg_tiles_t = TilesHandle;

/// Returns the number of tile pyramids the file declares.
///
/// Tile pyramids only: a table `gpkg_contents` declares as `features` or
/// `attributes` is not counted, and neither is a `tiles` row with no matching
/// `gpkg_tile_matrix_set` entry. `gpkg_tiles_name_at` walks the same list,
/// which is ordered by table name.
///
/// # Safety
///
/// `gpkg` must be a live container handle, `out` writable, `error` NULL or
/// writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_tiles_names_count(
    gpkg: *const Container,
    out: *mut usize,
    error: *mut gpkg_error_t,
) -> Status {
    if gpkg.is_null() || out.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "gpkg handle or out is NULL") };
        return Status::BadArgument;
    }
    // SAFETY: the caller guarantees a live container handle.
    match unsafe { (*gpkg).gpkg().tile_pyramids() } {
        Ok(pyramids) => {
            // SAFETY: the caller guarantees `out` is writable.
            unsafe { *out = pyramids.len() };
            Status::Ok
        }
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            Status::from(&err)
        }
    }
}

/// Returns the name of the `index`th tile pyramid, or NULL when out of range.
///
/// The list is the one `gpkg_tiles_names_count` counts, ordered by table name,
/// so the pair walks a file's pyramids. An index at or beyond the count is
/// `GPKG_STATUS_NOT_FOUND` rather than a failure to read.
///
/// Owned by the caller; release with `gpkg_string_free`.
///
/// # Safety
///
/// `gpkg` must be a live container handle; `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_tiles_name_at(
    gpkg: *const Container,
    index: usize,
    error: *mut gpkg_error_t,
) -> *mut c_char {
    if gpkg.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "gpkg handle is NULL") };
        return std::ptr::null_mut();
    }
    // SAFETY: the caller guarantees a live container handle.
    let pyramids = match unsafe { (*gpkg).gpkg().tile_pyramids() } {
        Ok(pyramids) => pyramids,
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            return std::ptr::null_mut();
        }
    };
    let Some(pyramid) = pyramids.get(index) else {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::NotFound, "pyramid index out of range") };
        return std::ptr::null_mut();
    };
    let name = pyramid.table_name().to_owned();
    // SAFETY: forwarded to the caller's guarantee on `error`.
    unsafe { out_string(&name, error) }
}

/// Opens a tile pyramid by name.
///
/// The name is a `gpkg_contents.table_name` whose row declares `tiles`.
/// A name no such row declares is `GPKG_STATUS_NOT_FOUND`, and a name that
/// declares something else is `GPKG_STATUS_INVALID_ARGUMENT`.
///
/// Opening reads the tile matrix set and every zoom level's matrix, so the
/// handle answers `gpkg_tiles_extent`, `gpkg_tiles_zoom_level_count` and
/// `gpkg_tiles_matrix_at` without going back to the file.
///
/// The returned handle borrows `gpkg`, which cannot be closed until the handle
/// is released with `gpkg_tiles_free`.
///
/// # Safety
///
/// `gpkg` must be a live container handle, `name` a NUL-terminated UTF-8
/// string, and `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_tiles_open(
    gpkg: *const Container,
    name: *const c_char,
    error: *mut gpkg_error_t,
) -> *mut gpkg_tiles_t {
    if gpkg.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "gpkg handle is NULL") };
        return std::ptr::null_mut();
    }
    // SAFETY: forwarded to the caller's guarantee on `name`.
    let Some(name) = (unsafe { borrow_str(name, error, "pyramid name") }) else {
        return std::ptr::null_mut();
    };
    // SAFETY: the caller guarantees a live container handle.
    match unsafe { (*gpkg).tiles(name) } {
        Ok(handle) => Box::into_raw(Box::new(handle)),
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            std::ptr::null_mut()
        }
    }
}

/// Releases a tile pyramid handle. Passing NULL does nothing.
///
/// # Safety
///
/// `tiles` must be NULL, or a handle from `gpkg_tiles_open` that has not
/// already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_tiles_free(tiles: *mut gpkg_tiles_t) {
    if tiles.is_null() {
        return;
    }
    // SAFETY: the caller guarantees the pointer came from `Box::into_raw` in
    // `gpkg_tiles_open` and has not been freed. Dropping it decrements the
    // container's child count, which is what later permits a close.
    drop(unsafe { Box::from_raw(tiles) });
}

/// Returns the pyramid's table name. Owned by the caller; release with
/// `gpkg_string_free`.
///
/// # Safety
///
/// `tiles` must be a live handle; `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_tiles_name(
    tiles: *const gpkg_tiles_t,
    error: *mut gpkg_error_t,
) -> *mut c_char {
    // SAFETY: forwarded to this function's contract.
    let Some(handle) = (unsafe { checked(tiles, error, "gpkg_tiles_name") }) else {
        return std::ptr::null_mut();
    };
    let name = handle.pyramid().table_name().to_owned();
    // SAFETY: forwarded to the caller's guarantee on `error`.
    unsafe { out_string(&name, error) }
}

/// Returns the number of tiles in the pyramid, across every zoom level.
///
/// # Safety
///
/// `tiles` must be a live handle, `out` writable, `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_tiles_count(
    tiles: *const gpkg_tiles_t,
    out: *mut i64,
    error: *mut gpkg_error_t,
) -> Status {
    if out.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "out is NULL") };
        return Status::BadArgument;
    }
    // SAFETY: forwarded to this function's contract.
    let Some(handle) = (unsafe { checked(tiles, error, "gpkg_tiles_count") }) else {
        return Status::BadArgument;
    };
    match handle.pyramid().tile_count() {
        Ok(count) => {
            // SAFETY: the caller guarantees `out` is writable.
            unsafe { *out = count };
            Status::Ok
        }
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            Status::from(&err)
        }
    }
}

/// Returns the number of tiles at one zoom level.
///
/// `zoom_level` is the number a level is declared with, which
/// `gpkg_tiles_matrix_at` reports, not its position in the ladder. A level the
/// pyramid does not declare contains no tiles, so the answer is 0 rather than
/// an error.
///
/// # Safety
///
/// As [`gpkg_tiles_count`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_tiles_count_at(
    tiles: *const gpkg_tiles_t,
    zoom_level: i64,
    out: *mut i64,
    error: *mut gpkg_error_t,
) -> Status {
    if out.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "out is NULL") };
        return Status::BadArgument;
    }
    // SAFETY: forwarded to this function's contract.
    let Some(handle) = (unsafe { checked(tiles, error, "gpkg_tiles_count_at") }) else {
        return Status::BadArgument;
    };
    match handle.pyramid().tile_count_at(zoom_level) {
        Ok(count) => {
            // SAFETY: the caller guarantees `out` is writable.
            unsafe { *out = count };
            Status::Ok
        }
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            Status::from(&err)
        }
    }
}

/// Returns the number of zoom levels the pyramid declares.
///
/// The number of `gpkg_tile_matrix` rows, which bounds the `index` that
/// `gpkg_tiles_matrix_at` takes. A declared level may hold no tiles.
///
/// # Safety
///
/// `tiles` must be a live handle, `out` writable, `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_tiles_zoom_level_count(
    tiles: *const gpkg_tiles_t,
    out: *mut usize,
    error: *mut gpkg_error_t,
) -> Status {
    if out.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "out is NULL") };
        return Status::BadArgument;
    }
    // SAFETY: forwarded to this function's contract.
    let Some(handle) = (unsafe { checked(tiles, error, "gpkg_tiles_zoom_level_count") }) else {
        return Status::BadArgument;
    };
    let count = handle.pyramid().matrices().len();
    // SAFETY: the caller guarantees `out` is writable.
    unsafe { *out = count };
    Status::Ok
}

/// Returns one zoom level's matrix, by position in the declared ladder rather than by
/// zoom number, so a caller can walk them without guessing which exist.
///
/// `index` runs from zero to `gpkg_tiles_zoom_level_count`, in ascending zoom
/// order, and `zoom_level` is the level's actual number, which is
/// what `gpkg_tiles_count_at`, `gpkg_tiles_get` and `gpkg_tiles_put` address
/// tiles by. `matrix_width` and `matrix_height` are the grid in tiles;
/// `tile_width` and `tile_height` are one tile in pixels.
///
/// Any out-parameter may be NULL, in which case that field is not written.
/// An index beyond the last level is `GPKG_STATUS_NOT_FOUND`.
///
/// # Safety
///
/// `tiles` must be a live handle; every non-NULL out-parameter must be
/// writable; `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_tiles_matrix_at(
    tiles: *const gpkg_tiles_t,
    index: usize,
    zoom_level: *mut i64,
    matrix_width: *mut i64,
    matrix_height: *mut i64,
    tile_width: *mut i64,
    tile_height: *mut i64,
    error: *mut gpkg_error_t,
) -> Status {
    // SAFETY: forwarded to this function's contract.
    let Some(handle) = (unsafe { checked(tiles, error, "gpkg_tiles_matrix_at") }) else {
        return Status::BadArgument;
    };
    let Some(matrix) = handle.pyramid().matrices().get(index) else {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::NotFound, "zoom level index out of range") };
        return Status::NotFound;
    };

    // Each written only when the caller asked for it.
    for (slot, value) in [
        (zoom_level, matrix.zoom_level),
        (matrix_width, matrix.matrix_width),
        (matrix_height, matrix.matrix_height),
        (tile_width, matrix.tile_width),
        (tile_height, matrix.tile_height),
    ] {
        if !slot.is_null() {
            // SAFETY: the caller guarantees every non-NULL out-parameter is
            // writable.
            unsafe { *slot = value };
        }
    }
    Status::Ok
}

/// Returns the pyramid's extent and spatial reference system.
///
/// The `gpkg_tile_matrix_set` bounds: the ground area the grids are indexed
/// against, which is what makes a tile address mean a place. It is not a
/// statement about which tiles are present, since a pyramid may be sparse.
///
/// Any out-parameter may be NULL.
///
/// # Safety
///
/// As [`gpkg_tiles_matrix_at`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_tiles_extent(
    tiles: *const gpkg_tiles_t,
    min_x: *mut f64,
    min_y: *mut f64,
    max_x: *mut f64,
    max_y: *mut f64,
    srs_id: *mut i32,
    error: *mut gpkg_error_t,
) -> Status {
    // SAFETY: forwarded to this function's contract.
    let Some(handle) = (unsafe { checked(tiles, error, "gpkg_tiles_extent") }) else {
        return Status::BadArgument;
    };
    let set = handle.pyramid().matrix_set();
    for (slot, value) in [
        (min_x, set.min_x),
        (min_y, set.min_y),
        (max_x, set.max_x),
        (max_y, set.max_y),
    ] {
        if !slot.is_null() {
            // SAFETY: the caller guarantees every non-NULL out-parameter is
            // writable.
            unsafe { *slot = value };
        }
    }
    if !srs_id.is_null() {
        // SAFETY: as above.
        unsafe { *srs_id = set.srs_id };
    }
    Status::Ok
}

/// Returns whether a tile is stored at an address.
///
/// Cheaper than fetching one to find out, since no payload is read. An address
/// no zoom level or grid contains is `false` rather than an error: it contains
/// no tile either.
///
/// # Safety
///
/// `tiles` must be a live handle, `out` writable, `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_tiles_has(
    tiles: *const gpkg_tiles_t,
    zoom_level: i64,
    column: i64,
    row: i64,
    out: *mut bool,
    error: *mut gpkg_error_t,
) -> Status {
    if out.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "out is NULL") };
        return Status::BadArgument;
    }
    // SAFETY: forwarded to this function's contract.
    let Some(handle) = (unsafe { checked(tiles, error, "gpkg_tiles_has") }) else {
        return Status::BadArgument;
    };
    match handle
        .pyramid()
        .has_tile(TileCoord::new(zoom_level, column, row))
    {
        Ok(has) => {
            // SAFETY: the caller guarantees `out` is writable.
            unsafe { *out = has };
            Status::Ok
        }
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            Status::from(&err)
        }
    }
}

/// Returns the bytes stored at a tile address.
///
/// On success `out_data` receives an allocation the caller owns and must
/// release with `gpkg_bytes_free`, and `out_len` its length. A tile that is not
/// there is `GPKG_STATUS_NOT_FOUND` with `out_data` left NULL and `out_len`
/// zero, which is an ordinary answer for a sparse pyramid rather than a failure
/// to report, and no message is set for it.
///
/// ```c
/// uint8_t *data = NULL;
/// size_t len = 0;
/// gpkg_status status = gpkg_tiles_get(tiles, 1, 0, 0, &data, &len, &error);
/// if (status == GPKG_STATUS_OK) {
///     fwrite(data, 1, len, out);
///     gpkg_bytes_free(data, len);   // exactly the length this call wrote
/// } else if (status != GPKG_STATUS_NOT_FOUND) {
///     return fail("gpkg_tiles_get", &error);
/// }
/// ```
///
/// `gpkg_tiles_get_into` is the alternative, and the better one for a caller
/// reading many tiles: it copies into the caller's own buffer, so no allocation
/// crosses the boundary and there is no length to keep hold of.
///
/// The bytes are what the file stores. Nothing is decoded.
///
/// # Safety
///
/// `tiles` must be a live handle, `out_data` and `out_len` writable, and
/// `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_tiles_get(
    tiles: *const gpkg_tiles_t,
    zoom_level: i64,
    column: i64,
    row: i64,
    out_data: *mut *mut u8,
    out_len: *mut usize,
    error: *mut gpkg_error_t,
) -> Status {
    if out_data.is_null() || out_len.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "out_data or out_len is NULL") };
        return Status::BadArgument;
    }
    // SAFETY: forwarded to this function's contract.
    let Some(handle) = (unsafe { checked(tiles, error, "gpkg_tiles_get") }) else {
        return Status::BadArgument;
    };

    let found = match handle
        .pyramid()
        .get_tile(TileCoord::new(zoom_level, column, row))
    {
        Ok(found) => found,
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            return Status::from(&err);
        }
    };
    let Some(bytes) = found else {
        // SAFETY: the caller guarantees `out_data` is writable.
        unsafe { *out_data = std::ptr::null_mut() };
        // SAFETY: the caller guarantees `out_len` is writable.
        unsafe { *out_len = 0 };
        return Status::NotFound;
    };

    // Boxed rather than handed over as a `Vec`, because a boxed slice has
    // capacity equal to its length, so the length is all `gpkg_bytes_free`
    // needs to reconstruct it.
    let boxed: Box<[u8]> = bytes.into_boxed_slice();
    let len = boxed.len();
    let raw = Box::into_raw(boxed).cast::<u8>();
    // SAFETY: the caller guarantees `out_data` is writable.
    unsafe { *out_data = raw };
    // SAFETY: the caller guarantees `out_len` is writable.
    unsafe { *out_len = len };
    Status::Ok
}

/// Stores bytes at a tile address.
///
/// The address is checked against the zoom level's grid and the payload's
/// header against the tile size the level declares. Reading a header is not
/// decoding an image: what it catches is a tile of the wrong pixel size, or in
/// a format the table may not hold.
///
/// ```c
/// if (gpkg_tiles_put(tiles, 1, 0, 0, png, png_len, &error) != GPKG_STATUS_OK) {
///     return fail("gpkg_tiles_put", &error);
/// }
/// ```
///
/// The bytes are copied into the file during the call, so the caller's buffer
/// is its own again on return. Writing where a tile is already stored replaces
/// it.
///
/// A zoom level the pyramid does not declare is `GPKG_STATUS_NOT_FOUND`, and a
/// format the table may not hold is `GPKG_STATUS_UNSUPPORTED`. A column or row
/// outside the level's grid, and a payload whose header disagrees with the
/// declared tile size, are `GPKG_STATUS_OTHER`, with the message saying which:
/// these come from the tile checks, which have no category of their own.
///
/// # Safety
///
/// `tiles` must be a live handle, `data` must point at `len` readable bytes,
/// and `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_tiles_put(
    tiles: *const gpkg_tiles_t,
    zoom_level: i64,
    column: i64,
    row: i64,
    data: *const u8,
    len: usize,
    error: *mut gpkg_error_t,
) -> Status {
    if data.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "data is NULL") };
        return Status::BadArgument;
    }
    // SAFETY: forwarded to this function's contract.
    let Some(handle) = (unsafe { checked(tiles, error, "gpkg_tiles_put") }) else {
        return Status::BadArgument;
    };
    // SAFETY: the caller guarantees `data` points at `len` readable bytes,
    // which stay valid and unmodified for the duration of this call.
    let payload = unsafe { std::slice::from_raw_parts(data, len) };
    let result = handle
        .pyramid()
        .put_tile(TileCoord::new(zoom_level, column, row), payload);
    // SAFETY: forwarded to the caller's guarantee on `error`.
    unsafe { report(result, error) }
}

/// Deletes the tile at an address, reporting through `out_deleted` whether one
/// was there. `out_deleted` may be NULL.
///
/// Deleting where there is no tile is success with `out_deleted` false, not an
/// error: the address is empty either way. The zoom level's `gpkg_tile_matrix`
/// row is left alone, so the level keeps its declared grid.
///
/// # Safety
///
/// `tiles` must be a live handle; `out_deleted` NULL or writable; `error` NULL
/// or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_tiles_delete(
    tiles: *const gpkg_tiles_t,
    zoom_level: i64,
    column: i64,
    row: i64,
    out_deleted: *mut bool,
    error: *mut gpkg_error_t,
) -> Status {
    // SAFETY: forwarded to this function's contract.
    let Some(handle) = (unsafe { checked(tiles, error, "gpkg_tiles_delete") }) else {
        return Status::BadArgument;
    };
    match handle
        .pyramid()
        .delete_tile(TileCoord::new(zoom_level, column, row))
    {
        Ok(deleted) => {
            if !out_deleted.is_null() {
                // SAFETY: the caller guarantees a writable out-parameter when
                // it is not NULL.
                unsafe { *out_deleted = deleted };
            }
            Status::Ok
        }
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            Status::from(&err)
        }
    }
}

/// Releases a byte buffer this library returned.
///
/// `gpkg_tiles_get` is the only call that returns one, and its `out_len` is
/// the `len` to pass here. Passing a NULL pointer does nothing.
///
/// Passing the wrong `len` is undefined behaviour, and not one a sanitizer can
/// be relied on to catch. A caller who would rather not track the length
/// should use [`gpkg_tiles_get_into`], where no allocation crosses the
/// boundary and there is nothing to get wrong.
///
/// # Safety
///
/// `data` must be NULL, or a buffer from `gpkg_tiles_get` that has not already
/// been freed, and `len` must be the length that call wrote.
// The sanitizer claim above was checked rather than assumed: deliberately
// freeing at half the length passes ASan on macOS, because the system
// allocator ignores the size it is given, and Miri cannot reach this code at
// all, since SQLite is built from source. So the contract rests entirely on
// the caller keeping the length it was handed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_bytes_free(data: *mut u8, len: usize) {
    if data.is_null() {
        return;
    }
    // SAFETY: the caller guarantees `data` came from `Box::into_raw` on a boxed
    // slice of exactly `len` bytes in `gpkg_tiles_get`, and has not been freed.
    // A boxed slice's capacity equals its length, so reconstructing it from the
    // pointer and that length frees exactly what was allocated.
    drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(data, len)) });
}

/// Borrows a pyramid handle, reporting a NULL one.
///
/// # Safety
///
/// `tiles` must be NULL or a live handle; `error` NULL or writable.
unsafe fn checked<'a>(
    tiles: *const gpkg_tiles_t,
    error: *mut gpkg_error_t,
    what: &str,
) -> Option<&'a TilesHandle> {
    if tiles.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe {
            set_error(
                error,
                Status::BadArgument,
                &format!("{what}: tiles handle is NULL"),
            );
        }
        return None;
    }
    // SAFETY: the caller guarantees a live pyramid handle.
    Some(unsafe { &*tiles })
}

/// Turns a unit-returning library call into a status, filling in `error`.
///
/// # Safety
///
/// `error` must be NULL or point at a writable `gpkg_error_t`.
unsafe fn report(result: geopackage::Result<()>, error: *mut gpkg_error_t) -> Status {
    match result {
        Ok(()) => Status::Ok,
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            Status::from(&err)
        }
    }
}

/// Reads the bytes at a tile address into a buffer the caller owns.
///
/// The safer of the two readers, and the faster one for a caller reading many
/// tiles: no allocation crosses the boundary, so there is no ownership to get
/// wrong and no per-tile allocation. Backed by the library's own
/// `get_tile_into`, which reuses the buffer it is given.
///
/// `out_len` always receives the tile's length when one is there, whether or
/// not it fitted. When `capacity` is too small the return is
/// `GPKG_STATUS_INVALID_ARGUMENT`, nothing is written, and `out_len` says how
/// much room the tile needs, so the usual shape is to call once with whatever
/// buffer is to hand and again if it was short.
///
/// ```c
/// unsigned char buffer[64 * 1024];
/// size_t len = 0;
/// gpkg_status status =
///     gpkg_tiles_get_into(tiles, 1, 0, 0, buffer, sizeof(buffer), &len, &error);
///
/// if (status == GPKG_STATUS_INVALID_ARGUMENT) {
///     gpkg_error_clear(&error);
///     unsigned char *bigger = malloc(len);
///     status = gpkg_tiles_get_into(tiles, 1, 0, 0, bigger, len, &len, &error);
///     // ... use bigger ...
///     free(bigger);
/// }
/// ```
///
/// Passing NULL with a `capacity` of zero is allowed, and is how to request
/// the length on its own: nothing is copied, `out_len` reports what the tile
/// needs, and the return is the `GPKG_STATUS_INVALID_ARGUMENT` that any buffer
/// too small gets.
///
/// A tile that is not there is `GPKG_STATUS_NOT_FOUND` with `out_len` zero, and
/// no message.
///
/// # Safety
///
/// `tiles` must be a live handle; `buffer` must point at `capacity` writable
/// bytes, or be NULL when `capacity` is zero; `out_len` must be writable; and
/// `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_tiles_get_into(
    tiles: *const gpkg_tiles_t,
    zoom_level: i64,
    column: i64,
    row: i64,
    buffer: *mut u8,
    capacity: usize,
    out_len: *mut usize,
    error: *mut gpkg_error_t,
) -> Status {
    if out_len.is_null() || (buffer.is_null() && capacity != 0) {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "out_len or buffer is NULL") };
        return Status::BadArgument;
    }
    // SAFETY: forwarded to this function's contract.
    let Some(handle) = (unsafe { checked(tiles, error, "gpkg_tiles_get_into") }) else {
        return Status::BadArgument;
    };

    // The library fills a `Vec` it reuses; this one is per-call, which still
    // spares the caller any ownership question. A caller wanting to reuse
    // storage across tiles reuses its own `buffer`.
    let mut scratch = Vec::new();
    let found = match handle
        .pyramid()
        .get_tile_into(TileCoord::new(zoom_level, column, row), &mut scratch)
    {
        Ok(found) => found,
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            return Status::from(&err);
        }
    };
    if !found {
        // SAFETY: the caller guarantees `out_len` is writable.
        unsafe { *out_len = 0 };
        return Status::NotFound;
    }

    let len = scratch.len();
    // SAFETY: the caller guarantees `out_len` is writable. Written before the
    // capacity check, so a short call still learns the size it needs.
    unsafe { *out_len = len };
    if len > capacity {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe {
            set_error(
                error,
                Status::InvalidArgument,
                "buffer too small; out_len contains the length required",
            );
        }
        return Status::InvalidArgument;
    }

    // SAFETY: `buffer` points at `capacity` writable bytes by the caller's
    // guarantee, `len <= capacity` was just checked, and `scratch` is a
    // distinct allocation, so the two cannot overlap.
    unsafe { std::ptr::copy_nonoverlapping(scratch.as_ptr(), buffer, len) };
    Status::Ok
}

/// One scan over a pyramid's stored tiles. Opaque; created by
/// `gpkg_tiles_cursor`, `gpkg_tiles_cursor_at` or `gpkg_tiles_cursor_in`,
/// and destroyed by `gpkg_tile_cursor_free`.
#[expect(
    non_camel_case_types,
    reason = "the C name is the type's name; cbindgen emits it verbatim"
)]
pub type gpkg_tile_cursor_t = TileCursorHandle;

/// Walks every stored tile, in matrix order.
///
/// The cursor visits what the pyramid stores rather than probing the declared
/// grid with `gpkg_tiles_has`, which on a sparse pyramid is the difference
/// between O(stored) and O(grid). One pass per cursor: at the end of the scan
/// open a new one.
///
/// The cursor counts against the *container*, like an Arrow stream: the
/// container cannot close while the cursor is alive, and the tiles handle
/// it came from may be freed first.
///
/// ```c
/// gpkg_tile_cursor_t *cursor = gpkg_tiles_cursor(tiles, &error);
/// if (!cursor) {
///     return fail("gpkg_tiles_cursor", &error);
/// }
/// for (;;) {
///     int64_t zoom = 0, column = 0, row = 0;
///     const uint8_t *data = NULL;
///     size_t len = 0;
///     if (gpkg_tile_cursor_next(cursor, &zoom, &column, &row, &data, &len,
///                               &error) != GPKG_STATUS_OK) {
///         gpkg_tile_cursor_free(cursor);
///         return fail("gpkg_tile_cursor_next", &error);
///     }
///     if (data == NULL) {
///         break;   // the scan is done
///     }
///     // `data` is borrowed: valid until the next call on the cursor.
///     fwrite(data, 1, len, out);
/// }
/// gpkg_tile_cursor_free(cursor);
/// ```
///
/// Returns NULL on failure.
///
/// # Safety
///
/// `tiles` must be a live pyramid handle and `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_tiles_cursor(
    tiles: *const gpkg_tiles_t,
    error: *mut gpkg_error_t,
) -> *mut gpkg_tile_cursor_t {
    // SAFETY: forwarded to this function's contract.
    unsafe { open_cursor(tiles, &CursorScan::All, error) }
}

/// Walks the stored tiles of one zoom level, in matrix order.
///
/// As `gpkg_tiles_cursor`, narrowed to `zoom`. A zoom level the pyramid does
/// not declare scans nothing, since nothing is stored at it; only
/// `gpkg_tiles_cursor_in` rejects an unknown level, because it needs the
/// level's grid to turn the box into tile indices.
///
/// # Safety
///
/// As [`gpkg_tiles_cursor`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_tiles_cursor_at(
    tiles: *const gpkg_tiles_t,
    zoom: i64,
    error: *mut gpkg_error_t,
) -> *mut gpkg_tile_cursor_t {
    // SAFETY: forwarded to this function's contract.
    unsafe { open_cursor(tiles, &CursorScan::At(zoom), error) }
}

/// Walks the stored tiles of one zoom level that a bounding box touches.
///
/// The box is in the pyramid's own spatial reference system; nothing is
/// transformed. A box outside the pyramid's extent scans nothing, which is
/// the answer rather than an error.
///
/// # Safety
///
/// As [`gpkg_tiles_cursor`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_tiles_cursor_in(
    tiles: *const gpkg_tiles_t,
    zoom: i64,
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    error: *mut gpkg_error_t,
) -> *mut gpkg_tile_cursor_t {
    let bbox = BoundingBox::new(min_x, min_y, max_x, max_y);
    // SAFETY: forwarded to this function's contract.
    unsafe { open_cursor(tiles, &CursorScan::In(zoom, bbox), error) }
}

/// The body of the three cursor constructors.
///
/// # Safety
///
/// As [`gpkg_tiles_cursor`].
unsafe fn open_cursor(
    tiles: *const gpkg_tiles_t,
    scan: &CursorScan,
    error: *mut gpkg_error_t,
) -> *mut gpkg_tile_cursor_t {
    if tiles.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "tiles handle is NULL") };
        return std::ptr::null_mut();
    }
    // SAFETY: the caller guarantees a live pyramid handle.
    match unsafe { (*tiles).cursor(scan) } {
        Ok(handle) => Box::into_raw(Box::new(handle)),
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            std::ptr::null_mut()
        }
    }
}

/// Returns the next stored tile, or the end of the scan.
///
/// On `GPKG_STATUS_OK`, either `out_data` points at the tile's payload with
/// its length in `out_len` and the address in the coordinate out-parameters,
/// or the scan is done, which is `out_data` NULL with `out_len` zero and the
/// coordinates untouched.
///
/// **The payload is borrowed, not owned**: it is valid until the next call on
/// this cursor, or its free, whichever comes first. Copy it to keep it. This
/// is what makes the cursor cheaper than `gpkg_tiles_get` for a caller
/// walking many tiles: nothing is allocated or copied per tile on this side
/// of the boundary.
///
/// The coordinate out-parameters may each be NULL to skip them; `out_data`
/// and `out_len` may not, since a scan whose payloads are discarded unread
/// has cheaper ways to count.
///
/// # Safety
///
/// `cursor` must be a live cursor handle, `out_data` and `out_len` writable,
/// the coordinate out-parameters NULL or writable, and `error` NULL or
/// writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_tile_cursor_next(
    cursor: *mut gpkg_tile_cursor_t,
    out_zoom: *mut i64,
    out_column: *mut i64,
    out_row: *mut i64,
    out_data: *mut *const u8,
    out_len: *mut usize,
    error: *mut gpkg_error_t,
) -> Status {
    if cursor.is_null() || out_data.is_null() || out_len.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe {
            set_error(
                error,
                Status::BadArgument,
                "cursor handle, out_data or out_len is NULL",
            );
        }
        return Status::BadArgument;
    }
    // SAFETY: the caller guarantees a live cursor handle, used from one thread
    // as the crate's threading rule requires.
    let handle = unsafe { &mut *cursor };
    match handle.next() {
        Ok(Some(tile)) => {
            let coord = tile.coord();
            let data = tile.data();
            if !out_zoom.is_null() {
                // SAFETY: the caller guarantees non-NULL out-parameters are
                // writable.
                unsafe { *out_zoom = coord.zoom_level };
            }
            if !out_column.is_null() {
                // SAFETY: as above.
                unsafe { *out_column = coord.column };
            }
            if !out_row.is_null() {
                // SAFETY: as above.
                unsafe { *out_row = coord.row };
            }
            // SAFETY: the caller guarantees `out_data` and `out_len` are
            // writable. The pointer stays valid until the statement steps
            // again, which is the documented contract.
            unsafe { *out_data = data.as_ptr() };
            // SAFETY: as above.
            unsafe { *out_len = data.len() };
            Status::Ok
        }
        Ok(None) => {
            // SAFETY: as above.
            unsafe { *out_data = std::ptr::null() };
            // SAFETY: as above.
            unsafe { *out_len = 0 };
            Status::Ok
        }
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            Status::from(&err)
        }
    }
}

/// Releases a tile cursor. Passing NULL does nothing.
///
/// # Safety
///
/// `cursor` must be NULL, or a handle from one of the cursor constructors
/// that has not already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_tile_cursor_free(cursor: *mut gpkg_tile_cursor_t) {
    if cursor.is_null() {
        return;
    }
    // SAFETY: the caller guarantees the pointer came from `Box::into_raw` in
    // `open_cursor` and has not been freed. Dropping it decrements the
    // container's child count, which is what later permits a close.
    drop(unsafe { Box::from_raw(cursor) });
}

/// Creates a tile pyramid, returning its handle.
///
/// The pyramid is declared over the extent `min_x, min_y, max_x, max_y` in
/// the spatial reference system `srs_id`, which must already exist in the
/// file; `gpkg_add_epsg_srs` puts one there. Zoom levels run from `min_zoom`
/// to `max_zoom` inclusive, each halving the tile span of the one before, on
/// a grid of `base_columns` by `base_rows` tiles at `min_zoom` with tiles of
/// `tile_width` by `tile_height` pixels. Zero for any of those four takes
/// the default: a 1 by 1 base grid of 256-pixel tiles.
///
/// The returned handle is `gpkg_tiles_open`'s, ready to fill with
/// `gpkg_tiles_put`, and released with `gpkg_tiles_free`. Creating the
/// pyramid registers its `gpkg_contents` and `gpkg_tile_matrix` rows; it
/// stores no tiles.
///
/// For the web mercator tiling every XYZ basemap uses, prefer
/// `gpkg_tiles_create_web_mercator`, which fixes the extent and grid so the
/// two cannot disagree.
///
/// Fails with `GPKG_STATUS_ALREADY_EXISTS` when the file already has a table
/// of that name, and `GPKG_STATUS_NOT_FOUND` when the SRS is not in the
/// file. Returns NULL on failure.
///
/// # Safety
///
/// `gpkg` must be a live container handle, `name` a NUL-terminated UTF-8
/// string, and `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_tiles_create(
    gpkg: *const Container,
    name: *const c_char,
    srs_id: i32,
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    min_zoom: i64,
    max_zoom: i64,
    base_columns: i64,
    base_rows: i64,
    tile_width: i64,
    tile_height: i64,
    error: *mut gpkg_error_t,
) -> *mut gpkg_tiles_t {
    let set = TileMatrixSet::new(srs_id, min_x, min_y, max_x, max_y);
    // SAFETY: forwarded to this function's contract.
    unsafe {
        create_pyramid(
            gpkg,
            name,
            set,
            min_zoom,
            max_zoom,
            (base_columns, base_rows),
            (tile_width, tile_height),
            error,
        )
    }
}

/// Creates a tile pyramid on the web mercator quad, returning its handle.
///
/// The tiling every XYZ basemap uses: `EPSG:3857`, the full quad extent, one
/// 256-pixel tile at zoom 0, doubling per level. `gpkg_add_epsg_srs(gpkg,
/// 3857, ...)` must have run first, since a pyramid cannot name an SRS the
/// file does not register. Everything else is as `gpkg_tiles_create`.
///
/// # Safety
///
/// As [`gpkg_tiles_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_tiles_create_web_mercator(
    gpkg: *const Container,
    name: *const c_char,
    min_zoom: i64,
    max_zoom: i64,
    error: *mut gpkg_error_t,
) -> *mut gpkg_tiles_t {
    let set = TileMatrixSet::web_mercator_quad();
    // SAFETY: forwarded to this function's contract.
    unsafe { create_pyramid(gpkg, name, set, min_zoom, max_zoom, (0, 0), (0, 0), error) }
}

/// The body of both creators.
///
/// # Safety
///
/// As [`gpkg_tiles_create`].
#[expect(
    clippy::too_many_arguments,
    reason = "a private body shared by two C entry points whose parameters are the C signature"
)]
unsafe fn create_pyramid(
    gpkg: *const Container,
    name: *const c_char,
    set: TileMatrixSet,
    min_zoom: i64,
    max_zoom: i64,
    base_grid: (i64, i64),
    tile_size: (i64, i64),
    error: *mut gpkg_error_t,
) -> *mut gpkg_tiles_t {
    if gpkg.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "gpkg handle is NULL") };
        return std::ptr::null_mut();
    }
    // SAFETY: forwarded to the caller's guarantee on `name`.
    let Some(name) = (unsafe { borrow_str(name, error, "pyramid name") }) else {
        return std::ptr::null_mut();
    };
    let mut ladder = ZoomLadder::new(min_zoom, max_zoom);
    if base_grid != (0, 0) {
        ladder = ladder.base_grid(base_grid.0, base_grid.1);
    }
    if tile_size != (0, 0) {
        ladder = ladder.tile_size(tile_size.0, tile_size.1);
    }
    let matrices = match set.ladder(ladder) {
        Ok(matrices) => matrices,
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_error(error, Status::InvalidArgument, &err.to_string()) };
            return std::ptr::null_mut();
        }
    };
    let builder = TilePyramidBuilder::new(name, set).matrices(matrices);
    // SAFETY: the caller guarantees a live container handle.
    match unsafe { (*gpkg).create_tiles(&builder) } {
        Ok(handle) => Box::into_raw(Box::new(handle)),
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            std::ptr::null_mut()
        }
    }
}
