//! Property tests over `gpkg_writer_t`: the C writer against the Rust
//! `FeatureWriter`, for any edits with any values.
//!
//! Each case applies the same drawn edits to two in-memory files, one
//! through the C API and one through the Rust API, and compares each call's
//! outcome and then both tables. Values the C API checks before the Rust API
//! sees them (a date or time out of range, text that is not UTF-8, a NULL
//! payload) are expected to fail with the documented status, and change
//! nothing.
//!
//! One `unsafe` block per call, as the rest of this crate's tests do.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::ffi::{CStr, CString};
use std::path::Path;

use geopackage::core::datetime::{Date, DateTime};
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{
    ColumnSpec, FeatureWriter, GeoPackage, GeometrySpec, TableSchemaBuilder, ValueRef,
};
use geopackage_ffi::{
    Status, ValueKind, gpkg_blob_t, gpkg_close, gpkg_create, gpkg_date_t, gpkg_datetime_t,
    gpkg_error_clear, gpkg_error_t, gpkg_layer_free, gpkg_layer_open, gpkg_layer_writer, gpkg_t,
    gpkg_value_payload, gpkg_value_t, gpkg_writer_commit, gpkg_writer_delete, gpkg_writer_free,
    gpkg_writer_insert, gpkg_writer_t, gpkg_writer_update, gpkg_writer_update_column,
};
use hegel::generators;

/// An error slot holding a value no call writes on success.
fn sentinel() -> gpkg_error_t {
    gpkg_error_t {
        code: Status::Other,
        message: std::ptr::null_mut(),
    }
}

/// Checks the error protocol for a call that returned `status`, and returns
/// the status with the message of a failure.
fn check(status: Status, error: &mut gpkg_error_t) -> (Status, String) {
    if status == Status::Ok {
        assert_eq!(error.code, Status::Other, "a success wrote the slot");
        assert!(error.message.is_null(), "a success wrote the slot");
        return (status, String::new());
    }
    assert!(!error.message.is_null(), "a failure without a message");
    // SAFETY: the library filled this in and has not freed it.
    let message = unsafe { CStr::from_ptr(error.message) }
        .to_string_lossy()
        .into_owned();
    assert_eq!(error.code, status, "the slot disagrees: {message}");
    // SAFETY: a slot this library filled in.
    unsafe { gpkg_error_clear(error) };
    *error = sentinel();
    (status, message)
}

/// The value columns of the layer, in order, with the type of each.
const COLUMNS: [(&str, ColumnType); 7] = [
    ("t", ColumnType::Text(None)),
    ("i", ColumnType::Integer),
    ("r", ColumnType::Double),
    ("b", ColumnType::Boolean),
    ("x", ColumnType::Blob(None)),
    ("d", ColumnType::Date),
    ("dt", ColumnType::DateTime),
];

fn schema() -> TableSchemaBuilder {
    COLUMNS
        .iter()
        .fold(TableSchemaBuilder::new("t"), |schema, (name, ty)| {
            schema.column(ColumnSpec::new(*name, ty.clone()))
        })
        .geometry(GeometrySpec::new(GeometryType::Geometry, 4326))
        .spatial_index(true)
}

/// A value as drawn, which may be one only the C API can express.
#[derive(Debug, Clone)]
enum Drawn {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    Text(CString),
    /// Text that is not UTF-8.
    BadText(CString),
    /// A NULL text pointer.
    NullText,
    Blob(Vec<u8>),
    /// A NULL blob with a non-zero length.
    NullBlob,
    Date(gpkg_date_t),
    DateTime(gpkg_datetime_t),
}

impl Drawn {
    /// The C form. Borrows text and blob payloads from `self`.
    fn to_c(&self) -> gpkg_value_t {
        let (kind, value) = match self {
            Self::Null => (ValueKind::Null, gpkg_value_payload { integer: 0 }),
            Self::Boolean(v) => (ValueKind::Boolean, gpkg_value_payload { boolean: *v }),
            Self::Integer(v) => (ValueKind::Integer, gpkg_value_payload { integer: *v }),
            Self::Float(v) => (ValueKind::Float, gpkg_value_payload { real: *v }),
            Self::Text(text) | Self::BadText(text) => (
                ValueKind::Text,
                gpkg_value_payload {
                    text: text.as_ptr(),
                },
            ),
            Self::NullText => (
                ValueKind::Text,
                gpkg_value_payload {
                    text: std::ptr::null(),
                },
            ),
            Self::Blob(bytes) => (
                ValueKind::Blob,
                gpkg_value_payload {
                    blob: gpkg_blob_t {
                        data: if bytes.is_empty() {
                            std::ptr::null()
                        } else {
                            bytes.as_ptr()
                        },
                        len: bytes.len(),
                    },
                },
            ),
            Self::NullBlob => (
                ValueKind::Blob,
                gpkg_value_payload {
                    blob: gpkg_blob_t {
                        data: std::ptr::null(),
                        len: 3,
                    },
                },
            ),
            Self::Date(date) => (ValueKind::Date, gpkg_value_payload { date: *date }),
            Self::DateTime(datetime) => (
                ValueKind::DateTime,
                gpkg_value_payload {
                    datetime: *datetime,
                },
            ),
        };
        gpkg_value_t { kind, value }
    }

