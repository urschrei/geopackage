//! The Arrow reads on a projected layer handle.
//!
//! `with_columns` and `without_geometry` narrow the Arrow schema exactly as
//! they narrow a row read: the primary key always, value columns as named,
//! the geometry only when projected in. The trap these tests exist for is the
//! bbox re-test on a layer whose projection excludes the geometry: the exact
//! filter still needs each candidate's geometry, so the read selects it as a
//! hidden trailing column that reaches no batch. Skipping the test returns
//! rows the row path does not; testing against a missing column returns
//! nothing at all. Both are wrong, and both are pinned here.

#![cfg(feature = "arrow")]
#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use arrow_array::RecordBatchReader;
use arrow_array::cast::AsArray;
use geo_types::Point;
use geopackage::arrow::ArrowReadOptions;
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{
    BoundingBox, ColumnSpec, GeoPackage, GeometrySpec, NewFeature, TableSchemaBuilder, Value,
    ValueRef,
};
use tempfile::TempDir;

/// A layer of `count` points on a diagonal, with two value columns.
fn points(count: i32) -> (TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("p.gpkg")).unwrap();
    gpkg.add_epsg_srs(4326).unwrap();
    gpkg.create_layer(
        &TableSchemaBuilder::new("pts")
            .column(ColumnSpec::new("name", ColumnType::Text(None)))
            .column(ColumnSpec::new("rank", ColumnType::Integer))
            .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
    )
    .unwrap();
    gpkg.layer("pts")
        .unwrap()
        .write_all(
            (0..count)
                .map(|i| {
                    NewFeature::new(
                        Point::new(f64::from(i), f64::from(i)),
                        vec![
                            Value::Text(format!("p{i}")),
                            Value::Integer(i64::from(i % 10)),
                        ],
                    )
                })
                .collect::<Vec<_>>(),
            1000,
        )
        .unwrap();
    (dir, gpkg)
}

/// Field names and total rows of a batch iterator.
fn shape(batches: geopackage::arrow::ArrowBatches<'_>) -> (Vec<String>, usize) {
    let names: Vec<String> = batches
        .schema()
        .fields()
        .iter()
        .map(|field| field.name().clone())
        .collect();
    let rows = batches
        .map(|batch| batch.unwrap().num_rows())
        .sum::<usize>();
    (names, rows)
}

#[test]
fn the_schema_and_batches_narrow_to_the_projection() {
    let (_dir, gpkg) = points(50);
    let layer = gpkg.layer("pts").unwrap().with_columns(&["rank"]).unwrap();

    assert_eq!(
        layer
            .arrow_schema()
            .unwrap()
            .fields()
            .iter()
            .map(|field| field.name().clone())
            .collect::<Vec<_>>(),
        vec!["fid", "rank"],
        "the primary key always, the named column, nothing else"
    );

    let batches: Vec<_> = layer
        .read_arrow(ArrowReadOptions::default())
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let rows: usize = batches.iter().map(arrow_array::RecordBatch::num_rows).sum();
    assert_eq!(rows, 50);
    for batch in &batches {
        assert_eq!(batch.num_columns(), 2);
        // The narrowed read returns the same values the full read would.
        let ranks = batch
            .column_by_name("rank")
            .unwrap()
            .as_primitive::<arrow_array::types::Int64Type>();
        let fids = batch
            .column_by_name("fid")
            .unwrap()
            .as_primitive::<arrow_array::types::Int64Type>();
        for (fid, rank) in fids.iter().zip(ranks.iter()) {
            let fid = fid.unwrap();
            assert_eq!(rank.unwrap(), (fid - 1) % 10, "fid {fid}");
        }
    }
}

#[test]
fn geometry_is_a_field_only_when_named() {
    let (_dir, gpkg) = points(5);

    let with_geometry = gpkg
        .layer("pts")
        .unwrap()
        .with_columns(&["name", "geom"])
        .unwrap();
    let (names, rows) = shape(
        with_geometry
            .read_arrow(ArrowReadOptions::default())
            .unwrap(),
    );
    assert_eq!(names, vec!["fid", "name", "geom"]);
    assert_eq!(rows, 5);

    let without = gpkg.layer("pts").unwrap().without_geometry();
    let (names, rows) = shape(without.read_arrow(ArrowReadOptions::default()).unwrap());
    assert_eq!(names, vec!["fid", "name", "rank"]);
    assert_eq!(rows, 5);
}

