//! Schema introspection and the spatial index, for `gpkg_layer_t`.
//!
//! Everything here reads or acts on a layer the caller already holds, so none
//! of it takes a new token: the layer handle's own is what keeps the container
//! alive.

use std::ffi::c_char;

use geopackage::SpatialIndexStatus;

use crate::error::{Status, gpkg_error_t, set_error, set_library_error};
use crate::handle::LayerHandle;
use crate::util::out_string;

/// Whether a layer holds features or attributes, as `"features"` or
/// `"attributes"`.
///
/// Owned by the caller; release with `gpkg_string_free`.
///
/// # Safety
///
/// `layer` must be a live handle; `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_kind(
    layer: *const LayerHandle,
    error: *mut gpkg_error_t,
) -> *mut c_char {
    // SAFETY: forwarded to this function's contract.
    let Some(handle) = (unsafe { checked(layer, error, "gpkg_layer_kind") }) else {
        return std::ptr::null_mut();
    };
    let kind = handle.layer().kind().as_str();
    // SAFETY: forwarded to the caller's guarantee on `error`.
    unsafe { out_string(kind, error) }
}

/// How many columns the layer's table has, geometry and primary key included.
///
/// # Safety
///
/// `layer` must be a live handle, `out` writable, `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_column_count(
    layer: *const LayerHandle,
    out: *mut usize,
    error: *mut gpkg_error_t,
) -> Status {
    if out.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "out is NULL") };
        return Status::BadArgument;
    }
    // SAFETY: forwarded to this function's contract.
    let Some(handle) = (unsafe { checked(layer, error, "gpkg_layer_column_count") }) else {
        return Status::BadArgument;
    };
    let count = handle.layer().schema().columns.len();
    // SAFETY: the caller guarantees `out` is writable.
    unsafe { *out = count };
    Status::Ok
}

/// The name of the `index`th column, or NULL when out of range.
///
/// Owned by the caller; release with `gpkg_string_free`.
///
/// # Safety
///
/// `layer` must be a live handle; `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_column_name(
    layer: *const LayerHandle,
    index: usize,
    error: *mut gpkg_error_t,
) -> *mut c_char {
    // SAFETY: forwarded to this function's contract.
    let Some(handle) = (unsafe { checked(layer, error, "gpkg_layer_column_name") }) else {
        return std::ptr::null_mut();
    };
    let Some(column) = handle.layer().schema().columns.get(index) else {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::NotFound, "column index out of range") };
        return std::ptr::null_mut();
    };
    let name = column.name.clone();
    // SAFETY: forwarded to the caller's guarantee on `error`.
    unsafe { out_string(&name, error) }
}

/// The declared SQL type of the `index`th column, exactly as the file spells
/// it, or the empty string for a column declared without one.
///
/// The declared text rather than a parsed enum, because a GeoPackage may carry
/// a type outside the spec's vocabulary and this crate keeps such a column
/// rather than rejecting it. A caller wanting the spec's own names can compare
/// against them.
///
/// Owned by the caller; release with `gpkg_string_free`.
///
/// # Safety
///
/// `layer` must be a live handle; `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_column_type(
    layer: *const LayerHandle,
    index: usize,
    error: *mut gpkg_error_t,
) -> *mut c_char {
    // SAFETY: forwarded to this function's contract.
    let Some(handle) = (unsafe { checked(layer, error, "gpkg_layer_column_type") }) else {
        return std::ptr::null_mut();
    };
    let Some(column) = handle.layer().schema().columns.get(index) else {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::NotFound, "column index out of range") };
        return std::ptr::null_mut();
    };
    let declared = column.declared_type.clone();
    // SAFETY: forwarded to the caller's guarantee on `error`.
    unsafe { out_string(&declared, error) }
}

/// Whether the `index`th column is the primary key. `false` for an index out
/// of range, which [`gpkg_layer_column_count`] distinguishes.
///
/// # Safety
///
/// `layer` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_column_is_primary_key(
    layer: *const LayerHandle,
    index: usize,
) -> bool {
    if layer.is_null() {
        return false;
    }
    // SAFETY: the caller guarantees a live layer handle.
    let handle = unsafe { &*layer };
    handle
        .layer()
        .schema()
        .columns
        .get(index)
        .is_some_and(geopackage::Column::is_primary_key)
}

