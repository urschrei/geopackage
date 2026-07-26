//! What each operation does on a read-only connection.
//!
//! Three policies coexist in the crate and the difference between them is not
//! obvious from the call names, so they are pinned here and tabulated in the
//! crate-root documentation: operations that never write, the one that writes
//! opportunistically and treats being unable to as a non-event, and those whose
//! whole purpose is to write and which fail when they cannot.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geo_types::Point;
use geopackage::core::types::GeometryType;
use geopackage::{
    Error, GeoPackage, GeometrySpec, SpatialIndexStatus, TableSchemaBuilder, ValueRef,
};

/// A file with an indexed `pts` layer and an unindexed `plain` one, closed and
/// then reopened read-only.
fn read_only_file() -> (tempfile::TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.gpkg");
    {
        let gpkg = GeoPackage::create(&path).unwrap();
        for (name, index) in [("pts", true), ("plain", false)] {
            gpkg.create_layer(
                &TableSchemaBuilder::new(name)
                    .geometry(GeometrySpec::new(GeometryType::Point, 4326))
                    .spatial_index(index),
            )
            .unwrap();
            let layer = gpkg.layer(name).unwrap();
            let mut w = layer.writer().unwrap();
            w.insert(None, &Point::new(1.0, 2.0), &[]).unwrap();
            w.insert(None, &Point::new(3.0, 4.0), &[]).unwrap();
            w.commit().unwrap();
        }
        // Leave an extent that has to be measured, so `extent` has work to do.
        gpkg.connection()
            .execute(
                "UPDATE gpkg_contents SET min_x = NULL, min_y = NULL, max_x = NULL, max_y = NULL",
                [],
            )
            .unwrap();
        gpkg.close().unwrap();
    }
    let gpkg = GeoPackage::open_read_only(&path).unwrap();
    (dir, gpkg)
}

/// Reads are reads, including the ones that read every geometry in the layer.
#[test]
fn reads_work_read_only() {
    let (_dir, gpkg) = read_only_file();
    let layer = gpkg.layer("pts").unwrap();

    assert_eq!(layer.features().unwrap().count(), 2);
    assert_eq!(
        layer.spatial_index_status().unwrap(),
        SpatialIndexStatus::Current
    );
    assert!(layer.has_spatial_index().unwrap());
    assert!(layer.audit_spatial_index().unwrap().is_consistent());
    assert_eq!(
        layer
            .features_in(geopackage::BoundingBox::new(0.0, 0.0, 2.0, 3.0))
            .unwrap()
            .count(),
        1
    );
}

/// `extent` records what it measures where it can, and being unable to is not a
/// failure: the measurement is still the answer.
#[test]
fn extent_measures_without_recording_read_only() {
    let (_dir, gpkg) = read_only_file();
    let layer = gpkg.layer("pts").unwrap();

    assert_eq!(
        layer.extent().unwrap(),
        Some(geopackage::BoundingBox::new(1.0, 2.0, 3.0, 4.0))
    );
    // Twice, to show the second call measures again rather than depending on a
    // recording that never happened.
    assert_eq!(
        layer.extent().unwrap(),
        Some(geopackage::BoundingBox::new(1.0, 2.0, 3.0, 4.0))
    );
}

/// Everything whose purpose is to write fails, and says so.
#[test]
fn writes_fail_read_only() {
    let (_dir, gpkg) = read_only_file();
    let pts = gpkg.layer("pts").unwrap();
    let plain = gpkg.layer("plain").unwrap();

    let is_readonly = |what: &str, result: geopackage::Result<()>| match result {
        Err(Error::Sqlite(e)) => assert_eq!(
            e.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ReadOnly),
            "{what}: expected a read-only failure, got {e:?}"
        ),
        other => panic!("{what}: expected a read-only failure, got {other:?}"),
    };

    is_readonly("recompute_extent", pts.recompute_extent().map(|_| ()));
    is_readonly("rebuild_spatial_index", pts.rebuild_spatial_index());
    is_readonly("create_spatial_index", plain.create_spatial_index());
    is_readonly("drop_spatial_index", pts.drop_spatial_index());

    // A writer opens: `BEGIN DEFERRED` takes no lock, so nothing has been
    // attempted yet. The failure lands on the first row it tries to write.
    let mut writer = plain.writer().unwrap();
    is_readonly(
        "insert",
        writer.insert(None, &Point::new(9.0, 9.0), &[]).map(|_| ()),
    );

    // A repair with nothing to repair is a no-op, and a no-op does not need to
    // write: the structural check finds the 1.4 trigger set already in place
    // and returns before touching anything.
    pts.repair_spatial_index().unwrap();
}

/// A layer with no index answers the two questions about one without failing,
/// and refuses the operations that need one to exist.
#[test]
fn an_unindexed_layer_answers_without_an_index() {
    let (_dir, gpkg) = read_only_file();
    let plain = gpkg.layer("plain").unwrap();

    assert_eq!(
        plain.spatial_index_status().unwrap(),
        SpatialIndexStatus::Absent
    );
    assert!(!plain.has_spatial_index().unwrap());
    assert!(matches!(
        plain.audit_spatial_index(),
        Err(Error::NoSpatialIndex { .. })
    ));
    assert!(matches!(
        plain.rebuild_spatial_index(),
        Err(Error::NoSpatialIndex { .. })
    ));
    // And a bbox query still answers, by scanning.
    let _ = ValueRef::Null;
    assert_eq!(
        plain
            .features_in(geopackage::BoundingBox::new(0.0, 0.0, 2.0, 3.0))
            .unwrap()
            .count(),
        1
    );
}
