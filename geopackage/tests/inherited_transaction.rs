//! Writing inside a transaction the caller began.
//!
//! SQLite does not nest transactions, so before the write paths learned to
//! inherit one, every test in this file failed with "cannot start a transaction
//! within a transaction". Each therefore checks two things: that the write
//! happens at all, and that the caller still owns the commit, which is what
//! distinguishes inheriting from the crate quietly opening its own.
//!
//! The second half is the one that matters. A commit that silently committed
//! the caller's transaction would pass every "did the write happen" assertion
//! and would be wrong, so each test rolls the caller's transaction back
//! afterwards and requires the work to disappear with it.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geo_types::Point;
use geopackage::core::tiles::{TileCoord, TileMatrixSet, ZoomLadder};
use geopackage::core::types::{GeometryType, ZmFlag};
use geopackage::{
    BulkIndexOptions, Error, GeoPackage, GeometrySpec, NewFeature, TableSchemaBuilder,
    TilePyramidBuilder,
};

fn gpkg() -> (tempfile::TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    (dir, gpkg)
}

/// A point layer, unindexed unless a test asks for the index itself.
fn point_layer(gpkg: &GeoPackage, table: &str, indexed: bool) {
    gpkg.create_layer(
        &TableSchemaBuilder::new(table)
            .geometry(GeometrySpec::new(GeometryType::Point, 4326).z(ZmFlag::Prohibited))
            .spatial_index(indexed),
    )
    .unwrap();
}

fn points(count: i64) -> Vec<NewFeature<Point<f64>>> {
    (0..count)
        .map(|i| {
            #[expect(
                clippy::cast_precision_loss,
                reason = "test coordinates, well inside f64's exact integer range"
            )]
            let v = i as f64;
            NewFeature::new(Point::new(v, v * 2.0), Vec::new())
        })
        .collect()
}

