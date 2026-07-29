//! `gpkg_status` and `gpkg_error_t`: how a failure crosses the boundary.
//!
//! Every fallible entry point takes an optional `gpkg_error_t *` out-parameter.
//! On failure it is filled in with a code and an owned, NUL-terminated UTF-8
//! message, which the caller releases with [`gpkg_error_clear`]. Passing NULL
//! is allowed and means the caller wants only the return value.
//!
//! # Using it
//!
//! Declare one error variable, pass its address, and report and clear on the
//! way out. A call that succeeds does not touch it, so the same variable serves
//! every call in a program.
//!
//! ```c
//! // Report and clear, so each call site is two lines.
//! static int fail(const char *what, gpkg_error_t *error) {
//!     fprintf(stderr, "%s: code=%d message=%s\n", what, (int)error->code,
//!             error->message ? error->message : "(none)");
//!     gpkg_error_clear(error);
//!     return 1;
//! }
//!
//! gpkg_error_t error = {GPKG_STATUS_OK, NULL};
//!
//! uint64_t rows = 0;
//! if (gpkg_layer_count(layer, &rows, &error) != GPKG_STATUS_OK) {
//!     return fail("gpkg_layer_count", &error);
//! }
//! ```
//!
//! Clearing before reuse is what keeps the message owned by exactly one place:
//! a second failure writes a new pointer into the same field, and the first
//! message can then no longer be released. A caller that wants only the status
//! passes NULL instead, and has nothing to release:
//!
//! ```c
//! if (gpkg_layer_count(layer, &rows, NULL) != GPKG_STATUS_OK) {
//!     return 1;
//! }
//! ```
//!
//! Some codes are ordinary answers rather than faults.
//! `gpkg_tiles_get` and `gpkg_tiles_get_into` return `GPKG_STATUS_NOT_FOUND`
//! for an address a sparse pyramid holds no tile at, and `gpkg_layer_extent`
//! returns it for a layer with nothing to measure. Branch on the code before
//! treating a call as failed.
//!
//! # Why the codes are categories
//!
//! `geopackage::Error` has 46 variants and gains more as the library grows.
//! Mapping each to its own C constant would put a header-breaking change behind
//! every new variant, so the codes here are a small closed set of categories a
//! caller can branch on, and the message carries the detail. A new library
//! variant classifies into an existing category or into [`Status::Other`];
//! neither changes the header.
//!
//! Several of those variants wrap an error enum of their own, and the
//! classification follows the wrapping down rather than stopping at the
//! outermost variant. A tile written off its grid and a geometry whose bytes
//! are malformed both arrive under one library variant, and neither is
//! `Other`.

use std::ffi::{CString, c_char};

use arrow_schema::ArrowError;
use geopackage::{
    Error,
    core::{Error as CoreError, GeometryError, GpbError, TileError},
};

/// What kind of failure occurred.
///
/// A small set of categories rather than one constant per library error, so
/// that a caller can branch on the code and read the message for the detail.
/// The values are assigned explicitly and are never reused for another meaning,
/// so a comparison written against this header keeps its meaning.
// `#[repr(i32)]` is what makes cbindgen emit a plain C enum with those values.
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
            Error::Tile(tile) => tile_status(tile),
            Error::Core(core) => core_status(core),
            Error::Arrow(arrow) => arrow_status(arrow),
            // `Error` is not `#[non_exhaustive]`, but it gains variants as the
            // library grows, and a new one should classify rather than fail to
            // compile a downstream crate. Anything unclassified is `Other`,
            // whose message still says exactly what happened.
            _ => Self::Other,
        }
    }
}

