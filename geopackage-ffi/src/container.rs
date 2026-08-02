//! `gpkg_t`: opening, closing, and what a file declares.
//!
//! Three ways in and one way out. [`gpkg_open`] opens an existing file
//! read-write and validates it strictly. [`gpkg_open_read_only`] opens one
//! read-only and accepts legacy and lightly non-conforming files, recording
//! what it accepted as warnings. [`gpkg_create`] makes a new GeoPackage 1.4
//! file. All three return NULL on failure, and [`gpkg_close`] destroys the
//! handle.
//!
//! # Opening a file and reading what it declares
//!
//! ```c
//! gpkg_error_t error = {GPKG_STATUS_OK, NULL};
//!
//! gpkg_t *gpkg = gpkg_open_read_only("places.gpkg", &error);
//! if (!gpkg) {
//!     return fail("gpkg_open_read_only", &error);
//! }
//!
//! char *version = gpkg_version(gpkg, &error);
//! printf("GeoPackage %s\n", version ? version : "(unknown)");
//! gpkg_string_free(version);
//!
//! // A lenient open records what it accepted instead of rejecting the file.
//! size_t warnings = gpkg_open_warning_count(gpkg);
//! for (size_t i = 0; i < warnings; i++) {
//!     char *warning = gpkg_open_warning(gpkg, i, &error);
//!     if (warning) {
//!         printf("warning: %s\n", warning);
//!         gpkg_string_free(warning);
//!     }
//! }
//!
//! size_t layers = 0;
//! if (gpkg_layer_names_count(gpkg, &layers, &error) != GPKG_STATUS_OK) {
//!     return fail("gpkg_layer_names_count", &error);
//! }
//! for (size_t i = 0; i < layers; i++) {
//!     char *name = gpkg_layer_name_at(gpkg, i, &error);
//!     if (name) {
//!         printf("layer: %s\n", name);
//!         gpkg_string_free(name);
//!     }
//! }
//!
//! if (gpkg_close(gpkg, &error) != GPKG_STATUS_OK) {
//!     return fail("gpkg_close", &error);
//! }
//! ```
//!
//! # Transactions
//!
//! [`gpkg_begin`], [`gpkg_commit`] and [`gpkg_rollback`] put a sequence of
//! writes under one commit. Every write made through the handle while a
//! transaction is open joins it, including the calls that would otherwise open
//! one of their own, so a layer's DDL, its spatial index and its rows can land
//! together or not at all.
//!
//! ```c
//! if (gpkg_begin(gpkg, &error) != GPKG_STATUS_OK) {
//!     return fail("gpkg_begin", &error);
//! }
//! if (gpkg_add_epsg_srs(gpkg, 4326, &error) != GPKG_STATUS_OK) {
//!     gpkg_rollback(gpkg, NULL);
//!     return fail("gpkg_add_epsg_srs", &error);
//! }
//! if (gpkg_create_layer_from_arrow_schema(gpkg, "cities", &schema, true,
//!                                         &error) != GPKG_STATUS_OK) {
//!     // A call that failed part-way leaves what preceded it staged, so the
//!     // rollback is what undoes the SRS row as well.
//!     gpkg_rollback(gpkg, NULL);
//!     return fail("gpkg_create_layer_from_arrow_schema", &error);
//! }
//! if (gpkg_commit(gpkg, &error) != GPKG_STATUS_OK) {
//!     return fail("gpkg_commit", &error);
//! }
//! ```
//!
//! SQLite does not nest transactions, so each of the three fails when the
//! state is not what it needs: [`gpkg_begin`] when one is already open, and
//! [`gpkg_commit`] and [`gpkg_rollback`] when none is.
//! [`gpkg_in_transaction`] asks without provoking the error, which is what a
//! cleanup path wants:
//!
//! ```c
//! if (gpkg_in_transaction(gpkg)) {
//!     gpkg_rollback(gpkg, NULL);
//! }
//! ```
//!
//! Rolling back undoes schema changes as well as rows, because SQLite's DDL is
//! transactional: dropping a spatial index inside a transaction takes the
//! virtual table, its triggers and its `gpkg_extensions` row with it, and a
//! rollback brings all of that back.

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

