//! Marshalling helpers: strings in, strings out.
//!
//! Strings are NUL-terminated UTF-8 in both directions, and ownership follows
//! the direction of travel. A string the caller passes in is borrowed for the
//! duration of the call only. A string this library returns is a fresh
//! allocation the caller owns and releases with [`gpkg_string_free`].
//!
//! ```c
//! char *name = gpkg_layer_name_at(gpkg, 0, &error);
//! if (!name) {
//!     return fail("gpkg_layer_name_at", &error);
//! }
//! gpkg_layer_t *layer = gpkg_layer_open(gpkg, name, &error);  // borrowed
//! gpkg_string_free(name);                                     // owned
//! ```
//!
//! A returning call can also fail because the text it would hand back holds an
//! interior NUL and so cannot be a C string at all. That is only reachable for
//! a name a file itself supplies, and it is reported as
//! `GPKG_STATUS_INVALID_ARGUMENT` rather than passed on truncated.

use std::ffi::{CStr, CString, c_char};

use crate::error::{Status, gpkg_error_t, set_error};

/// Borrow a C string as `&str`, reporting a NULL or non-UTF-8 argument.
///
/// `what` names the argument in the error message, so a caller passing two
/// strings can tell which one was wrong.
///
/// # Safety
///
/// `ptr`, when non-NULL, must point at a NUL-terminated string that stays valid
/// and unmodified for the returned borrow. `error` must be NULL or point at a
/// writable `gpkg_error_t`.
pub(crate) unsafe fn borrow_str<'a>(
    ptr: *const c_char,
    error: *mut gpkg_error_t,
    what: &str,
) -> Option<&'a str> {
    if ptr.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, &format!("{what} is NULL")) };
        return None;
    }
    // SAFETY: the caller guarantees a NUL-terminated string valid for the
    // returned lifetime.
    let raw = unsafe { CStr::from_ptr(ptr) };
    match raw.to_str() {
        Ok(text) => Some(text),
        Err(_) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe {
                set_error(
                    error,
                    Status::BadArgument,
                    &format!("{what} is not valid UTF-8"),
                );
            }
            None
        }
    }
}

/// Hand a Rust string to C as an owned allocation, to be released with
/// [`gpkg_string_free`].
///
/// # Safety
///
/// `error` must be NULL or point at a writable `gpkg_error_t`.
pub(crate) unsafe fn out_string(text: &str, error: *mut gpkg_error_t) -> *mut c_char {
    match CString::new(text) {
        Ok(owned) => owned.into_raw(),
        Err(_) => {
            // Only reachable for a string a file itself supplied, since nothing
            // this crate composes contains a NUL.
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe {
                set_error(
                    error,
                    Status::InvalidArgument,
                    "value contains an interior NUL and cannot be a C string",
                );
            }
            std::ptr::null_mut()
        }
    }
}

/// Release a string this library returned.
///
/// Every call that returns a `char *` returns an allocation the caller owns,
/// so each one is paired with this. Passing NULL does nothing, which is what
/// makes the pairing safe to write before the failure has been checked:
///
/// ```c
/// char *kind = gpkg_layer_kind(layer, &error);
/// if (kind) {
///     printf("%s\n", kind);
/// }
/// gpkg_string_free(kind);
/// ```
///
/// # Safety
///
/// `text` must be NULL, or a string returned by this library that has not
/// already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_string_free(text: *mut c_char) {
    if text.is_null() {
        return;
    }
    // SAFETY: the caller guarantees the pointer came from `CString::into_raw`
    // in this library and has not been freed. Retaking it frees it with the
    // allocator that made it.
    drop(unsafe { CString::from_raw(text) });
}

/// Convert and write a set of optional strings to NULL-able out-parameters.
///
/// Everything is converted before anything is written, so a failure writes
/// nothing and the caller has nothing partial to free. An absent value writes
/// NULL to its slot when the slot was requested, which `gpkg_string_free`
/// accepts harmlessly; a NULL slot is skipped.
///
/// # Safety
///
/// Every non-NULL slot must point at writable storage for a `char *`, and
/// `error` must be NULL or writable.
pub(crate) unsafe fn write_out_strings(
    slots: &[(*mut *mut c_char, Option<String>)],
    error: *mut gpkg_error_t,
) -> bool {
    let mut converted: Vec<*mut c_char> = Vec::with_capacity(slots.len());
    for (out, text) in slots {
        let wanted = match text {
            Some(text) if !out.is_null() => Some(text.as_str()),
            _ => None,
        };
        let Some(text) = wanted else {
            converted.push(std::ptr::null_mut());
            continue;
        };
        // SAFETY: forwarded to the caller's guarantee on `error`.
        let owned = unsafe { out_string(text, error) };
        if owned.is_null() {
            for earlier in converted.into_iter().filter(|p| !p.is_null()) {
                // SAFETY: `earlier` came from `CString::into_raw` inside
                // `out_string` and has not been handed to the caller.
                drop(unsafe { CString::from_raw(earlier) });
            }
            return false;
        }
        converted.push(owned);
    }
    for ((out, _), owned) in slots.iter().zip(converted) {
        if !out.is_null() {
            // SAFETY: the caller guarantees non-NULL slots are writable.
            unsafe { *(*out) = owned };
        }
    }
    true
}
