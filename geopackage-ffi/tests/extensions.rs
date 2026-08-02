//! The extensions catalogue through the C entry points.

use std::ffi::{CStr, CString, c_char};

use geopackage_ffi::{
    Status, gpkg_close, gpkg_error_clear, gpkg_error_t, gpkg_extension_at, gpkg_extensions_count,
    gpkg_open_read_only, gpkg_string_free, gpkg_t,
};

fn error_slot() -> gpkg_error_t {
    gpkg_error_t {
        code: Status::Ok,
        message: std::ptr::null_mut(),
    }
}

fn message(error: &gpkg_error_t) -> Option<String> {
    if error.message.is_null() {
        return None;
    }
    // SAFETY: the library filled this in and has not freed it.
    Some(
        unsafe { CStr::from_ptr(error.message) }
            .to_string_lossy()
            .into_owned(),
    )
}

fn take_string(ptr: *mut c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the pointer came from the library and is freed immediately after.
    let text = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    // SAFETY: the pointer came from the library and is not used again.
    unsafe { gpkg_string_free(ptr) };
    Some(text)
}

fn open() -> (*mut gpkg_t, gpkg_error_t) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate directory has a parent")
        .join("geopackage/tests/fixtures/gdal_multilayer_1_4.gpkg");
    let c_path = CString::new(path.to_str().expect("path is UTF-8")).expect("no interior NUL");
    let mut error = error_slot();
    // SAFETY: a valid path and a writable error slot.
    let gpkg = unsafe { gpkg_open_read_only(c_path.as_ptr(), &raw mut error) };
    assert!(!gpkg.is_null(), "{:?}", message(&error));
    (gpkg, error)
}

struct Row {
    name: Option<String>,
    table: Option<String>,
    column: Option<String>,
    scope: Option<String>,
    support: Option<String>,
}

/// One row, every field requested.
fn row_at(gpkg: *mut gpkg_t, index: usize, error: &mut gpkg_error_t) -> Row {
    let mut name: *mut c_char = std::ptr::null_mut();
    let mut table: *mut c_char = std::ptr::null_mut();
    let mut column: *mut c_char = std::ptr::null_mut();
    let mut scope: *mut c_char = std::ptr::null_mut();
    let mut support: *mut c_char = std::ptr::null_mut();
    // SAFETY: a live container handle and writable out-parameters.
    let status = unsafe {
        gpkg_extension_at(
            gpkg,
            index,
            &raw mut name,
            &raw mut table,
            &raw mut column,
            &raw mut scope,
            &raw mut support,
            &raw mut *error,
        )
    };
    assert_eq!(status, Status::Ok, "{:?}", message(error));
    Row {
        name: take_string(name),
        table: take_string(table),
        column: take_string(column),
        scope: take_string(scope),
        support: take_string(support),
    }
}

#[test]
fn the_catalogue_can_be_walked_with_support_levels() {
    let (gpkg, mut error) = open();

    let mut count = 0usize;
    // SAFETY: a live container handle and a writable out-parameter.
    let status = unsafe { gpkg_extensions_count(gpkg, &raw mut count, &raw mut error) };
    assert_eq!(status, Status::Ok, "{:?}", message(&error));
    assert_eq!(count, 4, "the fixture contains four extension rows");

    // Rows come back ordered by extension name, then table, then column, so
    // the metadata rows precede the rtree rows. A metadata row applies to a
    // whole table, so its column is NULL.
    let row = row_at(gpkg, 0, &mut error);
    assert_eq!(row.name.as_deref(), Some("gpkg_metadata"));
    assert_eq!(row.table.as_deref(), Some("gpkg_metadata"));
    assert_eq!(row.column, None);
    assert_eq!(row.support.as_deref(), Some("implemented"));

    // An RTree index row names its column and its declared scope.
    let row = row_at(gpkg, 2, &mut error);
    assert_eq!(row.name.as_deref(), Some("gpkg_rtree_index"));
    assert_eq!(row.table.as_deref(), Some("points"));
    assert_eq!(row.column.as_deref(), Some("geom"));
    assert_eq!(row.scope.as_deref(), Some("write-only"));
    assert_eq!(row.support.as_deref(), Some("implemented"));

    // Past the end reports rather than reading out of bounds.
    // SAFETY: a live container handle; the index is deliberately out of range.
    let status = unsafe {
        gpkg_extension_at(
            gpkg,
            count,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut error,
        )
    };
    assert_eq!(status, Status::NotFound);
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: enumerating took no handles, so nothing blocks the close.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}
