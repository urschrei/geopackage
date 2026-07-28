//! Schema introspection and the spatial index, through the C entry points.

use std::ffi::{CStr, CString, c_char};
use std::path::PathBuf;

use geopackage_ffi::{
    Status, gpkg_close, gpkg_error_clear, gpkg_error_t, gpkg_layer_column_count,
    gpkg_layer_column_is_primary_key, gpkg_layer_column_name, gpkg_layer_column_type,
    gpkg_layer_create_spatial_index, gpkg_layer_drop_spatial_index, gpkg_layer_extent,
    gpkg_layer_free, gpkg_layer_geometry_column, gpkg_layer_geometry_type,
    gpkg_layer_has_spatial_index, gpkg_layer_kind, gpkg_layer_open,
    gpkg_layer_repair_spatial_index, gpkg_layer_spatial_index_status, gpkg_layer_srs_id, gpkg_open,
    gpkg_string_free, gpkg_t,
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

fn fixture() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate directory has a parent")
        .join("geopackage/tests/fixtures/gdal_multilayer_1_4.gpkg")
}

/// A writable copy of the fixture, opened read-write so the index commands
/// have something they can act on.
fn open_copy(dir: &std::path::Path) -> (*mut gpkg_t, gpkg_error_t) {
    let path = dir.join("m.gpkg");
    std::fs::copy(fixture(), &path).expect("copy the fixture");
    let c_path = CString::new(path.to_str().expect("path is UTF-8")).expect("no interior NUL");
    let mut error = error_slot();
    // SAFETY: a valid path and a writable error slot.
    let gpkg = unsafe { gpkg_open(c_path.as_ptr(), &raw mut error) };
    assert!(!gpkg.is_null(), "{:?}", message(&error));
    (gpkg, error)
}

/// Open a named layer of an open container.
fn layer(
    gpkg: *mut gpkg_t,
    name: &str,
    error: &mut gpkg_error_t,
) -> *mut geopackage_ffi::gpkg_layer_t {
    let c_name = CString::new(name).expect("no interior NUL");
    // SAFETY: a live container handle and a valid name.
    let handle = unsafe { gpkg_layer_open(gpkg, c_name.as_ptr(), &raw mut *error) };
    assert!(!handle.is_null(), "{:?}", message(error));
    handle
}

