//! Property tests over opening files through the C API: `gpkg_open`,
//! `gpkg_open_read_only`, `gpkg_version` and the open warnings, against the
//! Rust API, for files of every kind a caller may pass.
//!
//! One `unsafe` block per call, as the rest of this crate's tests do.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::ffi::{CStr, CString, c_char};
use std::path::Path;

use geopackage::GeoPackage;
use geopackage_ffi::{
    Status, gpkg_close, gpkg_error_clear, gpkg_error_t, gpkg_open, gpkg_open_read_only,
    gpkg_open_warning, gpkg_open_warning_count, gpkg_string_free, gpkg_t, gpkg_version,
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

/// Takes a string a call returned, or the status of a NULL return.
fn take(text: *mut c_char, error: &mut gpkg_error_t) -> Result<String, Status> {
    if text.is_null() {
        return Err(check(error.code, error));
    }
    check(Status::Ok, error);
    // SAFETY: a string this library returned.
    let owned = unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned();
    // SAFETY: freed once.
    unsafe { gpkg_string_free(text) };
    Ok(owned)
}

/// Writes a file of a drawn kind at `path`, or nothing.
fn make(tc: &hegel::TestCase, path: &Path) -> u8 {
    let kind = tc.draw(generators::integers::<u8>().min_value(0).max_value(8));
    tc.event(format!("kind {kind}"));
    let sql = |statement: &str| {
        let gpkg = GeoPackage::create(path).unwrap();
        gpkg.connection().execute_batch(statement).unwrap();
        gpkg.close().unwrap();
    };
    match kind {
        0 => GeoPackage::create(path).unwrap().close().unwrap(),
        1 => sql("PRAGMA application_id = 1196437808"),
        // A feature table dropped behind the catalogue's back.
        2 => {
            let gpkg = GeoPackage::create(path).unwrap();
            gpkg.create_layer(&geopackage::TableSchemaBuilder::new("pts").geometry(
                geopackage::GeometrySpec::new(geopackage::core::types::GeometryType::Point, 4326),
            ))
            .unwrap();
            gpkg.connection().execute_batch("DROP TABLE pts").unwrap();
            gpkg.close().unwrap();
        }
        3 => sql("CREATE TABLE notes (id INTEGER PRIMARY KEY); \
             INSERT INTO gpkg_contents (table_name, data_type) VALUES ('NOTES', 'attributes')"),
        4 => sql(
            "CREATE TABLE gpkg_extensions (table_name TEXT, column_name TEXT, \
             extension_name TEXT NOT NULL, definition TEXT NOT NULL, scope TEXT NOT NULL); \
             INSERT INTO gpkg_extensions VALUES (NULL, NULL, 'acme_x', 'def', 'read-write')",
        ),
        5 => {
            let conn = rusqlite::Connection::open(path).unwrap();
            conn.execute_batch("CREATE TABLE t (x)").unwrap();
        }
        6 => std::fs::write(
            path,
            tc.draw(generators::vecs(generators::integers::<u8>()).max_size(64)),
        )
        .unwrap(),
        7 => std::fs::write(path, []).unwrap(),
        _ => {}
    }
    kind
}

/// What a container reports after opening: its version and its warnings,
/// with the answer to an index past the end.
type Opened = (Result<String, Status>, Vec<Result<String, Status>>);

fn report_c(gpkg: *const gpkg_t, error: &mut gpkg_error_t) -> Opened {
    // SAFETY: a live container.
    let version = take(unsafe { gpkg_version(gpkg, error) }, error);
    // SAFETY: a live container.
    let count = unsafe { gpkg_open_warning_count(gpkg) };
    let warnings = (0..=count)
        // SAFETY: a live container.
        .map(|index| take(unsafe { gpkg_open_warning(gpkg, index, error) }, error))
        .collect();
    (version, warnings)
}

fn report_rust(gpkg: &GeoPackage) -> Opened {
    let mut warnings: Vec<Result<String, Status>> = gpkg
        .open_warnings()
        .iter()
        .map(|warning| Ok(warning.to_string()))
        .collect();
    warnings.push(Err(Status::NotFound));
    (Ok(gpkg.version().as_str().to_owned()), warnings)
}

/// `gpkg_open` and `gpkg_open_read_only` open exactly the files the Rust
/// opens they wrap open, fail with the status of the Rust error otherwise,
/// and report the version and warnings the Rust API reports.
///
/// Each case writes a file, which costs far more per case on Windows CI
/// (issue #44), so this property runs 200 cases rather than the suite's
/// 2000.
#[hegel::test(test_cases = 200)]
fn opens_agree_with_the_rust_api(tc: hegel::TestCase) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.gpkg");
    let kind = make(&tc, &path);
    let read_only = tc.draw(generators::booleans());
    let mut error = sentinel();
    let c_path = CString::new(path.to_str().unwrap()).unwrap();
    let gpkg = if read_only {
        // SAFETY: a NUL-terminated path and a writable error slot.
        unsafe { gpkg_open_read_only(c_path.as_ptr(), &raw mut error) }
    } else {
        // SAFETY: as above.
        unsafe { gpkg_open(c_path.as_ptr(), &raw mut error) }
    };
    let c = if gpkg.is_null() {
        Err(check(error.code, &mut error))
    } else {
        check(Status::Ok, &mut error);
        let report = report_c(gpkg, &mut error);
        // SAFETY: a live container with no children.
        let status = unsafe { gpkg_close(gpkg, &raw mut error) };
        assert_eq!(check(status, &mut error), Status::Ok);
        Ok(report)
    };
    let rust = if read_only {
        GeoPackage::open_read_only_lenient(&path)
    } else {
        GeoPackage::open(&path)
    };
    let r = rust.as_ref().map(report_rust).map_err(Status::from);
    tc.event(format!("{:?}", c.as_ref().err()));
    assert_eq!(c, r, "read-only {read_only}");
    // A plain SQLite database, bytes that are not a database, and an empty
    // file are each not a GeoPackage.
    if (5..=7).contains(&kind) {
        assert_eq!(c.err(), Some(Status::NotAGeoPackage), "kind {kind}");
    }
}
