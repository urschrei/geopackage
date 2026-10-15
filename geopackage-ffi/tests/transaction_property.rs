//! Property tests over the C transaction calls, against a model of what is
//! committed and what is pending.
//!
//! One `unsafe` block per call, as the rest of this crate's tests do.

use std::ffi::CString;

use geopackage::core::types::GeometryType;
use geopackage::{GeoPackage, GeometrySpec, TableSchemaBuilder};
use geopackage_ffi::{
    Status, gpkg_begin, gpkg_close, gpkg_commit, gpkg_error_clear, gpkg_error_t,
    gpkg_in_transaction, gpkg_layer_count, gpkg_layer_create_spatial_index,
    gpkg_layer_drop_spatial_index, gpkg_layer_free, gpkg_layer_has_spatial_index, gpkg_layer_open,
    gpkg_layer_t, gpkg_layer_writer, gpkg_open, gpkg_rollback, gpkg_t, gpkg_writer_commit,
    gpkg_writer_free, gpkg_writer_insert,
};
use hegel::generators;

/// An error slot holding a value no call writes on success.
fn sentinel() -> gpkg_error_t {
    gpkg_error_t {
        code: Status::Other,
        message: std::ptr::null_mut(),
    }
}

/// Checks the error protocol for a call that returned `status`.
fn check(status: Status, error: &mut gpkg_error_t) -> Status {
    if status == Status::Ok {
        assert_eq!(error.code, Status::Other, "a success wrote the slot");
        assert!(error.message.is_null(), "a success wrote the slot");
        return status;
    }
    assert!(!error.message.is_null(), "a failure without a message");
    assert_eq!(error.code, status);
    // SAFETY: a slot this library filled in.
    unsafe { gpkg_error_clear(error) };
    *error = sentinel();
    status
}

/// A little-endian WKB point.
fn point(x: f64, y: f64) -> Vec<u8> {
    let mut bytes = vec![1u8];
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&x.to_le_bytes());
    bytes.extend_from_slice(&y.to_le_bytes());
    bytes
}

/// What the file holds: committed, and visible on the connection.
#[derive(Debug, Clone, Copy)]
struct State {
    rows: u64,
    indexed: bool,
}

/// The documented transaction rules.
struct Model {
    committed: State,
    visible: State,
    open: bool,
}

impl Model {
    /// A change outside a transaction is committed when its call returns.
    fn change(&mut self, change: impl Fn(&mut State)) {
        change(&mut self.visible);
        if !self.open {
            self.committed = self.visible;
        }
    }
}

/// Inserts `rows` points through a writer, then commits or frees it.
fn write(layer: *const gpkg_layer_t, rows: u64, commit: bool, error: &mut gpkg_error_t) {
    // SAFETY: a live layer handle.
    let writer = unsafe { gpkg_layer_writer(layer, error) };
    assert!(!writer.is_null());
    for i in 0..rows {
        #[expect(clippy::cast_precision_loss, reason = "a small count")]
        let wkb = point(i as f64, 0.0);
        // SAFETY: a live writer, a WKB buffer readable for its length, no
        // values.
        let status = unsafe {
            gpkg_writer_insert(
                writer,
                std::ptr::null(),
                wkb.as_ptr(),
                wkb.len(),
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                error,
            )
        };
        assert_eq!(check(status, error), Status::Ok);
    }
    if commit {
        // SAFETY: a live writer, committed once.
        let status = unsafe { gpkg_writer_commit(writer, error) };
        assert_eq!(check(status, error), Status::Ok);
    } else {
        // SAFETY: a live writer, freed once.
        unsafe { gpkg_writer_free(writer) };
    }
}

/// What the connection shows: whether a transaction is open, the row count
/// and whether the layer is indexed.
fn observe(
    gpkg: *const gpkg_t,
    layer: *const gpkg_layer_t,
    error: &mut gpkg_error_t,
) -> (bool, State) {
    // SAFETY: a live container.
    let open = unsafe { gpkg_in_transaction(gpkg) };
    let mut rows = 0;
    // SAFETY: a live layer and a writable out-parameter.
    let status = unsafe { gpkg_layer_count(layer, &raw mut rows, error) };
    assert_eq!(check(status, error), Status::Ok);
    let mut indexed = false;
    // SAFETY: as above.
    let status = unsafe { gpkg_layer_has_spatial_index(layer, &raw mut indexed, error) };
    assert_eq!(check(status, error), Status::Ok);
    (open, State { rows, indexed })
}

