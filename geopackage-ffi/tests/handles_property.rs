//! Property tests over handle lifetimes: any order of opening, using and
//! freeing child handles, with close attempts between, against a count of the
//! live children.
//!
//! One `unsafe` block per call, as the rest of this crate's tests do.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::ffi::CString;

use arrow_array::ffi_stream::FFI_ArrowArrayStream;
use geopackage::core::tiles::{TileCoord, TileMatrixSet, ZoomLadder};
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{ColumnSpec, GeometrySpec, TableSchemaBuilder, TilePyramidBuilder};
use geopackage_ffi::{
    Status, ValueKind, gpkg_attributes_open, gpkg_close, gpkg_create, gpkg_error_clear,
    gpkg_error_t, gpkg_finding_at, gpkg_findings_count, gpkg_findings_free, gpkg_findings_t,
    gpkg_layer_count, gpkg_layer_free, gpkg_layer_open, gpkg_layer_read_arrow, gpkg_layer_t,
    gpkg_layer_writer, gpkg_string_free, gpkg_t, gpkg_tile_cursor_free, gpkg_tile_cursor_next,
    gpkg_tile_cursor_t, gpkg_tiles_count, gpkg_tiles_cursor, gpkg_tiles_free, gpkg_tiles_open,
    gpkg_tiles_t, gpkg_validate, gpkg_value_payload, gpkg_value_t, gpkg_version,
    gpkg_writer_commit, gpkg_writer_free, gpkg_writer_insert, gpkg_writer_t,
};
use hegel::generators;

/// An error slot holding a value no call writes on success, so a call that
/// touches the slot when it succeeds shows.
fn sentinel() -> gpkg_error_t {
    gpkg_error_t {
        code: Status::Other,
        message: std::ptr::null_mut(),
    }
}

/// Checks the error protocol for a call that returned `status`: a success
/// leaves the slot as it was, and a failure fills it with the same status and
/// a message. Resets the slot to the sentinel.
fn check(status: Status, error: &mut gpkg_error_t) -> Status {
    if status == Status::Ok {
        assert_eq!(error.code, Status::Other, "a success wrote the slot");
        assert!(error.message.is_null(), "a success wrote the slot");
    } else {
        assert_eq!(error.code, status);
        assert!(!error.message.is_null(), "a failure without a message");
        // SAFETY: a slot this library filled in.
        unsafe { gpkg_error_clear(error) };
        *error = sentinel();
    }
    status
}

/// As [`check`], for a call that returns a pointer: NULL is a failure.
fn check_ptr<T>(pointer: *mut T, error: &mut gpkg_error_t) -> *mut T {
    let status = if pointer.is_null() {
        error.code
    } else {
        Status::Ok
    };
    if pointer.is_null() {
        assert_ne!(status, Status::Other, "NULL without a status");
    }
    check(status, error);
    pointer
}

/// A PNG header of the given size: enough for the payload probe.
fn png(side: u32) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&13_u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&side.to_be_bytes());
    bytes.extend_from_slice(&side.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    bytes
}

/// A little-endian WKB point.
fn point(x: f64, y: f64) -> Vec<u8> {
    let mut bytes = vec![1u8];
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&x.to_le_bytes());
    bytes.extend_from_slice(&y.to_le_bytes());
    bytes
}

/// Creates an in-memory container with a feature layer `pts`, an attribute
/// table `notes` and a tile pyramid `tiles`, each with content.
fn fixture() -> *mut gpkg_t {
    let mut error = sentinel();
    let path = CString::new(":memory:").unwrap();
    // SAFETY: a NUL-terminated path and a writable error slot.
    let gpkg = check_ptr(
        unsafe { gpkg_create(path.as_ptr(), &raw mut error) },
        &mut error,
    );
    // SAFETY: a live container handle; `gpkg_t` is the container.
    let rust = unsafe { (*gpkg).gpkg() };
    let layer = rust
        .create_layer(
            &TableSchemaBuilder::new("pts")
                .geometry(GeometrySpec::new(GeometryType::Point, 4326))
                .spatial_index(true),
        )
        .unwrap();
    let mut writer = layer.writer().unwrap();
    for x in [0.0, 1.0, 2.0] {
        writer.insert_wkb(None, &point(x, x), &[]).unwrap();
    }
    writer.commit().unwrap();
    rust.create_attributes_table(
        &TableSchemaBuilder::new("notes").column(ColumnSpec::new("body", ColumnType::Text(None))),
    )
    .unwrap();
    rust.connection()
        .execute("INSERT INTO notes (body) VALUES ('a'), ('b')", [])
        .unwrap();
    let set = TileMatrixSet::new(4326, 0.0, 0.0, 16.0, 16.0);
    let pyramid = rust
        .create_tile_pyramid(
            &TilePyramidBuilder::new("tiles", set)
                .matrices(set.ladder(ZoomLadder::new(0, 1).tile_size(16, 16)).unwrap()),
        )
        .unwrap();
    pyramid.put_tile(TileCoord::new(0, 0, 0), &png(16)).unwrap();
    pyramid.put_tile(TileCoord::new(1, 1, 1), &png(16)).unwrap();
    gpkg
}

