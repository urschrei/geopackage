//! Journal-mode options and the interchange-first close (design decision D4):
//! a WAL handle leaves no `-wal`/`-shm` sidecars after `close()` or drop, and
//! the file reads back as `DELETE`; `into_connection()` opts out; the
//! `synchronous` option is applied.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-unwrap-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::path::{Path, PathBuf};

use geopackage::{GeoPackage, JournalMode, OpenOptions, Synchronous};

/// The `-wal` and `-shm` sidecar paths for a database file.
fn sidecars(path: &Path) -> (PathBuf, PathBuf) {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    let mut shm = path.as_os_str().to_owned();
    shm.push("-shm");
    (PathBuf::from(wal), PathBuf::from(shm))
}

/// Add an attributes row so the connection actually writes (touching the WAL).
fn touch(gpkg: &GeoPackage, id: &str) {
    gpkg.connection()
        .execute(
            "INSERT INTO gpkg_contents (table_name, data_type, identifier, srs_id) \
             VALUES (?1, 'attributes', ?1, 0)",
            [id],
        )
        .unwrap();
}

fn journal_mode_str(gpkg: &GeoPackage) -> String {
    gpkg.connection()
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn wal_create_close_roundtrips_to_single_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.gpkg");
    let (wal, shm) = sidecars(&path);

    let gpkg = OpenOptions::new()
        .journal_mode(JournalMode::Wal)
        .create(&path)
        .unwrap();
    touch(&gpkg, "a");
    // While open in WAL, the connection reports WAL and the sidecars exist.
    assert_eq!(journal_mode_str(&gpkg), "wal");
    assert!(wal.exists(), "expected a -wal file while open in WAL");

    // Explicit close finalises the file back to a single DELETE file.
    gpkg.close().unwrap();
    assert!(!wal.exists(), "-wal sidecar left after close: {wal:?}");
    assert!(!shm.exists(), "-shm sidecar left after close: {shm:?}");

    // The journal-mode change is persisted in the header: reopen reads DELETE.
    let reopened = GeoPackage::open(&path).unwrap();
    assert_eq!(journal_mode_str(&reopened), "delete");
}

#[test]
fn wal_drop_also_finalises() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.gpkg");
    let (wal, shm) = sidecars(&path);

    GeoPackage::create(&path).unwrap();

    // Open in WAL and drop without an explicit close.
    {
        let gpkg = OpenOptions::new()
            .journal_mode(JournalMode::Wal)
            .open(&path)
            .unwrap();
        touch(&gpkg, "b");
        assert!(wal.exists(), "expected a -wal file while open in WAL");
    } // drop runs the best-effort finalise here

    assert!(!wal.exists(), "-wal sidecar left after drop: {wal:?}");
    assert!(!shm.exists(), "-shm sidecar left after drop: {shm:?}");
    let reopened = GeoPackage::open(&path).unwrap();
    assert_eq!(journal_mode_str(&reopened), "delete");
}

#[test]
fn into_connection_opts_out_of_the_finalise() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.gpkg");

    let gpkg = OpenOptions::new()
        .journal_mode(JournalMode::Wal)
        .create(&path)
        .unwrap();
    touch(&gpkg, "c");
    // Handing back the raw connection keeps WAL mode (no finalise on the
    // consumed handle).
    let conn = gpkg.into_connection();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        mode, "wal",
        "into_connection must not reset the journal mode"
    );
}

#[test]
fn plain_open_leaves_journal_mode_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.gpkg");

    // Put the file into WAL and hand back the raw connection (no finalise).
    let conn = OpenOptions::new()
        .journal_mode(JournalMode::Wal)
        .create(&path)
        .unwrap()
        .into_connection();
    drop(conn);

    // A plain open (unspecified journal mode) does not convert it.
    let gpkg = GeoPackage::open(&path).unwrap();
    assert_eq!(journal_mode_str(&gpkg), "wal");
}

#[test]
fn synchronous_option_is_applied() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.gpkg");

    let gpkg = OpenOptions::new()
        .synchronous(Synchronous::Off)
        .create(&path)
        .unwrap();
    let level: i64 = gpkg
        .connection()
        .query_row("PRAGMA synchronous", [], |r| r.get(0))
        .unwrap();
    assert_eq!(level, 0, "synchronous=OFF should read 0");

    let gpkg = OpenOptions::new()
        .synchronous(Synchronous::Normal)
        .open(&path)
        .unwrap();
    let level: i64 = gpkg
        .connection()
        .query_row("PRAGMA synchronous", [], |r| r.get(0))
        .unwrap();
    assert_eq!(level, 1, "synchronous=NORMAL should read 1");
}
