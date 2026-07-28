//! `GeoPackage::open_lenient` and the warnings it collects, versus strict
//! `open` (whose behaviour is unchanged).

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geopackage::core::ddl;
use geopackage::core::version::{APPLICATION_ID_GP10, APPLICATION_ID_GP11};
use geopackage::{GeoPackage, GpkgVersion, OpenWarning};
use std::path::Path;

/// Write a minimal GeoPackage with a chosen `application_id` and the two
/// required core tables, using raw SQL/pragmas.
fn legacy_file(path: &Path, application_id: u32) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.pragma_update(None, "application_id", application_id)
        .unwrap();
    conn.pragma_update(None, "user_version", 0).unwrap();
    conn.execute_batch(&format!(
        "{};\n{};",
        ddl::CREATE_GPKG_SPATIAL_REF_SYS,
        ddl::CREATE_GPKG_CONTENTS
    ))
    .unwrap();
    for stmt in ddl::SEED_SPATIAL_REF_SYS {
        conn.execute(stmt, []).unwrap();
    }
}

#[test]
fn legacy_application_id_warns_but_opens() {
    for (application_id, expected) in [
        (APPLICATION_ID_GP10, GpkgVersion::V1_0),
        (APPLICATION_ID_GP11, GpkgVersion::V1_1),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.gpkg");
        legacy_file(&path, application_id);

        let gpkg = GeoPackage::open_lenient(&path).unwrap();
        assert_eq!(gpkg.version(), expected);
        assert!(
            gpkg.open_warnings()
                .contains(&OpenWarning::LegacyApplicationId {
                    version: expected,
                    application_id,
                })
        );

        // Strict open already accepts these legacy ids (version.rs classifies
        // them); its behaviour is unchanged and it records no warnings.
        let strict = GeoPackage::open(&path).unwrap();
        assert_eq!(strict.version(), expected);
        assert!(strict.open_warnings().is_empty());
    }
}

#[test]
fn missing_geometry_columns_warns_for_attribute_only_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("attr.gpkg");
    // A freshly created GeoPackage has no gpkg_geometry_columns table.
    drop(GeoPackage::create(&path).unwrap());

    let gpkg = GeoPackage::open_lenient(&path).unwrap();
    assert!(
        gpkg.open_warnings()
            .contains(&OpenWarning::MissingGeometryColumns)
    );

    // Strict open is unchanged: it does not require the table and warns nothing.
    let strict = GeoPackage::open(&path).unwrap();
    assert!(strict.open_warnings().is_empty());
}

#[test]
fn wrong_case_table_name_warns_and_resolves() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("case.gpkg");
    {
        let gpkg = GeoPackage::create(&path).unwrap();
        // Physical table is lower-case `roads`; the catalogue rows spell it
        // `Roads`. SQLite resolves the table regardless, but a string join
        // between catalogue tables would not.
        gpkg.connection()
            .execute_batch(
                "CREATE TABLE roads (fid INTEGER PRIMARY KEY, geom POINT, name TEXT);\
                 INSERT INTO gpkg_contents (table_name, data_type, srs_id) \
                   VALUES ('Roads', 'features', 4326);\
                 CREATE TABLE gpkg_geometry_columns (\
                   table_name TEXT NOT NULL, column_name TEXT NOT NULL, \
                   geometry_type_name TEXT NOT NULL, srs_id INTEGER NOT NULL, \
                   z TINYINT NOT NULL, m TINYINT NOT NULL);\
                 INSERT INTO gpkg_geometry_columns VALUES ('roads', 'geom', 'POINT', 4326, 0, 0);",
            )
            .unwrap();
    }

    let gpkg = GeoPackage::open_lenient(&path).unwrap();
    assert!(
        gpkg.open_warnings()
            .contains(&OpenWarning::TableNameCaseMismatch {
                declared: "Roads".to_string(),
                actual: "roads".to_string(),
            })
    );

    // The layer still resolves to the real table, keeping its geometry column.
    let layer = gpkg.layer("Roads").unwrap();
    assert_eq!(layer.table_name(), "roads");
    assert!(layer.geometry_column().is_some());

    // And enumeration finds it despite the case-only mismatch in the join.
    let layers = gpkg.layers().unwrap();
    assert_eq!(layers.len(), 1);
    assert_eq!(layers[0].table_name(), "roads");
}

