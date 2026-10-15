//! Property tests over the C reads of catalogue rows: spatial reference
//! systems, extensions and findings, against the Rust API.
//!
//! The rows hold any text, including an interior NUL, which no C string can
//! hold. Each out-parameter is requested or not, as drawn. A read either
//! returns what the Rust API returns, or fails with
//! `GPKG_STATUS_INVALID_ARGUMENT` and writes no out-parameter.
//!
//! One `unsafe` block per call, as the rest of this crate's tests do.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::ffi::{CStr, CString, c_char};

use geopackage::{GeoPackage, Srs};
use geopackage_ffi::{
    Status, gpkg_close, gpkg_create, gpkg_error_clear, gpkg_error_t, gpkg_extension_at,
    gpkg_extensions_count, gpkg_finding_at, gpkg_findings_count, gpkg_findings_free, gpkg_srs,
    gpkg_string_free, gpkg_t, gpkg_validate,
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

/// A value no call writes to an out-parameter: a call that fails must leave
/// it, and a call that succeeds must replace it.
const UNWRITTEN: *mut c_char = std::ptr::dangling_mut();

/// An out-parameter for a string, requested or not.
struct Out(Option<*mut c_char>);

impl Out {
    fn draw(tc: &hegel::TestCase) -> Self {
        Self(tc.draw(generators::booleans()).then_some(UNWRITTEN))
    }

    fn pointer(&mut self) -> *mut *mut c_char {
        self.0
            .as_mut()
            .map_or(std::ptr::null_mut(), std::ptr::from_mut)
    }

    fn requested(&self) -> bool {
        self.0.is_some()
    }

    /// Checks the out-parameter after a call, and frees what it received.
    fn finish(self, ok: bool, expected: Option<&str>) {
        let Some(written) = self.0 else {
            return;
        };
        if !ok {
            assert_eq!(written, UNWRITTEN, "a failed call wrote an out-parameter");
            return;
        }
        let got = (!written.is_null()).then(|| {
            // SAFETY: a string this library returned.
            unsafe { CStr::from_ptr(written) }
                .to_str()
                .unwrap()
                .to_owned()
        });
        assert_eq!(got.as_deref(), expected);
        // SAFETY: a string this library returned, or NULL.
        unsafe { gpkg_string_free(written) };
    }
}

/// Whether every string a call would return to a requested out-parameter can
/// be a C string.
fn representable(outs: &[(&Out, Option<&str>)]) -> bool {
    outs.iter()
        .all(|(out, text)| !out.requested() || text.is_none_or(|text| !text.contains('\0')))
}

/// Draws text: usually any text, sometimes with an interior NUL.
fn draw_text(tc: &hegel::TestCase) -> String {
    let text = tc.draw(generators::text().max_size(8));
    if tc.draw(generators::weighted_booleans(0.2)) {
        format!("{text}\0{text}")
    } else {
        text
    }
}

fn open() -> (*mut gpkg_t, gpkg_error_t) {
    let mut error = sentinel();
    let path = CString::new(":memory:").unwrap();
    // SAFETY: a NUL-terminated path and a writable error slot.
    let gpkg = unsafe { gpkg_create(path.as_ptr(), &raw mut error) };
    assert!(!gpkg.is_null());
    (gpkg, error)
}

fn close(gpkg: *mut gpkg_t, error: &mut gpkg_error_t) {
    // SAFETY: a live container with no children.
    let status = unsafe { gpkg_close(gpkg, error) };
    assert_eq!(check(status, error), Status::Ok);
}

/// `gpkg_srs` reports the row the Rust API reads, for any row text and any
/// set of requested out-parameters, and `NOT_FOUND` for an absent id.
#[hegel::test]
fn gpkg_srs_reports_the_row(tc: hegel::TestCase) {
    let (gpkg, mut error) = open();
    // SAFETY: a live container handle; `gpkg_t` is the container.
    let rust: &GeoPackage = unsafe { (*gpkg).gpkg() };
    let srs = Srs {
        name: draw_text(&tc),
        srs_id: 90_000,
        organization: draw_text(&tc),
        organization_coordsys_id: tc.draw(generators::integers::<i32>()),
        definition: draw_text(&tc),
        description: None,
        definition_wkt2: tc.draw(generators::booleans()).then(|| draw_text(&tc)),
        epoch: tc.draw(generators::booleans()).then(|| {
            tc.draw(
                generators::floats::<f64>()
                    .allow_nan(false)
                    .allow_infinity(false),
            )
        }),
    };
    rust.add_srs(&srs).unwrap();
    let srs_id = tc.draw(generators::sampled_from(vec![90_000, 4326, 0, 12_345]));
    let row = rust.srs(srs_id).unwrap();

    let (mut name, mut organization, mut definition, mut wkt2) = (
        Out::draw(&tc),
        Out::draw(&tc),
        Out::draw(&tc),
        Out::draw(&tc),
    );
    let mut coordsys = i32::MIN;
    let mut epoch = -1.5;
    // SAFETY: a live container, out-parameters writable or NULL.
    let status = unsafe {
        gpkg_srs(
            gpkg,
            srs_id,
            name.pointer(),
            organization.pointer(),
            &raw mut coordsys,
            definition.pointer(),
            wkt2.pointer(),
            &raw mut epoch,
            &raw mut error,
        )
    };
    let status = check(status, &mut error);
    let Some(row) = row else {
        assert_eq!(status, Status::NotFound);
        for out in [name, organization, definition, wkt2] {
            out.finish(false, None);
        }
        return close(gpkg, &mut error);
    };
    let fits = representable(&[
        (&name, Some(&row.name)),
        (&organization, Some(&row.organization)),
        (&definition, Some(&row.definition)),
        (&wkt2, row.definition_wkt2.as_deref()),
    ]);
    tc.event(if fits {
        "representable"
    } else {
        "interior NUL"
    });
    let ok = status == Status::Ok;
    if fits {
        assert_eq!(status, Status::Ok);
        assert_eq!(coordsys, row.organization_coordsys_id);
        match row.epoch {
            Some(value) => assert_eq!(epoch.to_bits(), value.to_bits()),
            None => assert!(epoch.is_nan()),
        }
    } else {
        assert_eq!(status, Status::InvalidArgument);
    }
    name.finish(ok, Some(&row.name));
    organization.finish(ok, Some(&row.organization));
    definition.finish(ok, Some(&row.definition));
    wkt2.finish(ok, row.definition_wkt2.as_deref());
    close(gpkg, &mut error);
}

/// Draws extension rows: a known or any name, with or without a table and a
/// column, and any scope.
fn insert_extensions(tc: &hegel::TestCase, gpkg: &GeoPackage) {
    gpkg.connection()
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS gpkg_extensions (table_name TEXT, \
             column_name TEXT, extension_name TEXT NOT NULL, \
             definition TEXT NOT NULL, scope TEXT NOT NULL)",
        )
        .unwrap();
    let rows = tc.draw(generators::integers::<usize>().min_value(0).max_value(4));
    for _ in 0..rows {
        let name = if tc.draw(generators::booleans()) {
            tc.draw(generators::sampled_from(vec![
                "gpkg_rtree_index",
                "gpkg_srs_id_trigger",
                "gpkg_geom_CIRCULARSTRING",
            ]))
            .to_owned()
        } else {
            draw_text(tc)
        };
        let table = tc.draw(generators::booleans()).then(|| draw_text(tc));
        let column = tc.draw(generators::booleans()).then(|| draw_text(tc));
        let scope = if tc.draw(generators::booleans()) {
            tc.draw(generators::sampled_from(vec!["read-write", "write-only"]))
                .to_owned()
        } else {
            draw_text(tc)
        };
        gpkg.connection()
            .execute(
                "INSERT INTO gpkg_extensions VALUES (?1, ?2, ?3, 'def', ?4)",
                rusqlite::params![table, column, name, scope],
            )
            .unwrap();
    }
}

