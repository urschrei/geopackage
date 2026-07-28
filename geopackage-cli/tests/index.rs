//! `gpkg index` and `gpkg repair`: the commands that write.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FIXTURE: &str = "gdal_multilayer_1_4.gpkg";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("geopackage")
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// A writable copy of the fixture, optionally damaged first.
fn working_copy(sql: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(FIXTURE);
    std::fs::copy(fixture(FIXTURE), &path).unwrap();
    if !sql.is_empty() {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(sql).unwrap();
    }
    (dir, path)
}

fn gpkg(args: &[&str], path: &Path) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gpkg"));
    for arg in args {
        command.arg(arg);
    }
    command.arg(path).output().unwrap()
}

/// `gpkg <subcommand> <file> <trailing...>`, since the path comes before the
/// layer name on the command line.
fn gpkg_on(args: &[&str], path: &Path, trailing: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gpkg"));
    for arg in args {
        command.arg(arg);
    }
    command.arg(path);
    for arg in trailing {
        command.arg(arg);
    }
    command.output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

/// Drop a layer's rtree triggers, leaving the virtual table behind. That is the
/// `Stale` state: an index nothing maintains any more.
fn make_stale(path: &Path, layer: &str) {
    let conn = rusqlite::Connection::open(path).unwrap();
    let names: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'trigger' AND name LIKE ?1")
        .unwrap()
        .query_map([format!("rtree_{layer}_geom%")], |row| row.get(0))
        .unwrap()
        .map(std::result::Result::unwrap)
        .collect();
    assert!(!names.is_empty(), "the fixture should have rtree triggers");
    for name in names {
        conn.execute_batch(&format!("DROP TRIGGER \"{name}\""))
            .unwrap();
    }
}

fn index_states(path: &Path) -> String {
    stdout(&gpkg(&["info"], path))
}

#[test]
fn index_builds_one_where_there_was_none() {
    let (_dir, path) = working_copy("");
    assert!(index_states(&path).contains("index:    absent"));

    let output = gpkg_on(&["index"], &path, &["lines"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout(&output).contains("lines: index built over 2 rows"));

    // `validate` no longer has an advisory to make about that layer.
    let report = stdout(&gpkg(&["validate"], &path));
    assert!(
        !report.contains(r#"table "lines" has no spatial index"#),
        "{report}"
    );
}

#[test]
fn indexing_an_indexed_layer_is_a_no_op_rather_than_an_error() {
    let (_dir, path) = working_copy("");
    let output = gpkg_on(&["index"], &path, &["points"]);
    assert!(output.status.success());
    assert!(
        stdout(&output).contains("already indexed"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn indexing_over_a_broken_index_refuses_and_names_the_repair() {
    let (_dir, path) = working_copy("");
    make_stale(&path, "points");

    // Building over a present-but-wrong index is not what `index` means, and
    // silently repairing would be doing something other than what was asked.
    let output = gpkg_on(&["index"], &path, &["points"]);
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("gpkg repair"), "{err}");
}

#[test]
fn repair_fixes_a_stale_index_and_leaves_the_rest_alone() {
    let (_dir, path) = working_copy("");
    make_stale(&path, "points");
    assert!(index_states(&path).contains("index:    stale"));

    let output = gpkg(&["repair"], &path);
    assert!(output.status.success());
    assert!(
        stdout(&output).contains("points: stale index repaired"),
        "{}",
        stdout(&output)
    );

    let after = index_states(&path);
    assert!(!after.contains("index:    stale"), "{after}");
    // The layers that had no index still have none: an absent index is a
    // choice, and repair does not invent one.
    assert!(after.contains("index:    absent"), "{after}");
}

#[test]
fn repair_on_a_sound_file_says_there_was_nothing_to_do() {
    let (_dir, path) = working_copy("");
    let output = gpkg(&["repair"], &path);
    assert!(output.status.success());
    assert!(
        stdout(&output).contains("nothing to repair"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn repair_can_be_narrowed_to_one_layer() {
    let (_dir, path) = working_copy("");
    make_stale(&path, "points");
    make_stale(&path, "polygons");

    let output = gpkg_on(&["repair"], &path, &["points"]);
    assert!(output.status.success());
    let out = stdout(&output);
    assert!(out.contains("points: stale index repaired"), "{out}");
    assert!(!out.contains("polygons"), "{out}");

    // The layer that was not named is still stale.
    assert!(index_states(&path).contains("index:    stale"));
}
