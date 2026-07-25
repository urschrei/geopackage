//! The feature write path (Group B of the M2 write path): the `FeatureWriter`
//! insert/update/delete, `write_all` batching, `gpkg_contents` bbox and
//! `last_change` maintenance, Z/M validation, and, the critical case, writes
//! into a table that already carries the rtree spatial-index triggers.
//!
//! Round-trips write with the new API and read back through the M1 read path,
//! asserting equality of feature ids, geometry coordinates, and values,
//! including Z geometries, empty geometries, NULL geometries, NULLs, and
//! datetimes.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geo_traits::{Dimensions, GeometryTrait};
use geo_types::{LineString, Point, Polygon};
use geopackage::core::datetime::{Date, DateTime};
use geopackage::core::geometry::GpbGeometry;
use geopackage::core::gpb::{Envelope, encode_header};
use geopackage::core::triggers::{self, TriggerGeneration};
use geopackage::core::types::{ColumnType, GeometryType, ZmFlag};
use geopackage::{
    BulkIndexOptions, ColumnSpec, Error, GeoPackage, GeometrySpec, NewFeature, TableSchemaBuilder,
    Value,
};

fn gpkg() -> (tempfile::TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    (dir, gpkg)
}

/// A point layer with no attribute columns (only `fid` + `geom`).
fn point_layer(gpkg: &GeoPackage, table: &str, geom_type: GeometryType, z: ZmFlag) {
    gpkg.create_layer(
        &TableSchemaBuilder::new(table)
            .geometry(GeometrySpec::new(geom_type, 4326).z(z))
            // The index tests in this file install it themselves, sometimes
            // with raw trigger SQL, so they start from an unindexed layer.
            .spatial_index(false),
    )
    .unwrap();
}

/// A little-endian ISO WKB `POINT Z` (type 1001) wrapped in a bare GPB header,
/// parsed into a `GpbGeometry` that carries a Z dimension.
fn z_point_blob(x: f64, y: f64, z: f64) -> Vec<u8> {
    let mut blob = encode_header(4326, &Envelope::None, false, false);
    blob.push(1);
    blob.extend_from_slice(&1001u32.to_le_bytes());
    blob.extend_from_slice(&x.to_le_bytes());
    blob.extend_from_slice(&y.to_le_bytes());
    blob.extend_from_slice(&z.to_le_bytes());
    blob
}

