//! `gpkg copy`, the dogfood command.
//!
//! The full circle: a file GDAL wrote, copied through this crate, comes out
//! clean. `gpkg validate` stands in for the external validators here, which
//! run in their own CI job.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use geopackage::core::gpb::body_offset;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("geopackage")
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn gpkg(args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gpkg"));
    for arg in args {
        command.arg(arg);
    }
    command.output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

/// Copy `name` into a fresh directory, returning the copy's path.
fn copy_of(name: &str) -> (tempfile::TempDir, PathBuf, Output) {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.gpkg");
    let src = fixture(name);
    let output = gpkg(&["copy", src.to_str().unwrap(), dst.to_str().unwrap()]);
    (dir, dst, output)
}

/// Every geometry blob of every feature table, keyed by table, in rowid order.
fn geometries(path: &Path) -> Vec<(String, Vec<Option<Vec<u8>>>)> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let tables: Vec<String> = conn
        .prepare(
            "SELECT table_name FROM gpkg_contents WHERE data_type = 'features' ORDER BY table_name",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(std::result::Result::unwrap)
        .collect();

    tables
        .into_iter()
        .map(|table| {
            let column: String = conn
                .query_row(
                    "SELECT column_name FROM gpkg_geometry_columns WHERE table_name = ?1",
                    [&table],
                    |row| row.get(0),
                )
                .unwrap();
            let blobs = conn
                .prepare(&format!(
                    "SELECT \"{column}\" FROM \"{table}\" ORDER BY rowid"
                ))
                .unwrap()
                .query_map([], |row| row.get::<_, Option<Vec<u8>>>(0))
                .unwrap()
                .map(std::result::Result::unwrap)
                .collect();
            (table, blobs)
        })
        .collect()
}

#[test]
fn a_gdal_file_copied_through_us_validates_clean() {
    let (_dir, dst, output) = copy_of("gdal_multilayer_1_4.gpkg");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report = gpkg(&["validate", dst.to_str().unwrap()]);
    assert!(
        report.status.success(),
        "the copy should have no errors: {}",
        stdout(&report)
    );
}

#[test]
fn every_layer_and_row_crosses() {
    let (_dir, _dst, output) = copy_of("gdal_multilayer_1_4.gpkg");
    assert!(output.status.success());
    let out = stdout(&output);

    for (layer, rows) in [
        ("emptyline", 2),
        ("lines", 2),
        ("points", 3),
        ("points3d", 2),
        ("polygons", 2),
    ] {
        assert!(out.contains(&format!("{layer}: {rows} rows")), "{out}");
    }

    // The source's indexed layers are indexed in the copy, and its unindexed
    // ones are left unindexed rather than silently gaining one.
    assert!(out.contains("points: 3 rows, indexed"), "{out}");
    assert!(out.contains("lines: 2 rows\n"), "{out}");
}

#[test]
fn a_layer_with_z_survives_the_crossing() {
    // The z flag has to be carried onto the destination's geometry column:
    // defaulting it declares the dimension Prohibited and the first row with a
    // z is rejected. This layer is the one that catches it.
    let (_dir, dst, output) = copy_of("gdal_multilayer_1_4.gpkg");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout(&output).contains("points3d: 2 rows"));

    let z: i64 = rusqlite::Connection::open(&dst)
        .unwrap()
        .query_row(
            "SELECT z FROM gpkg_geometry_columns WHERE table_name = 'points3d'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(z, 1, "the destination should declare z as the source does");
}

#[test]
fn curve_geometries_cross_byte_for_byte() {
    // The reason geometry goes across as WKB rather than through geo-types: an
    // arc has no geo-traits representation, so a parse-and-rewrite would lose
    // these entirely.
    let (_dir, dst, output) = copy_of("gdal_curves.gpkg");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let source = geometries(&fixture("gdal_curves.gpkg"));
    let copied = geometries(&dst);
    assert_eq!(source.len(), copied.len());

    for ((src_table, src_blobs), (dst_table, dst_blobs)) in source.iter().zip(&copied) {
        assert_eq!(src_table, dst_table);
        assert_eq!(src_blobs.len(), dst_blobs.len(), "{src_table}");
        for (a, b) in src_blobs.iter().zip(dst_blobs) {
            // The GPB header may legitimately differ (the envelope is
            // recomputed), so the WKB body is what must match.
            match (a, b) {
                (Some(a), Some(b)) => {
                    let a_body = &a[body_offset(a).unwrap()..];
                    let b_body = &b[body_offset(b).unwrap()..];
                    assert_eq!(a_body, b_body, "{src_table}");
                }
                (None, None) => {}
                _ => panic!("{src_table}: one side had a NULL geometry and the other did not"),
            }
        }
    }
}

#[test]
fn what_was_not_carried_is_named() {
    let (_dir, _dst, output) = copy_of("gdal_multilayer_1_4.gpkg");
    let out = stdout(&output);
    // The source registers gpkg_metadata, which this command does not carry.
    assert!(out.contains("not copied"), "{out}");
    assert!(out.contains("gpkg_metadata"), "{out}");
}

#[test]
fn extensions_the_copy_registers_itself_are_not_reported_as_lost() {
    // Writing a curve layer registers its own gpkg_geom_<TYPE>, so those are
    // present in the copy and must not be listed as left behind.
    let (_dir, _dst, output) = copy_of("gdal_curves.gpkg");
    let out = stdout(&output);
    assert!(!out.contains("gpkg_geom_CIRCULARSTRING"), "{out}");
    assert!(!out.contains("not copied"), "{out}");
}

#[test]
fn an_existing_destination_is_refused_rather_than_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("out.gpkg");
    std::fs::write(&dst, b"not a geopackage").unwrap();
    let src = fixture("gdal_multilayer_1_4.gpkg");

    let output = gpkg(&["copy", src.to_str().unwrap(), dst.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("already exists"),
        "{:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Untouched.
    assert_eq!(std::fs::read(&dst).unwrap(), b"not a geopackage");
}
