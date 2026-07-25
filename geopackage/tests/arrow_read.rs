//! The columnar read path: `Layer::read_arrow`.

#![cfg(feature = "arrow")]
#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use arrow_array::cast::AsArray;
use arrow_array::types::{Date32Type, Float64Type, Int64Type, TimestampMicrosecondType};
use arrow_array::{Array, RecordBatch, RecordBatchReader};
use geo_types::Point;
use geopackage::arrow::ArrowReadOptions;
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{ColumnSpec, GeoPackage, GeometrySpec, NewFeature, TableSchemaBuilder, Value};
use rusqlite::limits::Limit;

/// A points layer with one attribute of each interesting type.
fn layer_with_rows(rows: usize) -> (tempfile::TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    let builder = TableSchemaBuilder::new("pts")
        .column(ColumnSpec::new("name", ColumnType::Text(None)))
        .column(ColumnSpec::new("count", ColumnType::Integer))
        .column(ColumnSpec::new("ratio", ColumnType::Double))
        .column(ColumnSpec::new("flag", ColumnType::Boolean))
        .column(ColumnSpec::new("seen", ColumnType::DateTime))
        .geometry(GeometrySpec::new(GeometryType::Point, 4326));
    let layer = gpkg.create_layer(&builder).unwrap();

    let features: Vec<NewFeature<Point<f64>>> = (1..=rows)
        .map(|i| {
            let f = i as f64;
            NewFeature::new(
                Point::new(f, -f),
                vec![
                    Value::Text(format!("row {i}")),
                    Value::Integer(i as i64),
                    Value::Float(f / 2.0),
                    Value::Boolean(i % 2 == 0),
                    Value::DateTime(
                        geopackage::core::datetime::DateTime::parse_strict(
                            "2026-07-24T12:34:56.789Z",
                        )
                        .unwrap(),
                    ),
                ],
            )
            .with_fid(i as i64)
        })
        .collect();
    layer.write_all(features, 0).unwrap();
    (dir, gpkg)
}