/// Opens an existing GeoPackage read-write.
///
/// The file is validated strictly: one that does not identify as a GeoPackage,
/// or that is missing a core table, is rejected rather than opened.
/// `gpkg_open_read_only` is the tolerant counterpart.
///
/// Returns NULL on failure, with `error` filled in when it is non-NULL.
///
/// ```c
/// gpkg_error_t error = {GPKG_STATUS_OK, NULL};
/// gpkg_t *gpkg = gpkg_open("places.gpkg", &error);
/// if (!gpkg) {
///     fprintf(stderr, "%s\n", error.message ? error.message : "(no message)");
///     gpkg_error_clear(&error);
///     return 1;
/// }
/// // ... work with the file ...
/// gpkg_close(gpkg, &error);
/// ```
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

/// Opens an existing GeoPackage read-only, accepting legacy and lightly
/// non-conforming files.
///
/// A reader that turned away the files worth reading would not be much use, so
/// this accepts what it can and records each thing it accepted;
/// `gpkg_open_warning_count` and `gpkg_open_warning` report them. What it will
/// not accept is a file that fails to identify as a GeoPackage at all.
///
/// The handle is read-only, so a write through it fails with `GPKG_STATUS_IO`
/// and SQLite's own message.
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

/// Creates a new GeoPackage 1.4 file.
///
/// The file is seeded with the two core tables and the spatial reference
/// systems the specification requires, and nothing else: a layer is added with
/// `gpkg_create_layer_from_arrow_schema`, and any other spatial reference
/// system with `gpkg_add_epsg_srs`.
///
/// Fails with `GPKG_STATUS_ALREADY_EXISTS` when `path` is there and not empty,
/// so an existing file is never written over.
///
/// # Safety
///
/// As [`gpkg_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_create(path: *const c_char, error: *mut gpkg_error_t) -> *mut gpkg_t {
    // SAFETY: forwarded to this function's own contract.
    unsafe { open_with(path, error, How::Create) }
}

