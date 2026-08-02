//! The Arrow C Data Interface data plane.
//!
//! Driven back through arrow-rs's *importer* rather than its exporter, which is
//! the right check: the stream is consumed exactly as any other C Data
//! Interface consumer would consume it, so a callback that gets the protocol
//! wrong shows up here rather than in someone else's program.

use std::ffi::{CStr, CString};
use std::path::PathBuf;

use arrow_array::RecordBatchReader;
use arrow_array::ffi_stream::{ArrowArrayStreamReader, FFI_ArrowArrayStream};
use geopackage_ffi::{
    Status, ValueKind, gpkg_close, gpkg_error_clear, gpkg_error_t, gpkg_layer_free,
    gpkg_layer_open, gpkg_layer_open_with_columns, gpkg_layer_read_arrow,
    gpkg_layer_read_arrow_filtered, gpkg_layer_read_arrow_in, gpkg_open_read_only, gpkg_t,
    gpkg_value_payload, gpkg_value_t,
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

fn fixture() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate directory has a parent")
        .join("geopackage/tests/fixtures/gdal_multilayer_1_4.gpkg")
}

fn open() -> (*mut gpkg_t, gpkg_error_t) {
    let path =
        CString::new(fixture().to_str().expect("fixture path is UTF-8")).expect("no interior NUL");
    let mut error = error_slot();
    // SAFETY: a valid path and a writable error slot.
    let gpkg = unsafe { gpkg_open_read_only(path.as_ptr(), &raw mut error) };
    assert!(!gpkg.is_null(), "{:?}", message(&error));
    (gpkg, error)
}

/// Pull a whole stream through arrow-rs's importer, returning the row count and
/// the schema's field names.
fn drain(stream: FFI_ArrowArrayStream) -> (usize, Vec<String>) {
    // SAFETY: `stream` was produced by this library and is handed over here,
    // which is the move the C Data Interface specifies.
    let reader = unsafe { ArrowArrayStreamReader::from_raw(&mut { stream }) }
        .expect("the exported stream should import");
    let names = reader
        .schema()
        .fields()
        .iter()
        .map(|field| field.name().clone())
        .collect();
    let rows = reader
        .map(|batch| batch.expect("a batch").num_rows())
        .sum::<usize>();
    (rows, names)
}