/// Collect every batch, failing on the first error.
fn read_all(gpkg: &GeoPackage, options: ArrowReadOptions) -> Vec<RecordBatch> {
    let layer = gpkg.layer("pts").unwrap();
    layer
        .read_arrow(options)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

#[test]
fn values_round_trip_through_arrow() {
    let (_dir, gpkg) = layer_with_rows(5);
    let batches = read_all(&gpkg, ArrowReadOptions::default());
    assert_eq!(batches.len(), 1);
    let batch = &batches[0];
    assert_eq!(batch.num_rows(), 5);

    let fid = batch
        .column_by_name("fid")
        .unwrap()
        .as_primitive::<Int64Type>();
    assert_eq!(fid.values(), &[1, 2, 3, 4, 5]);

    let name = batch.column_by_name("name").unwrap().as_string::<i32>();
    assert_eq!(name.value(0), "row 1");
    assert_eq!(name.value(4), "row 5");

    let ratio = batch
        .column_by_name("ratio")
        .unwrap()
        .as_primitive::<Float64Type>();
    assert_eq!(ratio.values(), &[0.5, 1.0, 1.5, 2.0, 2.5]);

    let flag = batch.column_by_name("flag").unwrap().as_boolean();
    assert!(!flag.value(0), "row 1 is odd");
    assert!(flag.value(1), "row 2 is even");

    // The DATETIME text is converted to an instant, not handed over as a
    // string: 2026-07-24T12:34:56.789Z in microseconds.
    let seen = batch
        .column_by_name("seen")
        .unwrap()
        .as_primitive::<TimestampMicrosecondType>();
    assert_eq!(seen.value(0), 1_784_896_496_789_000);
}

#[test]
fn the_geometry_column_is_wkb_not_gpb() {
    let (_dir, gpkg) = layer_with_rows(3);
    let batches = read_all(&gpkg, ArrowReadOptions::default());
    let geom = batches[0]
        .column_by_name("geom")
        .unwrap()
        .as_binary::<i32>();

    // Byte-for-byte the WKB body the scalar path exposes, so the header has
    // been stripped and nothing else has been touched.
    let layer = gpkg.layer("pts").unwrap();
    let features: Vec<_> = layer.features().unwrap().map(|f| f.unwrap()).collect();
    for (index, feature) in features.iter().enumerate() {
        let parsed = feature.geometry().unwrap().unwrap();
        assert_eq!(
            geom.value(index),
            parsed.wkb_body(),
            "row {index} geometry differs from the scalar path"
        );
    }

    // And it really is WKB: the first byte is a WKB byte-order marker, not the
    // 'G','P' of a GPB header.
    assert_ne!(&geom.value(0)[..2], b"GP");
    assert!(matches!(geom.value(0)[0], 0 | 1), "WKB byte-order marker");
}

#[test]
fn batches_split_at_the_requested_size_and_lose_nothing() {
    let (_dir, gpkg) = layer_with_rows(250);
    let batches = read_all(&gpkg, ArrowReadOptions::with_batch_size(100));

    assert_eq!(batches.len(), 3, "250 rows at 100 per batch");
    assert_eq!(batches[0].num_rows(), 100);
    assert_eq!(batches[1].num_rows(), 100);
    assert_eq!(batches[2].num_rows(), 50);

    // Every fid, once, in key order across the batch boundaries.
    let fids: Vec<i64> = batches
        .iter()
        .flat_map(|batch| {
            batch
                .column_by_name("fid")
                .unwrap()
                .as_primitive::<Int64Type>()
                .values()
                .to_vec()
        })
        .collect();
    assert_eq!(fids, (1..=250).collect::<Vec<i64>>());
}

#[test]
fn a_batch_size_that_divides_exactly_does_not_yield_an_empty_batch() {
    let (_dir, gpkg) = layer_with_rows(200);
    let batches = read_all(&gpkg, ArrowReadOptions::with_batch_size(100));
    assert_eq!(batches.len(), 2, "no trailing empty batch");
    assert_eq!(batches[1].num_rows(), 100);
}

#[test]
fn gaps_in_the_key_are_read_through() {
    // Pagination is keyset-based, so deleted rows must not stop the walk. A
    // range-based scheme that assumed dense keys would truncate here.
    let (_dir, gpkg) = layer_with_rows(20);
    gpkg.connection()
        .execute_batch("DELETE FROM pts WHERE fid BETWEEN 5 AND 15")
        .unwrap();

    let batches = read_all(&gpkg, ArrowReadOptions::with_batch_size(3));
    let fids: Vec<i64> = batches
        .iter()
        .flat_map(|batch| {
            batch
                .column_by_name("fid")
                .unwrap()
                .as_primitive::<Int64Type>()
                .values()
                .to_vec()
        })
        .collect();
    let expected: Vec<i64> = (1..=4).chain(16..=20).collect();
    assert_eq!(fids, expected);
}

#[test]
fn nulls_survive() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("n.gpkg")).unwrap();
    let builder = TableSchemaBuilder::new("pts")
        .column(ColumnSpec::new("name", ColumnType::Text(None)))
        .column(ColumnSpec::new("born", ColumnType::Date))
        .geometry(GeometrySpec::new(GeometryType::Point, 4326));
    let layer = gpkg.create_layer(&builder).unwrap();
    {
        let mut writer = layer.writer().unwrap();
        // A row with everything present, then one with nothing.
        writer
            .insert(
                Some(1),
                &Point::new(1.0, 2.0),
                &[
                    Value::Text("here".into()),
                    Value::Date(geopackage::core::datetime::Date::new(2026, 7, 25).unwrap()),
                ],
            )
            .unwrap();
        writer
            .insert_row(Some(2), &[Value::Null, Value::Null])
            .unwrap();
        writer.commit().unwrap();
    }

    let batches = read_all(&gpkg, ArrowReadOptions::default());
    let batch = &batches[0];
    assert_eq!(batch.num_rows(), 2);

    let name = batch.column_by_name("name").unwrap().as_string::<i32>();
    assert!(!name.is_null(0));
    assert!(name.is_null(1));

    let born = batch
        .column_by_name("born")
        .unwrap()
        .as_primitive::<Date32Type>();
    assert_eq!(born.value(0), 20659, "2026-07-25 in days since the epoch");
    assert!(born.is_null(1));

    // A NULL geometry is a null in the WKB column, not an empty blob.
    let geom = batch.column_by_name("geom").unwrap().as_binary::<i32>();
    assert!(!geom.is_null(0));
    assert!(geom.is_null(1));
}