#[test]
fn strict_open_of_a_normal_file_has_no_warnings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ok.gpkg");
    drop(GeoPackage::create(&path).unwrap());
    let gpkg = GeoPackage::open(&path).unwrap();
    assert!(gpkg.open_warnings().is_empty());
}

#[test]
fn open_lenient_still_rejects_a_non_geopackage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plain.sqlite");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE t(x);")
        .unwrap();
    assert!(matches!(
        GeoPackage::open_lenient(&path),
        Err(geopackage::Error::NotAGeoPackage { .. })
    ));
}

#[test]
fn read_only_lenient_tolerates_what_lenient_tolerates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.gpkg");
    legacy_file(&path, APPLICATION_ID_GP10);

    // Strict read-only refuses nothing here (the file is identifiable), but
    // the point is that leniency and read-only compose at all: before this
    // existed a caller had to pick tolerant-and-writable or read-only-and-
    // strict, and an inspection tool wants neither of those pairs.
    let gpkg = GeoPackage::open_read_only_lenient(&path).unwrap();
    assert_eq!(gpkg.version(), GpkgVersion::V1_0);
    // `legacy_file` writes only the two required core tables, so the absent
    // gpkg_geometry_columns is warned about alongside the application_id.
    assert_eq!(
        gpkg.open_warnings(),
        &[
            OpenWarning::LegacyApplicationId {
                version: GpkgVersion::V1_0,
                application_id: APPLICATION_ID_GP10,
            },
            OpenWarning::MissingGeometryColumns,
        ]
    );
}

#[test]
fn read_only_lenient_does_not_need_a_writable_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.gpkg");
    legacy_file(&path, APPLICATION_ID_GP11);

    // Drop write permission, which is what an inspection tool meets on a file
    // it does not own or on read-only media.
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&path, perms).unwrap();

    let gpkg = GeoPackage::open_read_only_lenient(&path).unwrap();
    assert_eq!(gpkg.version(), GpkgVersion::V1_1);
    assert!(
        gpkg.open_warnings()
            .contains(&OpenWarning::LegacyApplicationId {
                version: GpkgVersion::V1_1,
                application_id: APPLICATION_ID_GP11,
            })
    );

    // Read-only in the sense that matters: the connection refuses writes,
    // rather than merely happening to sit on an unwritable file. Note
    // `open_lenient` would *succeed* here, because SQLite's read-write open
    // falls back rather than failing; what it does not give is a handle that
    // cannot write, and a read-write connection may roll back a hot journal on
    // open, modifying the very file being inspected.
    gpkg.connection()
        .execute("CREATE TABLE scratch (a INTEGER)", [])
        .expect_err("a read-only connection must refuse a write");
}

#[test]
fn leniency_composes_with_the_other_open_options() {
    // What the unified path buys. Before `OpenOptions::lenient` existed,
    // leniency was reachable only through `open_lenient`, which takes no
    // options, so a caller wanting a legacy file *and* WAL, or a legacy file
    // *and* constraint enforcement, could have either but never both.
    use geopackage::{JournalMode, OpenOptions};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.gpkg");
    legacy_file(&path, APPLICATION_ID_GP10);

    let gpkg = OpenOptions::new()
        .lenient(true)
        .journal_mode(JournalMode::Wal)
        .enforce_column_constraints(true)
        .open(&path)
        .unwrap();

    // Lenient: the legacy application_id was tolerated and recorded.
    assert!(
        gpkg.open_warnings()
            .contains(&OpenWarning::LegacyApplicationId {
                version: GpkgVersion::V1_0,
                application_id: APPLICATION_ID_GP10,
            })
    );

    // And the other options took effect on the same handle, which is the part
    // that was previously impossible.
    let mode: String = gpkg
        .connection()
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");

    // Closing resets the file to a single file, as it does for any WAL handle,
    // so leniency does not opt out of the interchange guarantee either.
    gpkg.close().unwrap();
    assert!(!path.with_extension("gpkg-wal").exists());
}

#[test]
fn a_strict_open_still_records_no_warnings() {
    // The default is unchanged: `lenient` defaults to false, so an ordinary
    // `OpenOptions` open behaves exactly as `GeoPackage::open` does.
    use geopackage::OpenOptions;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.gpkg");
    legacy_file(&path, APPLICATION_ID_GP11);

    let gpkg = OpenOptions::new().open(&path).unwrap();
    assert!(gpkg.open_warnings().is_empty());
    assert_eq!(gpkg.version(), GpkgVersion::V1_1);
}
