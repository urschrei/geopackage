//! `Layer::read_arrow_where` and `read_arrow_in_where`: the columnar
//! counterparts of `select`, alone and composed with a bounding box.
//!
//! The governing property is agreement with the row paths: `read_arrow_where`
//! returns `select`'s rows, and the composed read returns the intersection of
//! `select`'s and `features_in`'s, in primary-key order. The clause keeps
//! `select`'s `?1` to `?N` placeholder contract even though the paginated
//! query numbers its own key, limit and rtree bounds around it, which is
//! exactly what these tests would catch getting wrong.

#![cfg(feature = "arrow")]
#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::collections::BTreeSet;

use arrow_array::cast::AsArray;
use geo_types::Point;
use geopackage::arrow::ArrowReadOptions;
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{
    BoundingBox, ColumnSpec, GeoPackage, GeometrySpec, NewFeature, TableSchemaBuilder, Value,
    ValueRef,
};
use tempfile::TempDir;

/// A layer of `count` points on a diagonal, indexed unless `index` is false.
fn points(count: i32, index: bool) -> (TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("p.gpkg")).unwrap();
    gpkg.add_epsg_srs(4326).unwrap();
    gpkg.create_layer(
        &TableSchemaBuilder::new("pts")
            .column(ColumnSpec::new("name", ColumnType::Text(None)))
            .column(ColumnSpec::new("rank", ColumnType::Integer))
            .geometry(GeometrySpec::new(GeometryType::Point, 4326))
            .spatial_index(index),
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

/// The `fid`s a batch iterator returns, in order.
fn batch_fids(batches: geopackage::arrow::ArrowBatches<'_>) -> Vec<i64> {
    let mut fids = Vec::new();
    for batch in batches {
        let batch = batch.unwrap();
        let column = batch.column_by_name("fid").unwrap();
        for value in column.as_primitive::<arrow_array::types::Int64Type>() {
            fids.push(value.unwrap());
        }
    }
    fids
}

/// The `fid`s `select` returns for the same clause, in order.
fn select_fids(gpkg: &GeoPackage, clause: &str, params: &[ValueRef<'_>]) -> Vec<i64> {
    gpkg.layer("pts")
        .unwrap()
        .select(clause, params)
        .unwrap()
        .map(|feature| feature.unwrap().fid())
        .collect()
}

#[test]
fn agrees_with_select() {
    let (_dir, gpkg) = points(300, true);
    let layer = gpkg.layer("pts").unwrap();
    for (clause, params) in [
        ("rank = ?1", vec![ValueRef::Integer(3)]),
        (
            "name = ?1 OR name = ?2",
            vec![ValueRef::Text("p7"), ValueRef::Text("p250")],
        ),
        (
            "rank > ?1 AND rank < ?2",
            vec![ValueRef::Integer(2), ValueRef::Integer(5)],
        ),
    ] {
        assert_eq!(
            batch_fids(
                layer
                    .read_arrow_where(clause, &params, ArrowReadOptions::default())
                    .unwrap()
            ),
            select_fids(&gpkg, clause, &params),
            "{clause}"
        );
    }
}

#[test]
fn a_single_row_reads_by_fid() {
    let (_dir, gpkg) = points(50, true);
    let layer = gpkg.layer("pts").unwrap();
    assert_eq!(
        batch_fids(
            layer
                .read_arrow_where(
                    "fid = ?1",
                    &[ValueRef::Integer(17)],
                    ArrowReadOptions::default()
                )
                .unwrap()
        ),
        vec![17]
    );
}

#[test]
fn the_composed_read_intersects_the_two_row_paths() {
    for index in [true, false] {
        let (_dir, gpkg) = points(400, index);
        let layer = gpkg.layer("pts").unwrap();
        let bbox = BoundingBox::new(50.0, 50.0, 250.0, 250.0);
        let clause = "rank = ?1";
        let params = [ValueRef::Integer(4)];

        let in_box: BTreeSet<i64> = layer
            .features_in(bbox)
            .unwrap()
            .map(|feature| feature.unwrap().fid())
            .collect();
        let expected: Vec<i64> = select_fids(&gpkg, clause, &params)
            .into_iter()
            .filter(|fid| in_box.contains(fid))
            .collect();
        assert!(!expected.is_empty(), "the case must select something");

        assert_eq!(
            batch_fids(
                layer
                    .read_arrow_in_where(bbox, clause, &params, ArrowReadOptions::default())
                    .unwrap()
            ),
            expected,
            "indexed: {index}"
        );
    }
}

#[test]
fn pagination_survives_pages_with_no_matches() {
    // With a one-row batch size and a clause matching only late rows, the
    // composed read walks many candidate pages whose rows are all dropped by
    // the client-side re-test before the first match arrives. This is the
    // page-advance hazard the bbox read was mutation-checked for, now under a
    // clause as well.
    let (_dir, gpkg) = points(200, true);
    let layer = gpkg.layer("pts").unwrap();
    let bbox = BoundingBox::new(150.0, 150.0, 199.0, 199.0);
    let options = ArrowReadOptions::with_batch_size(1);
    let fids = batch_fids(
        layer
            .read_arrow_in_where(bbox, "rank = ?1", &[ValueRef::Integer(9)], options)
            .unwrap(),
    );
    assert_eq!(fids, vec![160, 170, 180, 190, 200]);
}

#[test]
fn an_unmatched_clause_returns_no_batches() {
    let (_dir, gpkg) = points(30, true);
    let layer = gpkg.layer("pts").unwrap();
    let batches: Vec<_> = layer
        .read_arrow_where(
            "name = ?1",
            &[ValueRef::Text("absent")],
            ArrowReadOptions::default(),
        )
        .unwrap()
        .collect();
    assert!(batches.is_empty());
}

#[test]
fn a_clause_sqlite_cannot_prepare_surfaces_through_the_iterator() {
    let (_dir, gpkg) = points(10, true);
    let layer = gpkg.layer("pts").unwrap();
    let mut batches = layer
        .read_arrow_where("no_such_column = 1", &[], ArrowReadOptions::default())
        .unwrap();
    batches.next().unwrap().unwrap_err();
}

#[test]
fn the_composed_read_refuses_a_layer_without_geometry() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("a.gpkg")).unwrap();
    gpkg.create_attributes_table(
        &TableSchemaBuilder::new("t").column(ColumnSpec::new("name", ColumnType::Text(None))),
    )
    .unwrap();
    let layer = gpkg.attributes("t").unwrap();
    assert!(
        layer
            .read_arrow_in_where(
                BoundingBox::new(0.0, 0.0, 1.0, 1.0),
                "1 = 1",
                &[],
                ArrowReadOptions::default(),
            )
            .is_err()
    );
    // The plain filtered read is fine: a clause needs no geometry.
    assert_eq!(
        batch_fids(
            layer
                .read_arrow_where("1 = 1", &[], ArrowReadOptions::default())
                .unwrap()
        ),
        Vec::<i64>::new()
    );
}
