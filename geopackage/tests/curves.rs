//! Non-linear geometry end to end: create a curve layer, write a curve into
//! it, and find it again through the spatial index.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geopackage::{BoundingBox, GeoPackage, GeometrySpec, TableSchemaBuilder};
use geopackage_core::gpb;
use geopackage_core::types::GeometryType;
use tempfile::TempDir;

/// A little-endian CIRCULARSTRING body.
fn circular_string(points: &[[f64; 2]]) -> Vec<u8> {
    let mut bytes = vec![1u8];
    bytes.extend_from_slice(&8u32.to_le_bytes());
    bytes.extend_from_slice(&u32::try_from(points.len()).unwrap().to_le_bytes());
    for [x, y] in points {
        bytes.extend_from_slice(&x.to_le_bytes());
        bytes.extend_from_slice(&y.to_le_bytes());
    }
    bytes
}

/// An arc from 20 to 160 degrees on the unit circle, with its middle control
/// point at 40 degrees rather than at the apex.
///
/// The arc reaches y = 1 at 90 degrees, but no control point gets above
/// y = 0.643, so anything that bounds this by its control points is wrong by a
/// third of the height.
fn bulging_arc() -> Vec<u8> {
    let at = |degrees: f64| {
        let t = degrees.to_radians();
        [t.cos(), t.sin()]
    };
    circular_string(&[at(20.0), at(40.0), at(160.0)])
}

fn gpkg() -> (TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("curves.gpkg")).unwrap();
    gpkg.add_epsg_srs(4326).unwrap();
    (dir, gpkg)
}

fn curve_layer(gpkg: &GeoPackage, geometry_type: GeometryType) {
    gpkg.create_layer(
        &TableSchemaBuilder::new("arcs").geometry(GeometrySpec::new(geometry_type, 4326)),
    )
    .unwrap();
}

