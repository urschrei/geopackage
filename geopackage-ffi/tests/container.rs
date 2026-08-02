//! The container handle's lifecycle, driven through the C entry points.
//!
//! These are the tests that matter for soundness: the whole crate rests on a
//! layer handle never outliving the container it borrows, and on that being
//! enforced rather than asked of the caller. Run them under a sanitizer and
//! under miri (for the parts miri can reach) as well as plainly.

use std::ffi::{CStr, CString};

use geopackage_ffi::{
    Status, gpkg_close, gpkg_create, gpkg_error_clear, gpkg_error_t, gpkg_open,
    gpkg_open_read_only, gpkg_open_warning, gpkg_open_warning_count, gpkg_srs, gpkg_string_free,
    gpkg_version,
};

/// A zeroed error slot, as a C caller would declare one.
fn error_slot() -> gpkg_error_t {
    gpkg_error_t {
        code: Status::Ok,
        message: std::ptr::null_mut(),
    }
}

/// The message an error contains, if any.
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

/// Take an owned string back from the library, freeing it.
fn take_string(ptr: *mut std::ffi::c_char) -> Option<String> {
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

#[test]
fn create_open_and_close_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = CString::new(
        dir.path()
            .join("c.gpkg")
            .to_str()
            .expect("temp path is UTF-8"),
    )
    .expect("no interior NUL");
    let mut error = error_slot();

    // SAFETY: a valid path and a writable error slot.
    let gpkg = unsafe { gpkg_create(path.as_ptr(), &raw mut error) };
    assert!(!gpkg.is_null(), "{:?}", message(&error));

    // SAFETY: a live handle.
    let version_out = unsafe { gpkg_version(gpkg, &raw mut error) };
    assert_eq!(take_string(version_out).as_deref(), Some("1.4"));

    // SAFETY: a live handle, closed exactly once.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);

    // And it opens again.
    // SAFETY: a valid path and a writable error slot.
    let reopened = unsafe { gpkg_open(path.as_ptr(), &raw mut error) };
    assert!(!reopened.is_null(), "{:?}", message(&error));
    // SAFETY: a live handle, closed exactly once.
    let reclosed = unsafe { gpkg_close(reopened, &raw mut error) };
    assert_eq!(reclosed, Status::Ok);
}

#[test]
fn opening_something_that_is_not_there_reports_rather_than_crashes() {
    let path = CString::new("/definitely/not/here.gpkg").expect("present");
    let mut error = error_slot();

    // SAFETY: a valid path and a writable error slot.
    let gpkg = unsafe { gpkg_open(path.as_ptr(), &raw mut error) };
    assert!(gpkg.is_null());
    assert_ne!(error.code, Status::Ok);
    assert!(message(&error).is_some());

    // Clearing releases the message and resets the slot, and is idempotent.
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };
    assert_eq!(error.code, Status::Ok);
    assert!(error.message.is_null());
    // SAFETY: clearing an already-cleared slot is documented as safe.
    unsafe { gpkg_error_clear(&raw mut error) };
}

#[test]
fn a_null_error_slot_is_allowed() {
    let path = CString::new("/definitely/not/here.gpkg").expect("present");
    // SAFETY: NULL is explicitly permitted for the error out-parameter.
    let gpkg = unsafe { gpkg_open(path.as_ptr(), std::ptr::null_mut()) };
    assert!(gpkg.is_null());
}

#[test]
fn a_null_path_is_rejected_rather_than_dereferenced() {
    let mut error = error_slot();
    // SAFETY: NULL is the case under test; the function checks before reading.
    let gpkg = unsafe { gpkg_open(std::ptr::null(), &raw mut error) };
    assert!(gpkg.is_null());
    assert_eq!(error.code, Status::BadArgument);
    assert!(message(&error).expect("present").contains("path is NULL"));
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };
}

#[test]
fn closing_a_null_handle_is_rejected_rather_than_dereferenced() {
    let mut error = error_slot();
    // SAFETY: NULL is the case under test.
    let status = unsafe { gpkg_close(std::ptr::null_mut(), &raw mut error) };
    assert_eq!(status, Status::BadArgument);
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };
}

