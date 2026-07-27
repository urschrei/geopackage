//! `gpkg_spatial_ref_sys` lookup and registration.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

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
        definition_wkt2: None,
        epoch: None,
    };
    assert!(gpkg.add_srs(&srs).unwrap());
    assert_eq!(gpkg.srs(90001).unwrap().unwrap(), srs);
    assert!(!gpkg.add_srs(&srs).unwrap());
}

#[test]
fn a_file_without_the_extension_reports_no_wkt2() {
    let (_dir, gpkg) = create();
    // The columns do not exist, which is not the same as existing and holding
    // `undefined`; both read as None, and neither is an error.
    for srs in gpkg.srs_list().unwrap() {
        assert_eq!(srs.definition_wkt2, None, "{}", srs.srs_id);
        assert_eq!(srs.epoch, None, "{}", srs.srs_id);
    }
}

#[test]
fn a_caller_supplied_wkt2_definition_round_trips() {
    let (_dir, gpkg) = create();
    // D3 says a caller may supply arbitrary definitions. Until now that was
    // true of WKT1 only: there was no way to hand over a WKT2 definition for a
    // CRS the EPSG registry does not describe.
    let wkt2 = r#"ENGCRS["Site grid",EDATUM["Site datum"],CS[Cartesian,2],\
        AXIS["easting (X)",east],AXIS["northing (Y)",north],\
        LENGTHUNIT["metre",1.0]]"#;
    let srs = Srs {
        name: "Site grid".into(),
        srs_id: 90002,
        organization: "NONE".into(),
        organization_coordsys_id: 90002,
        definition: "undefined".into(),
        description: None,
        definition_wkt2: Some(wkt2.to_owned()),
        epoch: Some(2020.5),
    };
    assert!(gpkg.add_srs(&srs).unwrap());
    assert_eq!(gpkg.srs(90002).unwrap().unwrap(), srs);

    // Supplying either column extends the file, so the extension has to be
    // registered: a reader is entitled to reject an undeclared column.
    let declared: Vec<String> = gpkg
        .extensions()
        .unwrap()
        .into_iter()
        .map(|row| row.name)
        .collect();
    assert_eq!(declared, vec!["gpkg_crs_wkt_1_1", "gpkg_crs_wkt_1_1"]);
}

#[test]
fn undefined_reads_back_as_absent() {
    let (_dir, gpkg) = create();
    // Adding a WKT2-only code brings the columns into the file. Rows that have
    // no WKT2 form then hold the spec's `undefined` (Requirement 117) rather
    // than NULL, because the column is NOT NULL.
    gpkg.add_epsg_srs(4979).unwrap();
    let srs = Srs {
        name: "Plain grid".into(),
        srs_id: 90003,
        organization: "NONE".into(),
        organization_coordsys_id: 90003,
        definition: "undefined".into(),
        description: None,
        definition_wkt2: None,
        epoch: None,
    };
    assert!(gpkg.add_srs(&srs).unwrap());

    let stored: String = gpkg
        .connection()
        .query_row(
            "SELECT definition_12_063 FROM gpkg_spatial_ref_sys WHERE srs_id = 90003",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, "undefined");
    // Which is a definition nobody could produce, so it reads back as None
    // rather than as the string.
    assert_eq!(gpkg.srs(90003).unwrap().unwrap(), srs);
}

#[test]
fn the_extension_column_is_read_where_a_file_carries_it() {
    let (_dir, gpkg) = create();
    gpkg.add_epsg_srs(4979).unwrap();
    let srs = gpkg.srs(4979).unwrap().unwrap();
    // EPSG:4979 is geographic 3D and has no WKT1 form, so the definition lives
    // in the extension column and reading only `definition` would find nothing
    // but `undefined`.
    assert_eq!(srs.definition, "undefined");
    let wkt2 = srs.definition_wkt2.expect("4979 has a WKT2 definition");
    assert!(wkt2.contains("CS[ellipsoidal,3"), "{wkt2}");
    assert_eq!(srs.epoch, None);

    // The backfill reaches rows that were already there.
    let wgs84 = gpkg.srs(4326).unwrap().unwrap();
    assert!(wgs84.definition.starts_with("GEOGCS["));
    assert!(wgs84.definition_wkt2.is_some());
}
