//! `gpkg_value_t`: one cell of a row, crossing the boundary by value.
//!
//! The Arrow data plane moves whole layers. This is what moves one cell, for
//! the row-at-a-time writes in [`crate::writer`], and it is a direct image of
//! the Rust crate's `ValueRef`: the same eight cases, with text and binary
//! borrowed from the caller rather than copied.
//!
//! A value is a tag and a union, which C99 designated initialisers make
//! readable at the call site:
//!
//! ```c
//! gpkg_value_t name = {GPKG_VALUE_KIND_TEXT, {.text = "Dublin"}};
//! gpkg_value_t population = {GPKG_VALUE_KIND_INTEGER, {.integer = 592713}};
//! gpkg_value_t founded = {GPKG_VALUE_KIND_DATE, {.date = {988, 1, 1}}};
//! gpkg_value_t missing = {GPKG_VALUE_KIND_NULL, {0}};
//! ```
//!
//! Text and binary point at the caller's memory and are read during the call
//! that takes them, so a buffer may be reused as soon as that call returns.
//! Nothing here allocates or takes ownership.
//!
//! # Why dates are structured rather than text
//!
//! A GeoPackage stores dates as text, so passing them as text would work and
//! would be one case fewer. It is not done, because the crate validates a date
//! before writing it: a value arriving as text binds as text, and the column's
//! declared type is then the only thing standing between the file and
//! `"32nd Fibruary"`. Structured dates keep that check, and cost a caller
//! three integers.

use std::ffi::c_char;

use geopackage::ValueRef;
use geopackage::core::datetime::{Date, DateTime};

use crate::error::{Status, gpkg_error_t, set_error};
use crate::util::borrow_str;

/// Which case a [`gpkg_value_t`] contains.
///
/// `#[repr(i32)]` so cbindgen emits a plain C enum with stable values. Values
/// are assigned explicitly and must never be reused for another meaning.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    /// SQL `NULL`. The payload is not read.
    Null = 0,
    /// A boolean, stored as `0` or `1`.
    Boolean = 1,
    /// An integer of any declared width.
    Integer = 2,
    /// A floating-point number.
    Float = 3,
    /// Text, as a NUL-terminated UTF-8 string the caller owns.
    Text = 4,
    /// Binary, as a pointer and a length the caller owns.
    Blob = 5,
    /// A calendar date.
    Date = 6,
    /// A date and time.
    DateTime = 7,
}

/// A calendar date: year 0 to 9999, month 1 to 12, day 1 to the month's length.
///
/// Checked when the value is read, so an impossible date is a rejected call
/// rather than a bad row.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct gpkg_date_t {
    /// Year, 0 to 9999. The four-digit text form is what bounds it.
    pub year: u16,
    /// Month, 1 to 12.
    pub month: u8,
    /// Day, 1 to the length of the month.
    pub day: u8,
}

/// A date and time, with an optional UTC offset.
///
/// `has_offset` false means the time has no offset, which the spec permits
/// and which is not the same as an offset of zero: zero is `Z`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct gpkg_datetime_t {
    /// The calendar date.
    pub date: gpkg_date_t,
    /// Hour, 0 to 23.
    pub hour: u8,
    /// Minute, 0 to 59.
    pub minute: u8,
    /// Second, 0 to 59.
    pub second: u8,
    /// Sub-second component, 0 to 999 999 999.
    pub nanosecond: u32,
    /// Whether `offset_minutes` is to be read.
    pub has_offset: bool,
    /// UTC offset in minutes. `0` with `has_offset` set is `Z`.
    pub offset_minutes: i16,
}

/// Binary data the caller owns: a pointer and a length.
///
/// A length of zero is an empty blob, for which `data` may be NULL.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct gpkg_blob_t {
    /// The bytes, read during the call that takes them.
    pub data: *const u8,
    /// How many bytes.
    pub len: usize,
}

/// The payload of a [`gpkg_value_t`]. Which member is live is given by the
/// accompanying [`ValueKind`].
#[repr(C)]
#[derive(Clone, Copy)]
pub union gpkg_value_payload {
    /// Live for [`ValueKind::Boolean`].
    pub boolean: bool,
    /// Live for [`ValueKind::Integer`].
    pub integer: i64,
    /// Live for [`ValueKind::Float`].
    pub real: f64,
    /// Live for [`ValueKind::Text`]: NUL-terminated UTF-8.
    pub text: *const c_char,
    /// Live for [`ValueKind::Blob`].
    pub blob: gpkg_blob_t,
    /// Live for [`ValueKind::Date`].
    pub date: gpkg_date_t,
    /// Live for [`ValueKind::DateTime`].
    pub datetime: gpkg_datetime_t,
}

/// One cell of a row: a tag and a payload.
///
/// Nothing here is owned by the library. Text and binary point at the caller's
/// memory and are read during the call that takes them.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct gpkg_value_t {
    /// Which case `value` contains.
    pub kind: ValueKind,
    /// The payload. Read according to `kind`; not read at all for
    /// [`ValueKind::Null`].
    pub value: gpkg_value_payload,
}

