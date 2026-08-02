//! `gpkg_validate`: the library's file checks, from C.
//!
//! [`gpkg_validate`] runs every check `GeoPackage::validate` runs and hands
//! back a `gpkg_findings_t` the caller owns: a plain list of findings, most
//! severe first. It borrows nothing from the container, so it outlives a
//! close harmlessly and there is nothing to release but itself, with
//! [`gpkg_findings_free`].
//!
//! ```c
//! gpkg_findings_t *findings = NULL;
//! if (gpkg_validate(gpkg, &findings, &error) != GPKG_STATUS_OK) {
//!     return fail("gpkg_validate", &error);
//! }
//! size_t count = gpkg_findings_count(findings);
//! for (size_t i = 0; i < count; i++) {
//!     char *severity = NULL, *text = NULL, *repair = NULL;
//!     gpkg_finding_at(findings, i, &severity, &text, &repair, &error);
//!     printf("[%s] %s\n", severity, text);
//!     if (repair) {
//!         printf("        repair: %s\n", repair);
//!     }
//!     gpkg_string_free(severity);
//!     gpkg_string_free(text);
//!     gpkg_string_free(repair);
//! }
//! gpkg_findings_free(findings);
//! ```
//!
//! Severities are `error` (a reader can get a wrong answer), `warning` (the
//! file is out of step with the current spec but reads correctly) and
//! `advisory` (a remark, such as an unindexed layer). A clean file returns
//! zero findings, and `gpkg validate` in the companion CLI prints exactly
//! this list.

use std::ffi::c_char;

use geopackage::Finding;

use crate::error::{Status, gpkg_error_t, set_error, set_library_error};
use crate::handle::Container;
use crate::util::write_out_strings;

/// What `gpkg_validate` found. Owns its findings outright: unlike a layer or
/// tiles handle it borrows nothing, so it does not block `gpkg_close`.
pub struct FindingsHandle {
    findings: Vec<Finding>,
}

/// The findings of one validation run. Opaque; created by `gpkg_validate` and
/// destroyed by `gpkg_findings_free`.
#[expect(
    non_camel_case_types,
    reason = "the C name is the type's name; cbindgen emits it verbatim"
)]
pub type gpkg_findings_t = FindingsHandle;

/// Validates the file, returning the findings most severe first.
///
/// Runs every check the library has: the container tables, the spatial
/// indexes and their trigger generation, the extension catalogue, and the
/// metadata, schema and relation extensions where the file has them. A
/// conforming file returns an empty findings list, and the call succeeding
/// says nothing about the file being clean: ask `gpkg_findings_count`.
///
/// # Safety
///
/// `gpkg` must be a live container handle, `out` writable, `error` NULL or
/// writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_validate(
    gpkg: *const Container,
    out: *mut *mut gpkg_findings_t,
    error: *mut gpkg_error_t,
) -> Status {
    if gpkg.is_null() || out.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "gpkg handle or out is NULL") };
        return Status::BadArgument;
    }
    // SAFETY: the caller guarantees a live container handle.
    match unsafe { (*gpkg).gpkg().validate() } {
        Ok(findings) => {
            let handle = Box::new(FindingsHandle { findings });
            // SAFETY: the caller guarantees `out` is writable.
            unsafe { *out = Box::into_raw(handle) };
            Status::Ok
        }
        Err(err) => {
            // SAFETY: forwarded to the caller's guarantee on `error`.
            unsafe { set_library_error(error, &err) };
            Status::from(&err)
        }
    }
}

/// Releases a findings handle. Passing NULL does nothing.
///
/// # Safety
///
/// `findings` must be NULL, or a handle from `gpkg_validate` that has not
/// already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_findings_free(findings: *mut gpkg_findings_t) {
    if findings.is_null() {
        return;
    }
    // SAFETY: the caller guarantees the pointer came from `Box::into_raw` in
    // `gpkg_validate` and has not been freed.
    drop(unsafe { Box::from_raw(findings) });
}

/// Returns the number of findings the run produced. Zero for a clean file, and zero for
/// NULL, so a caller need not branch.
///
/// # Safety
///
/// `findings` must be NULL or a live findings handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_findings_count(findings: *const gpkg_findings_t) -> usize {
    if findings.is_null() {
        return 0;
    }
    // SAFETY: the caller guarantees a live findings handle.
    let handle = unsafe { &*findings };
    handle.findings.len()
}

/// Returns one finding: severity, description, and repair advice where repair exists.
///
/// Findings are ordered most severe first. Every out-parameter may be NULL to
/// skip it; each string written is owned by the caller and released with
/// `gpkg_string_free`. `out_severity` is `error`, `warning` or `advisory`.
/// `out_repair` names the library call that repairs the finding, and is NULL
/// where the fix needs the producing writer or a decision about data this
/// library should not take on the caller's behalf, which is most of the
/// extension findings.
///
/// An index at or beyond the count is `GPKG_STATUS_NOT_FOUND`. On any failure
/// nothing is written.
///
/// # Safety
///
/// `findings` must be a live findings handle; every out-parameter NULL or
/// writable; `error` NULL or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_finding_at(
    findings: *const gpkg_findings_t,
    index: usize,
    out_severity: *mut *mut c_char,
    out_message: *mut *mut c_char,
    out_repair: *mut *mut c_char,
    error: *mut gpkg_error_t,
) -> Status {
    if findings.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "findings handle is NULL") };
        return Status::BadArgument;
    }
    // SAFETY: the caller guarantees a live findings handle.
    let handle = unsafe { &*findings };
    let Some(finding) = handle.findings.get(index) else {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::NotFound, "finding index out of range") };
        return Status::NotFound;
    };
    let slots = [
        (out_severity, Some(finding.severity().to_string())),
        (out_message, Some(finding.to_string())),
        (out_repair, finding.repair().map(str::to_owned)),
    ];
    // SAFETY: forwarded to this function's contract on the out-parameters.
    if unsafe { write_out_strings(&slots, error) } {
        Status::Ok
    } else {
        Status::InvalidArgument
    }
}