fn row_count(gpkg: &GeoPackage, table: &str) -> i64 {
    gpkg.connection()
        .query_row(&format!("SELECT count(*) FROM \"{table}\""), [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn table_present(gpkg: &GeoPackage, table: &str) -> bool {
    gpkg.connection()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = ?1",
            [table],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
        > 0
}

/// The case the C ABI was blocked on: a caller's `BEGIN`, then the ordinary
/// write API.
///
/// `batch_size` is deliberately smaller than the write, so the writer reaches
/// its per-batch commit repeatedly. Each of those is staged rather than
/// durable, which the rollback is what proves: were any batch committed for
/// real, some rows would survive it.
#[test]
fn write_all_joins_the_callers_transaction_and_leaves_the_commit_to_them() {
    let (_dir, gpkg) = gpkg();
    point_layer(&gpkg, "wa", false);

    let tx = gpkg.connection().unchecked_transaction().unwrap();
    let layer = gpkg.layer("wa").unwrap();
    let fids = layer.write_all(points(250), 100).unwrap();
    assert_eq!(fids, (1..=250).collect::<Vec<_>>());
    // Visible on this connection, because it is the connection the caller's
    // transaction belongs to.
    assert_eq!(row_count(&gpkg, "wa"), 250);

    tx.rollback().unwrap();
    assert_eq!(
        row_count(&gpkg, "wa"),
        0,
        "a batch commit was durable, so batch_size still bounded a transaction"
    );
}

/// The same write, committed rather than rolled back, so the inherited path is
/// checked to produce the same file the owned path does.
#[test]
fn a_committed_caller_transaction_leaves_the_rows_and_the_catalogue_bounds() {
    let (_dir, gpkg) = gpkg();
    point_layer(&gpkg, "wa", false);

    let tx = gpkg.connection().unchecked_transaction().unwrap();
    gpkg.layer("wa")
        .unwrap()
        .write_all(points(250), 100)
        .unwrap();
    tx.commit().unwrap();

    assert_eq!(row_count(&gpkg, "wa"), 250);
    // The `gpkg_contents` flush is staged like every other statement, so it
    // must have travelled with the rows rather than being lost with the
    // writer's own commit.
    let entry = gpkg
        .contents()
        .unwrap()
        .into_iter()
        .find(|e| e.table_name == "wa")
        .unwrap();
    assert_eq!(entry.min_x, Some(0.0));
    assert_eq!(entry.max_x, Some(249.0));
    assert_eq!(entry.max_y, Some(498.0));
}

/// A writer driven directly, rather than through `write_all`.
#[test]
fn a_feature_writer_commits_into_the_callers_transaction() {
    let (_dir, gpkg) = gpkg();
    point_layer(&gpkg, "fw", false);

    let tx = gpkg.connection().unchecked_transaction().unwrap();
    let layer = gpkg.layer("fw").unwrap();
    let mut writer = layer.writer().unwrap();
    writer.insert(None, &Point::new(1.0, 2.0), &[]).unwrap();
    writer.insert(None, &Point::new(3.0, 4.0), &[]).unwrap();
    // Returns success, and that success means "staged", not "durable".
    writer.commit().unwrap();
    assert_eq!(row_count(&gpkg, "fw"), 2);

    tx.rollback().unwrap();
    assert_eq!(
        row_count(&gpkg, "fw"),
        0,
        "FeatureWriter::commit committed the caller's transaction"
    );
}

/// Dropping a writer without committing rolls nothing back when the
/// transaction is the caller's, which is the half of the contract the caller
/// has to know about: partial work is theirs to discard.
#[test]
fn dropping_an_inheriting_writer_leaves_its_rows_staged() {
    let (_dir, gpkg) = gpkg();
    point_layer(&gpkg, "dw", false);

    let tx = gpkg.connection().unchecked_transaction().unwrap();
    let layer = gpkg.layer("dw").unwrap();
    let mut writer = layer.writer().unwrap();
    writer.insert(None, &Point::new(1.0, 2.0), &[]).unwrap();
    drop(writer);
    assert_eq!(
        row_count(&gpkg, "dw"),
        1,
        "dropping the writer rolled back the caller's transaction"
    );

    tx.rollback().unwrap();
    assert_eq!(row_count(&gpkg, "dw"), 0);
}

/// The bulk path, which is the one that does DDL inside the transaction: it
/// drops the rtree triggers, writes the rows, builds the index outright and
/// reinstalls the triggers. All of that has to join the caller's transaction
/// too, and unwind with it.
#[test]
fn the_bulk_write_path_joins_the_callers_transaction() {
    let (_dir, gpkg) = gpkg();
    point_layer(&gpkg, "pts", true);
    assert!(table_present(&gpkg, "rtree_pts_geom"));

    let tx = gpkg.connection().unchecked_transaction().unwrap();
    let layer = gpkg.layer("pts").unwrap();
    layer
        .write_all_with(points(300), 0, BulkIndexOptions::always_bulk())
        .unwrap();
    assert_eq!(row_count(&gpkg, "pts"), 300);
    assert_eq!(row_count(&gpkg, "rtree_pts_geom"), 300);
    // Reinstalled inside the same transaction as the drop.
    assert!(layer.has_spatial_index().unwrap());

    tx.rollback().unwrap();
    assert_eq!(row_count(&gpkg, "pts"), 0);
    assert_eq!(row_count(&gpkg, "rtree_pts_geom"), 0);
    // The triggers the bulk path dropped came back with the rollback, so the
    // layer is not left indexed-but-untriggered.
    assert!(layer.has_spatial_index().unwrap());
}

/// Index creation is DDL plus a `gpkg_extensions` row, and previously could not
/// run inside a caller's transaction at all.
#[test]
fn creating_a_spatial_index_joins_the_callers_transaction() {
    let (_dir, gpkg) = gpkg();
    point_layer(&gpkg, "ix", false);
    gpkg.layer("ix").unwrap().write_all(points(20), 0).unwrap();
    assert!(!table_present(&gpkg, "rtree_ix_geom"));

    let tx = gpkg.connection().unchecked_transaction().unwrap();
    let layer = gpkg.layer("ix").unwrap();
    layer.create_spatial_index().unwrap();
    assert!(layer.has_spatial_index().unwrap());
    assert_eq!(row_count(&gpkg, "rtree_ix_geom"), 20);

    tx.rollback().unwrap();
    assert!(
        !table_present(&gpkg, "rtree_ix_geom"),
        "the index survived a rollback, so it was committed outside the caller's transaction"
    );
}

/// Layer creation, which is the DDL a copy tool issues before anything else.
#[test]
fn creating_a_layer_joins_the_callers_transaction() {
    let (_dir, gpkg) = gpkg();

    let tx = gpkg.connection().unchecked_transaction().unwrap();
    point_layer(&gpkg, "made", true);
    assert!(table_present(&gpkg, "made"));

    tx.rollback().unwrap();
    assert!(!table_present(&gpkg, "made"));
    // The catalogue row went with the table, so the public API agrees the layer
    // was never created.
    assert!(matches!(gpkg.layer("made"), Err(Error::NoSuchLayer { .. })));
}

/// The tile write path, whose writer has the same shape as the feature one.
#[test]
fn a_tile_writer_joins_the_callers_transaction() {
    let (_dir, gpkg) = gpkg();
    gpkg.add_epsg_srs(3857).unwrap();
    let matrix_set = TileMatrixSet::web_mercator_quad();
    let matrices = matrix_set.ladder(ZoomLadder::new(0, 2)).unwrap();
    gpkg.create_tile_pyramid(&TilePyramidBuilder::new("basemap", matrix_set).matrices(matrices))
        .unwrap();

    let tx = gpkg.connection().unchecked_transaction().unwrap();
    let pyramid = gpkg.tiles("basemap").unwrap();
    let mut writer = pyramid.writer().unwrap();
    writer.put(TileCoord::new(1, 0, 0), &png()).unwrap();
    writer.commit().unwrap();
    assert_eq!(row_count(&gpkg, "basemap"), 1);

    tx.rollback().unwrap();
    assert_eq!(
        row_count(&gpkg, "basemap"),
        0,
        "TileWriter::commit committed the caller's transaction"
    );
}

/// A 256x256 PNG header with a zeroed CRC: enough for the payload probe, which
/// reads headers and decodes nothing.
fn png() -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&13_u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&256_u32.to_be_bytes());
    bytes.extend_from_slice(&256_u32.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    bytes
}
