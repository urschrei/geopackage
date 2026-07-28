//! `gpkg_tiles_t`: a handle onto one tile pyramid.
//!
//! **No image is decoded anywhere here.** A tile goes out as the bytes the file
//! holds and comes in as the bytes the caller supplies. Writing checks the
//! address against the zoom level's grid and the payload's header against the
//! declared tile size, which is a header read rather than a decode; turning
//! tiles into pixels needs an image library on top of this one.
//!
//! `gpkg_tiles_get` is the call the handle design exists for. It measures about
//! 5 microseconds, so rebuilding a borrowed pyramid handle on every call, which
//! costs 40, would have made it eight times slower from C than from Rust. See
//! `roadmap/benchmarks/2026-07-28-handle-construction.md`.

use std::ffi::c_char;

use geopackage::core::tiles::TileCoord;

use crate::error::{Status, gpkg_error_t, set_error, set_library_error};
use crate::handle::{Container, TilesHandle};
use crate::util::{borrow_str, out_string};

/// A tile pyramid of an open GeoPackage. Opaque; created by `gpkg_tiles_open`
/// and destroyed by `gpkg_tiles_free`.
#[expect(
    non_camel_case_types,
    reason = "the C name is the type's name; cbindgen emits it verbatim"
)]
pub type gpkg_tiles_t = TilesHandle;

/// Open a tile pyramid by name.
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

/// Release a tile pyramid handle. Passing NULL does nothing.
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

/// The pyramid's table name. Owned by the caller; release with
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

/// How many tiles the pyramid holds, across every zoom level.
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

/// How many tiles the pyramid holds at one zoom level.
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

/// How many zoom levels the pyramid declares.
///
/// # Safety
///
/// `tiles` must be a live handle, `out` writable.
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

/// One zoom level's matrix, by position in the declared ladder rather than by
/// zoom number, so a caller can walk them without guessing which exist.
///
/// Any out-parameter may be NULL, in which case that field is not written.
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

/// The pyramid's extent and spatial reference system.
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

/// Whether a tile is stored at an address.
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

/// The bytes stored at a tile address.
///
/// On success `out_data` receives an allocation the caller owns and must
/// release with `gpkg_bytes_free`, and `out_len` its length. A tile that is not
/// there is `GPKG_STATUS_NOT_FOUND` with `out_data` left NULL, which is an
/// ordinary answer for a sparse pyramid rather than a failure to report.
///
/// The bytes are what the file holds. Nothing is decoded.
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

/// Store bytes at a tile address.
///
/// The address is checked against the zoom level's grid and the payload's
/// header against the tile size the level declares. Reading a header is not
/// decoding an image: what it catches is a tile of the wrong pixel size, or in
/// a format the table may not hold.
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

/// Delete the tile at an address, reporting through `out_deleted` whether one
/// was there. `out_deleted` may be NULL.
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

/// Release a byte buffer this library returned.
///
/// `len` must be the length the same call reported. Passing a NULL pointer does
/// nothing.
///
/// # A limit on what the sanitizer proves here
///
/// Passing the wrong `len` is undefined behaviour, and **AddressSanitizer does
/// not reliably catch it**: checked by deliberately freeing at half the length,
/// which ASan on macOS reports nothing for, because the system allocator
/// ignores the size it is given. Miri would catch it and cannot reach this code
/// at all, since SQLite is built from source. So the contract here rests on the
/// caller keeping the length it was handed, and callers who would rather not
/// carry that should use [`gpkg_tiles_get_into`], where no allocation crosses
/// the boundary and there is nothing to get wrong.
///
/// # Safety
///
/// `data` must be NULL, or a buffer from `gpkg_tiles_get` that has not already
/// been freed, and `len` must be the length that call wrote.
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

/// Borrow a pyramid handle, reporting a NULL one.
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

/// Turn a unit-returning library call into a status, filling in `error`.
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

/// The bytes at a tile address, into a buffer the caller owns.
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
/// A tile that is not there is `GPKG_STATUS_NOT_FOUND` with `out_len` zero.
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
                "buffer too small; out_len holds the length required",
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
