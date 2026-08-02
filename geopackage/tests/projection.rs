//! Column projection: [`Layer::with_columns`] and [`Layer::without_geometry`].
//!
//! The point of it is what a read does *not* fetch, which is hard to assert
//! directly, so these pin the observable consequences: which columns a feature
//! has, that an unselected geometry is an error rather than an empty
//! answer, that a bounding-box query still filters exactly without fetching the
//! geometry, and that a projected handle still writes whole rows.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geo_types::Point;
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{
    BoundingBox, ColumnSpec, Error, GeoPackage, GeometrySpec, TableSchemaBuilder, ValueRef,
};

/// A point layer with three value columns, one of them NULL in every row.
fn layer(gpkg: &GeoPackage, spatial_index: bool) {
    gpkg.create_layer(
        &TableSchemaBuilder::new("p")
            .geometry(GeometrySpec::new(GeometryType::Point, 4326))
            .column(ColumnSpec::new("name", ColumnType::Text(None)))
            .column(ColumnSpec::new("n", ColumnType::Integer))
            .column(ColumnSpec::new("nothing", ColumnType::Text(None)))
            .spatial_index(spatial_index),
    )
    .unwrap();
    let layer = gpkg.layer("p").unwrap();
    let mut w = layer.writer().unwrap();
    for i in 1..=3 {
        w.insert(
            None,
            &Point::new(f64::from(i), f64::from(i)),
            &[
                ValueRef::Text("keep"),
                ValueRef::Integer(i64::from(i)),
                ValueRef::Null,
            ],
        )
        .unwrap();
    }
    w.commit().unwrap();
}

fn gpkg() -> (tempfile::TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    (dir, gpkg)
}

#[test]
fn a_projection_selects_and_does_not_reorder() {
    let (_dir, gpkg) = gpkg();
    layer(&gpkg, false);
    // Named out of order and with a repeat: the table's order decides, and the
    // repeat selects once.
    let projected = gpkg
        .layer("p")
        .unwrap()
        .with_columns(&["n", "name", "n"])
        .unwrap();

    let features: Vec<_> = projected
        .features()
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(features.len(), 3);
    let first = &features[0];
    assert_eq!(first.columns(), ["name".to_owned(), "n".to_owned()]);
    assert_eq!(first.value("name"), Some(ValueRef::Text("keep")));
    assert_eq!(first.value("n"), Some(ValueRef::Integer(1)));
    assert_eq!(first.get(0), Some(ValueRef::Text("keep")));
    assert_eq!(first.fid(), 1, "the feature id is always present");
}

/// The distinction the projection exists to keep clear: a column that is
/// present but NULL, one the projection dropped, and one the table never had.
#[test]
fn a_dropped_column_is_distinguishable_from_a_null_one() {
    let (_dir, gpkg) = gpkg();
    layer(&gpkg, false);
    let projected = gpkg.layer("p").unwrap().with_columns(&["n"]).unwrap();
    let feature = projected.features().unwrap().next().unwrap().unwrap();

    // Selected and NULL is not the same as not selected.
    let unprojected = gpkg.layer("p").unwrap();
    let whole = unprojected.features().unwrap().next().unwrap().unwrap();
    assert_eq!(whole.value("nothing"), Some(ValueRef::Null));
    assert!(whole.has_column("nothing"));

    assert_eq!(feature.value("nothing"), None);
    assert!(!feature.has_column("nothing"));
    assert!(!feature.has_column("no_such_column"));
    assert!(feature.has_column("n"));
}

#[test]
fn an_unselected_geometry_is_an_error_not_an_empty_answer() {
    let (_dir, gpkg) = gpkg();
    layer(&gpkg, false);

    let projected = gpkg.layer("p").unwrap().without_geometry();
    let feature = projected.features().unwrap().next().unwrap().unwrap();
    assert!(!feature.has_geometry_column());
    assert!(matches!(
        feature.geometry(),
        Err(Error::GeometryNotProjected)
    ));
    assert_eq!(feature.geometry_bytes(), None);
    // Every value column is still there: `without_geometry` drops only the one.
    assert_eq!(feature.columns().len(), 3);

    // Named explicitly, it comes back.
    let with_geom = gpkg
        .layer("p")
        .unwrap()
        .with_columns(&["n", "geom"])
        .unwrap();
    let feature = with_geom.features().unwrap().next().unwrap().unwrap();
    assert!(feature.has_geometry_column());
    assert_eq!(
        feature.geometry().unwrap().unwrap().to_geo().unwrap(),
        geo_types::Geometry::Point(Point::new(1.0, 1.0))
    );
    assert_eq!(feature.columns(), ["n".to_owned()]);
}