    /// The Rust form, or the status the C API documents for a value the Rust
    /// API cannot be given.
    fn to_rust(&self) -> Result<ValueRef<'_>, Status> {
        Ok(match self {
            Self::Null => ValueRef::Null,
            Self::Boolean(v) => ValueRef::Boolean(*v),
            Self::Integer(v) => ValueRef::Integer(*v),
            Self::Float(v) => ValueRef::Float(*v),
            Self::Text(text) => ValueRef::Text(text.to_str().map_err(|_utf8| Status::BadArgument)?),
            Self::BadText(_) | Self::NullText => return Err(Status::BadArgument),
            Self::Blob(bytes) => ValueRef::Blob(bytes),
            Self::NullBlob => return Err(Status::InvalidArgument),
            Self::Date(date) => ValueRef::Date(to_date(*date)?),
            Self::DateTime(datetime) => {
                let in_range = datetime.hour <= 23
                    && datetime.minute <= 59
                    && datetime.second <= 59
                    && datetime.nanosecond <= 999_999_999;
                let date = to_date(datetime.date)?;
                if !in_range {
                    return Err(Status::InvalidArgument);
                }
                ValueRef::DateTime(DateTime {
                    date,
                    hour: datetime.hour,
                    minute: datetime.minute,
                    second: datetime.second,
                    nanosecond: datetime.nanosecond,
                    offset_minutes: datetime.has_offset.then_some(datetime.offset_minutes),
                })
            }
        })
    }
}

fn to_date(date: gpkg_date_t) -> Result<Date, Status> {
    Date::new(date.year, date.month, date.day).map_err(|_reason| Status::InvalidArgument)
}

/// Draws a date with each field near or beyond its range.
fn draw_date(tc: &hegel::TestCase) -> gpkg_date_t {
    gpkg_date_t {
        year: tc.draw(generators::integers::<u16>().min_value(0).max_value(10_001)),
        month: tc.draw(generators::integers::<u8>().min_value(0).max_value(13)),
        day: tc.draw(generators::integers::<u8>().min_value(0).max_value(32)),
    }
}

/// Draws a value of any kind, with text from all of Unicode.
fn draw_value(tc: &hegel::TestCase) -> Drawn {
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(10)) {
        0 => Drawn::Null,
        1 => Drawn::Boolean(tc.draw(generators::booleans())),
        2 => Drawn::Integer(tc.draw(generators::integers::<i64>())),
        3 => Drawn::Float(tc.draw(generators::floats::<f64>())),
        4 => {
            let text: String = tc
                .draw(generators::text().max_size(12))
                .chars()
                .filter(|c| *c != '\0')
                .collect();
            Drawn::Text(CString::new(text).unwrap())
        }
        5 => Drawn::BadText(CString::new([b'a', 0xFF, b'b']).unwrap()),
        6 => Drawn::NullText,
        7 => {
            if tc.draw(generators::weighted_booleans(0.1)) {
                Drawn::NullBlob
            } else {
                Drawn::Blob(tc.draw(generators::vecs(generators::integers::<u8>()).max_size(8)))
            }
        }
        8 => Drawn::Date(draw_date(tc)),
        _ => Drawn::DateTime(gpkg_datetime_t {
            date: draw_date(tc),
            hour: tc.draw(generators::integers::<u8>().min_value(0).max_value(24)),
            minute: tc.draw(generators::integers::<u8>().min_value(0).max_value(60)),
            second: tc.draw(generators::integers::<u8>().min_value(0).max_value(60)),
            nanosecond: tc.draw(
                generators::integers::<u32>()
                    .min_value(999_999_990)
                    .max_value(1_000_000_001),
            ),
            has_offset: tc.draw(generators::booleans()),
            offset_minutes: tc.draw(generators::integers::<i16>()),
        }),
    }
}