/// Closes a GeoPackage and releases its handle.
///
/// Fails with `GPKG_STATUS_HANDLE_IN_USE` while anything taken from it is
/// still alive, which means a layer handle, a tile pyramid handle, a writer or
/// an Arrow stream. In that case **the handle remains valid and open**, nothing
/// has been released, and the caller should free those children and call again.
/// On any other outcome the handle is destroyed and must not be used again,
/// including when this reports a failure: the underlying file was released
/// either way.
///
/// An open transaction is rolled back, because that is what SQLite does when a
/// connection goes. Commit before closing if the writes are to be kept.
///
/// Closing does not rewrite the file's journal mode. This ABI opens with
/// whatever mode the file already has and never asks for WAL, so a file that
/// was a single file stays one, and a file already in WAL keeps its mode.
///
/// ```c
/// gpkg_layer_free(layer);   // every child first
/// if (gpkg_close(gpkg, &error) != GPKG_STATUS_OK) {
///     return fail("gpkg_close", &error);
/// }
/// ```
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
        // The count is a single tally and does not record what each child is,
        // so the message names every destructor rather than guessing.
        let message = format!(
            "cannot close: {outstanding} handle(s) taken from this GeoPackage are still \
             alive; free them first with gpkg_layer_free, gpkg_tiles_free or \
             gpkg_writer_free, and release any Arrow stream through its own release \
             callback"
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

/// Returns the GeoPackage specification version the file declares, as `"1.0"` through
/// `"1.4"`.
///
/// This is what the file's `application_id` and `user_version` pragmas say
/// about itself, which is not a statement that its contents conform to that
/// version.
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

/// Returns the number of warnings a lenient open collected.
///
/// Always 0 for a handle from `gpkg_open` or `gpkg_create`, which are strict,
/// and for a file `gpkg_open_read_only` found nothing to warn about. Pair it
/// with `gpkg_open_warning`, which takes an index below this count.
///
/// ```c
/// size_t warnings = gpkg_open_warning_count(gpkg);
/// for (size_t i = 0; i < warnings; i++) {
///     char *warning = gpkg_open_warning(gpkg, i, &error);
///     if (warning) {
///         printf("warning: %s\n", warning);
///         gpkg_string_free(warning);
///     }
/// }
/// ```
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

/// Returns one warning from a lenient open, as text, or NULL when `index` is out of
/// range.
///
/// Each describes one thing the open accepted: a legacy `application_id`, a
/// missing `gpkg_geometry_columns` table, a catalogue name matching its table
/// only case-insensitively, or an extension the library cannot identify.
/// `gpkg_open_warning_count` bounds the index.
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

/// Runs one statement on the handle's connection, reporting through `error`.
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

/// Returns whether a transaction is open on this handle.
///
/// The way to ask before calling `gpkg_begin`, `gpkg_commit` or
/// `gpkg_rollback`, each of which fails when the state is not what it needs.
/// `false` for a NULL handle, which has no transaction either.
///
/// ```c
/// // On the way out of a function that may have failed part-way through.
/// if (gpkg_in_transaction(gpkg)) {
///     gpkg_rollback(gpkg, NULL);
/// }
/// ```
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

/// Begins a transaction, so that several writes commit or fail together.
///
/// Every write made through this handle until `gpkg_commit` or
/// `gpkg_rollback` joins this transaction, including the ones that would
/// otherwise manage their own: `gpkg_layer_write_arrow`,
/// `gpkg_layer_create_spatial_index`, `gpkg_tiles_put` and the rest. Nothing
/// they write is durable until the commit.
///
/// ```c
/// if (gpkg_begin(gpkg, &error) != GPKG_STATUS_OK) {
///     return fail("gpkg_begin", &error);
/// }
/// if (gpkg_layer_write_arrow(layer, &stream, 1000, NULL, &error) != GPKG_STATUS_OK) {
///     gpkg_rollback(gpkg, NULL);   // discard the rows that did land
///     return fail("gpkg_layer_write_arrow", &error);
/// }
/// if (gpkg_commit(gpkg, &error) != GPKG_STATUS_OK) {
///     return fail("gpkg_commit", &error);
/// }
/// ```
///
/// One consequence is worth stating: the `batch_size` argument the write calls
/// take stops bounding transactions while this is open, because every batch
/// belongs to this one. Passing a batch size is then a statement about memory
/// rather than about durability.
///
/// A deferred transaction, matching what this library opens for itself, so
/// SQLite takes the write lock at the first write rather than here. Until then
/// another connection can still write to the file.
///
/// Fails with `GPKG_STATUS_INVALID_ARGUMENT` when a transaction is already
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

/// Commits the open transaction, making everything written since `gpkg_begin`
/// durable.
///
/// Fails with `GPKG_STATUS_INVALID_ARGUMENT` when no transaction is open,
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

/// Discards everything written since `gpkg_begin`.
///
/// This is how a partly-failed sequence is undone: a write that fails part-way
/// through leaves what preceded it in the transaction, for the caller to keep
/// or discard. Schema changes go back too, since SQLite's DDL is transactional:
/// a spatial index dropped inside the transaction, along with its triggers and
/// its `gpkg_extensions` row, comes back.
///
/// Fails with `GPKG_STATUS_INVALID_ARGUMENT` when no transaction is open.
/// A cleanup path that cannot know either way should ask `gpkg_in_transaction`
/// first, or pass NULL for `error` and ignore the status.
///
/// # Safety
///
/// As `gpkg_begin`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_rollback(gpkg: *mut gpkg_t, error: *mut gpkg_error_t) -> Status {
    // SAFETY: forwarded to this function's own contract.
    unsafe { transaction_statement(gpkg, error, "gpkg_rollback", "ROLLBACK", true) }
}