/// The rtree index rows `(id, minx, miny)` for `pts`, ordered by id.
fn rtree_rows(gpkg: &GeoPackage) -> Vec<(i64, f64, f64)> {
    let conn = gpkg.connection();
    let mut stmt = conn
        .prepare("SELECT id, minx, miny FROM rtree_pts_geom ORDER BY id")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn roundtrip_geometries_and_values() {
    let (_dir, gpkg) = gpkg();
    let builder = TableSchemaBuilder::new("f")
        .column(ColumnSpec::new("name", ColumnType::Text(None)))
        .column(ColumnSpec::new("score", ColumnType::Double))
        .column(ColumnSpec::new("num", ColumnType::Integer))
        .column(ColumnSpec::new("active", ColumnType::Boolean))
        .column(ColumnSpec::new("seen", ColumnType::DateTime))
        .column(ColumnSpec::new("day", ColumnType::Date))
        .column(ColumnSpec::new("payload", ColumnType::Blob(None)))
        .geometry(GeometrySpec::new(GeometryType::Geometry, 4326).z(ZmFlag::Optional));
    gpkg.create_layer(&builder).unwrap();
    let layer = gpkg.layer("f").unwrap();

    let seen = DateTime::parse_strict("2026-07-24T12:34:56.789Z").unwrap();
    let day = Date::parse("2026-07-24").unwrap();
    let point = Point::new(3.0, 4.0);
    let line = LineString::from(vec![(0.0, 0.0), (10.0, -3.0), (4.0, 8.0)]);
    let poly = Polygon::new(
        LineString::from(vec![
            (0.0, 0.0),
            (6.0, 0.0),
            (6.0, 5.0),
            (0.0, 5.0),
            (0.0, 0.0),
        ]),
        vec![],
    );

    let mut w = layer.writer().unwrap();
    let f1 = w
        .insert(
            None,
            &point,
            &[
                Value::Text("a".into()),
                Value::Float(1.5),
                Value::Integer(10),
                Value::Boolean(true),
                Value::DateTime(seen),
                Value::Date(day),
                Value::Blob(vec![0x01, 0x02]),
            ],
        )
        .unwrap();
    let f2 = w
        .insert(
            Some(50),
            &line,
            &[
                Value::Text("b".into()),
                Value::Null,
                Value::Integer(-4),
                Value::Boolean(false),
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        )
        .unwrap();
    let f3 = w
        .insert(
            None,
            &poly,
            &[
                Value::Text("c".into()),
                Value::Float(9.0),
                Value::Integer(3),
                Value::Boolean(true),
                Value::DateTime(seen),
                Value::Date(day),
                Value::Null,
            ],
        )
        .unwrap();
    w.commit().unwrap();
    // Auto ids start at 1; the explicit 50 advances the sequence, so the next
    // auto id is 51.
    assert_eq!((f1, f2, f3), (1, 50, 51));

    let features: Vec<_> = layer.features().unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(features.len(), 3);
    let by_fid = |fid: i64| features.iter().find(|f| f.fid() == fid).unwrap();

    let a = by_fid(1);
    assert_eq!(a.value("name"), Some(&Value::Text("a".into())));
    assert_eq!(a.value("score"), Some(&Value::Float(1.5)));
    assert_eq!(a.value("num"), Some(&Value::Integer(10)));
    assert_eq!(a.value("active"), Some(&Value::Boolean(true)));
    assert_eq!(a.value("seen"), Some(&Value::DateTime(seen)));
    assert_eq!(a.value("day"), Some(&Value::Date(day)));
    assert_eq!(a.value("payload"), Some(&Value::Blob(vec![0x01, 0x02])));
    assert_eq!(
        a.geometry().unwrap().unwrap().to_geo().unwrap(),
        geo_types::Geometry::Point(point)
    );

    let b = by_fid(50);
    assert_eq!(b.value("name"), Some(&Value::Text("b".into())));
    assert_eq!(b.value("score"), Some(&Value::Null));
    assert_eq!(b.value("num"), Some(&Value::Integer(-4)));
    assert_eq!(b.value("active"), Some(&Value::Boolean(false)));
    assert_eq!(b.value("seen"), Some(&Value::Null));
    assert_eq!(b.value("day"), Some(&Value::Null));
    assert_eq!(
        b.geometry().unwrap().unwrap().to_geo().unwrap(),
        geo_types::Geometry::LineString(line)
    );

    let c = by_fid(51);
    assert_eq!(
        c.geometry().unwrap().unwrap().to_geo().unwrap(),
        geo_types::Geometry::Polygon(poly)
    );
}

#[test]
fn roundtrip_z_geometry_writes_xyz_envelope() {
    let (_dir, gpkg) = gpkg();
    point_layer(&gpkg, "z", GeometryType::Point, ZmFlag::Optional);
    let layer = gpkg.layer("z").unwrap();

    let blob = z_point_blob(1.0, 2.0, 9.0);
    let src = GpbGeometry::parse(&blob).unwrap();
    assert_eq!(src.dim(), Dimensions::Xyz);

    let mut w = layer.writer().unwrap();
    let fid = w.insert(None, &src, &[]).unwrap();
    w.commit().unwrap();

    let features: Vec<_> = layer.features().unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(features.len(), 1);
    let g = features[0].geometry().unwrap().unwrap();
    assert_eq!(features[0].fid(), fid);
    // The writer always emits an envelope, widened to Z here, and the body is
    // XYZ WKB.
    assert_eq!(
        g.header().envelope,
        Envelope::Xyz([1.0, 1.0, 2.0, 2.0, 9.0, 9.0])
    );
    assert_eq!(g.dim(), Dimensions::Xyz);
    assert_eq!(g.geometry_type(), GeometryType::Point);
}

#[test]
fn roundtrip_empty_and_null_geometry() {
    let (_dir, gpkg) = gpkg();
    point_layer(&gpkg, "g", GeometryType::LineString, ZmFlag::Prohibited);
    let layer = gpkg.layer("g").unwrap();

    let mut w = layer.writer().unwrap();
    // An empty geometry: the header carries the empty flag and no envelope.
    let empty = LineString::<f64>::new(vec![]);
    let empty_fid = w.insert(None, &empty, &[]).unwrap();
    // A NULL geometry (no geometry column value at all).
    let null_fid = w.insert_row(None, &[]).unwrap();
    w.commit().unwrap();

    let features: Vec<_> = layer.features().unwrap().collect::<Result<_, _>>().unwrap();
    let empty_feature = features.iter().find(|f| f.fid() == empty_fid).unwrap();
    let g = empty_feature.geometry().unwrap().unwrap();
    assert!(g.is_empty());
    assert_eq!(g.header().envelope, Envelope::None);
    assert!(g.header().empty);

    let null_feature = features.iter().find(|f| f.fid() == null_fid).unwrap();
    assert!(null_feature.geometry().unwrap().is_none());
    assert!(null_feature.geometry_bytes().is_none());
}

#[test]
fn insert_and_update_and_delete() {
    let (_dir, gpkg) = gpkg();
    point_layer(&gpkg, "u", GeometryType::Point, ZmFlag::Prohibited);
    let layer = gpkg.layer("u").unwrap();

    let mut w = layer.writer().unwrap();
    let fid = w.insert(None, &Point::new(1.0, 1.0), &[]).unwrap();
    w.commit().unwrap();

    let mut w = layer.writer().unwrap();
    assert!(w.update(fid, &Point::new(5.0, 6.0), &[]).unwrap());
    // A non-existent fid matches nothing.
    assert!(!w.update(999, &Point::new(0.0, 0.0), &[]).unwrap());
    w.commit().unwrap();

    let features: Vec<_> = layer.features().unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(features.len(), 1);
    assert_eq!(
        features[0].geometry().unwrap().unwrap().to_geo().unwrap(),
        geo_types::Geometry::Point(Point::new(5.0, 6.0))
    );

    let mut w = layer.writer().unwrap();
    assert!(w.delete(fid).unwrap());
    assert!(!w.delete(fid).unwrap());
    w.commit().unwrap();
    assert_eq!(layer.features().unwrap().count(), 0);
}

#[test]
fn uncommitted_writer_rolls_back() {
    let (_dir, gpkg) = gpkg();
    point_layer(&gpkg, "r", GeometryType::Point, ZmFlag::Prohibited);
    let layer = gpkg.layer("r").unwrap();
    {
        let mut w = layer.writer().unwrap();
        w.insert(None, &Point::new(1.0, 1.0), &[]).unwrap();
        // Dropped without commit.
    }
    assert_eq!(layer.features().unwrap().count(), 0);
}

#[test]
fn zm_presence_is_validated() {
    let (_dir, gpkg) = gpkg();

    // z Mandatory: a 2D geometry is rejected.
    point_layer(&gpkg, "mandatory", GeometryType::Point, ZmFlag::Mandatory);
    let layer = gpkg.layer("mandatory").unwrap();
    let mut w = layer.writer().unwrap();
    match w.insert(None, &Point::new(1.0, 2.0), &[]) {
        Err(Error::ZmViolation {
            dimension, verb, ..
        }) => {
            assert_eq!(dimension, "z");
            assert_eq!(verb, "lacks");
        }
        other => panic!("expected ZmViolation, got {other:?}"),
    }
    drop(w);

    // z Prohibited: a Z geometry is rejected.
    point_layer(&gpkg, "prohibited", GeometryType::Point, ZmFlag::Prohibited);
    let layer = gpkg.layer("prohibited").unwrap();
    let blob = z_point_blob(1.0, 2.0, 3.0);
    let src = GpbGeometry::parse(&blob).unwrap();
    let mut w = layer.writer().unwrap();
    match w.insert(None, &src, &[]) {
        Err(Error::ZmViolation {
            dimension, verb, ..
        }) => {
            assert_eq!(dimension, "z");
            assert_eq!(verb, "carries");
        }
        other => panic!("expected ZmViolation, got {other:?}"),
    }
}

#[test]
fn value_count_mismatch_is_typed_error() {
    let (_dir, gpkg) = gpkg();
    gpkg.create_layer(
        &TableSchemaBuilder::new("v")
            .column(ColumnSpec::new("name", ColumnType::Text(None)))
            .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
    )
    .unwrap();
    let layer = gpkg.layer("v").unwrap();
    let mut w = layer.writer().unwrap();
    match w.insert(None, &Point::new(0.0, 0.0), &[]) {
        Err(Error::ValueCountMismatch {
            expected, found, ..
        }) => {
            assert_eq!(expected, 1);
            assert_eq!(found, 0);
        }
        other => panic!("expected ValueCountMismatch, got {other:?}"),
    }
}

#[test]
fn bbox_and_last_change_maintained_on_commit() {
    let (_dir, gpkg) = gpkg();
    point_layer(&gpkg, "bb", GeometryType::Point, ZmFlag::Prohibited);
    let layer = gpkg.layer("bb").unwrap();

    let mut w = layer.writer().unwrap();
    w.insert(None, &Point::new(2.0, 3.0), &[]).unwrap();
    w.insert(None, &Point::new(-1.0, 8.0), &[]).unwrap();
    w.commit().unwrap();

    let entry = gpkg
        .contents()
        .unwrap()
        .into_iter()
        .find(|e| e.table_name == "bb")
        .unwrap();
    assert_eq!(entry.min_x, Some(-1.0));
    assert_eq!(entry.min_y, Some(3.0));
    assert_eq!(entry.max_x, Some(2.0));
    assert_eq!(entry.max_y, Some(8.0));

    // last_change is refreshed to the strict 1.4 datetime form.
    let last_change: String = gpkg
        .connection()
        .query_row(
            "SELECT last_change FROM gpkg_contents WHERE table_name = 'bb'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        DateTime::parse_strict(&last_change).is_ok(),
        "last_change {last_change:?} is not strict 1.4 form"
    );

    // A further write unions into the existing box, never shrinking it.
    let mut w = layer.writer().unwrap();
    w.insert(None, &Point::new(0.0, 100.0), &[]).unwrap();
    w.commit().unwrap();
    let entry = gpkg
        .contents()
        .unwrap()
        .into_iter()
        .find(|e| e.table_name == "bb")
        .unwrap();
    assert_eq!(entry.min_x, Some(-1.0));
    assert_eq!(entry.max_y, Some(100.0));
}

#[test]
fn write_all_batches_and_returns_fids() {
    let (_dir, gpkg) = gpkg();
    point_layer(&gpkg, "wa", GeometryType::Point, ZmFlag::Prohibited);
    let layer = gpkg.layer("wa").unwrap();

    let features: Vec<NewFeature<Point<f64>>> = (0..250)
        .map(|i| NewFeature::new(Point::new(f64::from(i), f64::from(i)), Vec::new()))
        .collect();
    let fids = layer.write_all(features, 100).unwrap();
    assert_eq!(fids, (1..=250).collect::<Vec<_>>());
    assert_eq!(layer.features().unwrap().count(), 250);

    // The bounding box spans every written point.
    let entry = gpkg
        .contents()
        .unwrap()
        .into_iter()
        .find(|e| e.table_name == "wa")
        .unwrap();
    assert_eq!(entry.min_x, Some(0.0));
    assert_eq!(entry.max_x, Some(249.0));
}

#[test]
fn write_all_empty_iterator_is_noop() {
    let (_dir, gpkg) = gpkg();
    point_layer(&gpkg, "e", GeometryType::Point, ZmFlag::Prohibited);
    let layer = gpkg.layer("e").unwrap();
    let fids = layer
        .write_all(Vec::<NewFeature<Point<f64>>>::new(), 0)
        .unwrap();
    assert!(fids.is_empty());
    assert_eq!(layer.features().unwrap().count(), 0);
}

#[test]
fn writes_into_indexed_table_track_rtree() {
    // The critical correctness case: writes through the FeatureWriter into a
    // table that already carries the 1.4 rtree triggers must maintain the
    // index. The index is built here with the same trigger SQL the M1
    // rtree_triggers.rs test uses.
    let (_dir, gpkg) = gpkg();
    point_layer(&gpkg, "pts", GeometryType::Point, ZmFlag::Prohibited);
    let conn = gpkg.connection();
    conn.execute_batch(&triggers::create_rtree_table_sql("pts", "geom").unwrap())
        .unwrap();
    for sql in triggers::create_triggers_sql("pts", "geom", "fid").unwrap() {
        conn.execute_batch(&sql).unwrap();
    }
    let layer = gpkg.layer("pts").unwrap();
    assert!(layer.has_spatial_index().unwrap());

    // Insert (auto and explicit fid): the insert trigger populates the rtree.
    let mut w = layer.writer().unwrap();
    let fid1 = w.insert(None, &Point::new(10.0, 20.0), &[]).unwrap();
    let fid2 = w.insert(Some(2), &Point::new(-5.0, 7.0), &[]).unwrap();
    w.commit().unwrap();
    let rows = rtree_rows(&gpkg);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, fid1);
    assert!((rows[0].1 - 10.0).abs() < 1e-6 && (rows[0].2 - 20.0).abs() < 1e-6);
    assert_eq!(rows[1].0, fid2);
    assert!((rows[1].1 + 5.0).abs() < 1e-6 && (rows[1].2 - 7.0).abs() < 1e-6);

    // Update: the update6 trigger moves the entry.
    let mut w = layer.writer().unwrap();
    assert!(w.update(fid1, &Point::new(100.0, 200.0), &[]).unwrap());
    w.commit().unwrap();
    let rows = rtree_rows(&gpkg);
    assert_eq!(rows.len(), 2);
    let moved = rows.iter().find(|r| r.0 == fid1).unwrap();
    assert!((moved.1 - 100.0).abs() < 1e-6 && (moved.2 - 200.0).abs() < 1e-6);

    // Delete: the delete trigger removes the entry.
    let mut w = layer.writer().unwrap();
    assert!(w.delete(fid2).unwrap());
    w.commit().unwrap();
    let rows = rtree_rows(&gpkg);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, fid1);

    // The bbox query path (M1) still agrees with the index.
    let hits: Vec<i64> = layer
        .features_in(geopackage::BoundingBox::new(90.0, 190.0, 110.0, 210.0))
        .unwrap()
        .map(|r| r.unwrap().fid())
        .collect();
    assert_eq!(hits, vec![fid1]);
}