/// A little-endian WKB point.
fn point(x: f64, y: f64) -> Vec<u8> {
    let mut bytes = vec![1u8];
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&x.to_le_bytes());
    bytes.extend_from_slice(&y.to_le_bytes());
    bytes
}

/// A little-endian WKB polygon with the given rings.
fn polygon(rings: &[&[(f64, f64)]]) -> Vec<u8> {
    let mut bytes = vec![1u8];
    bytes.extend_from_slice(&3u32.to_le_bytes());
    bytes.extend_from_slice(&u32::try_from(rings.len()).unwrap().to_le_bytes());
    for ring in rings {
        bytes.extend_from_slice(&u32::try_from(ring.len()).unwrap().to_le_bytes());
        for (x, y) in *ring {
            bytes.extend_from_slice(&x.to_le_bytes());
            bytes.extend_from_slice(&y.to_le_bytes());
        }
    }
    bytes
}

/// Draws a geometry argument: none, a point, a polygon, a polygon with a
/// hole and no exterior, truncated or arbitrary bytes, or an empty buffer.
fn draw_wkb(tc: &hegel::TestCase) -> Option<Vec<u8>> {
    let square: &[(f64, f64)] = &[(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 0.0)];
    let hole: &[(f64, f64)] = &[(1.0, 1.0), (2.0, 1.0), (2.0, 2.0), (1.0, 1.0)];
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(6)) {
        0 => None,
        1 => {
            let x = tc.draw(generators::floats::<f64>().allow_nan(false));
            Some(point(x, -x))
        }
        2 => Some(polygon(&[square, hole])),
        3 => Some(polygon(&[&[], hole])),
        4 => {
            let mut bytes = polygon(&[square]);
            let keep = tc.draw(
                generators::integers::<usize>()
                    .min_value(0)
                    .max_value(bytes.len() - 1),
            );
            bytes.truncate(keep);
            Some(bytes)
        }
        5 => Some(tc.draw(generators::vecs(generators::integers::<u8>()).max_size(12))),
        _ => Some(Vec::new()),
    }
}

/// One edit, as drawn.
#[derive(Debug)]
enum Edit {
    Insert {
        fid: Option<i64>,
        wkb: Option<Vec<u8>>,
        values: Vec<Drawn>,
    },
    Update {
        fid: i64,
        wkb: Option<Vec<u8>>,
        values: Vec<Drawn>,
    },
    UpdateColumn {
        fid: i64,
        column: CString,
        value: Drawn,
    },
    Delete {
        fid: i64,
    },
}

fn draw_fid(tc: &hegel::TestCase) -> i64 {
    if tc.draw(generators::booleans()) {
        tc.draw(generators::integers::<i64>().min_value(-1).max_value(4))
    } else {
        tc.draw(generators::integers::<i64>())
    }
}

/// Draws a value for column `index`: usually one of the column's own type,
/// otherwise any value.
fn draw_value_for(tc: &hegel::TestCase, index: usize) -> Drawn {
    if tc.draw(generators::weighted_booleans(0.2)) {
        return draw_value(tc);
    }
    let valid_date = |tc: &hegel::TestCase| gpkg_date_t {
        year: tc.draw(generators::integers::<u16>().min_value(0).max_value(9999)),
        month: tc.draw(generators::integers::<u8>().min_value(1).max_value(12)),
        day: tc.draw(generators::integers::<u8>().min_value(1).max_value(28)),
    };
    match index {
        0 => Drawn::Text(CString::new("text").unwrap()),
        1 => Drawn::Integer(tc.draw(generators::integers::<i64>())),
        2 => Drawn::Float(tc.draw(generators::floats::<f64>())),
        3 => Drawn::Boolean(tc.draw(generators::booleans())),
        4 => Drawn::Blob(tc.draw(generators::vecs(generators::integers::<u8>()).max_size(8))),
        5 => Drawn::Date(valid_date(tc)),
        _ => Drawn::DateTime(gpkg_datetime_t {
            date: valid_date(tc),
            hour: tc.draw(generators::integers::<u8>().min_value(0).max_value(23)),
            minute: 0,
            second: 0,
            nanosecond: 0,
            has_offset: tc.draw(generators::booleans()),
            offset_minutes: tc.draw(generators::integers::<i16>().min_value(-840).max_value(840)),
        }),
    }
}

