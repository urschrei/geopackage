//! The columnar write path: `Layer::write_arrow`.

#![cfg(feature = "arrow")]
#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use arrow_array::cast::AsArray;
use arrow_array::types::{Date32Type, Float64Type, Int64Type, TimestampMicrosecondType};
use arrow_array::{Array, RecordBatch};
use geo_types::Point;
use geopackage::arrow::ArrowReadOptions;
use geopackage::core::datetime::{Date, DateTime};
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{ColumnSpec, GeoPackage, GeometrySpec, NewFeature, TableSchemaBuilder, Value};

/// A layer with one column of each interesting type, at `path`.
fn typed_layer(gpkg: &GeoPackage, name: &str) {
    let builder = TableSchemaBuilder::new(name)
        .column(ColumnSpec::new("label", ColumnType::Text(None)))
        .column(ColumnSpec::new("count", ColumnType::Integer))
        .column(ColumnSpec::new("ratio", ColumnType::Double))
        .column(ColumnSpec::new("flag", ColumnType::Boolean))
        .column(ColumnSpec::new("born", ColumnType::Date))
        .column(ColumnSpec::new("seen", ColumnType::DateTime))
        .column(ColumnSpec::new("payload", ColumnType::Blob(None)))
        .geometry(GeometrySpec::new(GeometryType::Point, 4326));
    gpkg.create_layer(&builder).unwrap();
}

/// Populate `source` with `rows` features covering every column.
fn populate(gpkg: &GeoPackage, name: &str, rows: i64) {
    let layer = gpkg.layer(name).unwrap();
    let features: Vec<NewFeature<Point<f64>>> = (1..=rows)
        .map(|i| {
            NewFeature::new(
                Point::new(i as f64, -(i as f64)),
                vec![
                    Value::Text(format!("row {i}")),
                    Value::Integer(i * 7),
                    Value::Float(i as f64 / 4.0),
                    Value::Boolean(i % 2 == 0),
                    Value::Date(Date::new(2026, 7, 25).unwrap()),
                    Value::DateTime(DateTime::parse_strict("2026-07-24T12:34:56.789Z").unwrap()),
                    Value::Blob(vec![0xde, 0xad]),
                ],
            )
            .with_fid(i)
        })
        .collect();
    layer.write_all(features, 0).unwrap();
}