/// `gpkg_extensions_count` and `gpkg_extension_at` report the rows the Rust
/// API reads, in order, for any row text and any requested out-parameters;
/// an index past the end is `NOT_FOUND`.
#[hegel::test]
fn extension_reads_report_the_rows(tc: hegel::TestCase) {
    let (gpkg, mut error) = open();
    // SAFETY: a live container handle.
    let rust: &GeoPackage = unsafe { (*gpkg).gpkg() };
    insert_extensions(&tc, rust);
    let rows = rust.extensions().unwrap();
    let mut count = usize::MAX;
    // SAFETY: a live container and a writable out-parameter.
    let status = unsafe { gpkg_extensions_count(gpkg, &raw mut count, &raw mut error) };
    assert_eq!(check(status, &mut error), Status::Ok);
    assert_eq!(count, rows.len());
    for index in 0..=count {
        let mut outs: [Out; 5] = std::array::from_fn(|_| Out::draw(&tc));
        let [name, table, column, scope, support] = &mut outs;
        // SAFETY: a live container, out-parameters writable or NULL.
        let status = unsafe {
            gpkg_extension_at(
                gpkg,
                index,
                name.pointer(),
                table.pointer(),
                column.pointer(),
                scope.pointer(),
                support.pointer(),
                &raw mut error,
            )
        };
        let status = check(status, &mut error);
        let Some(row) = rows.get(index) else {
            assert_eq!(status, Status::NotFound);
            for out in outs {
                out.finish(false, None);
            }
            continue;
        };
        let scope_text = row.scope.to_string();
        let support_text = row.support().to_string();
        let expected = [
            Some(row.name.as_str()),
            row.table_name.as_deref(),
            row.column_name.as_deref(),
            Some(scope_text.as_str()),
            Some(support_text.as_str()),
        ];
        let pairs: Vec<(&Out, Option<&str>)> = outs.iter().zip(expected).collect();
        let fits = representable(&pairs);
        tc.event(if fits {
            "representable"
        } else {
            "interior NUL"
        });
        assert_eq!(
            status,
            if fits {
                Status::Ok
            } else {
                Status::InvalidArgument
            }
        );
        for (out, text) in outs.into_iter().zip(expected) {
            out.finish(status == Status::Ok, text);
        }
    }
    close(gpkg, &mut error);
}

