//! `gpkg_writer_t`: changing features a row at a time.
//!
//! [`crate::stream`] moves whole layers as Arrow and only ever appends. This is
//! the other half: insert one row, change one, remove one. A consumer editing
//! an existing file wants this; a consumer loading a million rows wants Arrow.
//!
//! A writer holds a transaction for its lifetime. Rows staged through it are
//! not durable until [`gpkg_writer_commit`], and [`gpkg_writer_free`] discards
//! them. Inside a transaction the caller opened with `gpkg_begin` the writer
//! joins that one instead, so its commit stages rather than commits and the
//! caller's `gpkg_commit` is what makes the work durable.
//!
//! ```c
//! gpkg_writer_t *writer = gpkg_layer_writer(layer, &error);
//! if (!writer) {
//!     return fail("gpkg_layer_writer", &error);
//! }
//!
//! // A point at 6.26W 53.35N, as little-endian WKB.
//! unsigned char wkb[] = {
//!     0x01, 0x01, 0x00, 0x00, 0x00,
//!     0x8d, 0x97, 0x6e, 0x12, 0x83, 0x08, 0x19, 0xc0,
//!     0x66, 0x66, 0x66, 0x66, 0x66, 0xac, 0x4a, 0x40,
//! };
//! gpkg_value_t values[] = {
//!     {GPKG_VALUE_KIND_TEXT, {.text = "Dublin"}},
//!     {GPKG_VALUE_KIND_INTEGER, {.integer = 592713}},
//! };
//!
//! int64_t fid = 0;
//! if (gpkg_writer_insert(writer, NULL, wkb, sizeof wkb, values, 2, &fid,
//!                        &error) != GPKG_STATUS_OK) {
//!     gpkg_writer_free(writer);
//!     return fail("gpkg_writer_insert", &error);
//! }
//!
//! // Correct one column of an existing row, and remove another row.
//! bool matched = false;
//! gpkg_value_t corrected = {GPKG_VALUE_KIND_INTEGER, {.integer = 592714}};
//! gpkg_writer_update_column(writer, fid, "population", &corrected, &matched,
//!                           &error);
//! gpkg_writer_delete(writer, 7, &matched, &error);
//!
//! // Commit consumes the writer, whether it succeeds or fails.
//! if (gpkg_writer_commit(writer, &error) != GPKG_STATUS_OK) {
//!     return fail("gpkg_writer_commit", &error);
//! }
//! ```
//!
//! # Feature ids and values
//!
//! A feature id is passed as a pointer so that "assign one" and "use this one"
//! are distinguishable: NULL assigns, and the assigned id comes back through
//! `out_fid`. No sentinel would do, since every `int64_t` is a legal id.
//!
//! `values` covers the layer's value columns in the order
//! [`crate::gpkg_layer_column_name`] reports them, excluding the primary key
//! and the geometry, which travel as their own arguments. A count that does not
//! match the layer is refused rather than padded.
//!
//! # Geometry
//!
//! Geometry crosses as WKB and is stored as it arrives, wrapped in the GPB
//! header the format requires. Nothing is decoded on the way, so a geometry
//! this library could not otherwise represent, a curve above all, survives a
//! write. `NULL` with a length of zero writes a row with no geometry.

use std::ffi::c_char;

use crate::error::{Status, gpkg_error_t, set_error, set_library_error};
use crate::handle::{LayerHandle, WriterHandle};
use crate::util::borrow_str;
use crate::value::{borrow_values, gpkg_value_t};

/// A write transaction over one layer. Opaque; created by
/// `gpkg_layer_writer`, and destroyed by `gpkg_writer_commit` or
/// `gpkg_writer_free`.
#[expect(
    non_camel_case_types,
    reason = "the C name is the type's name; cbindgen emits it verbatim"
)]
pub type gpkg_writer_t = WriterHandle;

/// Borrow a live writer, or report a NULL handle.
///
/// # Safety
///
/// `writer` must be NULL or a live writer handle, and `error` NULL or writable.
unsafe fn writer_mut<'a>(
    writer: *mut gpkg_writer_t,
    what: &str,
    error: *mut gpkg_error_t,
) -> Option<&'a mut WriterHandle> {
    if writer.is_null() {
        let message = format!("{what}: writer is NULL");
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, &message) };
        return None;
    }
    // SAFETY: the caller guarantees a live writer handle, which came from
    // `Box::into_raw` and has not been freed.
    Some(unsafe { &mut *writer })
}

/// Read a caller-supplied geometry: `NULL` with a length of zero means none.
///
/// # Safety
///
/// `wkb` must be NULL or readable for `wkb_len` bytes.
unsafe fn borrow_wkb<'a>(
    wkb: *const u8,
    wkb_len: usize,
    error: *mut gpkg_error_t,
) -> Option<Option<&'a [u8]>> {
    if wkb.is_null() {
        if wkb_len != 0 {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe {
                set_error(
                    error,
                    Status::BadArgument,
                    "geometry is NULL with a non-zero length",
                );
            }
            return None;
        }
        return Some(None);
    }
    // SAFETY: the caller guarantees `wkb` is readable for `wkb_len`.
    Some(Some(unsafe { std::slice::from_raw_parts(wkb, wkb_len) }))
}

