//! `gpkg_spatial_ref_sys` lookup and registration.

use geopackage::{Error, GeoPackage, Srs};

fn create() -> (tempfile::TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    (dir, gpkg)
}

#[test]
fn required_seed_rows_present() {
    let (_dir, gpkg) = create();
    let ids: Vec<i32> = gpkg.srs_list().unwrap().iter().map(|s| s.srs_id).collect();
    assert_eq!(ids, vec![-1, 0, 4326]);
    let wgs84 = gpkg.srs(4326).unwrap().unwrap();
    assert_eq!(wgs84.organization, "EPSG");
    assert!(wgs84.definition.starts_with("GEOGCS["));
}

#[test]
fn add_epsg_srs_inserts_vendored_definition() {
    let (_dir, gpkg) = create();
    assert!(gpkg.add_epsg_srs(27700).unwrap());
    let osgb = gpkg.srs(27700).unwrap().unwrap();
    assert_eq!(osgb.name, "OSGB36 / British National Grid");
    assert_eq!(osgb.organization_coordsys_id, 27700);
    assert!(osgb.definition.contains("Transverse_Mercator"));

    // Synthesised UTM zone.
    assert!(gpkg.add_epsg_srs(32629).unwrap());
    let utm = gpkg.srs(32629).unwrap().unwrap();
    assert_eq!(utm.name, "WGS 84 / UTM zone 29N");
}

#[test]
fn add_epsg_srs_is_idempotent_and_preserves_existing() {
    let (_dir, gpkg) = create();
    // 4326 is seeded at create with the spec's normative WKT; adding it again
    // must not replace that text with the vendored variant.
    let before = gpkg.srs(4326).unwrap().unwrap();
    assert!(!gpkg.add_epsg_srs(4326).unwrap());
    assert_eq!(gpkg.srs(4326).unwrap().unwrap(), before);
}

#[test]
fn unknown_epsg_code_is_a_typed_error() {
    let (_dir, gpkg) = create();
    match gpkg.add_epsg_srs(94326) {
        Err(Error::UnknownEpsgCode { code: 94326 }) => {}
        other => panic!("expected UnknownEpsgCode, got {other:?}"),
    }
}

#[test]
fn add_custom_srs() {
    let (_dir, gpkg) = create();
    let srs = Srs {
        name: "My local grid".into(),
        srs_id: 90001,
        organization: "NONE".into(),
        organization_coordsys_id: 90001,
        definition: "undefined".into(),
        description: Some("site-local engineering grid".into()),
    };
    assert!(gpkg.add_srs(&srs).unwrap());
    assert_eq!(gpkg.srs(90001).unwrap().unwrap(), srs);
    assert!(!gpkg.add_srs(&srs).unwrap());
}
