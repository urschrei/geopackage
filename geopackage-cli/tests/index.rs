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

/// One lost trigger is enough to be stale. Before the completeness check, the
/// names that were left read as a current 1.4 set, and `repair` said there
/// was nothing to do while inserts went unindexed.
#[test]
fn repair_fixes_an_index_that_lost_one_trigger() {
    let (_dir, path) = working_copy(r#"DROP TRIGGER "rtree_points_geom_insert""#);
    assert!(index_states(&path).contains("index:    stale"));

    let output = gpkg(&["repair"], &path);
    assert!(output.status.success());
    assert!(
        stdout(&output).contains("points: stale index repaired"),
        "{}",
        stdout(&output)
    );
    assert!(!index_states(&path).contains("index:    stale"));
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

/// A file with one unindexed POINT Z layer and one attribute table, each
/// holding a value in a form the spec does not give it: a point whose Z is
/// given by the high-bit flag, and `DATETIME` text with one fractional digit.
fn file_with_encodings_to_repair() -> (tempfile::TempDir, PathBuf) {
    use geopackage::core::types::{ColumnType, GeometryType, ZmFlag};
    use geopackage::{ColumnSpec, GeoPackage, GeometrySpec, TableSchemaBuilder};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("encodings.gpkg");
    let gpkg = GeoPackage::create(&path).unwrap();
    gpkg.add_epsg_srs(4326).unwrap();
    gpkg.create_layer(
        &TableSchemaBuilder::new("pts")
            .column(ColumnSpec::new("at", ColumnType::DateTime))
            .geometry(GeometrySpec::new(GeometryType::Point, 4326).z(ZmFlag::Mandatory))
            .spatial_index(false),
    )
    .unwrap();
    gpkg.create_attributes_table(
        &TableSchemaBuilder::new("visits").column(ColumnSpec::new("at", ColumnType::DateTime)),
    )
    .unwrap();
    gpkg.close().unwrap();

    let mut blob = vec![b'G', b'P', 0, 0x01];
    blob.extend_from_slice(&4326_i32.to_le_bytes());
    blob.push(1);
    blob.extend_from_slice(&0x8000_0001_u32.to_le_bytes());
    for v in [1.0_f64, 2.0, 3.0] {
        blob.extend_from_slice(&v.to_le_bytes());
    }
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "INSERT INTO pts (geom, at) VALUES (?1, '2017-08-01T00:00:00.0Z')",
        [blob],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO visits (at) VALUES ('2017-08-01T00:00:00.0Z')",
        [],
    )
    .unwrap();
    (dir, path)
}

#[test]
fn repair_rewrites_flagged_wkb_and_non_standard_datetimes() {
    let (_dir, path) = file_with_encodings_to_repair();
    let output = gpkg(&["repair"], &path);
    assert!(output.status.success());
    let out = stdout(&output);
    assert!(
        out.contains("pts: 1 geometries rewritten with ISO WKB type codes"),
        "{out}"
    );
    assert!(
        out.contains("pts: 1 DATETIME values rewritten in the GeoPackage form"),
        "{out}"
    );
    assert!(
        out.contains("visits: 1 DATETIME values rewritten in the GeoPackage form"),
        "{out}"
    );

    let again = gpkg(&["repair"], &path);
    assert!(
        stdout(&again).contains("nothing to repair"),
        "{}",
        stdout(&again)
    );
}

#[test]
fn repair_can_be_narrowed_to_one_attribute_table() {
    let (_dir, path) = file_with_encodings_to_repair();
    let output = gpkg_on(&["repair"], &path, &["visits"]);
    assert!(output.status.success());
    let out = stdout(&output);
    assert!(out.contains("visits: 1 DATETIME values"), "{out}");
    assert!(!out.contains("pts"), "{out}");
}