/// Report a library error and return its status.
///
/// # Safety
///
/// `error` must be NULL or writable.
unsafe fn failed(err: &geopackage::Error, error: *mut gpkg_error_t) -> Status {
    // SAFETY: forwarded to the caller's guarantee on `error`.
    unsafe { set_library_error(error, err) };
    Status::from(err)
}

/// Begin a write transaction over `layer`.
///
/// The writer borrows the layer's container, which cannot be closed while the
/// writer is alive. Finish with `gpkg_writer_commit`, or discard the work with
/// `gpkg_writer_free`.
///
/// Returns NULL on failure. A read-only handle fails here rather than at the
/// first write.
///
/// # Safety
///
/// `layer` must be a live layer handle and `error` NULL or writable. The
/// returned writer must be destroyed exactly once, with `gpkg_writer_commit` or
/// `gpkg_writer_free`, on the thread that created it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_writer(
    layer: *const LayerHandle,
    error: *mut gpkg_error_t,
) -> *mut gpkg_writer_t {
    if layer.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe {
            set_error(
                error,
                Status::BadArgument,
                "gpkg_layer_writer: layer is NULL",
            )
        };
        return std::ptr::null_mut();
    }
    // SAFETY: the caller guarantees a live layer handle.
    match unsafe { (*layer).writer() } {
        Ok(writer) => Box::into_raw(Box::new(writer)),
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            std::ptr::null_mut()
        }
    }
}

/// Commit the writer's work and destroy it.
///
/// The handle is destroyed whether this succeeds or fails, so it must not be
/// used again either way. Inside a transaction the caller opened with
/// `gpkg_begin`, this stages the work rather than committing it, and their
/// `gpkg_commit` is what makes it durable.
///
/// # Safety
///
/// `writer` must be a live writer handle that has not already been committed or
/// freed. `error` must be NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_writer_commit(
    writer: *mut gpkg_writer_t,
    error: *mut gpkg_error_t,
) -> Status {
    if writer.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe {
            set_error(
                error,
                Status::BadArgument,
                "gpkg_writer_commit: writer is NULL",
            )
        };
        return Status::BadArgument;
    }
    // SAFETY: the caller guarantees a live handle from `gpkg_layer_writer`,
    // produced by `Box::into_raw` and not yet freed.
    let handle = unsafe { Box::from_raw(writer) };
    match handle.commit() {
        Ok(()) => Status::Ok,
        // SAFETY: forwarded to the caller's guarantee on `error`.
        Err(err) => unsafe { failed(&err, error) },
    }
}

/// Discard the writer's work and destroy it.
///
/// Everything staged since `gpkg_layer_writer` is rolled back. Inside a
/// transaction the caller opened, nothing is rolled back here: the work stays
/// staged in that transaction, and `gpkg_rollback` is what discards it.
///
/// Freeing NULL does nothing.
///
/// # Safety
///
/// `writer` must be NULL, or a live writer handle that has not already been
/// committed or freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_writer_free(writer: *mut gpkg_writer_t) {
    if writer.is_null() {
        return;
    }
    // SAFETY: the caller guarantees a live handle from `gpkg_layer_writer`,
    // produced by `Box::into_raw` and not yet freed. Dropping it rolls the
    // transaction back and releases the container count.
    drop(unsafe { Box::from_raw(writer) });
}

/// Insert a feature, with or without a geometry.
///
/// `fid` is NULL to have an id assigned, or points at the id to use. The id
/// written is reported through `out_fid` when that is non-NULL.
///
/// `wkb` is the geometry, or NULL with `wkb_len` zero for a row with none.
/// `values` covers the layer's value columns in order; `values_len` must match
/// the layer.
///
/// # Safety
///
/// `writer` must be a live writer handle. `fid` must be NULL or readable.
/// `wkb` must be NULL or readable for `wkb_len` bytes. `values` must be NULL
/// (with `values_len` zero) or point at `values_len` readable values.
/// `out_fid` and `error` must be NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_writer_insert(
    writer: *mut gpkg_writer_t,
    fid: *const i64,
    wkb: *const u8,
    wkb_len: usize,
    values: *const gpkg_value_t,
    values_len: usize,
    out_fid: *mut i64,
    error: *mut gpkg_error_t,
) -> Status {
    // SAFETY: forwarded to this function's own contract.
    let Some(handle) = (unsafe { writer_mut(writer, "gpkg_writer_insert", error) }) else {
        return Status::BadArgument;
    };
    // SAFETY: the caller guarantees `fid` is NULL or readable.
    let fid = unsafe { fid.as_ref() }.copied();
    // SAFETY: forwarded to this function's own contract.
    let Some(geometry) = (unsafe { borrow_wkb(wkb, wkb_len, error) }) else {
        return Status::BadArgument;
    };
    // SAFETY: forwarded to this function's own contract.
    let Some(values) = (unsafe { borrow_values(values, values_len, error) }) else {
        return Status::InvalidArgument;
    };

    let written = match geometry {
        Some(wkb) => handle.writer_mut().insert_wkb(fid, wkb, &values),
        None => handle.writer_mut().insert_row(fid, &values),
    };
    match written {
        Ok(assigned) => {
            if !out_fid.is_null() {
                // SAFETY: the caller guarantees `out_fid` is NULL or writable.
                unsafe { *out_fid = assigned };
            }
            Status::Ok
        }
        // SAFETY: forwarded to the caller's guarantee on `error`.
        Err(err) => unsafe { failed(&err, error) },
    }
}

