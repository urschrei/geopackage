//! `gpkg_t`: opening, closing, and what a file declares.

use std::ffi::c_char;

use geopackage::{GeoPackage, OpenOptions};

use crate::error::{Status, gpkg_error_t, set_error, set_library_error};
use crate::handle::Container;
use crate::util::{borrow_str, out_string};

/// An open GeoPackage. Opaque; created by `gpkg_open`, `gpkg_open_read_only` or
/// `gpkg_create`, and destroyed by `gpkg_close`.
#[expect(
    non_camel_case_types,
    reason = "the C name is the type's name; cbindgen emits it verbatim"
)]
pub type gpkg_t = Container;

/// How a container was asked to be opened.
enum How {
    Open,
    ReadOnly,
    Create,
}

/// The shared body of the three entry points below.
///
/// # Safety
///
/// As the entry points: `path` must be a valid NUL-terminated UTF-8 string, and
/// `error` must be NULL or point at a writable `gpkg_error_t`.
unsafe fn open_with(path: *const c_char, error: *mut gpkg_error_t, how: How) -> *mut gpkg_t {
    // SAFETY: forwarded to the caller's guarantee on `path`.
    let Some(path) = (unsafe { borrow_str(path, error, "path") }) else {
        return std::ptr::null_mut();
    };

    let opened = match how {
        How::Open => GeoPackage::open(path),
        // Lenient as well as read-only: a reader that turns away the files
        // worth reading is not much of a reader, and the warnings it collects
        // are reachable through `gpkg_open_warning_count`.
        How::ReadOnly => GeoPackage::open_read_only_lenient(path),
        How::Create => OpenOptions::new().create(path),
    };

    match opened {
        Ok(gpkg) => Box::into_raw(Box::new(Container::new(gpkg))),
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            std::ptr::null_mut()
        }
    }
}

/// Open an existing GeoPackage read-write.
///
/// Returns NULL on failure, with `error` filled in when it is non-NULL.
///
/// # Safety
///
/// `path` must be a valid pointer to a NUL-terminated UTF-8 string. `error`
/// must be NULL or point at a writable `gpkg_error_t`. The returned handle must
/// be destroyed with `gpkg_close`, on the thread that created it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_open(path: *const c_char, error: *mut gpkg_error_t) -> *mut gpkg_t {
    // SAFETY: forwarded to this function's own contract.
    unsafe { open_with(path, error, How::Open) }
}

/// Open an existing GeoPackage read-only, tolerating legacy and lightly
/// non-conforming files.
///
/// # Safety
///
/// As [`gpkg_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_open_read_only(
    path: *const c_char,
    error: *mut gpkg_error_t,
) -> *mut gpkg_t {
    // SAFETY: forwarded to this function's own contract.
    unsafe { open_with(path, error, How::ReadOnly) }
}

/// Create a new GeoPackage 1.4 file.
///
/// # Safety
///
/// As [`gpkg_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_create(path: *const c_char, error: *mut gpkg_error_t) -> *mut gpkg_t {
    // SAFETY: forwarded to this function's own contract.
    unsafe { open_with(path, error, How::Create) }
}

/// Close a GeoPackage and release its handle.
///
/// Refuses with `GPKG_STATUS_HANDLE_IN_USE` while any layer handle taken from
/// it is still alive, in which case **the handle remains valid and open** and
/// the caller should free its layer handles first. On any other outcome the
/// handle is destroyed and must not be used again, including when this reports
/// a failure: the underlying file was released either way.
///
/// For a handle that opted into WAL this checkpoints the WAL and resets the
/// journal mode, so the file is left as a single file with no sidecars.
///
/// # Safety
///
/// `gpkg` must be a handle from one of the open functions that has not already
/// been closed. `error` must be NULL or point at a writable `gpkg_error_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_close(gpkg: *mut gpkg_t, error: *mut gpkg_error_t) -> Status {
    if gpkg.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "gpkg_close: handle is NULL") };
        return Status::BadArgument;
    }

    // Checked through a borrow, before anything is consumed, so a refusal
    // leaves the handle exactly as it was.
    // SAFETY: the caller guarantees a live handle from an open function, which
    // was produced by `Box::into_raw` and not yet freed.
    let outstanding = unsafe { (*gpkg).outstanding_children() };
    if outstanding != 0 {
        let message = format!(
            "cannot close: {outstanding} layer handle(s) still borrow this GeoPackage; \
             free them with gpkg_layer_free first"
        );
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::HandleInUse, &message) };
        return Status::HandleInUse;
    }

    // SAFETY: as above, and now nothing borrows it, so taking ownership back
    // from the raw pointer is sound. The caller must not use `gpkg` again,
    // which this function's contract states.
    let container = unsafe { Box::from_raw(gpkg) };
    match container.close() {
        Ok(()) => Status::Ok,
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            Status::from(&err)
        }
    }
}

/// The GeoPackage specification version the file declares, as `"1.0"` through
/// `"1.4"`.
///
/// The returned string is owned by the caller and must be released with
/// `gpkg_string_free`. Returns NULL on failure.
///
/// # Safety
///
/// `gpkg` must be a live handle. `error` must be NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_version(
    gpkg: *const gpkg_t,
    error: *mut gpkg_error_t,
) -> *mut c_char {
    if gpkg.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "gpkg_version: handle is NULL") };
        return std::ptr::null_mut();
    }
    // SAFETY: the caller guarantees a live handle.
    let version = unsafe { (*gpkg).gpkg().version() };
    // SAFETY: forwarded to the caller's guarantee on `error`.
    unsafe { out_string(version.as_str(), error) }
}