/// The geometry column's name, or NULL for an attribute layer.
///
/// A NULL return with `error` left at `GPKG_STATUS_OK` means the layer simply
/// has no geometry, which is not a failure.
///
/// Owned by the caller; release with `gpkg_string_free`.
///
/// # Safety
///
/// `layer` must be a live handle; `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_geometry_column(
    layer: *const LayerHandle,
    error: *mut gpkg_error_t,
) -> *mut c_char {
    // SAFETY: forwarded to this function's contract.
    let Some(handle) = (unsafe { checked(layer, error, "gpkg_layer_geometry_column") }) else {
        return std::ptr::null_mut();
    };
    let Some(geometry) = handle.layer().geometry_column() else {
        return std::ptr::null_mut();
    };
    let name = geometry.column_name.clone();
    // SAFETY: forwarded to the caller's guarantee on `error`.
    unsafe { out_string(&name, error) }
}

/// The geometry column's declared type, such as `"POINT"`, or NULL for an
/// attribute layer.
///
/// Owned by the caller; release with `gpkg_string_free`.
///
/// # Safety
///
/// `layer` must be a live handle; `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_geometry_type(
    layer: *const LayerHandle,
    error: *mut gpkg_error_t,
) -> *mut c_char {
    // SAFETY: forwarded to this function's contract.
    let Some(handle) = (unsafe { checked(layer, error, "gpkg_layer_geometry_type") }) else {
        return std::ptr::null_mut();
    };
    let Some(geometry) = handle.layer().geometry_column() else {
        return std::ptr::null_mut();
    };
    let name = geometry.geometry_type.to_string();
    // SAFETY: forwarded to the caller's guarantee on `error`.
    unsafe { out_string(&name, error) }
}

/// The geometry column's spatial reference system id, written through `out`.
///
/// # Safety
///
/// `layer` must be a live handle, `out` writable, `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_srs_id(
    layer: *const LayerHandle,
    out: *mut i32,
    error: *mut gpkg_error_t,
) -> Status {
    if out.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "out is NULL") };
        return Status::BadArgument;
    }
    // SAFETY: forwarded to this function's contract.
    let Some(handle) = (unsafe { checked(layer, error, "gpkg_layer_srs_id") }) else {
        return Status::BadArgument;
    };
    let Some(geometry) = handle.layer().geometry_column() else {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::NotFound, "layer has no geometry column") };
        return Status::NotFound;
    };
    // SAFETY: the caller guarantees `out` is writable.
    unsafe { *out = geometry.srs_id };
    Status::Ok
}

/// The layer's extent, written through the four out-parameters.
///
/// Returns `GPKG_STATUS_NOT_FOUND` when the layer has no extent to report,
/// which is an empty layer or one with no geometry column, leaving the
/// out-parameters untouched.
///
/// # Safety
///
/// `layer` must be a live handle, the four out-parameters writable, and
/// `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_extent(
    layer: *const LayerHandle,
    min_x: *mut f64,
    min_y: *mut f64,
    max_x: *mut f64,
    max_y: *mut f64,
    error: *mut gpkg_error_t,
) -> Status {
    if min_x.is_null() || min_y.is_null() || max_x.is_null() || max_y.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe {
            set_error(
                error,
                Status::BadArgument,
                "an extent out-parameter is NULL",
            )
        };
        return Status::BadArgument;
    }
    // SAFETY: forwarded to this function's contract.
    let Some(handle) = (unsafe { checked(layer, error, "gpkg_layer_extent") }) else {
        return Status::BadArgument;
    };
    let extent = match handle.layer().extent() {
        Ok(extent) => extent,
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            return Status::from(&err);
        }
    };
    let Some(bbox) = extent else {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::NotFound, "layer has no extent") };
        return Status::NotFound;
    };
    // SAFETY: the caller guarantees all four are writable.
    unsafe {
        *min_x = bbox.min_x;
    }
    // SAFETY: as above.
    unsafe {
        *min_y = bbox.min_y;
    }
    // SAFETY: as above.
    unsafe {
        *max_x = bbox.max_x;
    }
    // SAFETY: as above.
    unsafe {
        *max_y = bbox.max_y;
    }
    Status::Ok
}

