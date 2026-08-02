//! The registered `ST_*` functions over envelope-less GPB blobs, exercised
//! through SQL. These cover the fallback path: full WKB traversal for
//! non-point geometries whose GPB header has no envelope.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geo_types::{Geometry, LineString, MultiPolygon, Point, Polygon};
use geopackage::GeoPackage;
use geopackage::core::gpb::{Envelope, encode_header};
use wkb::Endianness;
use wkb::writer::{WriteOptions, write_geometry};

/// A GPB blob with no header envelope, wrapping `geom` as ISO WKB.
fn envelopeless_gpb(geom: &Geometry<f64>, little_endian: bool) -> Vec<u8> {
    let mut blob = encode_header(4326, &Envelope::None, false, false);
    let options = WriteOptions {
        endianness: if little_endian {
            Endianness::LittleEndian
        } else {
            Endianness::BigEndian
        },
    };
    write_geometry(&mut blob, geom, &options).unwrap();
    blob
}

fn gpkg() -> (tempfile::TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    (dir, gpkg)
}

/// `(min_x, max_x, min_y, max_y, is_empty)` from the registered functions.
fn st_bounds(gpkg: &GeoPackage, blob: &[u8]) -> (f64, f64, f64, f64, bool) {
    gpkg.connection()
        .query_row(
            "SELECT ST_MinX(?1), ST_MaxX(?1), ST_MinY(?1), ST_MaxY(?1), ST_IsEmpty(?1)",
            [blob],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap()
}

#[test]
fn linestring_bounds_from_traversal() {
    let (_dir, gpkg) = gpkg();
    let geom: Geometry<f64> = LineString::from(vec![(0.0, 0.0), (10.0, -3.0), (4.0, 8.0)]).into();
    let blob = envelopeless_gpb(&geom, true);
    assert_eq!(st_bounds(&gpkg, &blob), (0.0, 10.0, -3.0, 8.0, false));
}

#[test]
fn polygon_bounds_from_traversal() {
    let (_dir, gpkg) = gpkg();
    let geom: Geometry<f64> = Polygon::new(
        LineString::from(vec![
            (0.0, 0.0),
            (6.0, 0.0),
            (6.0, 5.0),
            (0.0, 5.0),
            (0.0, 0.0),
        ]),
        vec![],
    )
    .into();
    let blob = envelopeless_gpb(&geom, true);
    assert_eq!(st_bounds(&gpkg, &blob), (0.0, 6.0, 0.0, 5.0, false));
}

#[test]
fn multipolygon_bounds_from_traversal() {
    let (_dir, gpkg) = gpkg();
    let geom: Geometry<f64> = MultiPolygon::new(vec![
        Polygon::new(
            LineString::from(vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)]),
            vec![],
        ),
        Polygon::new(
            LineString::from(vec![(-5.0, -5.0), (-4.0, -5.0), (-4.0, -4.0), (-5.0, -5.0)]),
            vec![],
        ),
    ])
    .into();
    let blob = envelopeless_gpb(&geom, true);
    assert_eq!(st_bounds(&gpkg, &blob), (-5.0, 1.0, -5.0, 1.0, false));
}

#[test]
fn big_endian_body_bounds_from_traversal() {
    let (_dir, gpkg) = gpkg();
    let geom: Geometry<f64> = LineString::from(vec![(1.0, 2.0), (5.0, -1.0)]).into();
    let blob = envelopeless_gpb(&geom, false);
    assert_eq!(st_bounds(&gpkg, &blob), (1.0, 5.0, -1.0, 2.0, false));
}

#[test]
fn empty_linestring_reports_empty() {
    let (_dir, gpkg) = gpkg();
    let geom: Geometry<f64> = LineString::<f64>::new(vec![]).into();
    let blob = envelopeless_gpb(&geom, true);
    // A zero-coordinate geometry has no extent: ST_IsEmpty is true, and the
    // bound functions return NaN, which SQLite surfaces as NULL.
    let is_empty: bool = gpkg
        .connection()
        .query_row("SELECT ST_IsEmpty(?1)", [&blob], |r| r.get(0))
        .unwrap();
    assert!(is_empty, "a zero-coordinate linestring is empty");
    let min_x: Option<f64> = gpkg
        .connection()
        .query_row("SELECT ST_MinX(?1)", [&blob], |r| r.get(0))
        .unwrap();
    assert_eq!(min_x, None, "no extent surfaces as SQL NULL");
}

#[test]
fn envelopeless_point_still_works() {
    // The simplest fallback case, an envelope-less point, must keep working
    // through the same traversal.
    let (_dir, gpkg) = gpkg();
    let geom: Geometry<f64> = Point::new(3.0, -4.0).into();
    let blob = envelopeless_gpb(&geom, true);
    assert_eq!(st_bounds(&gpkg, &blob), (3.0, 3.0, -4.0, -4.0, false));
}

#[test]
fn curve_type_body_is_a_typed_error_not_a_panic() {
    // A CIRCULARSTRING (ISO WKB type 8) is a body the wkb crate cannot read.
    // ST_MinX must surface an error, never panic.
    let (_dir, gpkg) = gpkg();
    let mut blob = encode_header(4326, &Envelope::None, false, false);
    blob.push(1); // little-endian
    blob.extend_from_slice(&8u32.to_le_bytes()); // CircularString
    blob.extend_from_slice(&0u32.to_le_bytes()); // zero points
    let err = gpkg
        .connection()
        .query_row("SELECT ST_MinX(?1)", [&blob], |r| r.get::<_, f64>(0));
    assert!(err.is_err(), "curve type must be a typed SQL error");
}