/// Classify an Arrow failure, which reaches this crate when a stream the caller
/// supplied to `gpkg_layer_write_arrow` fails part-way through.
///
/// `ArrowError` belongs to `arrow-schema` and is not `#[non_exhaustive]`, so an
/// exhaustive match here would stop compiling on a dependency bump that adds a
/// variant. Only the ones this crate's paths can produce are named; the rest
/// describe compute, CSV, JSON and Parquet work that never runs here.
fn arrow_status(error: &ArrowError) -> Status {
    match error {
        // One of this library's own errors, carried across a stream boundary:
        // the read path wraps one when a worker fails, and it arrives here when
        // a caller pipes a stream from this library straight into it. Reaching
        // through is what keeps the round trip from flattening a category that
        // was already known.
        ArrowError::ExternalError(inner) => inner
            .downcast_ref::<Error>()
            .map_or(Status::Other, Status::from),
        // Something about the caller's data or schema: it does not fit the
        // layer, it will not convert, or it did not survive the C Data
        // Interface.
        ArrowError::InvalidArgumentError(_)
        | ArrowError::SchemaError(_)
        | ArrowError::CastError(_)
        | ArrowError::ParseError(_)
        | ArrowError::CDataInterface(_) => Status::InvalidArgument,
        ArrowError::NotYetImplemented(_) => Status::Unsupported,
        ArrowError::IoError(..) => Status::Io,
        _ => Status::Other,
    }
}

/// Classify a spec-level failure from `geopackage-core`, which `Error::Core`
/// carries whole.
///
/// Four variants, three of which wrap enums of their own, so this and the two
/// below follow the wrapping down to the variant that says what happened.
fn core_status(error: &CoreError) -> Status {
    match error {
        CoreError::Gpb(gpb) => gpb_status(gpb),
        CoreError::Geometry(geometry) => geometry_status(geometry),
        CoreError::Tile(tile) => tile_status(tile),
        // A table or column name that cannot be quoted safely, which is
        // whatever the caller named.
        CoreError::InvalidIdentifier(_) => Status::InvalidArgument,
        // As for `Error` above: `CoreError` is `#[non_exhaustive]`, and an
        // unclassified variant reports its message under `Other`.
        _ => Status::Other,
    }
}

/// Classify a GPB header failure.
fn gpb_status(error: &GpbError) -> Status {
    match error {
        // Bytes that are not a well-formed GPB blob. `Status::InvalidArgument`
        // names a malformed geometry among the things it covers, and that holds
        // whether the bytes came from the caller or from a file being read.
        GpbError::Truncated { .. }
        | GpbError::BadMagic(..)
        | GpbError::InvalidEnvelopeIndicator(_) => Status::InvalidArgument,
        // A GPB version this library does not implement, which is a property of
        // the file rather than a fault in it.
        GpbError::UnsupportedVersion(_) => Status::Unsupported,
        _ => Status::Other,
    }
}

/// Classify a geometry failure.
fn geometry_status(error: &GeometryError) -> Status {
    match error {
        GeometryError::Header(gpb) => gpb_status(gpb),
        // A body that cannot be read as ISO WKB: truncated, misdeclared, nested
        // past the depth cap, or carrying a type code that has no encoding.
        GeometryError::Body(_)
        | GeometryError::TruncatedWkb
        | GeometryError::UnknownWkbType(_)
        | GeometryError::AbstractWkbType(_)
        | GeometryError::TruncatedAt { .. }
        | GeometryError::InvalidByteOrder { .. }
        | GeometryError::NestingTooDeep => Status::InvalidArgument,
        // The body is well formed and this library cannot write it, which is
        // the one geometry failure that is not a fault in the bytes.
        GeometryError::NonLinearMember { .. } => Status::Unsupported,
        // Serialising a geometry this library itself holds failed, so the fault
        // is not in anything the caller passed.
        GeometryError::EncodeWkb(_) => Status::Other,
        _ => Status::Other,
    }
}