#[test]
fn a_layers_columns_can_be_walked_by_index() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (gpkg, mut error) = open_copy(dir.path());
    let points = layer(gpkg, "points", &mut error);

    let mut count = 0usize;
    // SAFETY: a live layer handle and a writable out-parameter.
    let status = unsafe { gpkg_layer_column_count(points, &raw mut count, &raw mut error) };
    assert_eq!(status, Status::Ok);
    assert_eq!(count, 4, "fid, geom, name, pop");

    let mut names = Vec::new();
    let mut primary_keys = 0;
    for index in 0..count {
        // SAFETY: a live layer handle and an in-range index.
        let name = unsafe { gpkg_layer_column_name(points, index, &raw mut error) };
        names.push(take_string(name).expect("a column name"));
        // SAFETY: as above.
        if unsafe { gpkg_layer_column_is_primary_key(points, index) } {
            primary_keys += 1;
        }
    }
    assert!(names.contains(&"fid".to_owned()), "{names:?}");
    assert!(names.contains(&"name".to_owned()), "{names:?}");
    assert_eq!(primary_keys, 1, "exactly one primary key: {names:?}");

    // The declared type comes back as the file spells it.
    let position = names
        .iter()
        .position(|n| n == "name")
        .expect("the name column");
    // SAFETY: a live layer handle and an in-range index.
    let declared = unsafe { gpkg_layer_column_type(points, position, &raw mut error) };
    assert_eq!(take_string(declared).as_deref(), Some("TEXT"));

    // Past the end reports rather than reading out of bounds.
    // SAFETY: a live layer handle; the index is deliberately out of range.
    let past_end = unsafe { gpkg_layer_column_name(points, count, &raw mut error) };
    assert!(past_end.is_null());
    assert_eq!(error.code, Status::NotFound);
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: a live layer handle, freed exactly once.
    unsafe { gpkg_layer_free(points) };
    // SAFETY: nothing borrows the container.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn a_feature_layer_reports_its_geometry_and_srs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (gpkg, mut error) = open_copy(dir.path());
    let points = layer(gpkg, "points", &mut error);

    // SAFETY: a live layer handle.
    let kind = unsafe { gpkg_layer_kind(points, &raw mut error) };
    assert_eq!(take_string(kind).as_deref(), Some("features"));

    // SAFETY: a live layer handle.
    let column = unsafe { gpkg_layer_geometry_column(points, &raw mut error) };
    assert_eq!(take_string(column).as_deref(), Some("geom"));

    // SAFETY: a live layer handle.
    let geometry_type = unsafe { gpkg_layer_geometry_type(points, &raw mut error) };
    assert_eq!(take_string(geometry_type).as_deref(), Some("POINT"));

    let mut srs = 0i32;
    // SAFETY: a live layer handle and a writable out-parameter.
    let status = unsafe { gpkg_layer_srs_id(points, &raw mut srs, &raw mut error) };
    assert_eq!(status, Status::Ok);
    assert_eq!(srs, 4326);

    // SAFETY: a live layer handle, freed exactly once.
    unsafe { gpkg_layer_free(points) };
    // SAFETY: nothing borrows the container.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn a_layers_extent_comes_back_when_there_is_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (gpkg, mut error) = open_copy(dir.path());
    let points = layer(gpkg, "points", &mut error);

    let (mut min_x, mut min_y, mut max_x, mut max_y) = (0.0, 0.0, 0.0, 0.0);
    // SAFETY: a live layer handle and four writable out-parameters.
    let status = unsafe {
        gpkg_layer_extent(
            points,
            &raw mut min_x,
            &raw mut min_y,
            &raw mut max_x,
            &raw mut max_y,
            &raw mut error,
        )
    };
    assert_eq!(status, Status::Ok, "{:?}", message(&error));
    assert!(
        min_x <= max_x && min_y <= max_y,
        "{min_x} {min_y} {max_x} {max_y}"
    );

    // SAFETY: a live layer handle, freed exactly once.
    unsafe { gpkg_layer_free(points) };
    // SAFETY: nothing borrows the container.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn the_spatial_index_can_be_reported_built_and_dropped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (gpkg, mut error) = open_copy(dir.path());

    // `lines` has no index in the fixture.
    let lines = layer(gpkg, "lines", &mut error);
    // SAFETY: a live layer handle.
    let status = unsafe { gpkg_layer_spatial_index_status(lines, &raw mut error) };
    assert_eq!(take_string(status).as_deref(), Some("absent"));

    let mut has = true;
    // SAFETY: a live layer handle and a writable out-parameter.
    let code = unsafe { gpkg_layer_has_spatial_index(lines, &raw mut has, &raw mut error) };
    assert_eq!(code, Status::Ok);
    assert!(!has);

    // Build it.
    // SAFETY: a live layer handle.
    let built = unsafe { gpkg_layer_create_spatial_index(lines, &raw mut error) };
    assert_eq!(built, Status::Ok, "{:?}", message(&error));

    // SAFETY: a live layer handle.
    let status = unsafe { gpkg_layer_spatial_index_status(lines, &raw mut error) };
    assert_eq!(take_string(status).as_deref(), Some("current"));

    // SAFETY: a live layer handle and a writable out-parameter.
    let code = unsafe { gpkg_layer_has_spatial_index(lines, &raw mut has, &raw mut error) };
    assert_eq!(code, Status::Ok);
    assert!(has);

    // And drop it again.
    // SAFETY: a live layer handle.
    let dropped = unsafe { gpkg_layer_drop_spatial_index(lines, &raw mut error) };
    assert_eq!(dropped, Status::Ok, "{:?}", message(&error));
    // SAFETY: a live layer handle.
    let status = unsafe { gpkg_layer_spatial_index_status(lines, &raw mut error) };
    assert_eq!(take_string(status).as_deref(), Some("absent"));

    // SAFETY: a live layer handle, freed exactly once.
    unsafe { gpkg_layer_free(lines) };
    // SAFETY: nothing borrows the container.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn repairing_an_index_that_is_already_current_does_nothing_and_succeeds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (gpkg, mut error) = open_copy(dir.path());
    let points = layer(gpkg, "points", &mut error);

    // SAFETY: a live layer handle.
    let repaired = unsafe { gpkg_layer_repair_spatial_index(points, &raw mut error) };
    assert_eq!(repaired, Status::Ok);
    // SAFETY: a live layer handle.
    let status = unsafe { gpkg_layer_spatial_index_status(points, &raw mut error) };
    assert_eq!(take_string(status).as_deref(), Some("current"));

    // SAFETY: a live layer handle, freed exactly once.
    unsafe { gpkg_layer_free(points) };
    // SAFETY: nothing borrows the container.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn an_attribute_layer_has_no_geometry_and_says_so() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("a.gpkg");
    {
        let gpkg = geopackage::GeoPackage::create(&path).expect("create");
        gpkg.create_attributes_table(&geopackage::TableSchemaBuilder::new("notes").column(
            geopackage::ColumnSpec::new("note", geopackage::core::types::ColumnType::Text(None)),
        ))
        .expect("create attributes table");
    }
    let c_path = CString::new(path.to_str().expect("path is UTF-8")).expect("no interior NUL");
    let mut error = error_slot();
    // SAFETY: a valid path and a writable error slot.
    let gpkg = unsafe { gpkg_open(c_path.as_ptr(), &raw mut error) };
    assert!(!gpkg.is_null());

    let name = CString::new("notes").expect("no interior NUL");
    // SAFETY: a live container handle and a valid name.
    let notes =
        unsafe { geopackage_ffi::gpkg_attributes_open(gpkg, name.as_ptr(), &raw mut error) };
    assert!(!notes.is_null(), "{:?}", message(&error));

    // SAFETY: a live layer handle.
    let kind = unsafe { gpkg_layer_kind(notes, &raw mut error) };
    assert_eq!(take_string(kind).as_deref(), Some("attributes"));

    // A NULL geometry column with no error set: not a failure, just no geometry.
    // SAFETY: a live layer handle.
    let column = unsafe { gpkg_layer_geometry_column(notes, &raw mut error) };
    assert!(column.is_null());
    assert_eq!(error.code, Status::Ok);

    // Asking for the SRS of a layer with no geometry is a genuine failure.
    let mut srs = 0i32;
    // SAFETY: a live layer handle and a writable out-parameter.
    let status = unsafe { gpkg_layer_srs_id(notes, &raw mut srs, &raw mut error) };
    assert_eq!(status, Status::NotFound);
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: a live layer handle, freed exactly once.
    unsafe { gpkg_layer_free(notes) };
    // SAFETY: nothing borrows the container.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn null_handles_are_rejected_throughout() {
    let mut error = error_slot();
    let mut count = 0usize;

    // SAFETY: NULL is the case under test throughout.
    let status =
        unsafe { gpkg_layer_column_count(std::ptr::null(), &raw mut count, &raw mut error) };
    assert_eq!(status, Status::BadArgument);
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: as above.
    let kind = unsafe { gpkg_layer_kind(std::ptr::null(), &raw mut error) };
    assert!(kind.is_null());
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: as above.
    let built = unsafe { gpkg_layer_create_spatial_index(std::ptr::null(), &raw mut error) };
    assert_eq!(built, Status::BadArgument);
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };
}