/// Any sequence of begins, commits, rollbacks, writer commits and frees, and
/// index changes keeps the documented rules: each call succeeds exactly when
/// the transaction state is what it needs; a write outside a transaction is
/// committed when it returns; a writer inside one only stages; a rollback
/// undoes rows and schema changes; closing rolls back what is open.
///
/// Each case creates a file to reopen, which costs far more per case on
/// Windows CI (issue #44), so this property runs 200 cases rather than the
/// suite's 2000.
#[hegel::test(test_cases = 200)]
fn transactions_follow_the_documented_rules(tc: hegel::TestCase) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.gpkg");
    {
        let gpkg = GeoPackage::create(&path).unwrap();
        gpkg.create_layer(
            &TableSchemaBuilder::new("pts")
                .geometry(GeometrySpec::new(GeometryType::Point, 4326))
                .spatial_index(true),
        )
        .unwrap();
        gpkg.close().unwrap();
    }
    let mut error = sentinel();
    let c_path = CString::new(path.to_str().unwrap()).unwrap();
    // SAFETY: a NUL-terminated path and a writable error slot.
    let gpkg = unsafe { gpkg_open(c_path.as_ptr(), &raw mut error) };
    assert!(!gpkg.is_null());
    let name = CString::new("pts").unwrap();
    // SAFETY: a live container and a NUL-terminated name.
    let layer = unsafe { gpkg_layer_open(gpkg, name.as_ptr(), &raw mut error) };
    assert!(!layer.is_null());

    let start = State {
        rows: 0,
        indexed: true,
    };
    let mut model = Model {
        committed: start,
        visible: start,
        open: false,
    };
    let steps = tc.draw(generators::integers::<usize>().min_value(1).max_value(12));
    for _ in 0..steps {
        let op = tc.draw(generators::integers::<u8>().min_value(0).max_value(4));
        tc.event(format!("operation {op}, open {}", model.open));
        match op {
            0 => {
                // SAFETY: a live container.
                let status = check(unsafe { gpkg_begin(gpkg, &raw mut error) }, &mut error);
                if model.open {
                    assert_eq!(status, Status::InvalidArgument);
                } else {
                    assert_eq!(status, Status::Ok);
                    model.open = true;
                }
            }
            1 | 2 => {
                let status = if op == 1 {
                    // SAFETY: a live container.
                    unsafe { gpkg_commit(gpkg, &raw mut error) }
                } else {
                    // SAFETY: a live container.
                    unsafe { gpkg_rollback(gpkg, &raw mut error) }
                };
                if model.open {
                    assert_eq!(check(status, &mut error), Status::Ok);
                    if op == 1 {
                        model.committed = model.visible;
                    } else {
                        model.visible = model.committed;
                    }
                    model.open = false;
                } else {
                    assert_eq!(check(status, &mut error), Status::InvalidArgument);
                }
            }
            3 => {
                let rows = tc.draw(generators::integers::<u64>().min_value(1).max_value(3));
                let commit = tc.draw(generators::booleans());
                write(layer, rows, commit, &mut error);
                // Inside a transaction, a writer stages either way; outside
                // one, freeing it discards its rows.
                if model.open || commit {
                    model.change(|state| state.rows += rows);
                }
            }
            _ => {
                let status = if model.visible.indexed {
                    // SAFETY: a live layer.
                    unsafe { gpkg_layer_drop_spatial_index(layer, &raw mut error) }
                } else {
                    // SAFETY: a live layer.
                    unsafe { gpkg_layer_create_spatial_index(layer, &raw mut error) }
                };
                assert_eq!(check(status, &mut error), Status::Ok);
                model.change(|state| state.indexed = !state.indexed);
            }
        }
        let (open, visible) = observe(gpkg, layer, &mut error);
        assert_eq!(open, model.open);
        assert_eq!(
            (visible.rows, visible.indexed),
            (model.visible.rows, model.visible.indexed)
        );
    }

    // SAFETY: a live layer, freed once.
    unsafe { gpkg_layer_free(layer) };
    // SAFETY: a live container with no children; closing rolls back.
    let status = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(check(status, &mut error), Status::Ok);

    let reopened = GeoPackage::open(&path).unwrap();
    let layer = reopened.layer("pts").unwrap();
    assert_eq!(layer.count().unwrap(), model.committed.rows);
    assert_eq!(layer.has_spatial_index().unwrap(), model.committed.indexed);
}
