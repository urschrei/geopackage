//! `gpkg_writer_t`: the row-at-a-time write surface.
//!
//! Arrow appends. This is what a C consumer editing an existing file needs, so
//! the tests are edits: insert a row, correct a column of it, delete another,
//! and check the file rather than the return values.
//!
//! One `unsafe` block per call, as the rest of this crate's tests do.

use std::ffi::{CStr, CString};
use std::path::Path;

use geopackage_ffi::{
    Status, ValueKind, gpkg_close, gpkg_error_clear, gpkg_error_t, gpkg_layer_free,
    gpkg_layer_open, gpkg_layer_writer, gpkg_open, gpkg_t, gpkg_value_payload, gpkg_value_t,
    gpkg_writer_commit, gpkg_writer_delete, gpkg_writer_free, gpkg_writer_insert, gpkg_writer_t,
    gpkg_writer_update, gpkg_writer_update_column,
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

/// A little-endian WKB point.
fn point(x: f64, y: f64) -> Vec<u8> {
    let mut bytes = vec![1u8];
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&x.to_le_bytes());
    bytes.extend_from_slice(&y.to_le_bytes());
    bytes
}

fn integer(v: i64) -> gpkg_value_t {
    gpkg_value_t {
        kind: ValueKind::Integer,
        value: gpkg_value_payload { integer: v },
    }
}

fn text(s: &CStr) -> gpkg_value_t {
    gpkg_value_t {
        kind: ValueKind::Text,
        value: gpkg_value_payload { text: s.as_ptr() },
    }
}

/// A file with one `places` layer: `name TEXT`, `population INTEGER`, and a
/// point geometry. Built through the Rust API, which is what a C consumer's
/// input file would have been built by anyway.
fn places(path: &Path) {
    use geopackage::core::types::{ColumnType, GeometryType};
    use geopackage::{ColumnSpec, GeoPackage, GeometrySpec, TableSchemaBuilder};

    let gpkg = GeoPackage::create(path).expect("create");
    gpkg.add_epsg_srs(4326).expect("srs");
    gpkg.create_layer(
        &TableSchemaBuilder::new("places")
            .column(ColumnSpec::new("name", ColumnType::Text(None)))
            .column(ColumnSpec::new("population", ColumnType::Integer))
            .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
    )
    .expect("layer");
    gpkg.close().expect("close");
}

