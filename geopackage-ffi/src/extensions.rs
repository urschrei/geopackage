//! The `gpkg_extensions` catalogue: what a file registers, and whether this
//! library supports it.
//!
//! The catalogue exists so a client can "fail fast": ask what a file registers
//! before working on it, instead of learning mid-write that a table is under
//! an extension this library cannot honour (which fails as
//! `GPKG_STATUS_UNSUPPORTED` at the write). This pair is the asking. [`gpkg_extensions_count`] sizes the catalogue and
//! [`gpkg_extension_at`] reads one row, with the support level this library
//! claims for it.
//!
//! ```c
//! size_t count = 0;
//! if (gpkg_extensions_count(gpkg, &count, &error) != GPKG_STATUS_OK) {
//!     return fail("gpkg_extensions_count", &error);
//! }
//! for (size_t i = 0; i < count; i++) {
//!     char *name = NULL, *table = NULL, *support = NULL;
//!     if (gpkg_extension_at(gpkg, i, &name, &table, NULL, NULL, &support,
//!                           &error) != GPKG_STATUS_OK) {
//!         return fail("gpkg_extension_at", &error);
//!     }
//!     printf("%s on %s: %s\n", name, table ? table : "(whole file)", support);
//!     gpkg_string_free(name);
//!     gpkg_string_free(table);
//!     gpkg_string_free(support);
//! }
//! ```
//!
//! The support level is one of `implemented`, `known, not read or written`,
//! `removed from the standard in 2016`, and `unrecognised`. A file whose
//! every row reports `implemented` contains nothing this library will reject;
//! a row reporting `unrecognised` names a table this library will not write
//! unless the open opted out of that protection.

use std::ffi::c_char;

use crate::error::{Status, gpkg_error_t, set_error, set_library_error};
use crate::handle::Container;
use crate::util::write_out_strings;

/// Returns the number of `gpkg_extensions` rows in the file.
///
/// Zero for a file with no `gpkg_extensions` table at all, which is a file
/// registering no extensions. `gpkg_extension_at` walks the same list, ordered
/// by extension name, then table, then column.
///
/// # Safety
///
/// `gpkg` must be a live container handle, `out` writable, `error` NULL or
/// writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_extensions_count(
    gpkg: *const Container,
    out: *mut usize,
    error: *mut gpkg_error_t,
) -> Status {
    if gpkg.is_null() || out.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "gpkg handle or out is NULL") };
        return Status::BadArgument;
    }
    // SAFETY: the caller guarantees a live container handle.
    match unsafe { (*gpkg).gpkg().extensions() } {
        Ok(rows) => {
            // SAFETY: the caller guarantees `out` is writable.
            unsafe { *out = rows.len() };
            Status::Ok
        }
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            Status::from(&err)
        }
    }
}

/// Returns one `gpkg_extensions` row, with the support level this library claims.
///
/// The list is the one `gpkg_extensions_count` counts. Every out-parameter
/// may be NULL to skip it; each string written is owned by the caller and
/// released with `gpkg_string_free`. `out_table` and `out_column` are NULL
/// when the row applies to the whole file or the whole table, which is what
/// their NULLs mean in the catalogue itself. `out_scope` is the row's
/// declared scope, `read-write` or `write-only`; `out_support` is this
/// library's support level for the extension, in the module documentation's
/// vocabulary.
///
/// An index at or beyond the count is `GPKG_STATUS_NOT_FOUND` rather than a
/// failure to read. On any failure nothing is written.
///
/// # Safety
///
/// `gpkg` must be a live container handle; every out-parameter NULL or
/// writable; `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_extension_at(
    gpkg: *const Container,
    index: usize,
    out_name: *mut *mut c_char,
    out_table: *mut *mut c_char,
    out_column: *mut *mut c_char,
    out_scope: *mut *mut c_char,
    out_support: *mut *mut c_char,
    error: *mut gpkg_error_t,
) -> Status {
    if gpkg.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "gpkg handle is NULL") };
        return Status::BadArgument;
    }
    // SAFETY: the caller guarantees a live container handle.
    let rows = match unsafe { (*gpkg).gpkg().extensions() } {
        Ok(rows) => rows,
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            return Status::from(&err);
        }
    };
    let Some(row) = rows.into_iter().nth(index) else {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::NotFound, "extension index out of range") };
        return Status::NotFound;
    };
    let support = row.support().to_string();
    let scope = row.scope.to_string();
    let slots = [
        (out_name, Some(row.name)),
        (out_table, row.table_name),
        (out_column, row.column_name),
        (out_scope, Some(scope)),
        (out_support, Some(support)),
    ];
    // SAFETY: forwarded to this function's contract on the out-parameters.
    if unsafe { write_out_strings(&slots, error) } {
        Status::Ok
    } else {
        Status::InvalidArgument
    }
}