/// Classify a tile failure, which `Error::Tile` carries whole.
///
/// Split out rather than folded into the match above because `TileError` is a
/// twelve-variant enum in its own right, and its variants divide differently:
/// most of them describe a pyramid that does not satisfy a rule the
/// specification states, which is a constraint rather than a bad argument.
fn tile_status(error: &TileError) -> Status {
    match error {
        // A pyramid whose own description of itself breaks a rule the spec
        // states, or a tile that does not match what its zoom level declares.
        // Reported against the file or the ladder, not against the call.
        TileError::InvalidExtent { .. }
        | TileError::NegativeZoomLevel { .. }
        | TileError::NonPositiveDimension { .. }
        | TileError::NonPositivePixelSize { .. }
        | TileError::PixelSizeNotDescending { .. }
        | TileError::ExtentMismatch { .. }
        | TileError::DuplicateZoomLevel { .. }
        | TileError::PayloadSizeMismatch { .. } => Status::Constraint,
        // What the call was given: an address off the grid, bytes that are not
        // a readable image, or a zoom range that does not describe a ladder.
        TileError::CoordOutsideMatrix { .. }
        | TileError::UnreadablePayload { .. }
        | TileError::InvalidZoomRange { .. } => Status::InvalidArgument,
        // The arguments are fine; this pyramid's grid is not one XYZ indices
        // address, which is a property of the file.
        TileError::NotAnXyzGrid { .. } => Status::Unsupported,
        // `TileError` is `#[non_exhaustive]`, so a new variant classifies here
        // until it is given a category, exactly as for `Error` above.
        _ => Status::Other,
    }
}

/// A failure, as C sees it.
///
/// Initialise one as `{GPKG_STATUS_OK, NULL}` and pass its address to any call
/// that takes a `gpkg_error_t *`. A successful call leaves it alone. A failing
/// one sets `code` and stores an owned, NUL-terminated UTF-8 `message`, which
/// the caller releases with `gpkg_error_clear` before reusing the variable.
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

/// Release the message an error holds, and reset its code to `GPKG_STATUS_OK`.
///
/// Safe to call on an error that was never filled in, and safe to call twice:
/// the message pointer is cleared as it is freed. Passing NULL does nothing.
///
/// ```c
/// gpkg_error_t error = {GPKG_STATUS_OK, NULL};
/// gpkg_t *gpkg = gpkg_open("places.gpkg", &error);
/// if (!gpkg) {
///     fprintf(stderr, "%s\n", error.message ? error.message : "(no message)");
///     gpkg_error_clear(&error);   // the message is freed here
/// }
/// // `error` is now back to {GPKG_STATUS_OK, NULL} and ready for reuse.
/// ```
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The Arrow classification is the one that cannot be reached from a C
    /// program without fabricating a stream that fails on demand, so it is
    /// checked here on the function itself.
    #[test]
    fn an_arrow_failure_classifies_by_variant() {
        assert_eq!(
            arrow_status(&ArrowError::SchemaError("mismatch".to_owned())),
            Status::InvalidArgument
        );
        assert_eq!(
            arrow_status(&ArrowError::NotYetImplemented("later".to_owned())),
            Status::Unsupported
        );
        // A variant from the parts of arrow this crate never runs.
        assert_eq!(
            arrow_status(&ArrowError::ParquetError("elsewhere".to_owned())),
            Status::Other
        );
    }

    /// The case worth pinning: one of this library's own errors boxed inside an
    /// `ExternalError`, which is how it survives a stream boundary. A downcast
    /// that stopped matching would fail silently, reporting `Other` for an
    /// error whose category was already known.
    #[test]
    fn an_arrow_failure_wrapping_one_of_ours_keeps_its_category() {
        let inner = Error::NoSuchLayer {
            table_name: "places".to_owned(),
        };
        let wrapped = ArrowError::ExternalError(Box::new(inner));
        assert_eq!(arrow_status(&wrapped), Status::NotFound);

        // Anything else in that box has no category to recover.
        let foreign = ArrowError::ExternalError(Box::new(std::fmt::Error));
        assert_eq!(arrow_status(&foreign), Status::Other);
    }
}