/// A layer with no geometry column has had nothing projected away, so it
/// answers as it always did.
#[test]
fn a_layer_without_a_geometry_column_still_answers_none() {
    let (_dir, gpkg) = gpkg();
    gpkg.create_attributes_table(
        &TableSchemaBuilder::new("a").column(ColumnSpec::new("n", ColumnType::Integer)),
    )
    .unwrap();
    let attrs = gpkg.attributes("a").unwrap();
    let mut w = attrs.writer().unwrap();
    w.insert_row(None, &[ValueRef::Integer(1)]).unwrap();
    w.commit().unwrap();

    let feature = attrs.features().unwrap().next().unwrap().unwrap();
    assert!(feature.has_geometry_column());
    assert!(feature.geometry().unwrap().is_none());
}

/// A bounding-box query reads each candidate's geometry to filter exactly, so
/// it must still answer correctly on a handle that does not carry geometry.
#[test]
fn a_bbox_query_still_filters_without_carrying_the_geometry() {
    for spatial_index in [false, true] {
        let (_dir, gpkg) = gpkg();
        layer(&gpkg, spatial_index);
        let projected = gpkg.layer("p").unwrap().without_geometry();

        let hits: Vec<_> = projected
            .features_in(BoundingBox::new(0.5, 0.5, 2.5, 2.5))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            hits.len(),
            2,
            "indexed={spatial_index}: the exact filter should still run"
        );
        assert_eq!(hits[0].fid(), 1);
        assert_eq!(hits[1].fid(), 2);
        assert!(matches!(
            hits[0].geometry(),
            Err(Error::GeometryNotProjected)
        ));
    }
}

#[test]
fn an_unknown_column_is_rejected_where_it_is_written() {
    let (_dir, gpkg) = gpkg();
    layer(&gpkg, false);
    match gpkg.layer("p").unwrap().with_columns(&["n", "nope"]) {
        Err(Error::NoSuchColumn {
            column_name,
            table_name,
        }) => {
            assert_eq!(column_name, "nope");
            assert_eq!(table_name, "p");
        }
        other => panic!("expected NoSuchColumn, got {:?}", other.map(|_| ())),
    }
    // The primary key is not a value column and is always present anyway.
    gpkg.layer("p").unwrap().with_columns(&["fid"]).unwrap_err();
}

/// A projection is a read concern: the writer keeps the layer's whole column
/// list, so a projected handle cannot insert a partial row by accident.
#[test]
fn a_projected_handle_still_writes_whole_rows() {
    let (_dir, gpkg) = gpkg();
    layer(&gpkg, false);
    let projected = gpkg.layer("p").unwrap().with_columns(&["n"]).unwrap();

    let mut w = projected.writer().unwrap();
    let fid = w
        .insert(
            None,
            &Point::new(9.0, 9.0),
            &[
                ValueRef::Text("written whole"),
                ValueRef::Integer(9),
                ValueRef::Text("and this"),
            ],
        )
        .unwrap();
    w.commit().unwrap();

    // Read back through an unprojected handle: every column arrived.
    let whole = gpkg.layer("p").unwrap();
    let feature = whole
        .features()
        .unwrap()
        .find_map(|f| {
            let f = f.unwrap();
            (f.fid() == fid).then_some(f)
        })
        .unwrap();
    assert_eq!(feature.value("name"), Some(ValueRef::Text("written whole")));
    assert_eq!(feature.value("nothing"), Some(ValueRef::Text("and this")));
}

/// The pairing the projection was built for: read only what you need, write
/// back only what changed.
#[test]
fn a_projection_pairs_with_a_partial_update() {
    let (_dir, gpkg) = gpkg();
    layer(&gpkg, false);
    let projected = gpkg.layer("p").unwrap().with_columns(&["n"]).unwrap();

    let mut cursor = projected.cursor().unwrap();
    let mut writer = projected.writer().unwrap();
    for feature in cursor.features().unwrap() {
        let feature = feature.unwrap();
        let n = feature.value("n").and_then(|v| v.as_i64()).unwrap_or(0);
        writer
            .update_column(feature.fid(), "n", ValueRef::Integer(n * 10))
            .unwrap();
    }
    writer.commit().unwrap();

    let whole = gpkg.layer("p").unwrap();
    let values: Vec<_> = whole
        .features()
        .unwrap()
        .map(|f| f.unwrap().value("n").and_then(|v| v.as_i64()).unwrap())
        .collect();
    assert_eq!(values, [10, 20, 30]);
    // And nothing else moved.
    let first = whole.features().unwrap().next().unwrap().unwrap();
    assert_eq!(first.value("name"), Some(ValueRef::Text("keep")));
    assert_eq!(
        first.geometry().unwrap().unwrap().to_geo().unwrap(),
        geo_types::Geometry::Point(Point::new(1.0, 1.0))
    );
}
