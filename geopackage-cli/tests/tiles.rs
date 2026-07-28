//! `gpkg tiles info` and `gpkg tiles get`.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FIXTURE: &str = "gdal_tiles.gpkg";

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
fn info_reports_the_grid_and_how_much_of_it_is_stored() {
    let path = fixture(FIXTURE);
    let output = gpkg(&["tiles", "info", path.to_str().unwrap()]);
    assert!(output.status.success());
    let out = stdout(&output);

    assert!(out.contains(r#"tiles "tiles""#), "{out}");
    assert!(out.contains("tiles:    1"), "{out}");
    assert!(
        out.contains("WGS 84 / Pseudo-Mercator (EPSG:3857)"),
        "{out}"
    );
    assert!(out.contains("extent:"), "{out}");
    // The declared grid and the number actually present, which differ on a
    // partly populated pyramid.
    assert!(out.contains("1 x 1"), "{out}");
    assert!(out.contains("256 x 256 px"), "{out}");
    assert!(out.contains("1 stored"), "{out}");
}

#[test]
fn info_on_a_file_with_no_pyramids_says_so() {
    let path = fixture("gdal_multilayer_1_4.gpkg");
    let output = gpkg(&["tiles", "info", path.to_str().unwrap()]);
    assert!(output.status.success());
    assert!(
        stdout(&output).contains("no tile pyramids"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn get_writes_the_stored_bytes_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("tile.png");
    let path = fixture(FIXTURE);

    let output = gpkg(&[
        "tiles",
        "get",
        path.to_str().unwrap(),
        "tiles",
        "0",
        "0",
        "0",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout(&output).contains("Png 256x256"),
        "{}",
        stdout(&output)
    );

    // Byte for byte what the pyramid holds: the payload is never decoded or
    // re-encoded on the way out.
    let written = std::fs::read(&out).unwrap();
    let stored: Vec<u8> = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT tile_data FROM tiles WHERE zoom_level = 0 AND tile_column = 0 AND tile_row = 0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(written, stored);
    // And it really is a PNG, so `--out tile.png` was not wishful.
    assert_eq!(&written[..4], b"\x89PNG");
}

#[test]
fn get_without_out_writes_the_bytes_to_stdout() {
    let path = fixture(FIXTURE);
    let output = gpkg(&[
        "tiles",
        "get",
        path.to_str().unwrap(),
        "tiles",
        "0",
        "0",
        "0",
    ]);
    assert!(output.status.success());
    assert_eq!(&output.stdout[..4], b"\x89PNG");
}

#[test]
fn an_absent_tile_fails_rather_than_writing_an_empty_file() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("tile.png");
    let path = fixture(FIXTURE);

    let output = gpkg(&[
        "tiles",
        "get",
        path.to_str().unwrap(),
        "tiles",
        "0",
        "5",
        "5",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no tile at 0/5/5"),
        "{:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !out.exists(),
        "nothing should be written when there is no tile"
    );
}
