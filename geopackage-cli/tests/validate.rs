//! `gpkg validate`: what it reports, and what it exits with.
//!
//! The exit status is the part a script depends on, so it is asserted as
//! carefully as the output.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("geopackage")
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn validate(path: &Path, strict: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_gpkg"));
    command.arg("validate").arg(path);
    if strict {
        command.arg("--strict");
    }
    command.output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

/// A copy of a fixture, writable, so a test can damage it.
fn damaged(name: &str, sql: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::fs::copy(fixture(name), &path).unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(sql).unwrap();
    drop(conn);
    (dir, path)
}

#[test]
fn a_sound_file_says_so_rather_than_exiting_silently() {
    let (_dir, path) = damaged("gdal_multilayer_1_4.gpkg", "");
    // The fixture's unindexed layers are advisories, which are not defects.
    let output = validate(&path, false);
    assert!(output.status.success());
    let out = stdout(&output);
    assert!(out.contains("advisory"), "{out}");
    assert!(out.contains("0 errors"), "{out}");
}

#[test]
fn an_error_exits_non_zero_and_names_its_repair() {
    let (_dir, path) = damaged(
        "gdal_multilayer_1_4.gpkg",
        "INSERT INTO gpkg_contents (table_name, data_type, identifier) \
         VALUES ('ghost', 'features', 'ghost')",
    );

    let output = validate(&path, false);
    assert!(
        !output.status.success(),
        "an error means a reader can get a wrong answer, so the exit must fail"
    );
    let out = stdout(&output);
    assert!(
        out.contains(r#"error: gpkg_contents names table "ghost""#),
        "{out}"
    );
    assert!(
        out.contains("repair: delete the gpkg_contents row"),
        "{out}"
    );
    assert!(out.contains("1 error"), "{out}");
}

#[test]
fn a_warning_passes_by_default_and_fails_under_strict() {
    let path = fixture("legacy_gp10.gpkg");

    let lenient = validate(&path, false);
    assert!(lenient.status.success());
    let out = stdout(&lenient);
    assert!(out.contains("warning: file declares"), "{out}");
    assert!(out.contains("1 warning"), "{out}");

    let strict = validate(&path, true);
    assert!(
        !strict.status.success(),
        "--strict is what promotes a warning to a failing exit"
    );
}

#[test]
fn findings_are_ordered_most_severe_first() {
    let (_dir, path) = damaged(
        "gdal_multilayer_1_4.gpkg",
        "INSERT INTO gpkg_contents (table_name, data_type, identifier) \
         VALUES ('ghost', 'features', 'ghost')",
    );
    let out = stdout(&validate(&path, false));
    let error_at = out.find("  error:").expect("an error line");
    let advisory_at = out.find("  advisory:").expect("an advisory line");
    assert!(error_at < advisory_at, "{out}");
}

#[test]
fn nothing_is_modified_by_validating() {
    let (_dir, path) = damaged("gdal_multilayer_1_4.gpkg", "");
    let before = std::fs::read(&path).unwrap();
    assert!(validate(&path, false).status.success());
    let after = std::fs::read(&path).unwrap();
    assert_eq!(before, after, "validate must not touch the file");
}