/// What the file holds for one feature, read through the Rust API after the C
/// handles are gone.
fn stored(path: &Path, fid: i64) -> Option<(String, i64)> {
    let gpkg = geopackage::GeoPackage::open(path).expect("reopen");
    gpkg.connection()
        .query_row(
            "SELECT name, population FROM places WHERE fid = ?1",
            [fid],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .ok()
}

fn row_count(path: &Path) -> i64 {
    let gpkg = geopackage::GeoPackage::open(path).expect("reopen");
    gpkg.connection()
        .query_row("SELECT count(*) FROM places", [], |row| row.get(0))
        .expect("count")
}

/// Open the file and its layer, returning both handles and an error slot.
fn opened(path: &Path) -> (*mut gpkg_t, *mut geopackage_ffi::gpkg_layer_t, gpkg_error_t) {
    let c_path = CString::new(path.to_str().expect("UTF-8 path")).expect("no interior NUL");
    let mut error = error_slot();
    // SAFETY: a valid path and a writable error slot.
    let gpkg = unsafe { gpkg_open(c_path.as_ptr(), &raw mut error) };
    assert!(!gpkg.is_null(), "{:?}", message(&error));
    let name = CString::new("places").expect("no interior NUL");
    // SAFETY: a live container handle and a valid name.
    let layer = unsafe { gpkg_layer_open(gpkg, name.as_ptr(), &raw mut error) };
    assert!(!layer.is_null(), "{:?}", message(&error));
    (gpkg, layer, error)
}

/// Free the layer then the container, which is the order the ABI requires.
fn closed(gpkg: *mut gpkg_t, layer: *mut geopackage_ffi::gpkg_layer_t, error: &mut gpkg_error_t) {
    // SAFETY: a live layer handle, freed exactly once.
    unsafe { gpkg_layer_free(layer) };
    // SAFETY: a live container handle with no children left, closed once.
    let closed = unsafe { gpkg_close(gpkg, &raw mut *error) };
    assert_eq!(closed, Status::Ok);
}

/// The whole point of the surface: add a feature, correct it, remove another.
#[test]
fn a_row_can_be_inserted_corrected_and_deleted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("w.gpkg");
    places(&path);
    let (gpkg, layer, mut error) = opened(&path);

    // SAFETY: a live layer handle and a writable error slot.
    let writer = unsafe { gpkg_layer_writer(layer, &raw mut error) };
    assert!(!writer.is_null(), "{:?}", message(&error));

    let dublin = CString::new("Dublin").expect("no interior NUL");
    let cork = CString::new("Cork").expect("no interior NUL");
    let wkb = point(-6.26, 53.35);

    let values = [text(&dublin), integer(592_713)];
    let mut fid = 0i64;
    // SAFETY: a live writer, a readable geometry and two readable values.
    let status = unsafe {
        gpkg_writer_insert(
            writer,
            std::ptr::null(),
            wkb.as_ptr(),
            wkb.len(),
            values.as_ptr(),
            values.len(),
            &raw mut fid,
            &raw mut error,
        )
    };
    assert_eq!(status, Status::Ok, "{:?}", message(&error));
    assert_eq!(fid, 1, "the first insert should be assigned fid 1");

    // A second row, to delete below.
    let second = [text(&cork), integer(210_000)];
    let mut second_fid = 0i64;
    // SAFETY: as above.
    let status = unsafe {
        gpkg_writer_insert(
            writer,
            std::ptr::null(),
            wkb.as_ptr(),
            wkb.len(),
            second.as_ptr(),
            second.len(),
            &raw mut second_fid,
            &raw mut error,
        )
    };
    assert_eq!(status, Status::Ok, "{:?}", message(&error));

    // Correct one column, leaving the name and the geometry alone.
    let column = CString::new("population").expect("no interior NUL");
    let corrected = integer(592_714);
    let mut matched = false;
    // SAFETY: a live writer, a valid column name and a readable value.
    let status = unsafe {
        gpkg_writer_update_column(
            writer,
            fid,
            column.as_ptr(),
            &raw const corrected,
            &raw mut matched,
            &raw mut error,
        )
    };
    assert_eq!(status, Status::Ok, "{:?}", message(&error));
    assert!(matched);

    // SAFETY: a live writer and a writable out-parameter.
    let status =
        unsafe { gpkg_writer_delete(writer, second_fid, &raw mut matched, &raw mut error) };
    assert_eq!(status, Status::Ok, "{:?}", message(&error));
    assert!(matched);

    // SAFETY: a live writer, committed exactly once.
    let status = unsafe { gpkg_writer_commit(writer, &raw mut error) };
    assert_eq!(status, Status::Ok, "{:?}", message(&error));

    closed(gpkg, layer, &mut error);

    assert_eq!(stored(&path, fid), Some(("Dublin".to_owned(), 592_714)));
    assert_eq!(row_count(&path), 1, "the deleted row is still there");
}

/// Freeing a writer instead of committing it discards everything staged, which
/// is what makes a failed edit safe to abandon.
#[test]
fn freeing_a_writer_discards_its_work() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("f.gpkg");
    places(&path);
    let (gpkg, layer, mut error) = opened(&path);

    // SAFETY: a live layer handle and a writable error slot.
    let writer = unsafe { gpkg_layer_writer(layer, &raw mut error) };
    assert!(!writer.is_null(), "{:?}", message(&error));

    let name = CString::new("Galway").expect("no interior NUL");
    let wkb = point(-9.05, 53.27);
    let values = [text(&name), integer(85_000)];
    // SAFETY: a live writer, a readable geometry and two readable values.
    let status = unsafe {
        gpkg_writer_insert(
            writer,
            std::ptr::null(),
            wkb.as_ptr(),
            wkb.len(),
            values.as_ptr(),
            values.len(),
            std::ptr::null_mut(),
            &raw mut error,
        )
    };
    assert_eq!(status, Status::Ok, "{:?}", message(&error));

    // SAFETY: a live writer, freed exactly once and not committed.
    unsafe { gpkg_writer_free(writer) };
    closed(gpkg, layer, &mut error);

    assert_eq!(row_count(&path), 0, "an uncommitted row reached the file");
}

