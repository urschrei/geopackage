//! `gpkg_status_t` and `gpkg_error_t`: how a failure crosses the boundary.
//!
//! Every fallible entry point takes an optional `gpkg_error_t *` out-parameter.
//! On failure it is filled in with a code and an owned, NUL-terminated UTF-8
//! message, which the caller releases with [`gpkg_error_clear`]. Passing NULL
//! is allowed and means the caller wants only the return value.
//!
//! # Why the codes are categories
//!
//! `geopackage::Error` has 46 variants and will grow. Mapping each to its own C
//! constant would put a header-breaking change behind every new variant, so the
//! codes here are a small closed set of categories a caller can branch on, and
//! the message carries the detail. A new library variant classifies into an
//! existing category or into [`Status::Other`]; neither changes the header.

use std::ffi::{CString, c_char};

use geopackage::Error;

/// What kind of failure occurred.
///
/// `#[repr(i32)]` so cbindgen emits a plain C enum with stable values. Values
/// are assigned explicitly and must never be reused for another meaning.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// No error.
    Ok = 0,
    /// The file could not be read or written: an I/O or SQLite failure.
    Io = 1,
    /// The file is not a GeoPackage, or not one this library can identify.
    NotAGeoPackage = 2,
    /// A named table, layer, column, SRS or record is not there.
    NotFound = 3,
    /// Something the call would create is already there.
    AlreadyExists = 4,
    /// An argument was rejected: a bad value, a type mismatch, a malformed
    /// geometry, or a count that does not line up.
    InvalidArgument = 5,
    /// The file uses something this library does not implement.
    Unsupported = 6,
    /// A constraint the file or the schema declares was violated.
    Constraint = 7,
    /// A handle is still borrowing what the call would destroy. Free the child
    /// handles first; nothing was changed.
    HandleInUse = 8,
    /// A string argument was not valid UTF-8, or a required pointer was NULL.
    BadArgument = 9,
    /// Anything else. The message says what happened.
    Other = 99,
}

impl From<&Error> for Status {
    fn from(error: &Error) -> Self {
        match error {
            Error::Sqlite(_) | Error::ExtentPersist { .. } => Self::Io,
            Error::NotAGeoPackage { .. } => Self::NotAGeoPackage,
            Error::NoSuchTable { .. }
            | Error::NoSuchColumn { .. }
            | Error::NoSuchLayer { .. }
            | Error::UnknownSrs { .. }
            | Error::NoSuchMetadata { .. }
            | Error::UnknownEpsgCode { .. }
            | Error::UnknownZoomLevel { .. }
            | Error::NoSpatialIndex { .. }
            | Error::NoTileMatrixSet { .. }
            | Error::NoGeometryColumn { .. }
            | Error::NoPrimaryKey { .. } => Self::NotFound,
            Error::AlreadyExists(_)
            | Error::TableAlreadyExists { .. }
            | Error::SpatialIndexExists { .. } => Self::AlreadyExists,
            Error::UnsupportedExtension { .. }
            | Error::UnsupportedArrowType { .. }
            | Error::GeometryValueUnsupported { .. }
            | Error::ZoomOtherNotEnabled { .. }
            | Error::TileFormatNotAllowed { .. } => Self::Unsupported,
            Error::ColumnConstraintViolation { .. }
            | Error::ZmViolation { .. }
            | Error::GeometryTypeMismatch { .. }
            | Error::NonConformantRelationName { .. }
            | Error::ReservedTablePrefix { .. }
            | Error::MetadataCycle { .. }
            | Error::SelfParentedMetadata { .. } => Self::Constraint,
            Error::ValueTypeMismatch { .. }
            | Error::ArrowValueMismatch { .. }
            | Error::NonBooleanInteger { .. }
            | Error::InvalidDateTimeValue { .. }
            | Error::InvalidZmFlag { .. }
            | Error::InvalidColumnConstraint { .. }
            | Error::UnknownGeometryType { .. }
            | Error::UnknownReferenceScope { .. }
            | Error::ValueCountMismatch { .. }
            | Error::DuplicateUpdateColumn { .. }
            | Error::MissingGeometrySpec { .. }
            | Error::UnexpectedGeometrySpec { .. }
            | Error::WrongDataType { .. }
            | Error::GeometryNotProjected => Self::InvalidArgument,
            // `Error` is not `#[non_exhaustive]`, but it grows every milestone,
            // and a new variant should classify rather than fail to compile a
            // downstream crate. Anything unclassified is `Other`, whose message
            // still says exactly what happened.
            _ => Self::Other,
        }
    }
}

/// A failure, as C sees it.
///
/// `code` is [`Status::Ok`] and `message` NULL when nothing went wrong. A
/// non-NULL `message` is an owned NUL-terminated UTF-8 string that the caller
/// must release with [`gpkg_error_clear`].
#[repr(C)]
pub struct gpkg_error_t {
    /// What kind of failure occurred.
    pub code: Status,
    /// A human-readable description, or NULL. Owned by the caller once set.
    pub message: *mut c_char,
}

/// Fill an error out-parameter, if the caller supplied one.
///
/// # Safety
///
/// `slot`, when non-NULL, must point at a writable `gpkg_error_t` that does not
/// already hold a message. Every entry point in this crate satisfies that by
/// only writing into a slot it has not written before during the call.
pub(crate) unsafe fn set_error(slot: *mut gpkg_error_t, code: Status, message: &str) {
    if slot.is_null() {
        return;
    }
    // A message with an interior NUL cannot be a C string. That can only come
    // from a table or column name a file itself contains, so it is reported as
    // a message rather than dropped.
    let owned = CString::new(message).unwrap_or_else(|_| {
        CString::new("error message contained an interior NUL").unwrap_or_default()
    });
    // SAFETY: the caller guarantees `slot` points at a writable `gpkg_error_t`,
    // so taking a mutable reference to it is sound. One reference rather than
    // two raw dereferences, so the block does a single unsafe operation.
    let slot = unsafe { &mut *slot };
    slot.code = code;
    slot.message = owned.into_raw();
}

/// Fill an error out-parameter from a library error.
///
/// # Safety
///
/// As [`set_error`].
pub(crate) unsafe fn set_library_error(slot: *mut gpkg_error_t, error: &Error) {
    // SAFETY: forwarded to the caller's guarantee on `slot`.
    unsafe { set_error(slot, Status::from(error), &error.to_string()) }
}

/// Release the message an error holds, and reset it to [`Status::Ok`].
///
/// Safe to call on an error that was never filled in, and safe to call twice:
/// the message pointer is cleared as it is freed. Passing NULL does nothing.
///
/// # Safety
///
/// `error`, when non-NULL, must point at a `gpkg_error_t` this library filled
/// in, and its `message` must not have been freed by any other means.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpkg_error_clear(error: *mut gpkg_error_t) {
    if error.is_null() {
        return;
    }
    // SAFETY: the caller guarantees a valid, writable `gpkg_error_t`.
    let error = unsafe { &mut *error };
    if !error.message.is_null() {
        // SAFETY: `message` was produced by `CString::into_raw` in `set_error`
        // and has not been freed, by the caller's guarantee. Retaking it as a
        // `CString` frees it with the allocator that made it.
        drop(unsafe { CString::from_raw(error.message) });
        error.message = std::ptr::null_mut();
    }
    error.code = Status::Ok;
}