/// How many warnings a lenient open collected.
///
/// Always 0 for a handle from `gpkg_open` or `gpkg_create`, which are strict.
///
/// # Safety
///
/// `gpkg` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_open_warning_count(gpkg: *const gpkg_t) -> usize {
    if gpkg.is_null() {
        return 0;
    }
    // SAFETY: the caller guarantees a live handle.
    unsafe { (*gpkg).gpkg().open_warnings().len() }
}

/// One warning from a lenient open, as text, or NULL when `index` is out of
/// range.
///
/// The returned string is owned by the caller and must be released with
/// `gpkg_string_free`.
///
/// # Safety
///
/// `gpkg` must be a live handle. `error` must be NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_open_warning(
    gpkg: *const gpkg_t,
    index: usize,
    error: *mut gpkg_error_t,
) -> *mut c_char {
    if gpkg.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe {
            set_error(
                error,
                Status::BadArgument,
                "gpkg_open_warning: handle is NULL",
            );
        }
        return std::ptr::null_mut();
    }
    // SAFETY: the caller guarantees a live handle.
    let warnings = unsafe { (*gpkg).gpkg().open_warnings() };
    let Some(warning) = warnings.get(index) else {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe {
            set_error(
                error,
                Status::NotFound,
                "gpkg_open_warning: index out of range",
            )
        };
        return std::ptr::null_mut();
    };
    // SAFETY: forwarded to the caller's guarantee on `error`.
    unsafe { out_string(&warning.to_string(), error) }
}

/// Run one statement on the handle's connection, reporting through `error`.
///
/// The three transaction calls differ only in their statement, the state they
/// require, and the message they give when that state is wrong.
///
/// # Safety
///
/// `gpkg` must be a live handle and `error` NULL or writable.
unsafe fn transaction_statement(
    gpkg: *mut gpkg_t,
    error: *mut gpkg_error_t,
    name: &str,
    sql: &str,
    want_open: bool,
) -> Status {
    if gpkg.is_null() {
        let message = format!("{name}: handle is NULL");
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, &message) };
        return Status::BadArgument;
    }
    // SAFETY: the caller guarantees a live handle.
    let conn = unsafe { (*gpkg).gpkg().connection() };
    if conn.is_autocommit() == want_open {
        let message = if want_open {
            format!("{name}: no transaction is open")
        } else {
            format!("{name}: a transaction is already open; SQLite does not nest them")
        };
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::InvalidArgument, &message) };
        return Status::InvalidArgument;
    }
    match conn.execute_batch(sql) {
        Ok(()) => Status::Ok,
        Err(source) => {
            let err = geopackage::Error::from(source);
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            Status::from(&err)
        }
    }
}

/// Whether a transaction is open on this handle.
///
/// The way to ask before calling `gpkg_begin`, `gpkg_commit` or
/// `gpkg_rollback`, each of which refuses when the state is not what it needs.
/// `false` for a NULL handle, which has no transaction either.
///
/// # Safety
///
/// `gpkg` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_in_transaction(gpkg: *const gpkg_t) -> bool {
    if gpkg.is_null() {
        return false;
    }
    // SAFETY: the caller guarantees a live handle.
    !unsafe { (*gpkg).gpkg().connection() }.is_autocommit()
}

/// Begin a transaction, so that several writes commit or fail together.
///
/// Every write made through this handle until `gpkg_commit` or
/// `gpkg_rollback` joins this transaction, including the ones that would
/// otherwise manage their own: `gpkg_layer_write_arrow`,
/// `gpkg_layer_create_spatial_index`, `gpkg_tiles_put` and the rest. Nothing
/// they write is durable until the commit.
///
/// One consequence is worth stating: the `batch_size` argument the write calls
/// take stops bounding transactions while this is open, because every batch
/// belongs to this one. Passing a batch size is then a statement about memory
/// rather than about durability.
///
/// A deferred transaction, matching what this library opens for itself, so
/// SQLite takes the write lock at the first write rather than here.
///
/// Refuses with `GPKG_STATUS_INVALID_ARGUMENT` when a transaction is already
/// open, since SQLite does not nest them. `gpkg_in_transaction` asks without
/// provoking the error.
///
/// Closing a handle with a transaction still open rolls it back, because that
/// is what SQLite does when the connection goes.
///
/// # Safety
///
/// `gpkg` must be a live handle from one of the open functions. `error` must be
/// NULL or point at a writable `gpkg_error_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_begin(gpkg: *mut gpkg_t, error: *mut gpkg_error_t) -> Status {
    // SAFETY: forwarded to this function's own contract.
    unsafe { transaction_statement(gpkg, error, "gpkg_begin", "BEGIN", false) }
}

/// Commit the open transaction, making everything written since `gpkg_begin`
/// durable.
///
/// Refuses with `GPKG_STATUS_INVALID_ARGUMENT` when no transaction is open,
/// rather than succeeding silently, so an unbalanced pair is reported where it
/// happens.
///
/// # Safety
///
/// As `gpkg_begin`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_commit(gpkg: *mut gpkg_t, error: *mut gpkg_error_t) -> Status {
    // SAFETY: forwarded to this function's own contract.
    unsafe { transaction_statement(gpkg, error, "gpkg_commit", "COMMIT", true) }
}

/// Discard everything written since `gpkg_begin`.
///
/// This is how a partly-failed sequence is undone: a write that fails part-way
/// through leaves what preceded it in the transaction, for the caller to keep
/// or discard.
///
/// Refuses with `GPKG_STATUS_INVALID_ARGUMENT` when no transaction is open.
///
/// # Safety
///
/// As `gpkg_begin`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_rollback(gpkg: *mut gpkg_t, error: *mut gpkg_error_t) -> Status {
    // SAFETY: forwarded to this function's own contract.
    unsafe { transaction_statement(gpkg, error, "gpkg_rollback", "ROLLBACK", true) }
}