/// The spatial index's state, as `"absent"`, `"current"`, `"legacy trigger
/// set"` or `"stale"`.
///
/// Owned by the caller; release with `gpkg_string_free`.
///
/// # Safety
///
/// `layer` must be a live handle; `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_spatial_index_status(
    layer: *const LayerHandle,
    error: *mut gpkg_error_t,
) -> *mut c_char {
    // SAFETY: forwarded to this function's contract.
    let Some(handle) = (unsafe { checked(layer, error, "gpkg_layer_spatial_index_status") }) else {
        return std::ptr::null_mut();
    };
    match handle.layer().spatial_index_status() {
        Ok(status) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { out_string(status.as_str(), error) }
        }
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            std::ptr::null_mut()
        }
    }
}

/// Whether a bounding-box query on this layer will use the spatial index.
///
/// False for a `stale` or absent index: a desynchronised index is not trusted,
/// and the query falls back to a correct full scan.
///
/// # Safety
///
/// `layer` must be a live handle, `out` writable, `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_has_spatial_index(
    layer: *const LayerHandle,
    out: *mut bool,
    error: *mut gpkg_error_t,
) -> Status {
    if out.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "out is NULL") };
        return Status::BadArgument;
    }
    // SAFETY: forwarded to this function's contract.
    let Some(handle) = (unsafe { checked(layer, error, "gpkg_layer_has_spatial_index") }) else {
        return Status::BadArgument;
    };
    match handle.layer().has_spatial_index() {
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

/// Build the spatial index on a layer that has none.
///
/// # Safety
///
/// `layer` must be a live handle; `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_create_spatial_index(
    layer: *const LayerHandle,
    error: *mut gpkg_error_t,
) -> Status {
    // SAFETY: forwarded to this function's contract.
    let Some(handle) = (unsafe { checked(layer, error, "gpkg_layer_create_spatial_index") }) else {
        return Status::BadArgument;
    };
    // SAFETY: forwarded to the caller's guarantee on `error`.
    unsafe { report(handle.layer().create_spatial_index(), error) }
}

/// Drop the spatial index and its triggers.
///
/// # Safety
///
/// `layer` must be a live handle; `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_drop_spatial_index(
    layer: *const LayerHandle,
    error: *mut gpkg_error_t,
) -> Status {
    // SAFETY: forwarded to this function's contract.
    let Some(handle) = (unsafe { checked(layer, error, "gpkg_layer_drop_spatial_index") }) else {
        return Status::BadArgument;
    };
    // SAFETY: forwarded to the caller's guarantee on `error`.
    unsafe { report(handle.layer().drop_spatial_index(), error) }
}

/// Put a legacy or desynchronised spatial index right, rebuilding it and
/// installing the GeoPackage 1.4 trigger set.
///
/// A layer with no index is left alone: an absent index is a choice rather
/// than a defect, and `gpkg_layer_create_spatial_index` is how one is asked
/// for.
///
/// # Safety
///
/// `layer` must be a live handle; `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_layer_repair_spatial_index(
    layer: *const LayerHandle,
    error: *mut gpkg_error_t,
) -> Status {
    // SAFETY: forwarded to this function's contract.
    let Some(handle) = (unsafe { checked(layer, error, "gpkg_layer_repair_spatial_index") }) else {
        return Status::BadArgument;
    };
    let status = match handle.layer().spatial_index_status() {
        Ok(status) => status,
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            return Status::from(&err);
        }
    };
    match status {
        SpatialIndexStatus::Legacy | SpatialIndexStatus::Stale => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { report(handle.layer().repair_spatial_index(), error) }
        }
        // Nothing to put right, which is success rather than failure.
        _ => Status::Ok,
    }
}

/// Borrow a layer handle, reporting a NULL one.
///
/// # Safety
///
/// `layer` must be NULL or a live layer handle; `error` NULL or writable.
unsafe fn checked<'a>(
    layer: *const LayerHandle,
    error: *mut gpkg_error_t,
    what: &str,
) -> Option<&'a LayerHandle> {
    if layer.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe {
            set_error(
                error,
                Status::BadArgument,
                &format!("{what}: layer is NULL"),
            )
        };
        return None;
    }
    // SAFETY: the caller guarantees a live layer handle.
    Some(unsafe { &*layer })
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