/// Draws the values of a row: usually one per column, sometimes too few or
/// too many.
fn draw_values(tc: &hegel::TestCase) -> Vec<Drawn> {
    let count = if tc.draw(generators::weighted_booleans(0.9)) {
        COLUMNS.len()
    } else {
        tc.draw(generators::integers::<usize>().min_value(0).max_value(9))
    };
    (0..count).map(|index| draw_value_for(tc, index)).collect()
}

fn draw_edit(tc: &hegel::TestCase) -> Edit {
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(3)) {
        0 => Edit::Insert {
            fid: tc.draw(generators::booleans()).then(|| draw_fid(tc)),
            wkb: draw_wkb(tc),
            values: draw_values(tc),
        },
        1 => Edit::Update {
            fid: draw_fid(tc),
            wkb: draw_wkb(tc),
            values: draw_values(tc),
        },
        2 => {
            let column = match tc.draw(generators::integers::<u8>().min_value(0).max_value(2)) {
                0 => "geom".to_owned(),
                1 => "absent".to_owned(),
                _ => tc
                    .draw(generators::sampled_from(COLUMNS.to_vec()))
                    .0
                    .to_owned(),
            };
            Edit::UpdateColumn {
                fid: draw_fid(tc),
                column: CString::new(column).unwrap(),
                value: draw_value(tc),
            }
        }
        _ => Edit::Delete { fid: draw_fid(tc) },
    }
}

/// What a call returned: its status, and the id or match flag it reported.
type Outcome = (Status, Option<i64>, Option<bool>);

/// Applies one edit through the C writer.
fn apply_c(writer: *mut gpkg_writer_t, edit: &Edit, error: &mut gpkg_error_t) -> Outcome {
    let wkb_args = |wkb: &Option<Vec<u8>>| match wkb {
        Some(bytes) => (bytes.as_ptr(), bytes.len()),
        None => (std::ptr::null(), 0),
    };
    let mut matched = false;
    match edit {
        Edit::Insert { fid, wkb, values } => {
            let c_values: Vec<gpkg_value_t> = values.iter().map(Drawn::to_c).collect();
            let (wkb, wkb_len) = wkb_args(wkb);
            let mut out_fid = 0;
            // SAFETY: a live writer; every pointer is NULL or readable for
            // its length; out-parameters are writable.
            let status = unsafe {
                gpkg_writer_insert(
                    writer,
                    fid.as_ref().map_or(std::ptr::null(), std::ptr::from_ref),
                    wkb,
                    wkb_len,
                    c_values.as_ptr(),
                    c_values.len(),
                    &raw mut out_fid,
                    error,
                )
            };
            let (status, _) = check(status, error);
            (status, (status == Status::Ok).then_some(out_fid), None)
        }
        Edit::Update { fid, wkb, values } => {
            let c_values: Vec<gpkg_value_t> = values.iter().map(Drawn::to_c).collect();
            let (wkb, wkb_len) = wkb_args(wkb);
            // SAFETY: as above.
            let status = unsafe {
                gpkg_writer_update(
                    writer,
                    *fid,
                    wkb,
                    wkb_len,
                    c_values.as_ptr(),
                    c_values.len(),
                    &raw mut matched,
                    error,
                )
            };
            let (status, _) = check(status, error);
            (status, None, (status == Status::Ok).then_some(matched))
        }
        Edit::UpdateColumn { fid, column, value } => {
            let c_value = value.to_c();
            // SAFETY: as above.
            let status = unsafe {
                gpkg_writer_update_column(
                    writer,
                    *fid,
                    column.as_ptr(),
                    &raw const c_value,
                    &raw mut matched,
                    error,
                )
            };
            let (status, _) = check(status, error);
            (status, None, (status == Status::Ok).then_some(matched))
        }
        Edit::Delete { fid } => {
            // SAFETY: as above.
            let status = unsafe { gpkg_writer_delete(writer, *fid, &raw mut matched, error) };
            let (status, _) = check(status, error);
            (status, None, (status == Status::Ok).then_some(matched))
        }
    }
}

/// Converts a row of drawn values, or returns the status the C API documents
/// for the first one the Rust API cannot be given.
fn rust_values(values: &[Drawn]) -> Result<Vec<ValueRef<'_>>, Status> {
    values.iter().map(Drawn::to_rust).collect()
}