/// The full rtree contents `(id, minx, maxx, miny, maxy)` for `pts`, by id.
fn rtree_full(gpkg: &GeoPackage) -> Vec<(i64, f64, f64, f64, f64)> {
    let conn = gpkg.connection();
    let mut stmt = conn
        .prepare("SELECT id, minx, maxx, miny, maxy FROM rtree_pts_geom ORDER BY id")
        .unwrap();
    stmt.query_map([], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

/// A manual `ST_*` envelope scan of `pts`, the reference the index must match.
fn envelope_scan(gpkg: &GeoPackage) -> Vec<(i64, f64, f64, f64, f64)> {
    let conn = gpkg.connection();
    let mut stmt = conn
        .prepare(
            "SELECT fid, ST_MinX(geom), ST_MaxX(geom), ST_MinY(geom), ST_MaxY(geom) \
             FROM pts WHERE geom NOT NULL AND NOT ST_IsEmpty(geom) ORDER BY fid",
        )
        .unwrap();
    stmt.query_map([], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

fn pts_generation(gpkg: &GeoPackage) -> TriggerGeneration {
    let conn = gpkg.connection();
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = 'pts'")
        .unwrap();
    let names: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    triggers::classify_triggers(names.iter().map(String::as_str), "pts", "geom")
}

#[test]
fn write_all_bulk_builds_index_matching_full_scan() {
    // Build a fresh, empty spatial index, then bulk-write into it: the bulk
    // path drops the triggers, inserts the rows, rebuilds the index in bulk,
    // and reinstalls the 1.4 triggers.
    let (_dir, gpkg) = gpkg();
    point_layer(&gpkg, "pts", GeometryType::Point, ZmFlag::Prohibited);
    let layer = gpkg.layer("pts").unwrap();
    layer.create_spatial_index().unwrap();
    assert!(layer.has_spatial_index().unwrap());
    assert!(rtree_full(&gpkg).is_empty());

    // Coordinates exact-in-f32 so the index bounds equal the envelope scan.
    let features: Vec<NewFeature<Point<f64>>> = (0..300)
        .map(|i| NewFeature::new(Point::new(f64::from(i), f64::from(i * 2)), Vec::new()))
        .collect();
    let fids = layer
        .write_all_with(features, 0, BulkIndexOptions::always_bulk())
        .unwrap();
    assert_eq!(fids, (1..=300).collect::<Vec<_>>());

    // The bulk-built index matches the full envelope scan and the 1.4 triggers
    // are reinstalled.
    assert_eq!(rtree_full(&gpkg), envelope_scan(&gpkg));
    assert_eq!(rtree_full(&gpkg).len(), 300);
    assert_eq!(pts_generation(&gpkg), TriggerGeneration::V1_4);

    // gpkg_contents bbox was maintained across the bulk write.
    let entry = gpkg
        .contents()
        .unwrap()
        .into_iter()
        .find(|e| e.table_name == "pts")
        .unwrap();
    assert_eq!(entry.min_x, Some(0.0));
    assert_eq!(entry.max_x, Some(299.0));
    assert_eq!(entry.max_y, Some(598.0));

    // A later triggered write into the now-populated index is still tracked:
    // the reinstalled triggers maintain the index, and an append does not take
    // the bulk path (the index is no longer empty).
    let more: Vec<NewFeature<Point<f64>>> =
        vec![NewFeature::new(Point::new(1000.0, 2000.0), Vec::new()).with_fid(301)];
    layer
        .write_all_with(more, 0, BulkIndexOptions::always_bulk())
        .unwrap();
    assert_eq!(rtree_full(&gpkg), envelope_scan(&gpkg));
    assert_eq!(rtree_full(&gpkg).len(), 301);
}

#[test]
#[ignore = "needs GDAL's ogrinfo on PATH (installed locally, absent in CI)"]
fn ogrinfo_reads_written_features() {
    use std::process::Command;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ogr.gpkg");
    let written = [
        (1i64, 3.0f64, 4.0f64, "alpha"),
        (2, -10.5, 22.25, "beta"),
        (3, 100.0, -0.5, "gamma"),
    ];
    {
        let gpkg = GeoPackage::create(&path).unwrap();
        gpkg.create_layer(
            &TableSchemaBuilder::new("places")
                .column(ColumnSpec::new("name", ColumnType::Text(None)))
                .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
        )
        .unwrap();
        let layer = gpkg.layer("places").unwrap();
        let mut w = layer.writer().unwrap();
        for (fid, x, y, name) in written {
            w.insert(Some(fid), &Point::new(x, y), &[Value::Text(name.into())])
                .unwrap();
        }
        w.commit().unwrap();
    }

    let output = match Command::new("ogrinfo")
        .args(["-json", "-al", "-features", path.to_str().unwrap()])
        .output()
    {
        Ok(output) => output,
        Err(e) => {
            eprintln!("ogrinfo not runnable ({e}); skipping");
            return;
        }
    };
    assert!(
        output.status.success(),
        "ogrinfo failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let layer = json["layers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["name"] == "places")
        .expect("places layer in ogrinfo output");
    let features = layer["features"].as_array().unwrap();
    assert_eq!(features.len(), written.len());

    for (fid, x, y, name) in written {
        let feature = features
            .iter()
            .find(|f| f["properties"]["name"] == name)
            .unwrap_or_else(|| panic!("feature {name:?} in ogrinfo output"));
        let coords = feature["geometry"]["coordinates"].as_array().unwrap();
        let gx = coords[0].as_f64().unwrap();
        let gy = coords[1].as_f64().unwrap();
        assert!(
            (gx - x).abs() < 1e-9 && (gy - y).abs() < 1e-9,
            "feature {fid} coordinates: got ({gx}, {gy}), want ({x}, {y})"
        );
    }
    eprintln!("ogrinfo round-trip OK: {} features verified", written.len());
}
