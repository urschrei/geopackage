//! `gpkg_geometry_columns` and `PRAGMA table_info` introspection.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geopackage::core::types::{ColumnType, GeometryType, ZmFlag};
use geopackage::{Error, GeoPackage};

/// A GeoPackage with a feature-style table `roads(fid, geom, name, ...)` and a
/// matching `gpkg_geometry_columns` row.
fn feature_gpkg() -> (tempfile::TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    let conn = gpkg.connection();
    conn.execute_batch(
        "CREATE TABLE roads (\
           fid INTEGER PRIMARY KEY AUTOINCREMENT, \
           geom LINESTRING, \
           name TEXT(64), \
           lanes MEDIUMINT NOT NULL DEFAULT 2, \
           surveyed DATETIME, \
           built DATE, \
           weird VARCHAR(20));\
         CREATE TABLE gpkg_geometry_columns (\
           table_name TEXT NOT NULL, column_name TEXT NOT NULL, \
           geometry_type_name TEXT NOT NULL, srs_id INTEGER NOT NULL, \
           z TINYINT NOT NULL, m TINYINT NOT NULL);\
         INSERT INTO gpkg_geometry_columns VALUES ('roads', 'geom', 'LINESTRING', 4326, 0, 0);",
    )
    .unwrap();
    (dir, gpkg)
}

#[test]
fn geometry_column_lookup() {
    let (_dir, gpkg) = feature_gpkg();
    let gc = gpkg.geometry_column("roads").unwrap().unwrap();
    assert_eq!(gc.column_name, "geom");
    assert_eq!(gc.geometry_type, GeometryType::LineString);
    assert_eq!(gc.srs_id, 4326);
    assert_eq!(gc.z, ZmFlag::Prohibited);
    assert_eq!(gc.m, ZmFlag::Prohibited);

    // A table without a geometry_columns row.
    assert!(gpkg.geometry_column("no_such_table").unwrap().is_none());

    let all = gpkg.geometry_columns().unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].table_name, "roads");
}

#[test]
fn geometry_columns_absent_is_no_rows() {
    // A freshly created GeoPackage has no gpkg_geometry_columns table.
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("attr.gpkg")).unwrap();
    assert!(gpkg.geometry_column("anything").unwrap().is_none());
    assert!(gpkg.geometry_columns().unwrap().is_empty());
}

#[test]
fn extension_geometry_type_is_read() {
    let (_dir, gpkg) = feature_gpkg();
    gpkg.connection()
        .execute(
            "INSERT INTO gpkg_geometry_columns VALUES ('curves', 'geom', 'CURVEPOLYGON', 4326, 1, 2)",
            [],
        )
        .unwrap();
    let gc = gpkg.geometry_column("curves").unwrap().unwrap();
    assert_eq!(gc.geometry_type, GeometryType::CurvePolygon);
    assert!(gc.geometry_type.is_extension());
    assert_eq!(gc.z, ZmFlag::Mandatory);
    assert_eq!(gc.m, ZmFlag::Optional);
}

#[test]
fn unknown_geometry_type_is_typed_error() {
    let (_dir, gpkg) = feature_gpkg();
    gpkg.connection()
        .execute(
            "INSERT INTO gpkg_geometry_columns VALUES ('bad', 'geom', 'TRIANGLE', 4326, 0, 0)",
            [],
        )
        .unwrap();
    match gpkg.geometry_column("bad") {
        Err(Error::UnknownGeometryType { table_name, name }) => {
            assert_eq!(table_name, "bad");
            assert_eq!(name, "TRIANGLE");
        }
        other => panic!("expected UnknownGeometryType, got {other:?}"),
    }
}

#[test]
fn invalid_zm_flag_is_typed_error() {
    let (_dir, gpkg) = feature_gpkg();
    gpkg.connection()
        .execute(
            "INSERT INTO gpkg_geometry_columns VALUES ('bad', 'geom', 'POINT', 4326, 3, 0)",
            [],
        )
        .unwrap();
    match gpkg.geometry_column("bad") {
        Err(Error::InvalidZmFlag {
            table_name,
            column,
            value,
        }) => {
            assert_eq!(table_name, "bad");
            assert_eq!(column, "z");
            assert_eq!(value, 3);
        }
        other => panic!("expected InvalidZmFlag, got {other:?}"),
    }
}

#[test]
fn table_schema_columns_and_pk() {
    let (_dir, gpkg) = feature_gpkg();
    let schema = gpkg.table_schema("roads").unwrap();
    assert_eq!(schema.table_name, "roads");

    // The geometry_columns row is attached.
    let gc = schema.geometry_column.as_ref().unwrap();
    assert_eq!(gc.geometry_type, GeometryType::LineString);

    // Primary key: the fid column, discovered (not assumed) from the pragma.
    let pk = schema.primary_key().unwrap();
    assert_eq!(pk.name, "fid");
    assert_eq!(pk.column_type, Some(ColumnType::Integer));
    assert_eq!(pk.primary_key, 1);
    assert_eq!(schema.primary_key_columns().len(), 1);

    // Assorted column types parse.
    assert_eq!(
        schema.column("name").unwrap().column_type,
        Some(ColumnType::Text(Some(64)))
    );
    assert_eq!(
        schema.column("surveyed").unwrap().column_type,
        Some(ColumnType::DateTime)
    );
    assert_eq!(
        schema.column("built").unwrap().column_type,
        Some(ColumnType::Date)
    );

    // NOT NULL and DEFAULT are surfaced.
    let lanes = schema.column("lanes").unwrap();
    assert_eq!(lanes.column_type, Some(ColumnType::MediumInt));
    assert!(lanes.not_null);
    assert_eq!(lanes.default_value.as_deref(), Some("2"));
    assert!(!lanes.is_primary_key());

    // A declared type outside the spec vocabulary keeps its raw string.
    let weird = schema.column("weird").unwrap();
    assert_eq!(weird.declared_type, "VARCHAR(20)");
    assert_eq!(weird.column_type, None);
}

#[test]
fn table_schema_no_primary_key() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    gpkg.connection()
        .execute_batch("CREATE TABLE notes (body TEXT, created DATETIME);")
        .unwrap();
    let schema = gpkg.table_schema("notes").unwrap();
    assert_eq!(schema.columns.len(), 2);
    assert!(schema.primary_key().is_none());
    assert!(schema.primary_key_columns().is_empty());
    assert!(schema.geometry_column.is_none());
}

#[test]
fn table_schema_composite_primary_key() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    gpkg.connection()
        .execute_batch(
            "CREATE TABLE grid (row INTEGER, col INTEGER, v REAL, PRIMARY KEY (row, col));",
        )
        .unwrap();
    let schema = gpkg.table_schema("grid").unwrap();
    // Composite key is surfaced, not rejected; the single-key accessor abstains.
    assert_eq!(schema.primary_key_columns().len(), 2);
    assert!(schema.primary_key().is_none());
    let ordered: Vec<&str> = schema
        .primary_key_columns()
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(ordered, vec!["row", "col"]);
}

#[test]
fn table_schema_missing_table_is_typed_error() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    match gpkg.table_schema("ghost") {
        Err(Error::NoSuchTable { table_name }) => assert_eq!(table_name, "ghost"),
        other => panic!("expected NoSuchTable, got {other:?}"),
    }
}
