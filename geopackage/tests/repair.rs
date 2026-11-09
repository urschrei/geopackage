//! The repairs that rewrite stored values: `Layer::repair_wkb_type_codes`.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geopackage::core::gpb::{self, Envelope};
use geopackage::core::types::{GeometryType, ZmFlag};
use geopackage::{Finding, GeoPackage, GeometrySpec, TableSchemaBuilder};
use tempfile::TempDir;

/// A GPB blob, little-endian, SRS 4326, with no envelope, around `body`.
fn gpb_blob(body: &[u8]) -> Vec<u8> {
    let mut blob = vec![b'G', b'P', 0, 0x01];
    blob.extend_from_slice(&4326_i32.to_le_bytes());
    blob.extend_from_slice(body);
    blob
}

/// A LINESTRING Z body from (0 0 5) to (2 1 5), with the given type code.
fn linestring_z(code: u32) -> Vec<u8> {
    let mut body = vec![1u8];
    body.extend_from_slice(&code.to_le_bytes());
    body.extend_from_slice(&2_u32.to_le_bytes());
    for v in [0.0_f64, 0.0, 5.0, 2.0, 1.0, 5.0] {
        body.extend_from_slice(&v.to_le_bytes());
    }
    body
}

/// A layer of three LINESTRING Z rows, stored through SQL so that the bodies
/// are what the test gives: rows 1 and 3 with the high-bit Z flag, row 2 in
/// ISO form.
fn roads() -> (TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("r.gpkg")).unwrap();
    gpkg.add_epsg_srs(4326).unwrap();
    gpkg.create_layer(
        &TableSchemaBuilder::new("roads")
            .geometry(GeometrySpec::new(GeometryType::LineString, 4326).z(ZmFlag::Mandatory)),
    )
    .unwrap();
    for code in [0x8000_0002, 1002, 0x8000_0002] {
        gpkg.connection()
            .execute(
                "INSERT INTO roads (geom) VALUES (?1)",
                [gpb_blob(&linestring_z(code))],
            )
            .unwrap();
    }
    (dir, gpkg)
}

fn blob(gpkg: &GeoPackage, fid: i64) -> Vec<u8> {
    gpkg.connection()
        .query_row("SELECT geom FROM roads WHERE fid = ?1", [fid], |row| {
            row.get(0)
        })
        .unwrap()
}

#[test]
fn flagged_geometries_are_rewritten_in_iso_form_with_an_envelope() {
    let (_dir, gpkg) = roads();
    let layer = gpkg.layer("roads").unwrap();
    let untouched = blob(&gpkg, 2);

    assert_eq!(layer.repair_wkb_type_codes().unwrap(), 2);

    for fid in [1, 3] {
        let rewritten = blob(&gpkg, fid);
        let (header, offset) = gpb::parse_header(&rewritten).unwrap();
        assert_eq!(header.srs_id, 4326);
        assert_eq!(
            header.envelope,
            Envelope::Xyz([0.0, 2.0, 0.0, 1.0, 5.0, 5.0])
        );
        assert_eq!(&rewritten[offset..], linestring_z(1002).as_slice());
    }
    // An ISO row is not written.
    assert_eq!(blob(&gpkg, 2), untouched);
    assert_eq!(gpkg.validate().unwrap(), Vec::<Finding>::new());
    assert!(layer.audit_spatial_index().unwrap().is_consistent());
    // Nothing is left to rewrite.
    assert_eq!(layer.repair_wkb_type_codes().unwrap(), 0);
}

#[test]
fn the_finding_names_the_repair() {
    let (_dir, gpkg) = roads();
    let findings = gpkg.validate().unwrap();
    let [finding @ Finding::NonIsoWkbTypeCode { rows: 2, .. }] = findings.as_slice() else {
        panic!("expected the two flagged rows, got {findings:?}");
    };
    assert!(finding.repair().unwrap().contains("repair_wkb_type_codes"));
}

#[test]
fn a_read_only_file_with_nothing_to_repair_is_left_alone() {
    let (dir, gpkg) = roads();
    gpkg.layer("roads")
        .unwrap()
        .repair_wkb_type_codes()
        .unwrap();
    gpkg.close().unwrap();
    let gpkg = GeoPackage::open_read_only(dir.path().join("r.gpkg")).unwrap();
    assert_eq!(
        gpkg.layer("roads")
            .unwrap()
            .repair_wkb_type_codes()
            .unwrap(),
        0
    );
}

#[test]
fn a_read_only_file_is_not_repaired() {
    let (dir, gpkg) = roads();
    gpkg.close().unwrap();
    let gpkg = GeoPackage::open_read_only(dir.path().join("r.gpkg")).unwrap();
    gpkg.layer("roads")
        .unwrap()
        .repair_wkb_type_codes()
        .unwrap_err();
}
