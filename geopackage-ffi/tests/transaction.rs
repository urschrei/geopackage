//! `gpkg_begin`, `gpkg_commit` and `gpkg_rollback`.
//!
//! These were not offered before the write paths learned to inherit an open
//! transaction, because a C consumer who called `gpkg_begin` and then wrote
//! anything would have got "cannot start a transaction within a transaction"
//! from the write rather than from the begin. So the tests that matter are the
//! ones that write inside the pair, not the ones that only check the state
//! flips.
//!
//! Each writes through the C API, finishes the transaction, closes the handle,
//! and reopens the file through the Rust API to see what actually landed on
//! disk. Asking the same handle would not distinguish staged from durable.
//!
//! One `unsafe` block per call, as the rest of this crate's tests do: the
//! crate sets `multiple_unsafe_ops_per_block`, so a block covering a whole
//! sequence would hide which call each SAFETY comment is about.

use std::ffi::{CStr, CString};
use std::path::Path;

use geopackage_ffi::{
    Status, gpkg_add_epsg_srs, gpkg_begin, gpkg_close, gpkg_commit, gpkg_create, gpkg_error_clear,
    gpkg_error_t, gpkg_in_transaction, gpkg_rollback, gpkg_t,
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

/// A fresh file and a handle on it.
fn created(path: &Path) -> (*mut gpkg_t, gpkg_error_t) {
    let c_path = CString::new(path.to_str().expect("temp path is UTF-8")).expect("no interior NUL");
    let mut error = error_slot();
    // SAFETY: a valid path and a writable error slot.
    let gpkg = unsafe { gpkg_create(c_path.as_ptr(), &raw mut error) };
    assert!(!gpkg.is_null(), "{:?}", message(&error));
    (gpkg, error)
}

/// Register EPSG:3857, the write each test stages.
fn add_3857(gpkg: *mut gpkg_t, error: &mut gpkg_error_t) -> Status {
    // SAFETY: a live container handle and a writable error slot.
    unsafe { gpkg_add_epsg_srs(gpkg.cast(), 3857, &raw mut *error) }
}

/// Whether the file on disk carries EPSG:3857, read through the Rust API after
/// the C handle has been closed.
fn has_3857(path: &Path) -> bool {
    let gpkg = geopackage::GeoPackage::open(path).expect("reopen");
    gpkg.srs(3857).expect("srs lookup").is_some()
}

#[test]
fn a_committed_transaction_keeps_what_it_wrote() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("c.gpkg");
    let (gpkg, mut error) = created(&path);

    // SAFETY: a live handle.
    assert!(!unsafe { gpkg_in_transaction(gpkg) });
    // SAFETY: a live handle and a writable error slot.
    assert_eq!(unsafe { gpkg_begin(gpkg, &raw mut error) }, Status::Ok);
    // SAFETY: a live handle.
    assert!(unsafe { gpkg_in_transaction(gpkg) });
    assert_eq!(add_3857(gpkg, &mut error), Status::Ok);
    // SAFETY: a live handle and a writable error slot.
    assert_eq!(unsafe { gpkg_commit(gpkg, &raw mut error) }, Status::Ok);
    // SAFETY: a live handle.
    assert!(!unsafe { gpkg_in_transaction(gpkg) });
    // SAFETY: a live handle, closed exactly once.
    assert_eq!(unsafe { gpkg_close(gpkg, &raw mut error) }, Status::Ok);

    assert!(has_3857(&path));
}

/// The case the pair exists for: a write inside a transaction is undone by
/// rolling back, which is what a C consumer could not do at all before.
#[test]
fn a_rolled_back_transaction_discards_what_it_wrote() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("r.gpkg");
    let (gpkg, mut error) = created(&path);

    // SAFETY: a live handle and a writable error slot.
    assert_eq!(unsafe { gpkg_begin(gpkg, &raw mut error) }, Status::Ok);
    assert_eq!(add_3857(gpkg, &mut error), Status::Ok);
    // SAFETY: a live handle and a writable error slot.
    assert_eq!(unsafe { gpkg_rollback(gpkg, &raw mut error) }, Status::Ok);
    // SAFETY: a live handle.
    assert!(!unsafe { gpkg_in_transaction(gpkg) });
    // SAFETY: a live handle, closed exactly once.
    assert_eq!(unsafe { gpkg_close(gpkg, &raw mut error) }, Status::Ok);

    assert!(
        !has_3857(&path),
        "the row survived a rollback, so the write committed on its own"
    );
}