/// `gpkg_validate` and `gpkg_finding_at` report the findings the Rust API
/// returns, in order, with the severity, message and repair text the Rust
/// API gives them; an index past the end is `NOT_FOUND`.
#[hegel::test]
fn findings_report_what_validate_returns(tc: hegel::TestCase) {
    let (gpkg, mut error) = open();
    // SAFETY: a live container handle.
    let rust: &GeoPackage = unsafe { (*gpkg).gpkg() };
    insert_extensions(&tc, rust);
    let findings = rust.validate().unwrap();
    let mut handle = std::ptr::null_mut();
    // SAFETY: a live container and a writable out-parameter.
    let status = unsafe { gpkg_validate(gpkg, &raw mut handle, &raw mut error) };
    assert_eq!(check(status, &mut error), Status::Ok);
    close(gpkg, &mut error);
    // SAFETY: a live findings handle, which outlives its container.
    let count = unsafe { gpkg_findings_count(handle) };
    assert_eq!(count, findings.len());
    for index in 0..=count {
        let mut outs: [Out; 3] = std::array::from_fn(|_| Out::draw(&tc));
        let [severity, message, repair] = &mut outs;
        // SAFETY: a live findings handle, out-parameters writable or NULL.
        let status = unsafe {
            gpkg_finding_at(
                handle,
                index,
                severity.pointer(),
                message.pointer(),
                repair.pointer(),
                &raw mut error,
            )
        };
        let status = check(status, &mut error);
        let Some(finding) = findings.get(index) else {
            assert_eq!(status, Status::NotFound);
            for out in outs {
                out.finish(false, None);
            }
            continue;
        };
        let severity_text = finding.severity().to_string();
        let message_text = finding.to_string();
        let expected = [
            Some(severity_text.as_str()),
            Some(message_text.as_str()),
            finding.repair(),
        ];
        let pairs: Vec<(&Out, Option<&str>)> = outs.iter().zip(expected).collect();
        let fits = representable(&pairs);
        tc.event(if fits {
            "representable"
        } else {
            "interior NUL"
        });
        assert_eq!(
            status,
            if fits {
                Status::Ok
            } else {
                Status::InvalidArgument
            }
        );
        for (out, text) in outs.into_iter().zip(expected) {
            out.finish(status == Status::Ok, text);
        }
    }
    // SAFETY: a findings handle, freed once.
    unsafe { gpkg_findings_free(handle) };
}

/// `gpkg_add_epsg_srs` succeeds or fails as the Rust API does for the same
/// codes, and leaves the same rows.
#[hegel::test]
fn gpkg_add_epsg_srs_agrees_with_the_rust_api(tc: hegel::TestCase) {
    let (gpkg, mut error) = open();
    let twin = GeoPackage::create(std::path::Path::new(":memory:")).unwrap();
    let calls = tc.draw(generators::integers::<usize>().min_value(1).max_value(4));
    for _ in 0..calls {
        let code = if tc.draw(generators::booleans()) {
            tc.draw(generators::sampled_from(vec![
                4326, 3857, 27700, 32630, 4979, 999_999,
            ]))
        } else {
            tc.draw(generators::integers::<i32>())
        };
        // SAFETY: a live container and a writable error slot.
        let status = check(
            unsafe { geopackage_ffi::gpkg_add_epsg_srs(gpkg, code, &raw mut error) },
            &mut error,
        );
        let expected = twin
            .add_epsg_srs(code)
            .map_or_else(|e| Status::from(&e), |_| Status::Ok);
        tc.event(format!("{status:?}"));
        assert_eq!(status, expected, "EPSG:{code}");
    }
    // SAFETY: a live container handle.
    let rows = unsafe { (*gpkg).gpkg() }.srs_list().unwrap();
    assert_eq!(rows, twin.srs_list().unwrap());
    close(gpkg, &mut error);
}
