//! Validation findings through the C entry points.

use std::ffi::{CStr, CString, c_char};

use geopackage_ffi::{
    Status, gpkg_close, gpkg_error_clear, gpkg_error_t, gpkg_finding_at, gpkg_findings_count,
    gpkg_findings_free, gpkg_findings_t, gpkg_open_read_only, gpkg_string_free, gpkg_t,
    gpkg_validate,
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

fn open(fixture: &str) -> (*mut gpkg_t, gpkg_error_t) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate directory has a parent")
        .join("geopackage/tests/fixtures")
        .join(fixture);
    let c_path = CString::new(path.to_str().expect("path is UTF-8")).expect("no interior NUL");
    let mut error = error_slot();
    // SAFETY: a valid path and a writable error slot.
    let gpkg = unsafe { gpkg_open_read_only(c_path.as_ptr(), &raw mut error) };
    assert!(!gpkg.is_null(), "{:?}", message(&error));
    (gpkg, error)
}

#[test]
fn findings_come_back_with_severity_and_text() {
    // The multilayer fixture is deliberately partly unindexed, so validation
    // reports advisories; what exactly it reports is pinned by the library's
    // own tests, and what this one checks is the boundary.
    let (gpkg, mut error) = open("gdal_multilayer_1_4.gpkg");

    let mut findings: *mut gpkg_findings_t = std::ptr::null_mut();
    // SAFETY: a live container handle and writable out-parameters.
    let status = unsafe { gpkg_validate(gpkg, &raw mut findings, &raw mut error) };
    assert_eq!(status, Status::Ok, "{:?}", message(&error));
    assert!(!findings.is_null());

    // The findings outlive the container by design: they borrow nothing, so
    // the close is not blocked and the handle stays readable after it.
    // SAFETY: nothing borrows the container.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);

    // SAFETY: a live findings handle.
    let count = unsafe { gpkg_findings_count(findings) };
    assert!(count > 0, "the fixture has unindexed layers to report");

    for index in 0..count {
        let mut severity: *mut c_char = std::ptr::null_mut();
        let mut text: *mut c_char = std::ptr::null_mut();
        let mut repair: *mut c_char = std::ptr::null_mut();
        // SAFETY: a live findings handle and writable out-parameters.
        let status = unsafe {
            gpkg_finding_at(
                findings,
                index,
                &raw mut severity,
                &raw mut text,
                &raw mut repair,
                &raw mut error,
            )
        };
        assert_eq!(status, Status::Ok, "{:?}", message(&error));
        let severity = take_string(severity).expect("severity is never absent");
        assert!(
            ["error", "warning", "advisory"].contains(&severity.as_str()),
            "{severity}"
        );
        assert!(take_string(text).is_some_and(|text| !text.is_empty()));
        // Repair advice is optional per finding; freeing its NULL is fine and
        // `take_string` already did.
        drop(take_string(repair));
    }

    // Past the end reports rather than reading out of bounds.
    // SAFETY: a live findings handle; the index is deliberately out of range.
    let status = unsafe {
        gpkg_finding_at(
            findings,
            count,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut error,
        )
    };
    assert_eq!(status, Status::NotFound);
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: a live findings handle, freed exactly once.
    unsafe { gpkg_findings_free(findings) };

    // NULL is harmless in both remaining calls.
    // SAFETY: NULL is explicitly permitted.
    assert_eq!(unsafe { gpkg_findings_count(std::ptr::null()) }, 0);
    // SAFETY: NULL is explicitly permitted.
    unsafe { gpkg_findings_free(std::ptr::null_mut()) };
}
