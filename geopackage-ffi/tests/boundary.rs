//! The parts of the boundary miri can actually check.
//!
//! Miri cannot execute native code, and SQLite here is bundled and built from
//! source, so anything that opens a database is out of its reach: it stops at
//! `can't call foreign function sqlite3_threadsafe`. What is left, and what
//! this file contains, is every path that validates an argument or moves a
//! string across the boundary without touching a file. That is a narrow slice,
//! but it is the slice where the pointer handling lives, so it is worth gating.
//!
//! The rest of the unsafe, and in particular the lifetime erasure in
//! `handle.rs`, is checked by AddressSanitizer over `tests/layer.rs` instead,
//! which does run native code. Neither tool covers the other's ground.
//!
//! Run with: `cargo +nightly miri test -p geopackage-ffi --test boundary`.

use std::ffi::{CStr, CString};

use geopackage_ffi::{
    Status, gpkg_close, gpkg_error_clear, gpkg_error_t, gpkg_layer_free, gpkg_layer_name,
    gpkg_layer_open, gpkg_open, gpkg_string_free, gpkg_version,
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

#[test]
fn a_null_path_is_rejected_before_anything_is_dereferenced() {
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
fn a_non_utf8_path_is_rejected_before_anything_is_opened() {
    // Lone continuation byte: a valid C string, not valid UTF-8.
    let raw = [0xFFu8, 0x00];
    let mut error = error_slot();
    // SAFETY: `raw` is NUL-terminated and outlives the call.
    let gpkg = unsafe { gpkg_open(raw.as_ptr().cast(), &raw mut error) };
    assert!(gpkg.is_null());
    assert_eq!(error.code, Status::BadArgument);
    assert!(
        message(&error)
            .expect("present")
            .contains("not valid UTF-8")
    );
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };
}

#[test]
fn null_handles_are_rejected_rather_than_dereferenced() {
    let mut error = error_slot();

    // SAFETY: NULL is the case under test; the function checks before reading.
    let status = unsafe { gpkg_close(std::ptr::null_mut(), &raw mut error) };
    assert_eq!(status, Status::BadArgument);
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: as above.
    let version = unsafe { gpkg_version(std::ptr::null(), &raw mut error) };
    assert!(version.is_null());
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    let name = CString::new("anything").expect("present");
    // SAFETY: a NULL container handle with an otherwise valid name.
    let layer = unsafe { gpkg_layer_open(std::ptr::null(), name.as_ptr(), &raw mut error) };
    assert!(layer.is_null());
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: a NULL layer handle.
    let layer_name = unsafe { gpkg_layer_name(std::ptr::null(), &raw mut error) };
    assert!(layer_name.is_null());
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };
}

#[test]
fn a_null_error_slot_is_accepted_everywhere() {
    // SAFETY: NULL is explicitly permitted for the error out-parameter.
    let opened = unsafe { gpkg_open(std::ptr::null(), std::ptr::null_mut()) };
    assert!(opened.is_null());

    // SAFETY: as above.
    let closed = unsafe { gpkg_close(std::ptr::null_mut(), std::ptr::null_mut()) };
    assert_eq!(closed, Status::BadArgument);

    // SAFETY: as above.
    let version = unsafe { gpkg_version(std::ptr::null(), std::ptr::null_mut()) };
    assert!(version.is_null());
}

#[test]
fn clearing_an_error_frees_its_message_and_is_idempotent() {
    let mut error = error_slot();
    // SAFETY: NULL path, which fills the slot with a message.
    unsafe { gpkg_open(std::ptr::null(), &raw mut error) };
    assert!(!error.message.is_null());

    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };
    assert!(error.message.is_null());
    assert_eq!(error.code, Status::Ok);

    // Documented as safe to repeat: the pointer is cleared as it is freed, so
    // a second clear has nothing to free rather than a double free.
    // SAFETY: clearing an already-cleared slot.
    unsafe { gpkg_error_clear(&raw mut error) };
    // SAFETY: clearing a NULL slot.
    unsafe { gpkg_error_clear(std::ptr::null_mut()) };
}

#[test]
fn freeing_null_is_a_no_op_for_every_destructor() {
    // SAFETY: NULL is explicitly permitted.
    unsafe { gpkg_string_free(std::ptr::null_mut()) };
    // SAFETY: NULL is explicitly permitted.
    unsafe { gpkg_layer_free(std::ptr::null_mut()) };
    // SAFETY: NULL is explicitly permitted.
    unsafe { gpkg_error_clear(std::ptr::null_mut()) };
}

#[test]
fn an_error_message_survives_being_read_after_the_call_returns() {
    // The message is owned by the caller, not borrowed from the library, so
    // reading it long after the call is not a use-after-free.
    let mut error = error_slot();
    // SAFETY: NULL path, which fills the slot with a message.
    unsafe { gpkg_open(std::ptr::null(), &raw mut error) };

    let mut seen = Vec::new();
    for _ in 0..10 {
        seen.push(message(&error).expect("present"));
    }
    assert!(seen.iter().all(|text| text.contains("NULL")));

    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };
}