#[test]
fn an_empty_layer_yields_no_batches() {
    let (_dir, gpkg) = layer_with_rows(0);
    let batches = read_all(&gpkg, ArrowReadOptions::default());
    assert!(batches.is_empty());
}

#[test]
fn the_reader_reports_its_schema() {
    let (_dir, gpkg) = layer_with_rows(3);
    let layer = gpkg.layer("pts").unwrap();
    let reader = layer.read_arrow(ArrowReadOptions::default()).unwrap();

    // RecordBatchReader::schema must agree with the batches it goes on to
    // produce, or a consumer building on it reads garbage.
    let schema = reader.schema();
    let batches: Vec<_> = reader.collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(batches[0].schema(), schema);
    assert_eq!(schema, layer.arrow_schema().unwrap());
}

/// A layer the aggregate cannot serve falls back to the direct row loop, and
/// reads exactly the same.
///
/// The aggregate passes one function argument per column, so a table wider than
/// SQLite's function-argument limit cannot use it. Rather than build a
/// thousand-column table, the limit is lowered on the connection, which is the
/// same condition from the code's point of view and is also how an embedder
/// could reach this path on an ordinary table.
///
/// Every other test in this file goes through the aggregate, so without this one
/// the fallback would be unexercised and free to rot. Verified to fail when the
/// fallback is broken.
#[test]
fn a_layer_the_aggregate_cannot_serve_falls_back() {
    let (_dir, gpkg) = layer_with_rows(5);

    // Below the eleven columns this layer has, so `read_arrow` must fall back.
    gpkg.connection()
        .set_limit(Limit::SQLITE_LIMIT_FUNCTION_ARG, 4)
        .unwrap();

    let batches = read_all(&gpkg, ArrowReadOptions::with_batch_size(2));
    assert_eq!(batches.len(), 3, "5 rows at 2 per batch, via the fallback");

    let fids: Vec<i64> = batches
        .iter()
        .flat_map(|batch| {
            batch
                .column_by_name("fid")
                .unwrap()
                .as_primitive::<Int64Type>()
                .values()
                .to_vec()
        })
        .collect();
    assert_eq!(fids, vec![1, 2, 3, 4, 5]);

    // Values and geometry, not just shape: the fallback converts as the
    // aggregate does.
    let name = batches[0]
        .column_by_name("name")
        .unwrap()
        .as_string::<i32>();
    assert_eq!(name.value(0), "row 1");
    let geom = batches[0]
        .column_by_name("geom")
        .unwrap()
        .as_binary::<i32>();
    assert!(!geom.is_null(0));
    assert!(matches!(geom.value(0)[0], 0 | 1), "WKB byte-order marker");
}

/// Read every fid, in order, through whichever path `read` chooses.
fn fids_via(gpkg: &GeoPackage, parallel: bool, options: ArrowReadOptions) -> Vec<i64> {
    let layer = gpkg.layer("pts").unwrap();
    let reader = if parallel {
        layer.read_arrow_parallel(options).unwrap()
    } else {
        layer.read_arrow(options).unwrap()
    };
    reader
        .map(|batch| batch.unwrap())
        .flat_map(|batch| {
            batch
                .column_by_name("fid")
                .unwrap()
                .as_primitive::<Int64Type>()
                .values()
                .to_vec()
        })
        .collect()
}

