//! `gpkg_layer_t`: a handle onto one layer of an open GeoPackage.
//!
//! A layer is opened by name, either as a feature layer with
//! [`gpkg_layer_open`] or as a non-spatial attribute table with
//! [`gpkg_attributes_open`], and released with [`gpkg_layer_free`]. The handle
//! is what everything about a layer hangs off: the schema calls in
//! [`crate::schema`], and the Arrow reads and writes in [`crate::stream`].
//!
//! [`gpkg_layer_names_count`] and [`gpkg_layer_name_at`] enumerate the file's
//! feature layers, in table-name order. Attribute tables are not in that list,
//! so a caller wanting one opens it by a name it already knows.
//!
//! ```c
//! size_t layers = 0;
//! if (gpkg_layer_names_count(gpkg, &layers, &error) != GPKG_STATUS_OK) {
//!     return fail("gpkg_layer_names_count", &error);
//! }
//!
//! for (size_t i = 0; i < layers; i++) {
//!     char *name = gpkg_layer_name_at(gpkg, i, &error);
//!     if (!name) {
//!         return fail("gpkg_layer_name_at", &error);
//!     }
//!
//!     gpkg_layer_t *layer = gpkg_layer_open(gpkg, name, &error);
//!     if (!layer) {
//!         gpkg_string_free(name);
//!         return fail("gpkg_layer_open", &error);
//!     }
//!
//!     uint64_t rows = 0;
//!     if (gpkg_layer_count(layer, &rows, &error) != GPKG_STATUS_OK) {
//!         gpkg_layer_free(layer);
//!         gpkg_string_free(name);
//!         return fail("gpkg_layer_count", &error);
//!     }
//!     printf("%s: %llu rows\n", name, (unsigned long long)rows);
//!
//!     gpkg_layer_free(layer);
//!     gpkg_string_free(name);
//! }
//! ```
//!
//! A layer handle borrows its container. That borrow is erased to `'static` in
//! [`crate::handle`], and what makes the erasure sound is the rule enforced
//! here and in `gpkg_close`: a container cannot close while any layer
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

/// Opens a feature layer by name.
///
/// The name is a `gpkg_contents.table_name`, matched without regard to case,
/// as SQLite resolves table names. A name no row declares is
/// `GPKG_STATUS_NOT_FOUND`; a name that declares something other than a feature
/// layer, an attribute table or a tile pyramid, is
/// `GPKG_STATUS_INVALID_ARGUMENT`, with the message naming what was found.
///
/// Opening reads the table's schema, so it is the expensive part of working
/// with a layer. Hold the handle for as long as the work lasts rather than
/// reopening per call.
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

/// Opens an attribute (non-spatial) layer by name.
///
/// The same call as `gpkg_layer_open` for a table `gpkg_contents` declares as
/// `attributes` rather than `features`. Such a table has no geometry column, so
/// `gpkg_layer_geometry_column` returns NULL for it and the bounding-box read
/// does not apply; everything else, including the Arrow read and write, works
/// the same way.
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

/// Opens a feature layer that reads only the named columns.
///
/// `gpkg_layer_open`, projected: a read of the returned handle selects the
/// feature id, the named value columns, and the geometry only if its column
/// is named. On a layer whose geometries are large, reading one attribute
/// through a projected handle touches none of the geometry blobs, which is
/// most of the read on such layers. The Arrow schema narrows to match, so a
/// stream from this handle yields exactly these fields.
///
/// Columns come back in the table's order whatever order they are named in,
/// and naming one twice selects it once. The feature id need not be named. A
/// name that is neither a value column nor the geometry column is
/// `GPKG_STATUS_NOT_FOUND`, naming the column, so a typo fails at the open
/// rather than quietly selecting nothing.
///
/// A bounding-box read still works when the geometry is not named: each
/// candidate is still tested exactly against its true envelope, and the
/// geometry simply reaches no batch. Writing through the handle is
/// unaffected: a writer writes the layer's whole column list, so a partial
/// row cannot be inserted by accident.
///
/// ```c
/// const char *columns[] = {"name", "population"};
/// gpkg_layer_t *narrow =
///     gpkg_layer_open_with_columns(gpkg, "cities", columns, 2, &error);
/// ```
///
/// # Safety
///
/// As [`gpkg_layer_open`], and additionally: `columns` must be NULL (only
/// when `columns_len` is 0) or point at `columns_len` readable pointers, each
/// a NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_open_with_columns(
    gpkg: *const Container,
    name: *const c_char,
    columns: *const *const c_char,
    columns_len: usize,
    error: *mut gpkg_error_t,
) -> *mut gpkg_layer_t {
    // SAFETY: forwarded to this function's contract.
    unsafe { open_layer_with_columns(gpkg, name, columns, columns_len, error, false) }
}