/// Read `name` as Arrow batches.
fn batches(gpkg: &GeoPackage, name: &str, options: ArrowReadOptions) -> Vec<RecordBatch> {
    gpkg.layer(name)
        .unwrap()
        .read_arrow(options)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

/// Everything this crate can read as Arrow, it can write back, and the result
/// matches the original column for column.
///
/// This is the property the whole columnar write path exists to have. It also
/// pins the geometry pass-through: the WKB the reader produced goes back into a
/// GPB blob without a round trip through a geometry object, so anything lost in
/// that translation would show up here.
#[test]
fn a_layer_round_trips_through_arrow() {
    let dir = tempfile::tempdir().unwrap();
    let source = GeoPackage::create(dir.path().join("src.gpkg")).unwrap();
    typed_layer(&source, "pts");
    populate(&source, "pts", 250);

    let target = GeoPackage::create(dir.path().join("dst.gpkg")).unwrap();
    typed_layer(&target, "pts");

    let read = batches(&source, "pts", ArrowReadOptions::with_batch_size(64));
    assert!(
        read.len() > 1,
        "more than one batch, so batching is covered"
    );
    let written = target
        .layer("pts")
        .unwrap()
        .write_arrow(read.into_iter().map(Ok), 0)
        .unwrap();
    assert_eq!(written.len(), 250);

    let before = batches(&source, "pts", ArrowReadOptions::default());
    let after = batches(&target, "pts", ArrowReadOptions::default());
    assert_eq!(before.len(), 1);
    assert_eq!(after.len(), 1);
    assert_eq!(
        before[0], after[0],
        "the copy differs from the original somewhere"
    );
}

/// Spot-check the individual conversions, so a failure says which column.
#[test]
fn every_column_type_survives_the_write() {
    let dir = tempfile::tempdir().unwrap();
    let source = GeoPackage::create(dir.path().join("s.gpkg")).unwrap();
    typed_layer(&source, "pts");
    populate(&source, "pts", 3);
    let target = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    typed_layer(&target, "pts");

    let read = batches(&source, "pts", ArrowReadOptions::default());
    target
        .layer("pts")
        .unwrap()
        .write_arrow(read.into_iter().map(Ok), 0)
        .unwrap();

    let after = batches(&target, "pts", ArrowReadOptions::default());
    let batch = &after[0];
    assert_eq!(batch.num_rows(), 3);

    assert_eq!(
        batch
            .column_by_name("fid")
            .unwrap()
            .as_primitive::<Int64Type>()
            .values(),
        &[1, 2, 3],
        "explicit feature ids are preserved"
    );
    assert_eq!(
        batch
            .column_by_name("label")
            .unwrap()
            .as_string::<i32>()
            .value(0),
        "row 1"
    );
    assert_eq!(
        batch
            .column_by_name("count")
            .unwrap()
            .as_primitive::<Int64Type>()
            .values(),
        &[7, 14, 21]
    );
    assert_eq!(
        batch
            .column_by_name("ratio")
            .unwrap()
            .as_primitive::<Float64Type>()
            .values(),
        &[0.25, 0.5, 0.75]
    );
    assert!(
        !batch.column_by_name("flag").unwrap().as_boolean().value(0),
        "row 1 is odd"
    );
    assert_eq!(
        batch
            .column_by_name("born")
            .unwrap()
            .as_primitive::<Date32Type>()
            .value(0),
        20659,
        "2026-07-25, unchanged by the round trip through Date32"
    );
    assert_eq!(
        batch
            .column_by_name("seen")
            .unwrap()
            .as_primitive::<TimestampMicrosecondType>()
            .value(0),
        1_784_896_496_789_000,
        "the instant survives both conversions"
    );
    assert_eq!(
        batch
            .column_by_name("payload")
            .unwrap()
            .as_binary::<i32>()
            .value(0),
        &[0xde, 0xad]
    );
}

/// A NULL geometry and NULL attributes survive the write as nulls, not as empty
/// values.
#[test]
fn nulls_survive_the_write() {
    let dir = tempfile::tempdir().unwrap();
    let source = GeoPackage::create(dir.path().join("s.gpkg")).unwrap();
    typed_layer(&source, "pts");
    {
        let layer = source.layer("pts").unwrap();
        let mut writer = layer.writer().unwrap();
        writer
            .insert_row(
                Some(1),
                &[
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                ],
            )
            .unwrap();
        writer.commit().unwrap();
    }
    let target = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    typed_layer(&target, "pts");

    let read = batches(&source, "pts", ArrowReadOptions::default());
    target
        .layer("pts")
        .unwrap()
        .write_arrow(read.into_iter().map(Ok), 0)
        .unwrap();

    let after = batches(&target, "pts", ArrowReadOptions::default());
    let batch = &after[0];
    assert!(batch.column_by_name("geom").unwrap().is_null(0));
    assert!(batch.column_by_name("label").unwrap().is_null(0));
    assert!(batch.column_by_name("born").unwrap().is_null(0));
}

/// A batch naming a column the layer does not have is refused rather than
/// having that column silently dropped.
#[test]
fn an_unknown_column_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let source = GeoPackage::create(dir.path().join("s.gpkg")).unwrap();
    typed_layer(&source, "pts");
    populate(&source, "pts", 2);

    // A target without the `payload` column the source has.
    let target = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    let builder = TableSchemaBuilder::new("pts")
        .column(ColumnSpec::new("label", ColumnType::Text(None)))
        .geometry(GeometrySpec::new(GeometryType::Point, 4326));
    target.create_layer(&builder).unwrap();

    let read = batches(&source, "pts", ArrowReadOptions::default());
    let result = target
        .layer("pts")
        .unwrap()
        .write_arrow(read.into_iter().map(Ok), 0);
    assert!(
        result.is_err(),
        "writing columns the layer has no room for should fail, not drop them"
    );
}

/// A write large enough for the bulk index path takes it, and the index comes
/// out matching a full scan.
///
/// A `RecordBatchReader` never says how long it is, which is the case issue #17
/// taught the bulk path to buffer for rather than trust a size hint. This is
/// that case arriving for real.
#[test]
fn a_large_columnar_write_builds_the_index_in_bulk() {
    let dir = tempfile::tempdir().unwrap();
    let source = GeoPackage::create(dir.path().join("s.gpkg")).unwrap();
    typed_layer(&source, "pts");
    populate(&source, "pts", 400);

    let target = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    typed_layer(&target, "pts");
    let layer = target.layer("pts").unwrap();
    layer.create_spatial_index().unwrap();

    let read = batches(&source, "pts", ArrowReadOptions::with_batch_size(50));
    // A threshold of 1 makes the bulk path engage; the point is that it can,
    // from a source that never declares its length.
    layer
        .write_arrow_with(
            read.into_iter().map(Ok),
            0,
            geopackage::BulkIndexOptions::with_threshold(1),
        )
        .unwrap();

    let conn = target.connection();
    let indexed: i64 = conn
        .query_row("SELECT count(*) FROM rtree_pts_geom", [], |r| r.get(0))
        .unwrap();
    assert_eq!(indexed, 400, "every row indexed");
    assert_eq!(
        layer.spatial_index_status().unwrap(),
        geopackage::SpatialIndexStatus::Current,
        "the triggers are back"
    );
}

