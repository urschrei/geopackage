//! `gpkg_layer_t`: a handle onto one layer of an open GeoPackage.
//!
//! A layer handle borrows its container. That borrow is erased to `'static` in
//! [`crate::handle`], and what keeps the erasure honest is the rule enforced
//! here and in `gpkg_close`: a container refuses to close while any layer
//! handle it produced is still alive. Free the layer first.

use std::ffi::c_char;

use crate::error::{Status, gpkg_error_t, set_error, set_library_error};
use crate::handle::{Container, LayerHandle};
use crate::util::{borrow_str, out_string};

/// A layer of an open GeoPackage. Opaque; created by `gpkg_layer_open` or
/// `gpkg_attributes_open`, and destroyed by `gpkg_layer_free`.
#[expect(
    non_camel_case_types,
    reason = "the C name is the type's name; cbindgen emits it verbatim"
)]
pub type gpkg_layer_t = LayerHandle;

/// Open a feature layer by name.
///
/// The returned handle borrows `gpkg`, which cannot be closed until the handle
/// is released with `gpkg_layer_free`.
///
/// Returns NULL on failure.
///
/// # Safety
///
/// `gpkg` must be a live container handle, `name` a NUL-terminated UTF-8
/// string, and `error` NULL or a writable `gpkg_error_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_open(
    gpkg: *const Container,
    name: *const c_char,
    error: *mut gpkg_error_t,
) -> *mut gpkg_layer_t {
    // SAFETY: forwarded to this function's contract.
    unsafe { open_layer(gpkg, name, error, false) }
}

/// Open an attribute (non-spatial) layer by name.
///
/// # Safety
///
/// As [`gpkg_layer_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_attributes_open(
    gpkg: *const Container,
    name: *const c_char,
    error: *mut gpkg_error_t,
) -> *mut gpkg_layer_t {
    // SAFETY: forwarded to this function's contract.
    unsafe { open_layer(gpkg, name, error, true) }
}

/// The body of both, differing only in which library method opens the layer.
///
/// # Safety
///
/// As [`gpkg_layer_open`].
unsafe fn open_layer(
    gpkg: *const Container,
    name: *const c_char,
    error: *mut gpkg_error_t,
    attributes: bool,
) -> *mut gpkg_layer_t {
    if gpkg.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "gpkg handle is NULL") };
        return std::ptr::null_mut();
    }
    // SAFETY: forwarded to the caller's guarantee on `name`.
    let Some(name) = (unsafe { borrow_str(name, error, "layer name") }) else {
        return std::ptr::null_mut();
    };

    // SAFETY: the caller guarantees a live container handle.
    let container = unsafe { &*gpkg };
    let opened = if attributes {
        container.attributes(name)
    } else {
        container.layer(name)
    };
    match opened {
        Ok(handle) => Box::into_raw(Box::new(handle)),
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            std::ptr::null_mut()
        }
    }
}

/// Release a layer handle.
///
/// The container it came from can be closed once every handle taken from it has
/// been freed. Passing NULL does nothing.
///
/// # Safety
///
/// `layer` must be NULL, or a handle from `gpkg_layer_open` or
/// `gpkg_attributes_open` that has not already been freed. The container it
/// came from must still be alive, which is guaranteed by `gpkg_close` refusing
/// while this handle exists.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_free(layer: *mut gpkg_layer_t) {
    if layer.is_null() {
        return;
    }
    // SAFETY: the caller guarantees the pointer came from `Box::into_raw` in
    // `open_layer` and has not been freed. Dropping it decrements the
    // container's child count, which is what later permits a close.
    drop(unsafe { Box::from_raw(layer) });
}

/// The layer's table name.
///
/// Owned by the caller; release with `gpkg_string_free`.
///
/// # Safety
///
/// `layer` must be a live handle; `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_name(
    layer: *const gpkg_layer_t,
    error: *mut gpkg_error_t,
) -> *mut c_char {
    if layer.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "layer handle is NULL") };
        return std::ptr::null_mut();
    }
    // SAFETY: the caller guarantees a live layer handle.
    let name = unsafe { (*layer).layer().table_name().to_owned() };
    // SAFETY: forwarded to the caller's guarantee on `error`.
    unsafe { out_string(&name, error) }
}

/// The number of rows in the layer, written through `out`.
///
/// # Safety
///
/// `layer` must be a live handle, `out` must point at a writable `uint64_t`,
/// and `error` must be NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_count(
    layer: *const gpkg_layer_t,
    out: *mut u64,
    error: *mut gpkg_error_t,
) -> Status {
    if layer.is_null() || out.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "layer handle or out is NULL") };
        return Status::BadArgument;
    }
    // SAFETY: the caller guarantees a live layer handle.
    match unsafe { (*layer).layer().count() } {
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

/// How many feature layers the file declares.
///
/// # Safety
///
/// `gpkg` must be a live container handle; `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_names_count(
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
    match unsafe { (*gpkg).gpkg().layers() } {
        Ok(layers) => {
            // SAFETY: the caller guarantees `out` is writable.
            unsafe { *out = layers.len() };
            Status::Ok
        }
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            Status::from(&err)
        }
    }
}

/// The name of the `index`th feature layer, or NULL when out of range.
///
/// Owned by the caller; release with `gpkg_string_free`.
///
/// # Safety
///
/// `gpkg` must be a live container handle; `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_name_at(
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
    let layers = match unsafe { (*gpkg).gpkg().layers() } {
        Ok(layers) => layers,
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            return std::ptr::null_mut();
        }
    };
    let Some(layer) = layers.get(index) else {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::NotFound, "layer index out of range") };
        return std::ptr::null_mut();
    };
    let name = layer.table_name().to_owned();
    // SAFETY: forwarded to the caller's guarantee on `error`.
    unsafe { out_string(&name, error) }
}