/// A live child handle. Each is freed by exactly one call.
enum Child {
    /// A layer, and whether it has a geometry column.
    Layer(*mut gpkg_layer_t, bool),
    Tiles(*mut gpkg_tiles_t),
    Cursor(*mut gpkg_tile_cursor_t),
    /// A writer, and whether its layer has a geometry column.
    Writer(*mut gpkg_writer_t, bool),
    Stream(Box<FFI_ArrowArrayStream>),
}

impl Child {
    /// Uses the handle once, so a handle whose borrow has gone bad shows.
    fn exercise(&mut self, error: &mut gpkg_error_t) {
        match self {
            Self::Layer(layer, _) => {
                let mut rows = 0;
                // SAFETY: a live layer handle and writable out-parameters.
                let status = unsafe { gpkg_layer_count(*layer, &raw mut rows, error) };
                assert_eq!(check(status, error), Status::Ok);
            }
            Self::Tiles(tiles) => {
                let mut count = 0;
                // SAFETY: a live tiles handle and writable out-parameters.
                let status = unsafe { gpkg_tiles_count(*tiles, &raw mut count, error) };
                assert_eq!(check(status, error), Status::Ok);
                assert_eq!(count, 2);
            }
            Self::Cursor(cursor) => {
                let (mut zoom, mut column, mut row) = (0, 0, 0);
                let mut data = std::ptr::null();
                let mut len = 0;
                // SAFETY: a live cursor handle and writable out-parameters.
                let status = unsafe {
                    gpkg_tile_cursor_next(
                        *cursor,
                        &raw mut zoom,
                        &raw mut column,
                        &raw mut row,
                        &raw mut data,
                        &raw mut len,
                        error,
                    )
                };
                // OK with a tile, or OK with NULL data at the end.
                assert_eq!(check(status, error), Status::Ok);
            }
            Self::Writer(writer, spatial) => {
                // `pts` has only its geometry; `notes` has one text column.
                let wkb = point(1.0, 2.0);
                let null = [gpkg_value_t {
                    kind: ValueKind::Null,
                    value: gpkg_value_payload { integer: 0 },
                }];
                let (wkb, len, values, values_len) = if *spatial {
                    (wkb.as_ptr(), wkb.len(), std::ptr::null(), 0)
                } else {
                    (std::ptr::null(), 0, null.as_ptr(), 1)
                };
                let mut fid = 0;
                // SAFETY: a live writer, a WKB buffer readable for `len` or
                // NULL, no values, and writable out-parameters.
                let status = unsafe {
                    gpkg_writer_insert(
                        *writer,
                        std::ptr::null(),
                        wkb,
                        len,
                        values,
                        values_len,
                        &raw mut fid,
                        error,
                    )
                };
                assert_eq!(check(status, error), Status::Ok);
            }
            // A stream is used by being released.
            Self::Stream(_) => {}
        }
    }

    /// Frees the handle, by commit or by free for a writer.
    fn free(self, commit: bool, error: &mut gpkg_error_t) {
        match self {
            // SAFETY: each handle below is live and freed exactly once.
            Self::Layer(layer, _) => unsafe { gpkg_layer_free(layer) },
            // SAFETY: as above.
            Self::Tiles(tiles) => unsafe { gpkg_tiles_free(tiles) },
            // SAFETY: as above.
            Self::Cursor(cursor) => unsafe { gpkg_tile_cursor_free(cursor) },
            Self::Writer(writer, _) if commit => {
                // SAFETY: as above; commit frees the writer either way.
                let status = unsafe { gpkg_writer_commit(writer, error) };
                assert_eq!(check(status, error), Status::Ok);
            }
            // SAFETY: as above.
            Self::Writer(writer, _) => unsafe { gpkg_writer_free(writer) },
            // Dropping the stream calls its release callback.
            Self::Stream(stream) => drop(stream),
        }
    }
}

/// Draws one of the live children that `select` accepts, if there is one.
fn pick<T>(
    tc: &hegel::TestCase,
    children: &[Child],
    select: impl Fn(&Child) -> Option<*mut T>,
) -> Option<*mut T> {
    let candidates: Vec<*mut T> = children.iter().filter_map(select).collect();
    let last = candidates.len().checked_sub(1)?;
    let index = tc.draw(generators::integers::<usize>().min_value(0).max_value(last));
    candidates.get(index).copied()
}