/// Update the feature `fid`, replacing every value column and, when `wkb` is
/// given, the geometry.
///
/// `out_matched` reports whether a row with that id was there to update; a
/// missing row is not an error.
///
/// # Safety
///
/// As [`gpkg_writer_insert`], with `out_matched` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_writer_update(
    writer: *mut gpkg_writer_t,
    fid: i64,
    wkb: *const u8,
    wkb_len: usize,
    values: *const gpkg_value_t,
    values_len: usize,
    out_matched: *mut bool,
    error: *mut gpkg_error_t,
) -> Status {
    // SAFETY: forwarded to this function's own contract.
    let Some(handle) = (unsafe { writer_mut(writer, "gpkg_writer_update", error) }) else {
        return Status::BadArgument;
    };
    // SAFETY: forwarded to this function's own contract.
    let Some(geometry) = (unsafe { borrow_wkb(wkb, wkb_len, error) }) else {
        return Status::BadArgument;
    };
    // SAFETY: forwarded to this function's own contract.
    let Some(values) = (unsafe { borrow_values(values, values_len, error) }) else {
        return Status::InvalidArgument;
    };

    let updated = match geometry {
        Some(wkb) => handle.writer_mut().update_wkb(fid, wkb, &values),
        None => handle.writer_mut().update_row(fid, &values),
    };
    // SAFETY: forwarded to this function's own contract.
    unsafe { report_matched(updated, out_matched, error) }
}

/// Update one column of the feature `fid`, leaving every other value and the
/// geometry alone.
///
/// The shape a caller wants when correcting one field: nothing else has to be
/// supplied, and nothing else is touched.
///
/// # Safety
///
/// `writer` must be a live writer handle, `column` a NUL-terminated UTF-8
/// string, `value` a readable value, and `out_matched` and `error` NULL or
/// writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_writer_update_column(
    writer: *mut gpkg_writer_t,
    fid: i64,
    column: *const c_char,
    value: *const gpkg_value_t,
    out_matched: *mut bool,
    error: *mut gpkg_error_t,
) -> Status {
    // SAFETY: forwarded to this function's own contract.
    let Some(handle) = (unsafe { writer_mut(writer, "gpkg_writer_update_column", error) }) else {
        return Status::BadArgument;
    };
    // SAFETY: forwarded to this function's own contract.
    let Some(column) = (unsafe { borrow_str(column, error, "column") }) else {
        return Status::BadArgument;
    };
    // SAFETY: forwarded to this function's own contract.
    let Some(values) = (unsafe { borrow_values(value, 1, error) }) else {
        return Status::InvalidArgument;
    };
    let Some(cell) = values.first().copied() else {
        return Status::InvalidArgument;
    };

    let updated = handle.writer_mut().update_column(fid, column, cell);
    // SAFETY: forwarded to this function's own contract.
    unsafe { report_matched(updated, out_matched, error) }
}

/// Delete the feature `fid`.
///
/// `out_matched` reports whether a row with that id was there to delete; a
/// missing row is not an error.
///
/// The layer's recorded bounding box is not shrunk, here or anywhere else: an
/// over-estimate is what the specification permits, and shrinking would mean
/// rescanning the layer.
///
/// # Safety
///
/// `writer` must be a live writer handle, and `out_matched` and `error` NULL or
/// writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_writer_delete(
    writer: *mut gpkg_writer_t,
    fid: i64,
    out_matched: *mut bool,
    error: *mut gpkg_error_t,
) -> Status {
    // SAFETY: forwarded to this function's own contract.
    let Some(handle) = (unsafe { writer_mut(writer, "gpkg_writer_delete", error) }) else {
        return Status::BadArgument;
    };
    let deleted = handle.writer_mut().delete(fid);
    // SAFETY: forwarded to this function's own contract.
    unsafe { report_matched(deleted, out_matched, error) }
}

/// Report the outcome of an update or delete through `out_matched`.
///
/// # Safety
///
/// `out_matched` and `error` must be NULL or writable.
unsafe fn report_matched(
    outcome: geopackage::Result<bool>,
    out_matched: *mut bool,
    error: *mut gpkg_error_t,
) -> Status {
    match outcome {
        Ok(matched) => {
            if !out_matched.is_null() {
                // SAFETY: the caller guarantees `out_matched` is NULL or
                // writable.
                unsafe { *out_matched = matched };
            }
            Status::Ok
        }
        // SAFETY: forwarded to the caller's guarantee on `error`.
        Err(err) => unsafe { failed(&err, error) },
    }
}