#[test]
fn a_lenient_read_only_open_reports_its_warnings() {
    // The legacy fixture: a 1.0 application_id, which strict open refuses to
    // treat as unremarkable and lenient open reports.
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate directory has a parent")
        .join("geopackage/tests/fixtures/legacy_gp10.gpkg");
    let path =
        CString::new(fixture.to_str().expect("fixture path is UTF-8")).expect("no interior NUL");
    let mut error = error_slot();

    // SAFETY: a valid path and a writable error slot.
    let gpkg = unsafe { gpkg_open_read_only(path.as_ptr(), &raw mut error) };
    assert!(!gpkg.is_null(), "{:?}", message(&error));

    // SAFETY: a live handle.
    let count = unsafe { gpkg_open_warning_count(gpkg) };
    assert!(count >= 1, "expected the legacy application_id warning");

    // SAFETY: a live handle and an in-range index.
    let first_out = unsafe { gpkg_open_warning(gpkg, 0, &raw mut error) };
    assert!(take_string(first_out).expect("present").contains("GP10"));

    // Out of range reports rather than reading past the end.
    // SAFETY: a live handle; the index is deliberately out of range.
    let past_end = unsafe { gpkg_open_warning(gpkg, count, &raw mut error) };
    assert!(past_end.is_null());
    assert_eq!(error.code, Status::NotFound);
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: a live handle, closed exactly once.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn version_of_a_null_handle_is_rejected() {
    let mut error = error_slot();
    // SAFETY: NULL is the case under test.
    let version = unsafe { gpkg_version(std::ptr::null(), &raw mut error) };
    assert!(version.is_null());
    assert_eq!(error.code, Status::BadArgument);
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };
}

#[test]
fn freeing_a_null_string_does_nothing() {
    // SAFETY: NULL is explicitly permitted.
    unsafe { gpkg_string_free(std::ptr::null_mut()) };
}

#[test]
fn an_srs_reads_back_with_its_definition() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate directory has a parent")
        .join("geopackage/tests/fixtures/gdal_multilayer_1_4.gpkg");
    let c_path = CString::new(path.to_str().expect("path is UTF-8")).expect("no interior NUL");
    let mut error = error_slot();
    // SAFETY: a valid path and a writable error slot.
    let gpkg = unsafe { gpkg_open_read_only(c_path.as_ptr(), &raw mut error) };
    assert!(!gpkg.is_null(), "{:?}", message(&error));

    let mut name: *mut std::ffi::c_char = std::ptr::null_mut();
    let mut organization: *mut std::ffi::c_char = std::ptr::null_mut();
    let mut code = 0i32;
    let mut definition: *mut std::ffi::c_char = std::ptr::null_mut();
    let mut wkt2: *mut std::ffi::c_char = std::ptr::null_mut();
    let mut epoch = 0f64;
    // SAFETY: a live container handle and writable out-parameters.
    let status = unsafe {
        gpkg_srs(
            gpkg,
            4326,
            &raw mut name,
            &raw mut organization,
            &raw mut code,
            &raw mut definition,
            &raw mut wkt2,
            &raw mut epoch,
            &raw mut error,
        )
    };
    assert_eq!(status, Status::Ok, "{:?}", message(&error));
    assert_eq!(take_string(organization).as_deref(), Some("EPSG"));
    assert_eq!(code, 4326);
    assert!(take_string(name).is_some_and(|text| !text.is_empty()));
    assert!(
        take_string(definition).is_some_and(|text| text.contains("4326") || text.contains("WGS")),
        "the WKT definition should describe WGS 84"
    );
    // This file has no CRS WKT extension, so there is no WKT2 definition
    // and no epoch.
    assert!(wkt2.is_null());
    assert!(epoch.is_nan());

    // Out-parameters may be skipped: only the definition, nothing else.
    let mut only_definition: *mut std::ffi::c_char = std::ptr::null_mut();
    // SAFETY: as above, with the other out-parameters deliberately NULL.
    let status = unsafe {
        gpkg_srs(
            gpkg,
            4326,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut only_definition,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut error,
        )
    };
    assert_eq!(status, Status::Ok, "{:?}", message(&error));
    assert!(take_string(only_definition).is_some());

    // An id no row declares reports rather than inventing.
    // SAFETY: as above.
    let status = unsafe {
        gpkg_srs(
            gpkg,
            999_999,
            std::ptr::null_mut(),
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

    // SAFETY: nothing borrows the container.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}