/// SQLite does not nest, so the second begin is refused rather than attempted.
#[test]
fn a_second_begin_is_refused_and_leaves_the_first_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("n.gpkg");
    let (gpkg, mut error) = created(&path);

    // SAFETY: a live handle and a writable error slot.
    assert_eq!(unsafe { gpkg_begin(gpkg, &raw mut error) }, Status::Ok);
    // SAFETY: as above; the refusal is the case under test.
    let nested = unsafe { gpkg_begin(gpkg, &raw mut error) };
    assert_eq!(nested, Status::InvalidArgument);
    let text = message(&error).expect("a refusal carries a message");
    assert!(text.contains("already open"), "unexpected message: {text}");
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // The first transaction is untouched by the refusal.
    // SAFETY: a live handle.
    assert!(unsafe { gpkg_in_transaction(gpkg) });
    // SAFETY: a live handle and a writable error slot.
    assert_eq!(unsafe { gpkg_rollback(gpkg, &raw mut error) }, Status::Ok);
    // SAFETY: a live handle, closed exactly once.
    assert_eq!(unsafe { gpkg_close(gpkg, &raw mut error) }, Status::Ok);
}

/// An unbalanced commit or rollback is reported where it happens rather than
/// succeeding silently.
#[test]
fn finishing_a_transaction_that_was_never_begun_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("u.gpkg");
    let (gpkg, mut error) = created(&path);

    // SAFETY: a live handle and a writable error slot.
    let committed = unsafe { gpkg_commit(gpkg, &raw mut error) };
    assert_eq!(committed, Status::InvalidArgument);
    let text = message(&error).expect("a refusal carries a message");
    assert!(
        text.contains("no transaction is open"),
        "unexpected message: {text}"
    );
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: a live handle and a writable error slot.
    let rolled_back = unsafe { gpkg_rollback(gpkg, &raw mut error) };
    assert_eq!(rolled_back, Status::InvalidArgument);
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: a live handle, closed exactly once.
    assert_eq!(unsafe { gpkg_close(gpkg, &raw mut error) }, Status::Ok);
}

/// Closing with a transaction still open discards it, which is what SQLite does
/// when the connection goes, and what the documentation promises.
#[test]
fn closing_with_a_transaction_open_discards_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("d.gpkg");
    let (gpkg, mut error) = created(&path);

    // SAFETY: a live handle and a writable error slot.
    assert_eq!(unsafe { gpkg_begin(gpkg, &raw mut error) }, Status::Ok);
    assert_eq!(add_3857(gpkg, &mut error), Status::Ok);
    // SAFETY: a live handle, closed exactly once, with a transaction open.
    assert_eq!(unsafe { gpkg_close(gpkg, &raw mut error) }, Status::Ok);

    assert!(!has_3857(&path));
}

#[test]
fn null_handles_are_rejected_rather_than_dereferenced() {
    let mut error = error_slot();
    // SAFETY: NULL is the case under test.
    assert!(!unsafe { gpkg_in_transaction(std::ptr::null()) });

    // SAFETY: NULL is the case under test, and the error slot is writable.
    let begun = unsafe { gpkg_begin(std::ptr::null_mut(), &raw mut error) };
    assert_eq!(begun, Status::BadArgument);
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: as above.
    let committed = unsafe { gpkg_commit(std::ptr::null_mut(), &raw mut error) };
    assert_eq!(committed, Status::BadArgument);
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: as above.
    let rolled_back = unsafe { gpkg_rollback(std::ptr::null_mut(), &raw mut error) };
    assert_eq!(rolled_back, Status::BadArgument);
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // And NULL is permitted for the error slot itself.
    // SAFETY: both arguments NULL, which the contract allows.
    let no_slot = unsafe { gpkg_begin(std::ptr::null_mut(), std::ptr::null_mut()) };
    assert_eq!(no_slot, Status::BadArgument);
}