/// Reads every finding of a findings handle.
fn read_findings(findings: *const gpkg_findings_t, error: &mut gpkg_error_t) {
    // SAFETY: a live findings handle.
    let count = unsafe { gpkg_findings_count(findings) };
    for index in 0..count {
        let (mut severity, mut message, mut repair) = (
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        // SAFETY: a live findings handle, an index below its count and
        // writable out-parameters.
        let status = unsafe {
            gpkg_finding_at(
                findings,
                index,
                &raw mut severity,
                &raw mut message,
                &raw mut repair,
                error,
            )
        };
        assert_eq!(check(status, error), Status::Ok);
        for text in [severity, message, repair] {
            // SAFETY: strings this library returned, or NULL.
            unsafe { gpkg_string_free(text) };
        }
    }
}

/// Any order of opening, using and freeing children, with close attempts
/// between, keeps the rule the crate documents: a close fails with
/// `HANDLE_IN_USE` and changes nothing while a child other than a findings
/// handle is alive, and succeeds otherwise; findings stay readable after the
/// close.
#[hegel::test]
fn close_succeeds_exactly_when_no_child_is_alive(tc: hegel::TestCase) {
    let mut error = sentinel();
    let gpkg = fixture();
    let mut children: Vec<Child> = Vec::new();
    let mut findings: Vec<*mut gpkg_findings_t> = Vec::new();
    let names = [
        CString::new("pts").unwrap(),
        CString::new("notes").unwrap(),
        CString::new("tiles").unwrap(),
    ];
    let steps = tc.draw(generators::integers::<usize>().min_value(1).max_value(20));
    let mut closed = false;
    for _ in 0..steps {
        let op = tc.draw(generators::integers::<u8>().min_value(0).max_value(8));
        tc.event(format!("operation {op}"));
        match op {
            0 => {
                // SAFETY: a live container and a NUL-terminated name.
                let layer = unsafe { gpkg_layer_open(gpkg, names[0].as_ptr(), &raw mut error) };
                children.push(Child::Layer(check_ptr(layer, &mut error), true));
            }
            1 => {
                // SAFETY: as above.
                let layer =
                    unsafe { gpkg_attributes_open(gpkg, names[1].as_ptr(), &raw mut error) };
                children.push(Child::Layer(check_ptr(layer, &mut error), false));
            }
            2 => {
                // SAFETY: as above.
                let tiles = unsafe { gpkg_tiles_open(gpkg, names[2].as_ptr(), &raw mut error) };
                children.push(Child::Tiles(check_ptr(tiles, &mut error)));
            }
            // A cursor from a live tiles handle.
            3 => {
                let Some(tiles) = pick(&tc, &children, |child| match child {
                    Child::Tiles(tiles) => Some(*tiles),
                    _ => None,
                }) else {
                    continue;
                };
                // SAFETY: a live tiles handle.
                let cursor = unsafe { gpkg_tiles_cursor(tiles, &raw mut error) };
                children.push(Child::Cursor(check_ptr(cursor, &mut error)));
            }
            // A writer or a stream from a live layer handle.
            4 | 5 => {
                let Some(layer) = pick(&tc, &children, |child| match child {
                    Child::Layer(layer, _) => Some(*layer),
                    _ => None,
                }) else {
                    continue;
                };
                if op == 4 {
                    // SAFETY: a live layer handle.
                    let writer = unsafe { gpkg_layer_writer(layer, &raw mut error) };
                    // A second writer joins the transaction the first began.
                    let spatial = children
                        .iter()
                        .any(|child| matches!(child, Child::Layer(l, true) if *l == layer));
                    children.push(Child::Writer(check_ptr(writer, &mut error), spatial));
                } else {
                    let mut stream = Box::new(FFI_ArrowArrayStream::empty());
                    // SAFETY: a live layer handle and a writable stream.
                    let status =
                        unsafe { gpkg_layer_read_arrow(layer, &raw mut *stream, &raw mut error) };
                    assert_eq!(check(status, &mut error), Status::Ok);
                    children.push(Child::Stream(stream));
                }
            }
            6 => {
                let mut out = std::ptr::null_mut();
                // SAFETY: a live container and a writable out-parameter.
                let status = unsafe { gpkg_validate(gpkg, &raw mut out, &raw mut error) };
                assert_eq!(check(status, &mut error), Status::Ok);
                findings.push(out);
            }
            7 => {
                if children.is_empty() {
                    continue;
                }
                let index = tc.draw(
                    generators::integers::<usize>()
                        .min_value(0)
                        .max_value(children.len() - 1),
                );
                let commit = tc.draw(generators::booleans());
                children.swap_remove(index).free(commit, &mut error);
            }
            _ => {
                // SAFETY: a live container; it is destroyed only on OK.
                let status = unsafe { gpkg_close(gpkg, &raw mut error) };
                let status = check(status, &mut error);
                if children.is_empty() {
                    assert_eq!(status, Status::Ok);
                    closed = true;
                    break;
                }
                assert_eq!(status, Status::HandleInUse);
                // The container is still open and usable.
                // SAFETY: a live container.
                let version = unsafe { gpkg_version(gpkg, &raw mut error) };
                // SAFETY: a string this library returned.
                unsafe { gpkg_string_free(check_ptr(version, &mut error)) };
            }
        }
        for child in &mut children {
            child.exercise(&mut error);
        }
    }
    for child in children.drain(..) {
        child.free(false, &mut error);
    }
    if !closed {
        // SAFETY: a live container with no children.
        let status = unsafe { gpkg_close(gpkg, &raw mut error) };
        assert_eq!(check(status, &mut error), Status::Ok);
    }
    for handle in findings {
        read_findings(handle, &mut error);
        // SAFETY: a findings handle freed exactly once.
        unsafe { gpkg_findings_free(handle) };
    }
}