impl gpkg_date_t {
    /// Validate this into a [`Date`].
    fn to_date(self) -> Result<Date, String> {
        Date::new(self.year, self.month, self.day).map_err(|source| {
            format!(
                "{:04}-{:02}-{:02} is not a date: {source}",
                self.year, self.month, self.day
            )
        })
    }
}

impl gpkg_datetime_t {
    /// Validate this into a [`DateTime`].
    fn to_datetime(self) -> Result<DateTime, String> {
        let date = self.date.to_date()?;
        if self.hour > 23 || self.minute > 59 || self.second > 59 {
            return Err(format!(
                "{:02}:{:02}:{:02} is not a time of day",
                self.hour, self.minute, self.second
            ));
        }
        if self.nanosecond > 999_999_999 {
            return Err(format!("{} is not a nanosecond count", self.nanosecond));
        }
        Ok(DateTime {
            date,
            hour: self.hour,
            minute: self.minute,
            second: self.second,
            nanosecond: self.nanosecond,
            offset_minutes: self.has_offset.then_some(self.offset_minutes),
        })
    }
}

/// Reads one caller-supplied value into the borrowed form the write path takes.
///
/// The returned `ValueRef` borrows `value`'s text or binary, so it lives only
/// as long as the caller's memory does, which for these entry points is the
/// duration of the call.
///
/// `index` names the position in the caller's array, so a rejected cell says
/// which one it was.
///
/// # Safety
///
/// `value` must point at a readable `gpkg_value_t` whose payload matches its
/// `kind`, with any text NUL-terminated and any blob readable for its length.
/// `error` must be NULL or point at a writable `gpkg_error_t`.
pub(crate) unsafe fn borrow_value<'a>(
    value: *const gpkg_value_t,
    index: usize,
    error: *mut gpkg_error_t,
) -> Option<ValueRef<'a>> {
    if value.is_null() {
        let message = format!("value {index} is NULL");
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, &message) };
        return None;
    }
    // SAFETY: the caller guarantees a readable `gpkg_value_t`.
    let value = unsafe { &*value };
    let rejected = |what: String, error: *mut gpkg_error_t| {
        let message = format!("value {index}: {what}");
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::InvalidArgument, &message) };
        None
    };

    match value.kind {
        ValueKind::Null => Some(ValueRef::Null),
        // SAFETY: the caller guarantees the payload matches the kind.
        ValueKind::Boolean => Some(ValueRef::Boolean(unsafe { value.value.boolean })),
        // SAFETY: as above.
        ValueKind::Integer => Some(ValueRef::Integer(unsafe { value.value.integer })),
        // SAFETY: as above.
        ValueKind::Float => Some(ValueRef::Float(unsafe { value.value.real })),
        ValueKind::Text => {
            // SAFETY: the caller guarantees the payload matches the kind.
            let ptr = unsafe { value.value.text };
            // SAFETY: `borrow_str` requires exactly what the caller guarantees
            // of a text payload: NUL-terminated UTF-8.
            let text = unsafe { borrow_str(ptr, error, "value text") }?;
            Some(ValueRef::Text(text))
        }
        ValueKind::Blob => {
            // SAFETY: as above.
            let blob = unsafe { value.value.blob };
            if blob.len == 0 {
                return Some(ValueRef::Blob(&[]));
            }
            if blob.data.is_null() {
                return rejected("blob is NULL with a non-zero length".to_owned(), error);
            }
            // SAFETY: the caller guarantees `data` is readable for `len`.
            Some(ValueRef::Blob(unsafe {
                std::slice::from_raw_parts(blob.data, blob.len)
            }))
        }
        // SAFETY: as above.
        ValueKind::Date => match unsafe { value.value.date }.to_date() {
            Ok(date) => Some(ValueRef::Date(date)),
            Err(why) => rejected(why, error),
        },
        // SAFETY: as above.
        ValueKind::DateTime => match unsafe { value.value.datetime }.to_datetime() {
            Ok(datetime) => Some(ValueRef::DateTime(datetime)),
            Err(why) => rejected(why, error),
        },
    }
}

/// Reads a caller-supplied array of values.
///
/// # Safety
///
/// `values` must be NULL (only when `len` is 0) or point at `len` readable
/// `gpkg_value_t`, each as [`borrow_value`] requires. `error` must be NULL or
/// writable.
pub(crate) unsafe fn borrow_values<'a>(
    values: *const gpkg_value_t,
    len: usize,
    error: *mut gpkg_error_t,
) -> Option<Vec<ValueRef<'a>>> {
    if len == 0 {
        return Some(Vec::new());
    }
    if values.is_null() {
        // SAFETY: forwarded to the caller's guarantee on `error`.
        unsafe { set_error(error, Status::BadArgument, "values is NULL") };
        return None;
    }
    let mut out = Vec::with_capacity(len);
    for index in 0..len {
        // SAFETY: the caller guarantees `len` readable values from `values`, so
        // every offset below `len` is in bounds.
        let value = unsafe { values.add(index) };
        // SAFETY: forwarded to this function's own contract.
        out.push(unsafe { borrow_value(value, index, error) }?);
    }
    Some(out)
}