#[test]
fn a_layer_streams_every_row_with_its_schema() {
    let (gpkg, mut error) = open();
    let name = CString::new("points").expect("no interior NUL");
    // SAFETY: a live container handle and a valid name.
    let layer = unsafe { gpkg_layer_open(gpkg, name.as_ptr(), &raw mut error) };
    assert!(!layer.is_null());

    let mut stream = FFI_ArrowArrayStream::empty();
    // SAFETY: a live layer handle and writable out-parameters.
    let status = unsafe { gpkg_layer_read_arrow(layer, &raw mut stream, &raw mut error) };
    assert_eq!(status, Status::Ok, "{:?}", message(&error));

    let (rows, names) = drain(stream);
    assert_eq!(rows, 3);
    assert!(names.contains(&"fid".to_owned()), "{names:?}");
    assert!(names.contains(&"geom".to_owned()), "{names:?}");
    assert!(names.contains(&"name".to_owned()), "{names:?}");

    // SAFETY: a live layer handle, freed exactly once.
    unsafe { gpkg_layer_free(layer) };
    // SAFETY: the stream was released by dropping the reader, and the layer is
    // freed, so nothing borrows the container.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn a_live_stream_blocks_a_close_just_as_a_layer_handle_does() {
    // The soundness rule, extended to streams: the stream stores an erased
    // borrow of the container, so it must count against the same tally.
    let (gpkg, mut error) = open();
    let name = CString::new("points").expect("no interior NUL");
    // SAFETY: a live container handle and a valid name.
    let layer = unsafe { gpkg_layer_open(gpkg, name.as_ptr(), &raw mut error) };
    assert!(!layer.is_null());

    let mut stream = FFI_ArrowArrayStream::empty();
    // SAFETY: a live layer handle and writable out-parameters.
    let status = unsafe { gpkg_layer_read_arrow(layer, &raw mut stream, &raw mut error) };
    assert_eq!(status, Status::Ok);

    // Free the layer handle. The stream still borrows the container, so this
    // alone must not be enough to permit a close.
    // SAFETY: a live layer handle, freed exactly once.
    unsafe { gpkg_layer_free(layer) };

    // SAFETY: a live container handle; the close is expected to be refused.
    let refused = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(
        refused,
        Status::HandleInUse,
        "a live stream must block a close"
    );
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // Releasing the stream is what finally permits it.
    // SAFETY: the stream is live and released exactly once, which is what
    // dropping the imported reader does.
    drop(unsafe { ArrowArrayStreamReader::from_raw(&raw mut stream) });

    // SAFETY: nothing borrows the container now.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn a_bounding_box_stream_returns_only_what_it_should() {
    let (gpkg, mut error) = open();
    let name = CString::new("points").expect("no interior NUL");
    // SAFETY: a live container handle and a valid name.
    let layer = unsafe { gpkg_layer_open(gpkg, name.as_ptr(), &raw mut error) };
    assert!(!layer.is_null());

    // The whole world: every row.
    let mut all = FFI_ArrowArrayStream::empty();
    // SAFETY: a live layer handle and writable out-parameters.
    let status = unsafe {
        gpkg_layer_read_arrow_in(
            layer,
            -180.0,
            -90.0,
            180.0,
            90.0,
            &raw mut all,
            &raw mut error,
        )
    };
    assert_eq!(status, Status::Ok, "{:?}", message(&error));
    assert_eq!(drain(all).0, 3);

    // Somewhere with nothing in it.
    let mut none = FFI_ArrowArrayStream::empty();
    // SAFETY: as above.
    let status = unsafe {
        gpkg_layer_read_arrow_in(
            layer,
            100.0,
            80.0,
            101.0,
            81.0,
            &raw mut none,
            &raw mut error,
        )
    };
    assert_eq!(status, Status::Ok, "{:?}", message(&error));
    assert_eq!(drain(none).0, 0, "an empty box should stream no rows");

    // SAFETY: a live layer handle, freed exactly once.
    unsafe { gpkg_layer_free(layer) };
    // SAFETY: both streams were released by draining them.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn null_arguments_are_rejected_rather_than_dereferenced() {
    let mut stream = FFI_ArrowArrayStream::empty();
    let mut error = error_slot();

    // SAFETY: NULL is the case under test.
    let status =
        unsafe { gpkg_layer_read_arrow(std::ptr::null(), &raw mut stream, &raw mut error) };
    assert_eq!(status, Status::BadArgument);
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    let (gpkg, mut error) = open();
    let name = CString::new("points").expect("no interior NUL");
    // SAFETY: a live container handle and a valid name.
    let layer = unsafe { gpkg_layer_open(gpkg, name.as_ptr(), &raw mut error) };
    // SAFETY: a live layer handle with a NULL out-parameter, the case under test.
    let status = unsafe { gpkg_layer_read_arrow(layer, std::ptr::null_mut(), &raw mut error) };
    assert_eq!(status, Status::BadArgument);
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // A rejected call took no token, so the close is permitted once the layer
    // handle is gone.
    // SAFETY: a live layer handle, freed exactly once.
    unsafe { gpkg_layer_free(layer) };
    // SAFETY: nothing borrows the container.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn many_streams_can_be_opened_and_released() {
    // Exercises the token arithmetic on the stream path, which is what stands
    // between the erasure and a dangling reader.
    let (gpkg, mut error) = open();
    let name = CString::new("points").expect("no interior NUL");
    // SAFETY: a live container handle and a valid name.
    let layer = unsafe { gpkg_layer_open(gpkg, name.as_ptr(), &raw mut error) };

    for _ in 0..25 {
        let mut stream = FFI_ArrowArrayStream::empty();
        // SAFETY: a live layer handle and writable out-parameters.
        let status = unsafe { gpkg_layer_read_arrow(layer, &raw mut stream, &raw mut error) };
        assert_eq!(status, Status::Ok);
        assert_eq!(drain(stream).0, 3);
    }

    // SAFETY: a live layer handle, freed exactly once.
    unsafe { gpkg_layer_free(layer) };
    // SAFETY: every stream was drained and released.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

fn text_value(s: &CStr) -> gpkg_value_t {
    gpkg_value_t {
        kind: ValueKind::Text,
        value: gpkg_value_payload { text: s.as_ptr() },
    }
}

#[test]
fn a_filtered_stream_returns_the_matching_rows() {
    let (gpkg, mut error) = open();
    let name = CString::new("points").expect("no interior NUL");
    // SAFETY: a live container handle and a valid name.
    let layer = unsafe { gpkg_layer_open(gpkg, name.as_ptr(), &raw mut error) };
    assert!(!layer.is_null());

    // One row by name, through a bound parameter, including non-ASCII text.
    let wanted = CString::new("béta ☃").expect("no interior NUL");
    let clause = CString::new("name = ?1").expect("no interior NUL");
    let param = text_value(&wanted);
    let mut stream = FFI_ArrowArrayStream::empty();
    // SAFETY: a live layer handle, a valid clause with one matching value, and
    // writable out-parameters.
    let status = unsafe {
        gpkg_layer_read_arrow_filtered(
            layer,
            std::ptr::null(),
            clause.as_ptr(),
            &raw const param,
            1,
            &raw mut stream,
            &raw mut error,
        )
    };
    assert_eq!(status, Status::Ok, "{:?}", message(&error));
    assert_eq!(drain(stream).0, 1);

    // Both filters NULL: the general form degenerates to the whole layer.
    let mut all = FFI_ArrowArrayStream::empty();
    // SAFETY: as above, with no filters at all.
    let status = unsafe {
        gpkg_layer_read_arrow_filtered(
            layer,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            &raw mut all,
            &raw mut error,
        )
    };
    assert_eq!(status, Status::Ok, "{:?}", message(&error));
    assert_eq!(drain(all).0, 3);

    // Composed with a bounding box covering everything, the clause still
    // decides; composed with one covering nothing, nothing survives it.
    for (bbox, expected) in [
        ([-180.0, -90.0, 180.0, 90.0], 1),
        ([500.0, 500.0, 501.0, 501.0], 0),
    ] {
        let mut composed = FFI_ArrowArrayStream::empty();
        // SAFETY: as above, with four readable doubles.
        let status = unsafe {
            gpkg_layer_read_arrow_filtered(
                layer,
                bbox.as_ptr(),
                clause.as_ptr(),
                &raw const param,
                1,
                &raw mut composed,
                &raw mut error,
            )
        };
        assert_eq!(status, Status::Ok, "{:?}", message(&error));
        assert_eq!(drain(composed).0, expected, "{bbox:?}");
    }

    // SAFETY: a live layer handle, freed exactly once.
    unsafe { gpkg_layer_free(layer) };
    // SAFETY: every stream was drained and released.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn params_without_a_clause_are_refused() {
    let (gpkg, mut error) = open();
    let name = CString::new("points").expect("no interior NUL");
    // SAFETY: a live container handle and a valid name.
    let layer = unsafe { gpkg_layer_open(gpkg, name.as_ptr(), &raw mut error) };
    assert!(!layer.is_null());

    let text = CString::new("alpha").expect("no interior NUL");
    let param = text_value(&text);
    let mut stream = FFI_ArrowArrayStream::empty();
    // SAFETY: a live layer handle; the argument combination is deliberately
    // invalid, which the call must report rather than dereference into.
    let status = unsafe {
        gpkg_layer_read_arrow_filtered(
            layer,
            std::ptr::null(),
            std::ptr::null(),
            &raw const param,
            1,
            &raw mut stream,
            &raw mut error,
        )
    };
    assert_eq!(status, Status::InvalidArgument);
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: a live layer handle, freed exactly once.
    unsafe { gpkg_layer_free(layer) };
    // SAFETY: the failed open registered no stream, so nothing blocks a close.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}

#[test]
fn a_projected_open_narrows_the_stream() {
    let (gpkg, mut error) = open();
    let name = CString::new("points").expect("no interior NUL");
    let column = CString::new("name").expect("no interior NUL");
    let columns = [column.as_ptr()];
    // SAFETY: a live container handle, a valid name and one readable column
    // pointer.
    let layer = unsafe {
        gpkg_layer_open_with_columns(gpkg, name.as_ptr(), columns.as_ptr(), 1, &raw mut error)
    };
    assert!(!layer.is_null(), "{:?}", message(&error));

    let mut stream = FFI_ArrowArrayStream::empty();
    // SAFETY: a live layer handle and writable out-parameters.
    let status = unsafe { gpkg_layer_read_arrow(layer, &raw mut stream, &raw mut error) };
    assert_eq!(status, Status::Ok, "{:?}", message(&error));
    let (rows, names) = drain(stream);
    assert_eq!(rows, 3);
    assert_eq!(
        names,
        vec!["fid".to_owned(), "name".to_owned()],
        "the geometry was not named, so the stream must not carry it"
    );

    // A bounding-box read still works: the hidden geometry feeds the exact
    // re-test without reaching the batches.
    let mut in_box = FFI_ArrowArrayStream::empty();
    // SAFETY: as above.
    let status = unsafe {
        gpkg_layer_read_arrow_in(
            layer,
            -180.0,
            -90.0,
            180.0,
            90.0,
            &raw mut in_box,
            &raw mut error,
        )
    };
    assert_eq!(status, Status::Ok, "{:?}", message(&error));
    let (rows, names) = drain(in_box);
    assert_eq!(rows, 3);
    assert_eq!(names, vec!["fid".to_owned(), "name".to_owned()]);

    // SAFETY: a live layer handle, freed exactly once.
    unsafe { gpkg_layer_free(layer) };

    // A column the table does not have fails at the open, naming it.
    let wrong = CString::new("nope").expect("no interior NUL");
    let wrong_columns = [wrong.as_ptr()];
    // SAFETY: as the successful open; the column name is the case under test.
    let refused = unsafe {
        gpkg_layer_open_with_columns(
            gpkg,
            name.as_ptr(),
            wrong_columns.as_ptr(),
            1,
            &raw mut error,
        )
    };
    assert!(refused.is_null());
    assert_eq!(error.code, Status::NotFound);
    assert!(
        message(&error).is_some_and(|text| text.contains("nope")),
        "the refusal should name the column"
    );
    // SAFETY: an error slot this library filled in.
    unsafe { gpkg_error_clear(&raw mut error) };

    // SAFETY: every handle and stream is released.
    let closed = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(closed, Status::Ok);
}