#[test]
fn a_bbox_read_on_a_projected_layer_agrees_with_the_row_path() {
    let (_dir, gpkg) = points(300);
    let full = gpkg.layer("pts").unwrap();
    let projected = gpkg.layer("pts").unwrap().without_geometry();
    let bbox = BoundingBox::new(10.0, 10.0, 40.0, 40.0);

    let expected: Vec<i64> = full
        .features_in(bbox)
        .unwrap()
        .map(|feature| feature.unwrap().fid())
        .collect();
    assert!(!expected.is_empty());

    let mut fids = Vec::new();
    for batch in projected
        .read_arrow_in(bbox, ArrowReadOptions::default())
        .unwrap()
    {
        let batch = batch.unwrap();
        assert!(
            batch.column_by_name("geom").is_none(),
            "the hidden geometry must reach no batch"
        );
        for fid in batch
            .column_by_name("fid")
            .unwrap()
            .as_primitive::<arrow_array::types::Int64Type>()
        {
            fids.push(fid.unwrap());
        }
    }
    assert_eq!(fids, expected);
}

#[test]
fn the_hidden_geometry_still_feeds_the_exact_refilter() {
    // The f32-widening case from the unprojected read, on a handle whose
    // projection excludes the geometry. The index offers a candidate the
    // answer does not contain; only the exact re-test, fed by the hidden
    // column, can drop it.
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("f.gpkg")).unwrap();
    gpkg.add_epsg_srs(4326).unwrap();
    gpkg.create_layer(
        &TableSchemaBuilder::new("pts")
            .column(ColumnSpec::new("name", ColumnType::Text(None)))
            .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
    )
    .unwrap();
    gpkg.layer("pts")
        .unwrap()
        .write_all(
            (0..200)
                .map(|i| {
                    let x = f64::from(i).mul_add(0.1, 0.05);
                    NewFeature::new(Point::new(x, x), vec![Value::Text("p".into())])
                })
                .collect::<Vec<_>>(),
            1000,
        )
        .unwrap();

    let bbox = BoundingBox::new(
        0.030_000_000_000_000_002,
        0.030_000_000_000_000_002,
        0.049_999_999,
        0.049_999_999,
    );
    // The premise: the index really does over-return here.
    let candidates: i64 = gpkg
        .connection()
        .query_row(
            "SELECT count(*) FROM rtree_pts_geom \
             WHERE minx <= ?1 AND maxx >= ?2 AND miny <= ?3 AND maxy >= ?4",
            [bbox.max_x, bbox.min_x, bbox.max_y, bbox.min_y],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(candidates, 1, "the index should over-return here");

    let projected = gpkg.layer("pts").unwrap().without_geometry();
    let (_, rows) = shape(
        projected
            .read_arrow_in(bbox, ArrowReadOptions::default())
            .unwrap(),
    );
    assert_eq!(rows, 0, "the candidate must be dropped, not returned");

    // And a box that does contain points returns them, so the re-test is
    // reading a present column rather than dropping everything.
    let (_, rows) = shape(
        projected
            .read_arrow_in(
                BoundingBox::new(0.0, 0.0, 1.0, 1.0),
                ArrowReadOptions::default(),
            )
            .unwrap(),
    );
    assert_eq!(rows, 10);
}

#[test]
fn the_threaded_read_declines_on_a_projected_layer() {
    let (_dir, gpkg) = points(300);
    let projected = gpkg.layer("pts").unwrap().with_columns(&["name"]).unwrap();
    // Small batches would qualify the layer for the parallel path; a projected
    // layer must decline it, because the workers rebuild the layer by name and
    // would read every column. What is observable is the schema of what comes
    // back.
    let options = ArrowReadOptions::with_batch_size(32).with_threads(4);
    let (names, rows) = shape(projected.read_arrow(options).unwrap());
    assert_eq!(names, vec!["fid", "name"]);
    assert_eq!(rows, 300);
}

#[test]
fn a_clause_may_name_a_projected_out_column() {
    // The WHERE clause is SQL over the table, not over the projection, so it
    // may filter on a column the batches do not carry. Same as the row path's
    // select on a projected handle.
    let (_dir, gpkg) = points(100);
    let projected = gpkg.layer("pts").unwrap().with_columns(&["name"]).unwrap();
    let (names, rows) = shape(
        projected
            .read_arrow_where(
                "rank = ?1",
                &[ValueRef::Integer(3)],
                ArrowReadOptions::default(),
            )
            .unwrap(),
    );
    assert_eq!(names, vec!["fid", "name"]);
    assert_eq!(rows, 10);
}
