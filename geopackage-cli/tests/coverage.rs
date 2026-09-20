//! `gpkg coverage info` and `gpkg coverage get`.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FIXTURE: &str = "gdal_coverage.gpkg";

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

#[test]
fn info_reports_what_the_samples_mean() {
    let path = fixture(FIXTURE);
    let output = gpkg(&["coverage", "info", path.to_str().unwrap()]);
    assert!(output.status.success());
    let out = stdout(&output);

    assert!(out.contains(r#"coverage "coverage""#), "{out}");
    assert!(out.contains("tiles:    1"), "{out}");
    // The four numbers a reader needs before a sample means anything.
    assert!(out.contains("samples:  float"), "{out}");
    assert!(out.contains("scale:    1 offset: 0"), "{out}");
    assert!(out.contains("null:     -9999"), "{out}");
    assert!(out.contains("cells:    grid-value-is-center"), "{out}");
    assert!(
        out.contains("WGS 84 / Pseudo-Mercator (EPSG:3857)"),
        "{out}"
    );
    // And what the payloads actually are, read from one header rather than
    // assumed from the datatype.
    assert!(out.contains("payloads: image/tiff Float32 64x64"), "{out}");
}

#[test]
fn info_on_a_file_with_no_coverage_says_so() {
    let path = fixture("gdal_tiles.gpkg");
    let output = gpkg(&["coverage", "info", path.to_str().unwrap()]);
    assert!(output.status.success());
    assert!(
        stdout(&output).contains("no tiled gridded coverages"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn a_pyramid_is_not_a_coverage() {
    // The two commands do not overlap, which is the point of having both.
    let path = fixture("gdal_tiles.gpkg");
    let output = gpkg(&["coverage", "info", path.to_str().unwrap(), "tiles"]);
    assert!(!output.status.success());
    let err = String::from_utf8(output.stderr).unwrap();
    assert!(err.contains("data_type"), "{err}");

    let path = fixture(FIXTURE);
    let output = gpkg(&["tiles", "info", path.to_str().unwrap(), "coverage"]);
    assert!(!output.status.success());
}

#[test]
fn get_writes_the_stored_bytes_and_describes_them() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("tile.tif");
    let path = fixture(FIXTURE);
    let output = gpkg(&[
        "coverage",
        "get",
        path.to_str().unwrap(),
        "coverage",
        "0",
        "0",
        "0",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert!(output.status.success());
    let printed = stdout(&output);
    assert!(printed.contains("image/tiff Float32 64x64"), "{printed}");
    // The statistics are the only thing this crate can say about the samples,
    // since it decodes none of them.
    assert!(printed.contains("recorded range 0.5 to 96.5"), "{printed}");

    // The bytes are the file's, untouched: a TIFF, and the size the header
    // declared.
    let written = std::fs::read(&out).unwrap();
    assert_eq!(&written[..4], b"II\x2a\x00");
    assert!(written.len() > 100);
}

#[test]
fn get_reports_a_tile_that_is_not_there() {
    let path = fixture(FIXTURE);
    let output = gpkg(&[
        "coverage",
        "get",
        path.to_str().unwrap(),
        "coverage",
        "0",
        "9",
        "9",
    ]);
    assert!(!output.status.success());
    let err = String::from_utf8(output.stderr).unwrap();
    assert!(err.contains("no tile at 0/9/9"), "{err}");
}

#[test]
fn the_file_summary_lists_a_coverage_as_one() {
    let path = fixture(FIXTURE);
    let output = gpkg(&["info", path.to_str().unwrap()]);
    assert!(output.status.success());
    let out = stdout(&output);
    assert!(out.contains(r#"coverage "coverage""#), "{out}");
    assert!(out.contains("samples:  float"), "{out}");
    // It is not listed as a tile pyramid, which is what `tiles` would print.
    assert!(!out.contains(r#"tiles "coverage""#), "{out}");
}