/// A geometry replaced through `gpkg_writer_update`, which is the call the ABI
/// had no counterpart for at all.
#[test]
fn a_geometry_can_be_replaced() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("g.gpkg");
    places(&path);
    let (gpkg, layer, mut error) = opened(&path);

    // SAFETY: a live layer handle and a writable error slot.
    let writer = unsafe { gpkg_layer_writer(layer, &raw mut error) };
    let name = CString::new("Limerick").expect("no interior NUL");
    let wkb = point(-8.62, 52.66);
    let values = [text(&name), integer(94_000)];
    let mut fid = 0i64;
    // SAFETY: a live writer, a readable geometry and two readable values.
    unsafe {
        gpkg_writer_insert(
            writer,
            std::ptr::null(),
            wkb.as_ptr(),
            wkb.len(),
            values.as_ptr(),
            values.len(),
            &raw mut fid,
            &raw mut error,
        )
    };

    let moved = point(-8.63, 52.67);
    let mut matched = false;
    // SAFETY: as above, with a live fid.
    let status = unsafe {
        gpkg_writer_update(
            writer,
            fid,
            moved.as_ptr(),
            moved.len(),
            values.as_ptr(),
            values.len(),
            &raw mut matched,
            &raw mut error,
        )
    };
    assert_eq!(status, Status::Ok, "{:?}", message(&error));
    assert!(matched);

    // SAFETY: a live writer, committed exactly once.
    unsafe { gpkg_writer_commit(writer, &raw mut error) };
    closed(gpkg, layer, &mut error);

    // The stored geometry is the replacement, checked at the blob so nothing
    // decodes it on the way.
    let reopened = geopackage::GeoPackage::open(&path).expect("reopen");
    let blob: Vec<u8> = reopened
        .connection()
        .query_row("SELECT geom FROM places WHERE fid = ?1", [fid], |row| {
            row.get(0)
        })
        .expect("geometry");
    assert!(
        blob.windows(moved.len()).any(|w| w == moved.as_slice()),
        "the replacement geometry is not in the stored blob"
    );
}

/// A value count that does not match the layer is refused rather than padded
/// or truncated.
#[test]
fn a_wrong_value_count_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("c.gpkg");
    places(&path);
    let (gpkg, layer, mut error) = opened(&path);

    // SAFETY: a live layer handle and a writable error slot.
    let writer = unsafe { gpkg_layer_writer(layer, &raw mut error) };
    let name = CString::new("Sligo").expect("no interior NUL");
    let wkb = point(-8.47, 54.27);
    let only_one = [text(&name)];
    // SAFETY: a live writer, a readable geometry and one readable value.
    let status = unsafe {
        gpkg_writer_insert(
            writer,
            std::ptr::null(),
            wkb.as_ptr(),
            wkb.len(),
            only_one.as_ptr(),
            only_one.len(),
            std::ptr::null_mut(),
            &raw mut error,
        )
    };
    assert_ne!(status, Status::Ok, "a short value list was accepted");
    assert!(message(&error).is_some());
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: a live writer, freed exactly once.
    unsafe { gpkg_writer_free(writer) };
    closed(gpkg, layer, &mut error);
}

/// An impossible date is rejected at the boundary, which is why dates cross as
/// structures rather than as text.
#[test]
fn an_impossible_date_is_rejected_at_the_boundary() {
    use geopackage_ffi::gpkg_date_t;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("d.gpkg");
    places(&path);
    let (gpkg, layer, mut error) = opened(&path);

    // SAFETY: a live layer handle and a writable error slot.
    let writer = unsafe { gpkg_layer_writer(layer, &raw mut error) };
    let name = CString::new("Nowhere").expect("no interior NUL");
    let wkb = point(0.0, 0.0);
    let values = [
        text(&name),
        gpkg_value_t {
            kind: ValueKind::Date,
            value: gpkg_value_payload {
                date: gpkg_date_t {
                    year: 2026,
                    month: 2,
                    day: 31,
                },
            },
        },
    ];
    // SAFETY: a live writer, a readable geometry and two readable values.
    let status = unsafe {
        gpkg_writer_insert(
            writer,
            std::ptr::null(),
            wkb.as_ptr(),
            wkb.len(),
            values.as_ptr(),
            values.len(),
            std::ptr::null_mut(),
            &raw mut error,
        )
    };
    assert_eq!(status, Status::InvalidArgument);
    let text = message(&error).expect("a refusal carries a message");
    assert!(text.contains("not a date"), "unexpected message: {text}");
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: a live writer, freed exactly once.
    unsafe { gpkg_writer_free(writer) };
    closed(gpkg, layer, &mut error);
}

/// A live writer borrows its container, so a close is refused until it is gone,
/// exactly as a layer handle or a stream is.
#[test]
fn a_live_writer_blocks_a_close() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("b.gpkg");
    places(&path);
    let (gpkg, layer, mut error) = opened(&path);

    // SAFETY: a live layer handle and a writable error slot.
    let writer = unsafe { gpkg_layer_writer(layer, &raw mut error) };
    assert!(!writer.is_null());

    // SAFETY: a live layer handle, freed exactly once. The writer still holds
    // its own count, which is the case under test.
    unsafe { gpkg_layer_free(layer) };
    // SAFETY: a live container handle; the refusal leaves it open and usable.
    let refused = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(refused, Status::HandleInUse);
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: a live writer, freed exactly once.
    unsafe { gpkg_writer_free(writer) };
    // SAFETY: nothing borrows the container now.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok, "{:?}", message(&error));
}

