//! Container lifecycle: create, open, pragma validation, `gpkg_contents`.

use geopackage::{ContentsDataType, GeoPackage, GpkgVersion};

#[test]
fn create_reopen_validate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.gpkg");

    let gpkg = GeoPackage::create(&path).unwrap();
    assert_eq!(gpkg.version(), GpkgVersion::V1_4);
    assert!(gpkg.contents().unwrap().is_empty());
    drop(gpkg);

    let gpkg = GeoPackage::open_read_only(&path).unwrap();
    assert_eq!(gpkg.version(), GpkgVersion::V1_4);

    // Requirement 11: the three mandatory SRS rows.
    let srs_ids: Vec<i32> = {
        let conn = gpkg.connection();
        let mut stmt = conn
            .prepare("SELECT srs_id FROM gpkg_spatial_ref_sys ORDER BY srs_id")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert_eq!(srs_ids, vec![-1, 0, 4326]);

    // Pragmas.
    let app_id: i64 = gpkg
        .connection()
        .pragma_query_value(None, "application_id", |r| r.get(0))
        .unwrap();
    assert_eq!(app_id as u32, 0x4750_4B47);
}

#[test]
fn refuses_non_gpkg() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plain.sqlite");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1);")
        .unwrap();
    assert!(matches!(
        GeoPackage::open(&path),
        Err(geopackage::Error::NotAGeoPackage { .. })
    ));
}

#[test]
fn refuses_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.gpkg");
    GeoPackage::create(&path).unwrap();
    assert!(matches!(
        GeoPackage::create(&path),
        Err(geopackage::Error::AlreadyExists(_))
    ));
}

#[test]
fn contents_data_types() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("c.gpkg")).unwrap();
    gpkg.connection()
        .execute(
            "INSERT INTO gpkg_contents (table_name, data_type, identifier, srs_id) \
             VALUES ('t1', 'attributes', 'T1', 0)",
            [],
        )
        .unwrap();
    let contents = gpkg.contents().unwrap();
    assert_eq!(contents.len(), 1);
    assert_eq!(contents[0].data_type, ContentsDataType::Attributes);
    assert_eq!(contents[0].identifier.as_deref(), Some("T1"));
}
