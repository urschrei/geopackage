//! The `gpkg_contents` extent: what the writer records, what it refuses to
//! record, and [`Layer::recompute_extent`].
//!
//! The governing rule is that a wrong extent is worse than an absent one,
//! because a well-ordered box is believed indefinitely by every reader while a
//! NULL one makes them measure. These tests pin the cases where the writer has
//! to decline to record what it folded.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geo_types::Point;
use geopackage::core::types::GeometryType;
use geopackage::{BoundingBox, ColumnSpec, Error, GeoPackage, GeometrySpec, TableSchemaBuilder};

fn layer_with_points(table: &str, points: &[(f64, f64)]) -> (tempfile::TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    gpkg.create_layer(
        &TableSchemaBuilder::new(table)
            .geometry(GeometrySpec::new(GeometryType::Point, 4326))
            .spatial_index(false),
    )
    .unwrap();
    if !points.is_empty() {
        let layer = gpkg.layer(table).unwrap();
        let mut w = layer.writer().unwrap();
        for (x, y) in points {
            w.insert(None, &Point::new(*x, *y), &[]).unwrap();
        }
        w.commit().unwrap();
    }
    (dir, gpkg)
}

/// The four recorded bounds, as stored, without going through `extent()`.
fn recorded(
    gpkg: &GeoPackage,
    table: &str,
) -> (Option<f64>, Option<f64>, Option<f64>, Option<f64>) {
    gpkg.connection()
        .query_row(
            "SELECT min_x, min_y, max_x, max_y FROM gpkg_contents WHERE table_name = ?1",
            [table],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
}

fn set_recorded(gpkg: &GeoPackage, table: &str, bounds: Option<[f64; 4]>) {
    match bounds {
        Some([min_x, min_y, max_x, max_y]) => gpkg.connection().execute(
            "UPDATE gpkg_contents SET min_x = ?1, min_y = ?2, max_x = ?3, max_y = ?4 \
             WHERE table_name = ?5",
            rusqlite::params![min_x, min_y, max_x, max_y, table],
        ),
        None => gpkg.connection().execute(
            "UPDATE gpkg_contents SET min_x = NULL, min_y = NULL, max_x = NULL, max_y = NULL \
             WHERE table_name = ?1",
            [table],
        ),
    }
    .unwrap();
}

/// The case this whole module exists for. A file arrives with a NULL extent,
/// which is spec-legal, over a table that already holds features. Writing one
/// more feature must not record a box covering only that feature: doing so
/// replaces an honest "unknown" with a confidently wrong extent that excludes
/// every pre-existing row, and no reader would ever question it.
#[test]
fn a_null_extent_over_existing_rows_is_left_alone() {
    let (_dir, gpkg) = layer_with_points("p", &[(0.0, 0.0), (10.0, 10.0)]);
    set_recorded(&gpkg, "p", None);

    let layer = gpkg.layer("p").unwrap();
    let mut w = layer.writer().unwrap();
    w.insert(None, &Point::new(5.0, 5.0), &[]).unwrap();
    w.commit().unwrap();

    assert_eq!(
        recorded(&gpkg, "p"),
        (None, None, None, None),
        "the writer recorded an extent it could not vouch for"
    );
    // The true extent is still available on demand, and still covers everything.
    assert_eq!(
        layer.extent().unwrap(),
        Some(BoundingBox::new(0.0, 0.0, 10.0, 10.0))
    );
}

/// The mirror case: no recorded extent over an empty table means the writer's
/// fold is the whole content, so it is exact and gets recorded.
#[test]
fn a_null_extent_over_an_empty_table_is_recorded() {
    let (_dir, gpkg) = layer_with_points("p", &[]);
    set_recorded(&gpkg, "p", None);

    let layer = gpkg.layer("p").unwrap();
    let mut w = layer.writer().unwrap();
    w.insert(None, &Point::new(1.0, 2.0), &[]).unwrap();
    w.insert(None, &Point::new(3.0, 4.0), &[]).unwrap();
    w.commit().unwrap();

    assert_eq!(
        recorded(&gpkg, "p"),
        (Some(1.0), Some(2.0), Some(3.0), Some(4.0))
    );
}

/// An inverted box carries no information about where the data is, so it is
/// read as absent rather than grown, which is also how GDAL reads it.
#[test]
fn an_inverted_extent_is_treated_as_absent() {
    let (_dir, gpkg) = layer_with_points("p", &[(0.0, 0.0), (10.0, 10.0)]);
    set_recorded(&gpkg, "p", Some([10.0, 10.0, 0.0, 0.0]));

    let layer = gpkg.layer("p").unwrap();
    // Not returned as an extent, and not grown from: measured instead.
    assert_eq!(
        layer.extent().unwrap(),
        Some(BoundingBox::new(0.0, 0.0, 10.0, 10.0))
    );

    // The writer declines to record its fold, and leaves what it found alone
    // rather than normalising it: an inverted box and a NULL one send every
    // reader down the same recompute path, so rewriting it would buy nothing.
    let mut w = layer.writer().unwrap();
    w.insert(None, &Point::new(5.0, 5.0), &[]).unwrap();
    w.commit().unwrap();
    assert_eq!(
        recorded(&gpkg, "p"),
        (Some(10.0), Some(10.0), Some(0.0), Some(0.0)),
        "an inverted box was grown rather than declined"
    );

    // And the sanctioned repair still fixes it.
    assert_eq!(
        layer.recompute_extent().unwrap(),
        Some(BoundingBox::new(0.0, 0.0, 10.0, 10.0))
    );
}

/// A usable recorded box is grown to cover what is written, never replaced.
/// An over-estimate is expressly spec-legal, so a box that was already too
/// large stays too large.
#[test]
fn a_usable_extent_is_grown_not_replaced() {
    let (_dir, gpkg) = layer_with_points("p", &[(0.0, 0.0)]);
    set_recorded(&gpkg, "p", Some([-100.0, -100.0, 100.0, 100.0]));

    let layer = gpkg.layer("p").unwrap();
    let mut w = layer.writer().unwrap();
    w.insert(None, &Point::new(500.0, 1.0), &[]).unwrap();
    w.commit().unwrap();

    assert_eq!(
        recorded(&gpkg, "p"),
        (Some(-100.0), Some(-100.0), Some(500.0), Some(100.0))
    );
    // And `extent` reports it as it stands rather than measuring.
    assert_eq!(
        layer.extent().unwrap(),
        Some(BoundingBox::new(-100.0, -100.0, 500.0, 100.0))
    );
}

#[test]
fn recompute_extent_replaces_a_wrong_box() {
    let (_dir, gpkg) = layer_with_points("p", &[(1.0, 2.0), (3.0, 4.0)]);
    set_recorded(&gpkg, "p", Some([-1000.0, -1000.0, 1000.0, 1000.0]));
    let layer = gpkg.layer("p").unwrap();

    assert_eq!(
        layer.recompute_extent().unwrap(),
        Some(BoundingBox::new(1.0, 2.0, 3.0, 4.0))
    );
    assert_eq!(
        recorded(&gpkg, "p"),
        (Some(1.0), Some(2.0), Some(3.0), Some(4.0))
    );
    // And it persisted, so a plain read now returns it without measuring.
    assert_eq!(
        layer.extent().unwrap(),
        Some(BoundingBox::new(1.0, 2.0, 3.0, 4.0))
    );
}

/// Nothing to measure means NULL, not an invented box: NULL is the value that
/// makes a reader compute the answer for itself.
#[test]
fn recompute_extent_nulls_a_layer_with_nothing_to_measure() {
    let (_dir, gpkg) = layer_with_points("p", &[]);
    set_recorded(&gpkg, "p", Some([1.0, 2.0, 3.0, 4.0]));
    let layer = gpkg.layer("p").unwrap();

    assert_eq!(layer.recompute_extent().unwrap(), None);
    assert_eq!(recorded(&gpkg, "p"), (None, None, None, None));
}

/// Reading an extent must not modify the file, which is the same rule
/// `repair_spatial_index` follows. GDAL persists here; this crate does not.
#[test]
fn extent_does_not_persist_what_it_measures() {
    let (_dir, gpkg) = layer_with_points("p", &[(1.0, 2.0), (3.0, 4.0)]);
    set_recorded(&gpkg, "p", None);
    let layer = gpkg.layer("p").unwrap();

    assert_eq!(
        layer.extent().unwrap(),
        Some(BoundingBox::new(1.0, 2.0, 3.0, 4.0))
    );
    assert_eq!(
        recorded(&gpkg, "p"),
        (None, None, None, None),
        "reading the extent wrote to the file"
    );
}

/// A NULL geometry contributes nothing to the measurement, exactly as it
/// contributes nothing to the RTree.
#[test]
fn null_geometries_do_not_affect_the_measurement() {
    let (_dir, gpkg) = layer_with_points("p", &[(1.0, 2.0)]);
    let layer = gpkg.layer("p").unwrap();
    let mut w = layer.writer().unwrap();
    w.insert_row(None, &[]).unwrap();
    w.commit().unwrap();

    assert_eq!(
        layer.recompute_extent().unwrap(),
        Some(BoundingBox::new(1.0, 2.0, 1.0, 2.0))
    );
}

/// A layer with no geometry column has no extent to speak of, and says so the
/// same way whether or not its `gpkg_contents` row happens to carry bounds.
#[test]
fn an_attributes_layer_has_no_extent() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    gpkg.create_attributes_table(&TableSchemaBuilder::new("a").column(ColumnSpec::new(
        "n",
        geopackage::core::types::ColumnType::Integer,
    )))
    .unwrap();
    set_recorded(&gpkg, "a", Some([1.0, 2.0, 3.0, 4.0]));

    let layer = gpkg.attributes("a").unwrap();
    match layer.extent() {
        Err(Error::NoGeometryColumn { table_name }) => assert_eq!(table_name, "a"),
        other => panic!("expected NoGeometryColumn, got {other:?}"),
    }
    match layer.recompute_extent() {
        Err(Error::NoGeometryColumn { table_name }) => assert_eq!(table_name, "a"),
        other => panic!("expected NoGeometryColumn, got {other:?}"),
    }
}