#[test]
fn null_handles_are_rejected_rather_than_dereferenced() {
    let mut error = error_slot();
    // SAFETY: NULL is the case under test, and the error slot is writable.
    let writer = unsafe { gpkg_layer_writer(std::ptr::null(), &raw mut error) };
    assert!(writer.is_null());
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: NULL is the case under test.
    let status = unsafe {
        gpkg_writer_delete(
            std::ptr::null_mut(),
            1,
            std::ptr::null_mut(),
            &raw mut error,
        )
    };
    assert_eq!(status, Status::BadArgument);
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: freeing NULL is documented as a no-op.
    unsafe { gpkg_writer_free(std::ptr::null_mut::<gpkg_writer_t>()) };
}

/// A little-endian CIRCULARSTRING body.
fn circular_string(points: &[[f64; 2]]) -> Vec<u8> {
    let mut bytes = vec![1u8];
    bytes.extend_from_slice(&8u32.to_le_bytes());
    bytes.extend_from_slice(&u32::try_from(points.len()).expect("small").to_le_bytes());
    for [x, y] in points {
        bytes.extend_from_slice(&x.to_le_bytes());
        bytes.extend_from_slice(&y.to_le_bytes());
    }
    bytes
}

/// A little-endian container body of ISO WKB type `code`, holding `members`.
fn container(code: u32, members: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = vec![1u8];
    bytes.extend_from_slice(&code.to_le_bytes());
    bytes.extend_from_slice(&u32::try_from(members.len()).expect("small").to_le_bytes());
    for member in members {
        bytes.extend_from_slice(member);
    }
    bytes
}

/// Insert `wkb` and report the status, so the geometry is the only thing that
/// differs between the cases below.
fn insert_geometry(writer: *mut gpkg_writer_t, wkb: &[u8], error: &mut gpkg_error_t) -> Status {
    let name = CString::new("nowhere").expect("no interior NUL");
    let values = [text(&name), integer(0)];
    let mut fid = 0i64;
    // SAFETY: a live writer, a readable geometry and two readable values.
    unsafe {
        gpkg_writer_insert(
            writer,
            std::ptr::null(),
            wkb.as_ptr(),
            wkb.len(),
            values.as_ptr(),
            values.len(),
            &raw mut fid,
            &raw mut *error,
        )
    }
}

#[test]
fn a_geometry_failure_reports_a_category_rather_than_other() {
    // Spec-level failures from geopackage-core used to reach C as
    // GPKG_STATUS_OTHER, the whole enum classifying through one unhandled
    // variant. The two cases here are the ones a caller has to tell apart:
    // bytes that are wrong, and bytes that are right but not writable.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("w.gpkg");
    places(&path);
    let (gpkg, layer, mut error) = opened(&path);

    // SAFETY: a live layer handle and a writable error slot.
    let writer = unsafe { gpkg_layer_writer(layer, &raw mut error) };
    assert!(!writer.is_null(), "{:?}", message(&error));

    // A type code that is not a GeoPackage geometry type: the argument is bad.
    let mut nonsense = vec![1u8];
    nonsense.extend_from_slice(&99u32.to_le_bytes());
    let status = insert_geometry(writer, &nonsense, &mut error);
    assert_eq!(status, Status::InvalidArgument, "{:?}", message(&error));
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // A well-formed GEOMETRYCOLLECTION holding a CIRCULARSTRING. Nothing is
    // wrong with the bytes; this library cannot write that shape, and saying
    // "invalid argument" would send the caller looking for a fault in them.
    let arc = circular_string(&[[0.0, 0.0], [1.0, 1.0], [2.0, 0.0]]);
    let status = insert_geometry(writer, &container(7, &[arc]), &mut error);
    assert_eq!(status, Status::Unsupported, "{:?}", message(&error));
    assert!(
        message(&error).is_some_and(|text| text.contains("CIRCULARSTRING")),
        "the message should name what could not be written"
    );
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: a live writer, freed exactly once; nothing was committed.
    unsafe { gpkg_writer_free(writer) };
    closed(gpkg, layer, &mut error);
    assert_eq!(row_count(&path), 0, "neither insert reached the file");
}