/// Opens an attribute (non-spatial) layer that reads only the named columns.
///
/// `gpkg_attributes_open` with `gpkg_layer_open_with_columns`'s projection,
/// on the same terms; there is no geometry column to name.
///
/// # Safety
///
/// As [`gpkg_layer_open_with_columns`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_attributes_open_with_columns(
    gpkg: *const Container,
    name: *const c_char,
    columns: *const *const c_char,
    columns_len: usize,
    error: *mut gpkg_error_t,
) -> *mut gpkg_layer_t {
    // SAFETY: forwarded to this function's contract.
    unsafe { open_layer_with_columns(gpkg, name, columns, columns_len, error, true) }
}

/// The body of both projected opens.
///
/// # Safety
///
/// As [`gpkg_layer_open_with_columns`].
unsafe fn open_layer_with_columns(
    gpkg: *const Container,
    name: *const c_char,
    columns: *const *const c_char,
    columns_len: usize,
    error: *mut gpkg_error_t,
    attributes: bool,
) -> *mut gpkg_layer_t {
    if gpkg.is_null() || (columns.is_null() && columns_len != 0) {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "gpkg handle or columns is NULL") };
        return std::ptr::null_mut();
    }
    // SAFETY: forwarded to the caller's guarantee on `name`.
    let Some(name) = (unsafe { borrow_str(name, error, "layer name") }) else {
        return std::ptr::null_mut();
    };
    let mut names = Vec::with_capacity(columns_len);
    for index in 0..columns_len {
        // SAFETY: the caller guarantees `columns_len` readable pointers, so
        // every offset below `columns_len` is in bounds.
        let slot = unsafe { columns.add(index) };
        // SAFETY: `slot` is in bounds per the line above, and readable.
        let column = unsafe { *slot };
        // SAFETY: forwarded to the caller's guarantee on each column string.
        let Some(text) = (unsafe { borrow_str(column, error, "column name") }) else {
            return std::ptr::null_mut();
        };
        names.push(text);
    }

    // SAFETY: the caller guarantees a live container handle.
    let container = unsafe { &*gpkg };
    match container.layer_with_columns(name, &names, attributes) {
        Ok(handle) => Box::into_raw(Box::new(handle)),
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            std::ptr::null_mut()
        }
    }
}

/// Releases a layer handle.
///
/// The container it came from can be closed once every handle taken from it has
/// been freed, so this is what a `GPKG_STATUS_HANDLE_IN_USE` from `gpkg_close`
/// is asking for. Passing NULL does nothing, which lets a cleanup path free a
/// handle it may never have opened.
///
/// An Arrow stream taken from this layer counts against the container in its
/// own right, so freeing the layer is not on its own enough to permit a close:
/// the stream has to be released too.
///
/// # Safety
///
/// `layer` must be NULL, or a handle from `gpkg_layer_open` or
/// `gpkg_attributes_open` that has not already been freed. The container it
/// came from must still be alive, which is guaranteed by `gpkg_close` failing
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

/// Returns the layer's table name.
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

/// Writes the number of rows in the layer through `out`.
///
/// A `SELECT count(*)` over the table, so it reads the table rather than a
/// stored figure and costs what that costs.
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

/// Returns the number of feature layers the file declares.
///
/// Feature layers only: a table `gpkg_contents` declares as `attributes` or
/// `tiles` is not counted, and neither is a `features` row with no matching
/// `gpkg_geometry_columns` entry. `gpkg_layer_name_at` walks the same list,
/// which is ordered by table name.
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

/// Returns the name of the `index`th feature layer, or NULL when out of range.
///
/// The list is the one `gpkg_layer_names_count` counts, ordered by table name,
/// so the pair walks a file's layers. An index at or beyond the count is
/// `GPKG_STATUS_NOT_FOUND` rather than a failure to read.
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
