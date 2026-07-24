//! `gpkg_geometry_columns` and `PRAGMA table_info` introspection.

use geopackage::core::types::{GeometryType, ZmFlag};
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