/// Applies one edit through the Rust writer, or returns the status the C API
/// documents when an argument cannot reach the Rust API.
fn apply_rust(writer: &mut FeatureWriter<'_>, edit: &Edit) -> Outcome {
    let status = |error: &geopackage::Error| Status::from(error);
    match edit {
        Edit::Insert { fid, wkb, values } => match rust_values(values) {
            Err(code) => (code, None, None),
            Ok(values) => {
                let written = match wkb {
                    Some(wkb) => writer.insert_wkb(*fid, wkb, &values),
                    None => writer.insert_row(*fid, &values),
                };
                match written {
                    Ok(id) => (Status::Ok, Some(id), None),
                    Err(error) => (status(&error), None, None),
                }
            }
        },
        Edit::Update { fid, wkb, values } => match rust_values(values) {
            Err(code) => (code, None, None),
            Ok(values) => {
                let updated = match wkb {
                    Some(wkb) => writer.update_wkb(*fid, wkb, &values),
                    None => writer.update_row(*fid, &values),
                };
                match updated {
                    Ok(matched) => (Status::Ok, None, Some(matched)),
                    Err(error) => (status(&error), None, None),
                }
            }
        },
        Edit::UpdateColumn { fid, column, value } => match value.to_rust() {
            Err(code) => (code, None, None),
            Ok(value) => match writer.update_column(*fid, column.to_str().unwrap(), value) {
                Ok(matched) => (Status::Ok, None, Some(matched)),
                Err(error) => (status(&error), None, None),
            },
        },
        Edit::Delete { fid } => match writer.delete(*fid) {
            Ok(matched) => (Status::Ok, None, Some(matched)),
            Err(error) => (status(&error), None, None),
        },
    }
}

/// Every row of the layer's table, as SQLite stores it.
fn table(gpkg: &GeoPackage) -> Vec<Vec<rusqlite::types::Value>> {
    let mut stmt = gpkg
        .connection()
        .prepare("SELECT * FROM t ORDER BY fid")
        .unwrap();
    let width = stmt.column_count();
    stmt.query_map([], |row| (0..width).map(|i| row.get(i)).collect())
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// Any edits with any values give the same outcomes through the C writer as
/// through the Rust writer, every failure follows the error protocol with a
/// classified status, and both tables end the same, whether the writers
/// commit or are discarded.
#[hegel::test]
fn the_c_writer_agrees_with_the_rust_writer(tc: hegel::TestCase) {
    let mut error = sentinel();
    let path = CString::new(":memory:").unwrap();
    // SAFETY: a NUL-terminated path and a writable error slot.
    let gpkg: *mut gpkg_t = unsafe { gpkg_create(path.as_ptr(), &raw mut error) };
    assert!(!gpkg.is_null());
    // SAFETY: a live container handle; `gpkg_t` is the container.
    unsafe { (*gpkg).gpkg() }.create_layer(&schema()).unwrap();
    let name = CString::new("t").unwrap();
    // SAFETY: a live container and a NUL-terminated name.
    let layer = unsafe { gpkg_layer_open(gpkg, name.as_ptr(), &raw mut error) };
    assert!(!layer.is_null());
    // SAFETY: a live layer handle.
    let writer = unsafe { gpkg_layer_writer(layer, &raw mut error) };
    assert!(!writer.is_null());

    let rust = GeoPackage::create(Path::new(":memory:")).unwrap();
    let rust_layer = rust.create_layer(&schema()).unwrap();
    let mut rust_writer = rust_layer.writer().unwrap();

    let edits = tc.draw(generators::integers::<usize>().min_value(1).max_value(10));
    for _ in 0..edits {
        let edit = draw_edit(&tc);
        let c = apply_c(writer, &edit, &mut error);
        let r = apply_rust(&mut rust_writer, &edit);
        tc.event(format!("{:?}", c.0));
        assert_eq!(c, r, "{edit:?}");
        assert_ne!(c.0, Status::Other, "an unclassified failure: {edit:?}");
    }

    if tc.draw(generators::booleans()) {
        // SAFETY: a live writer, committed once.
        let status = unsafe { gpkg_writer_commit(writer, &raw mut error) };
        assert_eq!(check(status, &mut error).0, Status::Ok);
        rust_writer.commit().unwrap();
    } else {
        // SAFETY: a live writer, freed once.
        unsafe { gpkg_writer_free(writer) };
        drop(rust_writer);
    }
    // SAFETY: a live container handle.
    assert_eq!(table(unsafe { (*gpkg).gpkg() }), table(&rust));
    // SAFETY: a live layer handle, freed once.
    unsafe { gpkg_layer_free(layer) };
    // SAFETY: a live container with no children.
    let status = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(check(status, &mut error).0, Status::Ok);
}