/// The threaded read returns exactly what the single-threaded one does, in the
/// same order.
///
/// Order is the property worth pinning: workers are assigned batches in
/// rotation and the consumer takes from them in the same rotation, so key order
/// falls out without a reordering buffer. If that assignment and that
/// consumption ever disagree, this is what notices.
#[test]
fn a_threaded_read_matches_a_single_threaded_one() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("p.gpkg")).unwrap();
    // A file, not `:memory:`, since workers open their own connections.
    let builder = TableSchemaBuilder::new("pts")
        .column(ColumnSpec::new("name", ColumnType::Text(None)))
        .geometry(GeometrySpec::new(GeometryType::Point, 4326));
    let layer = gpkg.create_layer(&builder).unwrap();
    let features: Vec<NewFeature<Point<f64>>> = (1..=1000)
        .map(|i| {
            NewFeature::new(
                Point::new(i as f64, -(i as f64)),
                vec![Value::Text(format!("row {i}"))],
            )
            .with_fid(i)
        })
        .collect();
    layer.write_all(features, 0).unwrap();

    // A batch size that leaves a partial final batch, and does not divide
    // evenly by the thread count, so the rotation has to be right.
    let options = ArrowReadOptions::with_batch_size(70).with_threads(3);
    let threaded = fids_via(&gpkg, true, options);
    let sequential = fids_via(&gpkg, false, options);

    assert_eq!(threaded, (1..=1000).collect::<Vec<i64>>(), "key order");
    assert_eq!(threaded, sequential, "the two paths disagree");
}

/// A layer whose keys have gaps reads correctly, because the threaded path
/// declines to engage rather than assuming a row count from a key range.
#[test]
fn gaps_in_the_key_fall_back_to_a_single_thread() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("g.gpkg")).unwrap();
    let builder =
        TableSchemaBuilder::new("pts").geometry(GeometrySpec::new(GeometryType::Point, 4326));
    let layer = gpkg.create_layer(&builder).unwrap();
    let features: Vec<NewFeature<Point<f64>>> = (1..=300)
        .map(|i| NewFeature::new(Point::new(i as f64, -(i as f64)), Vec::new()).with_fid(i))
        .collect();
    layer.write_all(features, 0).unwrap();
    gpkg.connection()
        .execute_batch("DELETE FROM pts WHERE fid BETWEEN 100 AND 200")
        .unwrap();

    let options = ArrowReadOptions::with_batch_size(32).with_threads(4);
    let expected: Vec<i64> = (1..=99).chain(201..=300).collect();
    assert_eq!(fids_via(&gpkg, true, options), expected);
}

/// An in-memory database cannot be opened by a second connection, so the
/// threaded entry point reads it on the calling thread instead of failing.
#[test]
fn an_in_memory_database_falls_back_to_a_single_thread() {
    let gpkg = GeoPackage::create(std::path::Path::new(":memory:")).unwrap();
    let builder =
        TableSchemaBuilder::new("pts").geometry(GeometrySpec::new(GeometryType::Point, 4326));
    let layer = gpkg.create_layer(&builder).unwrap();
    let features: Vec<NewFeature<Point<f64>>> = (1..=50)
        .map(|i| NewFeature::new(Point::new(i as f64, -(i as f64)), Vec::new()).with_fid(i))
        .collect();
    layer.write_all(features, 0).unwrap();

    let options = ArrowReadOptions::with_batch_size(8).with_threads(4);
    assert_eq!(
        fids_via(&gpkg, true, options),
        (1..=50).collect::<Vec<i64>>()
    );
}

/// Dropping the reader before it is drained stops the workers rather than
/// leaving them blocked on a channel nobody will read.
#[test]
fn abandoning_a_threaded_read_stops_its_workers() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("a.gpkg")).unwrap();
    let builder =
        TableSchemaBuilder::new("pts").geometry(GeometrySpec::new(GeometryType::Point, 4326));
    let layer = gpkg.create_layer(&builder).unwrap();
    let features: Vec<NewFeature<Point<f64>>> = (1..=5000)
        .map(|i| NewFeature::new(Point::new(i as f64, -(i as f64)), Vec::new()).with_fid(i))
        .collect();
    layer.write_all(features, 0).unwrap();

    {
        let layer = gpkg.layer("pts").unwrap();
        let mut reader = layer
            .read_arrow_parallel(ArrowReadOptions::with_batch_size(16).with_threads(4))
            .unwrap();
        // One batch of many, then walk away. `Drop` joins the workers, so this
        // test hanging is the failure mode it guards against.
        assert!(reader.next().is_some());
    }

    // The layer is still readable afterwards, so nothing was left holding it.
    assert_eq!(
        fids_via(&gpkg, false, ArrowReadOptions::default()).len(),
        5000
    );
}