/// Reads a spatial reference system's definition and identity.
///
/// Reports what the file's `gpkg_spatial_ref_sys` row records for `srs_id`,
/// which is the id `gpkg_layer_srs_id` reports. Every out-parameter may be NULL to
/// skip it; each string written is owned by the caller and released with
/// `gpkg_string_free`.
///
/// - `out_definition` is the WKT definition every GeoPackage records. The
///   spec's value for a definition that could not be produced, `undefined`,
///   is returned as the string it is.
/// - `out_definition_wkt2` is the WKT2 definition, present only in a file
///   with the CRS WKT extension and populated for this row; NULL
///   otherwise.
/// - `out_epoch` receives the coordinate epoch as a decimal year, or NaN when
///   the row records none, which is the common case.
/// - `out_organization` and `out_organization_coordsys_id` are the authority
///   and its code, such as `EPSG` and `4326`; `out_name` is the row's own
///   name.
///
/// ```c
/// int32_t srs_id = 0;
/// gpkg_layer_srs_id(layer, &srs_id, &error);
///
/// char *definition = NULL;
/// if (gpkg_srs(gpkg, srs_id, NULL, NULL, NULL, &definition, NULL, NULL,
///              &error) == GPKG_STATUS_OK) {
///     // ... hand definition to a projection library ...
///     gpkg_string_free(definition);
/// }
/// ```
///
/// An id no row declares is `GPKG_STATUS_NOT_FOUND`. On any failure nothing
/// is written to any out-parameter.
///
/// # Safety
///
/// `gpkg` must be a live container handle; every out-parameter NULL or
/// writable; `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_srs(
    gpkg: *const Container,
    srs_id: i32,
    out_name: *mut *mut c_char,
    out_organization: *mut *mut c_char,
    out_organization_coordsys_id: *mut i32,
    out_definition: *mut *mut c_char,
    out_definition_wkt2: *mut *mut c_char,
    out_epoch: *mut f64,
    error: *mut gpkg_error_t,
) -> Status {
    if gpkg.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "gpkg handle is NULL") };
        return Status::BadArgument;
    }
    // SAFETY: the caller guarantees a live container handle.
    let srs = match unsafe { (*gpkg).gpkg().srs(srs_id) } {
        Ok(Some(srs)) => srs,
        Ok(None) => {
            let message = format!("no spatial reference system with srs_id {srs_id}");
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_error(error, Status::NotFound, &message) };
            return Status::NotFound;
        }
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            return Status::from(&err);
        }
    };

    // Every string is converted before anything is written, so a failure
    // writes nothing and the caller has nothing partial to free.
    let mut strings = [
        (out_name, Some(srs.name)),
        (out_organization, Some(srs.organization)),
        (out_definition, Some(srs.definition)),
        (out_definition_wkt2, srs.definition_wkt2),
    ]
    .map(|(out, text)| (out, text, std::ptr::null_mut::<c_char>()));
    for (out, text, converted) in &mut strings {
        if let (false, Some(text)) = (out.is_null(), text.as_deref()) {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            let owned = unsafe { out_string(text, error) };
            if owned.is_null() {
                for (_, _, earlier) in &strings {
                    if !earlier.is_null() {
                        // SAFETY: `earlier` came from `CString::into_raw` in
                        // `out_string` above and has not been handed out.
                        drop(unsafe { std::ffi::CString::from_raw(*earlier) });
                    }
                }
                return Status::InvalidArgument;
            }
            *converted = owned;
        }
    }
    for (out, _, converted) in &strings {
        if !out.is_null() {
            // SAFETY: the caller guarantees non-NULL out-parameters are
            // writable. An absent optional writes NULL, which the caller may
            // free harmlessly.
            unsafe { *(*out) = *converted };
        }
    }
    if !out_organization_coordsys_id.is_null() {
        // SAFETY: the caller guarantees a writable out-parameter.
        unsafe { *out_organization_coordsys_id = srs.organization_coordsys_id };
    }
    if !out_epoch.is_null() {
        // SAFETY: as above.
        unsafe { *out_epoch = srs.epoch.unwrap_or(f64::NAN) };
    }
    Status::Ok
}
