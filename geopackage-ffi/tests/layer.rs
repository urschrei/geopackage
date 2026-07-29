//! Layer handles, and the rule that keeps the lifetime erasure honest.
//!
//! `handle.rs` erases a layer's borrow of its container to `'static`. That is
//! sound only because a container refuses to close while any layer handle it
//! produced is alive. If that check were wrong, the tests here would be a
//! use-after-free rather than a failing assertion, which is why they are worth
//! running under a sanitizer as well as plainly.

use std::ffi::{CStr, CString, c_char};
use std::path::PathBuf;

use geopackage_ffi::{
    Status, gpkg_close, gpkg_error_clear, gpkg_error_t, gpkg_layer_count, gpkg_layer_free,
    gpkg_layer_name, gpkg_layer_name_at, gpkg_layer_names_count, gpkg_layer_open,
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

fn fixture() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate directory has a parent")
        .join("geopackage/tests/fixtures/gdal_multilayer_1_4.gpkg")
}

/// An open read-only handle onto the multilayer fixture.
fn open() -> (*mut gpkg_t, gpkg_error_t) {
    let path =
        CString::new(fixture().to_str().expect("fixture path is UTF-8")).expect("no interior NUL");
    let mut error = error_slot();
    // SAFETY: a valid path and a writable error slot.
    let gpkg = unsafe { gpkg_open_read_only(path.as_ptr(), &raw mut error) };
    assert!(!gpkg.is_null(), "{:?}", message(&error));
    (gpkg, error)
}

#[test]
fn a_layer_handle_reports_its_name_and_row_count() {
    let (gpkg, mut error) = open();
    let name = CString::new("points").expect("present");

    // SAFETY: a live container handle and a valid name.
    let layer = unsafe { gpkg_layer_open(gpkg, name.as_ptr(), &raw mut error) };
    assert!(!layer.is_null(), "{:?}", message(&error));

    // SAFETY: a live layer handle.
    let name_out = unsafe { gpkg_layer_name(layer, &raw mut error) };
    assert_eq!(take_string(name_out).as_deref(), Some("points"));

    let mut count = 0u64;
    // SAFETY: a live layer handle and a writable out-parameter.
    let status = unsafe { gpkg_layer_count(layer, &raw mut count, &raw mut error) };
    assert_eq!(status, Status::Ok);
    assert_eq!(count, 3);

    // SAFETY: a live layer handle, freed exactly once.
    unsafe { gpkg_layer_free(layer) };
    // SAFETY: no children outstanding now, so the close is permitted.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn closing_with_a_live_layer_handle_is_refused_and_changes_nothing() {
    // The soundness rule. Were the container closed here, the layer handle
    // would hold a `Layer<'static>` pointing into freed memory, and the next
    // use of it would be a use-after-free rather than an error.
    let (gpkg, mut error) = open();
    let name = CString::new("points").expect("present");
    // SAFETY: a live container handle and a valid name.
    let layer = unsafe { gpkg_layer_open(gpkg, name.as_ptr(), &raw mut error) };
    assert!(!layer.is_null());

    // SAFETY: a live container handle; the refusal path leaves it open.
    let status = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(status, Status::HandleInUse);
    let text = message(&error).expect("present");
    assert!(text.contains("1 handle"), "{text}");
    assert!(text.contains("gpkg_layer_free"), "{text}");
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // The refusal left both handles usable, which is what "changes nothing"
    // means: the container was not half torn down.
    let mut count = 0u64;
    // SAFETY: both handles are still live, the close having been refused.
    let counted = unsafe { gpkg_layer_count(layer, &raw mut count, &raw mut error) };
    assert_eq!(counted, Status::Ok);
    assert_eq!(count, 3);

    // SAFETY: a live layer handle, freed exactly once.
    unsafe { gpkg_layer_free(layer) };
    // SAFETY: the last child is gone, so this now succeeds.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn the_refusal_counts_every_outstanding_handle() {
    let (gpkg, mut error) = open();
    let points = CString::new("points").expect("present");
    let lines = CString::new("lines").expect("present");

    // SAFETY: a live container handle and valid names.
    let a = unsafe { gpkg_layer_open(gpkg, points.as_ptr(), &raw mut error) };
    // SAFETY: as above.
    let b = unsafe { gpkg_layer_open(gpkg, lines.as_ptr(), &raw mut error) };
    assert!(!a.is_null() && !b.is_null());

    // SAFETY: a live container handle; the close is refused.
    let refused = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(refused, Status::HandleInUse);
    assert!(message(&error).expect("a message").contains("2 handle"));
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // Freeing one is not enough.
    // SAFETY: a live layer handle, freed exactly once.
    unsafe { gpkg_layer_free(a) };
    // SAFETY: a live container handle; still one child outstanding.
    let still_refused = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(still_refused, Status::HandleInUse);
    assert!(message(&error).expect("a message").contains("1 handle"));
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: a live layer handle, freed exactly once.
    unsafe { gpkg_layer_free(b) };
    // SAFETY: no children left.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn handles_can_be_opened_and_freed_repeatedly() {
    // Exercises the counter's arithmetic in both directions, which is what
    // stands between the erasure and a dangling pointer.
    let (gpkg, mut error) = open();
    let name = CString::new("points").expect("present");

    for _ in 0..50 {
        // SAFETY: a live container handle and a valid name.
        let layer = unsafe { gpkg_layer_open(gpkg, name.as_ptr(), &raw mut error) };
        assert!(!layer.is_null());
        // SAFETY: a live layer handle, freed exactly once.
        unsafe { gpkg_layer_free(layer) };
    }

    // SAFETY: every handle taken above was freed, so the count is back to zero.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn a_layer_that_is_not_there_reports_rather_than_returning_a_handle() {
    let (gpkg, mut error) = open();
    let name = CString::new("nope").expect("present");

    // SAFETY: a live container handle and a valid name.
    let layer = unsafe { gpkg_layer_open(gpkg, name.as_ptr(), &raw mut error) };
    assert!(layer.is_null());
    assert_eq!(error.code, Status::NotFound);
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // A failed open registered no child, so the close is permitted.
    // SAFETY: a live container handle with no children.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn the_layer_list_can_be_walked_by_index() {
    let (gpkg, mut error) = open();

    let mut count = 0usize;
    // SAFETY: a live container handle and a writable out-parameter.
    let status = unsafe { gpkg_layer_names_count(gpkg, &raw mut count, &raw mut error) };
    assert_eq!(status, Status::Ok);
    assert_eq!(count, 5);

    let mut names = Vec::new();
    for index in 0..count {
        // SAFETY: a live container handle and an in-range index.
        names.push(
            take_string(unsafe { gpkg_layer_name_at(gpkg, index, &raw mut error) })
                .expect("present"),
        );
    }
    assert!(names.contains(&"points".to_owned()), "{names:?}");
    assert!(names.contains(&"polygons".to_owned()), "{names:?}");

    // Past the end reports rather than reading out of bounds.
    // SAFETY: a live container handle; the index is deliberately out of range.
    let past_end = unsafe { gpkg_layer_name_at(gpkg, count, &raw mut error) };
    assert!(past_end.is_null());
    assert_eq!(error.code, Status::NotFound);
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: enumerating took no handles, so nothing blocks the close.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn freeing_a_null_layer_handle_does_nothing() {
    // SAFETY: NULL is explicitly permitted.
    unsafe { gpkg_layer_free(std::ptr::null_mut()) };
}