/// A layer created from an Arrow schema receives that schema's data, and the
/// copy matches the original.
///
/// This is the shape a `gpkg copy` command needs: read a layer, create the
/// target from what the read describes, write into it, without the caller
/// restating the schema.
#[test]
fn a_layer_can_be_created_from_an_arrow_schema() {
    let dir = tempfile::tempdir().unwrap();
    let source = GeoPackage::create(dir.path().join("s.gpkg")).unwrap();
    typed_layer(&source, "pts");
    populate(&source, "pts", 40);

    let schema = source.layer("pts").unwrap().arrow_schema().unwrap();
    let target = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    let builder = TableSchemaBuilder::new("pts")
        .from_arrow_schema(&schema)
        .unwrap();
    target.create_layer(&builder).unwrap();

    let read = batches(&source, "pts", ArrowReadOptions::default());
    target
        .layer("pts")
        .unwrap()
        .write_arrow(read.into_iter().map(Ok), 0)
        .unwrap();

    let before = batches(&source, "pts", ArrowReadOptions::default());
    let after = batches(&target, "pts", ArrowReadOptions::default());
    assert_eq!(
        before[0], after[0],
        "the derived layer holds different data"
    );
}

/// The derived schema carries the SRS across, and declares a geometry column
/// that accepts anything, since WKB does not say what it will contain.
#[test]
fn a_derived_layer_keeps_the_srs_and_takes_any_geometry() {
    // A non-4326 SRS, so the test would notice a hard-coded default. It has to
    // be registered in a file before a layer there can use it.
    let british_grid = || geopackage::Srs {
        name: "OSGB36 / British National Grid".into(),
        srs_id: 27700,
        organization: "EPSG".into(),
        organization_coordsys_id: 27700,
        definition: "undefined".into(),
        description: None,
    };

    let dir = tempfile::tempdir().unwrap();
    let source = GeoPackage::create(dir.path().join("s.gpkg")).unwrap();
    source.add_srs(&british_grid()).unwrap();
    source
        .create_layer(
            &TableSchemaBuilder::new("pts").geometry(GeometrySpec::new(GeometryType::Point, 27700)),
        )
        .unwrap();

    let schema = source.layer("pts").unwrap().arrow_schema().unwrap();
    let target = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    target.add_srs(&british_grid()).unwrap();
    target
        .create_layer(
            &TableSchemaBuilder::new("pts")
                .from_arrow_schema(&schema)
                .unwrap(),
        )
        .unwrap();

    let geometry = target
        .layer("pts")
        .unwrap()
        .geometry_column()
        .unwrap()
        .clone();
    assert_eq!(
        geometry.srs_id, 27700,
        "the EPSG code survived the round trip"
    );
    assert_eq!(
        geometry.geometry_type,
        GeometryType::Geometry,
        "WKB does not declare a type, so the column must accept any"
    );
}

/// The primary key is not turned into an attribute column: the builder makes
/// it, so the schema's `fid` field is skipped.
#[test]
fn the_primary_key_field_is_not_duplicated() {
    let dir = tempfile::tempdir().unwrap();
    let source = GeoPackage::create(dir.path().join("s.gpkg")).unwrap();
    typed_layer(&source, "pts");

    let schema = source.layer("pts").unwrap().arrow_schema().unwrap();
    let target = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    target
        .create_layer(
            &TableSchemaBuilder::new("pts")
                .from_arrow_schema(&schema)
                .unwrap(),
        )
        .unwrap();

    let layer = target.layer("pts").unwrap();
    assert_eq!(layer.primary_key_column(), Some("fid"));
    let fid_columns = layer
        .schema()
        .columns
        .iter()
        .filter(|column| column.name == "fid")
        .count();
    assert_eq!(fid_columns, 1, "fid appears once, as the key");
}