#[test]
fn a_curve_layer_registers_both_extensions_it_uses() {
    let (_dir, gpkg) = gpkg();
    curve_layer(&gpkg, GeometryType::CircularString);

    let names: Vec<String> = gpkg
        .connection()
        .prepare("SELECT extension_name FROM gpkg_extensions ORDER BY extension_name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();

    // Annex F.1 for the geometry type, Annex F.3 for the index over it. The
    // second is the one the spec used to be read as forbidding here.
    assert_eq!(names, ["gpkg_geom_CIRCULARSTRING", "gpkg_rtree_index"]);
}

#[test]
fn a_written_curve_carries_a_header_envelope_that_bounds_the_arc() {
    let (_dir, gpkg) = gpkg();
    curve_layer(&gpkg, GeometryType::CircularString);

    let layer = gpkg.layer("arcs").unwrap();
    let mut writer = layer.writer().unwrap();
    let fid = writer.insert_wkb(None, &bulging_arc(), &[]).unwrap();
    writer.commit().unwrap();

    let blob: Vec<u8> = gpkg
        .connection()
        .query_row("SELECT geom FROM arcs WHERE fid = ?1", [fid], |row| {
            row.get(0)
        })
        .unwrap();

    let (header, offset) = gpb::parse_header(&blob).unwrap();
    assert!(header.extended, "Annex F.1 Requirement 68");
    assert!(!header.empty);

    let (min_x, max_x, min_y, max_y) = header.envelope.xy_bounds().expect("an envelope");
    assert!(
        (max_y - 1.0).abs() < 1e-12,
        "header envelope should reach the arc's apex, got max_y {max_y}"
    );
    assert!((min_y - 20.0_f64.to_radians().sin()).abs() < 1e-12);
    assert!((min_x - 160.0_f64.to_radians().cos()).abs() < 1e-12);
    assert!((max_x - 20.0_f64.to_radians().cos()).abs() < 1e-12);

    // The body is copied through byte for byte, so a reader sees the curve it
    // was given rather than a linearisation of it.
    assert_eq!(blob.get(offset..), Some(bulging_arc().as_slice()));
}

/// `update_wkb` is `insert_wkb`'s counterpart, and exists for the same reason:
/// a curve has no `geo-types` representation, so an update that went through
/// one could not carry a curve at all. The replacement body has to arrive byte
/// for byte, and the recorded envelope has to follow it.
#[test]
fn a_curve_can_be_replaced_by_another_curve_byte_for_byte() {
    let (_dir, gpkg) = gpkg();
    curve_layer(&gpkg, GeometryType::CircularString);

    // A quarter turn near the origin, well inside the bulging arc's box, so a
    // failure to re-measure would leave the recorded envelope visibly wrong.
    let replacement = circular_string(&[[0.0, 0.0], [0.25, 0.25], [0.5, 0.0]]);

    let layer = gpkg.layer("arcs").unwrap();
    let mut writer = layer.writer().unwrap();
    let fid = writer.insert_wkb(None, &bulging_arc(), &[]).unwrap();
    let matched = writer.update_wkb(fid, &replacement, &[]).unwrap();
    assert!(matched, "the row written a moment ago was not found");
    writer.commit().unwrap();

    let blob: Vec<u8> = gpkg
        .connection()
        .query_row("SELECT geom FROM arcs WHERE fid = ?1", [fid], |row| {
            row.get(0)
        })
        .unwrap();
    let (header, offset) = gpb::parse_header(&blob).unwrap();
    assert_eq!(
        blob.get(offset..),
        Some(replacement.as_slice()),
        "the replacement body did not survive the update"
    );

    // The header envelope describes the replacement, not what was there before.
    let (min_x, max_x, min_y, max_y) = header.envelope.xy_bounds().expect("an envelope");
    assert!((min_x - 0.0).abs() < 1e-12);
    assert!((max_x - 0.5).abs() < 1e-12);
    assert!((min_y - 0.0).abs() < 1e-12);
    assert!(
        max_y < 0.3,
        "envelope still covers the old arc, got max_y {max_y}"
    );
}

/// An id that is not there is not an error, the same answer `update_row` and
/// `delete` give.
#[test]
fn replacing_a_curve_that_is_not_there_reports_no_match() {
    let (_dir, gpkg) = gpkg();
    curve_layer(&gpkg, GeometryType::CircularString);

    let layer = gpkg.layer("arcs").unwrap();
    let mut writer = layer.writer().unwrap();
    assert!(!writer.update_wkb(99, &bulging_arc(), &[]).unwrap());
    writer.commit().unwrap();
}

#[test]
fn a_query_window_only_the_arc_reaches_still_finds_it() {
    let (_dir, gpkg) = gpkg();
    curve_layer(&gpkg, GeometryType::CircularString);

    let layer = gpkg.layer("arcs").unwrap();
    let mut writer = layer.writer().unwrap();
    writer.insert_wkb(None, &bulging_arc(), &[]).unwrap();
    writer.commit().unwrap();

    // Above every control point, but the arc passes through it.
    let window = BoundingBox::new(-0.2, 0.8, 0.2, 1.2);
    let found = layer.features_in(window).unwrap().count();
    assert_eq!(found, 1, "the arc's apex is inside the window");

    // Below the arc entirely: nothing there to find.
    let miss = BoundingBox::new(-0.2, -1.2, 0.2, -0.8);
    assert_eq!(layer.features_in(miss).unwrap().count(), 0);
}

#[test]
fn the_index_entry_matches_the_arc_not_its_control_points() {
    let (_dir, gpkg) = gpkg();
    curve_layer(&gpkg, GeometryType::CircularString);

    let layer = gpkg.layer("arcs").unwrap();
    let mut writer = layer.writer().unwrap();
    writer.insert_wkb(None, &bulging_arc(), &[]).unwrap();
    writer.commit().unwrap();

    let (max_y,): (f64,) = gpkg
        .connection()
        .query_row("SELECT maxy FROM rtree_arcs_geom", [], |row| {
            Ok((row.get(0)?,))
        })
        .unwrap();

    // f32 storage, rounded outward, so the stored bound is at or above 1.0.
    assert!(
        (1.0..1.000_001).contains(&max_y),
        "rtree maxy should bound the apex, got {max_y}"
    );
}

#[test]
fn a_curve_reads_back_as_bytes_but_not_as_a_geometry() {
    let (_dir, gpkg) = gpkg();
    curve_layer(&gpkg, GeometryType::CircularString);

    let layer = gpkg.layer("arcs").unwrap();
    let mut writer = layer.writer().unwrap();
    writer.insert_wkb(None, &bulging_arc(), &[]).unwrap();
    writer.commit().unwrap();

    let mut features = layer.features().unwrap();
    let feature = features.next().unwrap().unwrap();

    // The blob is there and the body is the curve that went in.
    let blob = feature.geometry_bytes().expect("a geometry cell");
    let (_, offset) = gpb::parse_header(blob).unwrap();
    assert_eq!(blob.get(offset..), Some(bulging_arc().as_slice()));

    // But `geo-traits` has no way to describe an arc, so the typed accessor
    // cannot hand one back. This is the limitation the README states.
    assert!(
        feature.geometry().is_err(),
        "a curve has no geo-traits representation"
    );
}

#[test]
fn a_compound_curve_round_trips_through_a_curve_polygon() {
    let (_dir, gpkg) = gpkg();
    curve_layer(&gpkg, GeometryType::CurvePolygon);

    // A CurvePolygon whose single ring is a CircularString closing on itself:
    // the whole unit circle.
    let ring = circular_string(&[[1.0, 0.0], [-1.0, 0.0], [1.0, 0.0]]);
    let mut body = vec![1u8];
    body.extend_from_slice(&10u32.to_le_bytes());
    body.extend_from_slice(&1u32.to_le_bytes());
    body.extend_from_slice(&ring);

    let layer = gpkg.layer("arcs").unwrap();
    let mut writer = layer.writer().unwrap();
    writer.insert_wkb(None, &body, &[]).unwrap();
    writer.commit().unwrap();

    let stored: Vec<u8> = gpkg
        .connection()
        .query_row("SELECT geom FROM arcs", [], |row| row.get(0))
        .unwrap();
    let (header, offset) = gpb::parse_header(&stored).unwrap();
    let (min_x, max_x, min_y, max_y) = header.envelope.xy_bounds().expect("an envelope");
    assert!((min_x + 1.0).abs() < 1e-12 && (max_x - 1.0).abs() < 1e-12);
    assert!((min_y + 1.0).abs() < 1e-12 && (max_y - 1.0).abs() < 1e-12);
    assert_eq!(stored.get(offset..), Some(body.as_slice()));

    assert_eq!(
        layer
            .features_in(BoundingBox::new(-2.0, -2.0, 2.0, 2.0))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn an_abstract_geometry_type_has_no_encoding_and_is_refused() {
    let (_dir, gpkg) = gpkg();
    // CURVE is a legal declared column type (Requirement 65) even though no
    // body can carry its code, so the layer is created and the write fails.
    curve_layer(&gpkg, GeometryType::Curve);

    let layer = gpkg.layer("arcs").unwrap();
    let mut writer = layer.writer().unwrap();
    let mut body = vec![1u8];
    body.extend_from_slice(&13u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    writer
        .insert_wkb(None, &body, &[])
        .expect_err("CURVE has no encoding");
}

// --- the GDAL-written fixture -------------------------------------------------
//
// `gdal_curves.gpkg` holds one GDAL-written layer per non-linear type, each
// spatially indexed. GDAL computed those RTree entries with its own arc
// mathematics (`OGRCircularString::ExtendEnvelopeWithCircular`, which sweeps
// quadrant boundaries) rather than with the chord-side test this crate uses, so
// the entries are an independent oracle for our envelopes rather than a
// restatement of them.

/// Every layer in the fixture, with the extent GDAL recorded for its one
/// feature. Kept here rather than read from the file so a regression in our
/// reader cannot quietly rewrite the expectation.
///
/// `circularstring` and `compoundcurve` reach y = 1 at the arc's apex, which is
/// above every control point. The other three close their arcs into whole
/// circles, so all four axis extremes are on them.
const FIXTURE_EXTENTS: &[(&str, [f64; 4])] = &[
    (
        "circularstring",
        [
            -0.939_692_620_785_908_3,
            0.939_692_620_785_908_4,
            0.342_020_143_325_668_7,
            1.0,
        ],
    ),
    ("compoundcurve", [-3.0, 0.939_692_620_785_908_4, -1.0, 1.0]),
    ("curvepolygon", [0.0, 2.0, -1.0, 1.0]),
    ("multicurve", [5.0, 7.0, 5.0, 6.0]),
    ("multisurface", [10.0, 12.0, 9.0, 11.0]),
];

fn fixture() -> (TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("gdal_curves.gpkg");
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("gdal_curves.gpkg");
    std::fs::copy(src, &dst).unwrap();
    let gpkg = GeoPackage::open(&dst).unwrap();
    (dir, gpkg)
}

#[test]
fn the_fixture_opens_and_reports_each_declared_curve_type() {
    let (_dir, gpkg) = fixture();
    let mut got: Vec<(String, GeometryType)> = gpkg
        .layers()
        .unwrap()
        .into_iter()
        .map(|layer| {
            let column = layer.geometry_column().expect("a feature layer").clone();
            (column.table_name, column.geometry_type)
        })
        .collect();
    got.sort_by(|a, b| a.0.cmp(&b.0));

    assert_eq!(
        got,
        vec![
            ("circularstring".to_owned(), GeometryType::CircularString),
            ("compoundcurve".to_owned(), GeometryType::CompoundCurve),
            ("curvepolygon".to_owned(), GeometryType::CurvePolygon),
            ("multicurve".to_owned(), GeometryType::MultiCurve),
            ("multisurface".to_owned(), GeometryType::MultiSurface),
        ]
    );
}

#[test]
fn our_envelope_matches_what_gdal_recorded_for_each_arc() {
    let (_dir, gpkg) = fixture();
    for (table, expected) in FIXTURE_EXTENTS {
        let blob: Vec<u8> = gpkg
            .connection()
            .query_row(&format!("SELECT geom FROM \"{table}\""), [], |row| {
                row.get(0)
            })
            .unwrap();
        let ours = geopackage_core::geometry::blob_xy_envelope(&blob)
            .unwrap()
            .unwrap_or_else(|| panic!("{table}: no envelope"));

        for (index, name) in ["min_x", "max_x", "min_y", "max_y"].iter().enumerate() {
            let got = ours[index];
            let want = expected[index];
            // GDAL's quadrant sweep and our chord-side test are different
            // routes to the same box, so they agree to within a few ulps
            // rather than bit for bit.
            assert!(
                (got - want).abs() <= 1e-12,
                "{table} {name}: ours {got}, GDAL {want}"
            );
        }
    }
}

#[test]
fn our_envelope_sits_inside_the_index_cell_gdal_wrote() {
    // The stronger statement: whatever GDAL's own index claims for the row, our
    // envelope is inside it, so a query planned against GDAL's index and
    // re-filtered by our exact test cannot lose the feature.
    let (_dir, gpkg) = fixture();
    for (table, _) in FIXTURE_EXTENTS {
        let (cell_min_x, cell_max_x, cell_min_y, cell_max_y): (f64, f64, f64, f64) = gpkg
            .connection()
            .query_row(
                &format!("SELECT minx, maxx, miny, maxy FROM \"rtree_{table}_geom\""),
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();

        let blob: Vec<u8> = gpkg
            .connection()
            .query_row(&format!("SELECT geom FROM \"{table}\""), [], |row| {
                row.get(0)
            })
            .unwrap();
        let [min_x, max_x, min_y, max_y] = geopackage_core::geometry::blob_xy_envelope(&blob)
            .unwrap()
            .unwrap();

        assert!(
            min_x >= cell_min_x && max_x <= cell_max_x,
            "{table}: x [{min_x}, {max_x}] outside GDAL cell [{cell_min_x}, {cell_max_x}]"
        );
        assert!(
            min_y >= cell_min_y && max_y <= cell_max_y,
            "{table}: y [{min_y}, {max_y}] outside GDAL cell [{cell_min_y}, {cell_max_y}]"
        );
    }
}

#[test]
fn a_query_through_gdals_index_finds_the_apex_of_a_gdal_written_arc() {
    let (_dir, gpkg) = fixture();
    let layer = gpkg.layer("circularstring").unwrap();

    // Above every control point of the arc, which tops out at y = 0.643.
    let apex = BoundingBox::new(-0.2, 0.95, 0.2, 1.2);
    assert_eq!(layer.features_in(apex).unwrap().count(), 1);

    let below = BoundingBox::new(-0.2, -1.2, 0.2, -0.8);
    assert_eq!(layer.features_in(below).unwrap().count(), 0);
}

#[test]
fn every_extension_row_in_the_fixture_classifies() {
    use geopackage::core::extensions::ExtensionSupport;

    let (_dir, gpkg) = fixture();
    for row in gpkg.extensions().unwrap() {
        assert_ne!(
            row.extension().support(),
            ExtensionSupport::Unrecognised,
            "unrecognised extension {} on {:?}",
            row.name,
            row.table_name
        );
    }
}

#[test]
fn gdal_registers_member_types_that_we_do_not() {
    // A divergence worth pinning rather than discovering later: GDAL registers
    // a gpkg_geom_<TYPE> row for the member types it actually wrote, so its
    // multicurve layer carries CIRCULARSTRING alongside MULTICURVE. Our
    // create_layer registers only the declared column type, because that is all
    // it is given. Recorded as an open roadmap item under M5 phase 6.
    let (_dir, gpkg) = fixture();
    let mut names: Vec<String> = gpkg
        .table_extensions("multicurve")
        .unwrap()
        .into_iter()
        .map(|row| row.name)
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "gpkg_geom_CIRCULARSTRING",
            "gpkg_geom_MULTICURVE",
            "gpkg_rtree_index",
        ]
    );
}
