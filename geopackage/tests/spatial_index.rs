//! Spatial-index lifecycle: `Layer::create_spatial_index`,
//! `drop_spatial_index`, and `repair_spatial_index`.
//!
//! These exercise the write-side index management on top of the read-side
//! `has_spatial_index` / `features_in` from M1: building an index over an
//! already-populated table, its `gpkg_extensions` registration, and the D7
//! legacy-trigger repair path.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geo_types::Point;
use geopackage::core::gpb::{Envelope, encode_header};
use geopackage::core::triggers::{self, TriggerGeneration};
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{
    BoundingBox, BulkIndexOptions, ColumnSpec, GeoPackage, GeometrySpec, NewFeature,
    StructuralCheck, TableSchemaBuilder, Value,
};
use hegel::generators;
use rusqlite::{Connection, OptionalExtension};
use std::path::Path;

/// A GPB blob for an empty point (empty flag + NaN coordinates, no envelope):
/// the population guard must skip this exactly as the triggers' `ST_IsEmpty`
/// check does.
fn gpb_empty_point(srs_id: i32) -> Vec<u8> {
    let mut blob = encode_header(srs_id, &Envelope::None, true, false);
    blob.push(1);
    blob.extend_from_slice(&1u32.to_le_bytes());
    blob.extend_from_slice(&f64::NAN.to_le_bytes());
    blob.extend_from_slice(&f64::NAN.to_le_bytes());
    blob
}

/// A GPB blob for an XY point with an XY envelope (little-endian WKB).
fn gpb_point(srs_id: i32, x: f64, y: f64) -> Vec<u8> {
    let mut blob = encode_header(srs_id, &Envelope::Xy([x, x, y, y]), false, false);
    blob.push(1);
    blob.extend_from_slice(&1u32.to_le_bytes());
    blob.extend_from_slice(&x.to_le_bytes());
    blob.extend_from_slice(&y.to_le_bytes());
    blob
}

/// Add a `pts(fid, name, geom)` feature layer to `gpkg` and populate it with
/// the given `(fid, x, y)` points via the high-level writer. Coordinates are
/// chosen exact-in-`f32` by callers so the index (which stores `f32` bounds)
/// compares equal to the header envelope.
fn add_points_layer(gpkg: &GeoPackage, points: &[(i64, f64, f64)]) {
    let builder = TableSchemaBuilder::new("pts")
        .column(ColumnSpec::new("name", ColumnType::Text(None)))
        .geometry(GeometrySpec::new(GeometryType::Point, 4326));
    let layer = gpkg.create_layer(&builder).unwrap();
    let mut writer = layer.writer().unwrap();
    for &(fid, x, y) in points {
        writer
            .insert(Some(fid), &Point::new(x, y), &[Value::Null])
            .unwrap();
    }
    writer.commit().unwrap();
}

/// Create an on-disk GeoPackage in a fresh tempdir, carrying the `pts` layer
/// from [`add_points_layer`].
fn gpkg_with_points(points: &[(i64, f64, f64)]) -> (tempfile::TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    add_points_layer(&gpkg, points);
    (dir, gpkg)
}

/// As [`gpkg_with_points`], but held entirely in SQLite's private per-connection
/// in-memory database (the `:memory:` filename) rather than in a file.
///
/// The property tests below build a whole GeoPackage (or two) per generated
/// case, and that cost is dominated by the container, not by the number of
/// points: measured per case, a fixture with 26 points costs the same as one
/// with none. On Windows CI, where creating a file, cycling a rollback journal
/// per transaction and syncing all cost orders of magnitude more than on
/// macOS/Linux, this reached roughly 3.8s per case and tripped hegel's
/// `TooSlow` health check, which requires 10 valid cases inside 30s. Dropping
/// the file removes that term on every platform, so the input space stays as
/// wide as it was rather than being trimmed to buy back time.
///
/// Nothing these properties assert depends on the container being a file: they
/// compare rtree table contents against a triggered build and against an `ST_*`
/// scan of the same database. The example-based tests in this file exercise the
/// same index-building code over real files.
fn in_memory_with_points(points: &[(i64, f64, f64)]) -> GeoPackage {
    let gpkg = GeoPackage::create(Path::new(":memory:")).unwrap();
    add_points_layer(&gpkg, points);
    gpkg
}

/// The rtree contents as `(id, minx, maxx, miny, maxy)`, ordered by id.
fn rtree_rows(conn: &Connection) -> Vec<(i64, f64, f64, f64, f64)> {
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

/// A manual envelope scan of the user table via the registered `ST_*`
/// functions, with the same NULL/empty guard the population statement uses:
/// `(fid, minx, maxx, miny, maxy)` ordered by fid. This is the reference the
/// index contents must match.
fn envelope_scan(conn: &Connection) -> Vec<(i64, f64, f64, f64, f64)> {
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

/// The trigger names present for the `pts.geom` rtree, from `sqlite_master`.
fn trigger_names(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = 'pts'")
        .unwrap();
    let mut names: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    names.sort();
    names
}

fn generation(conn: &Connection) -> TriggerGeneration {
    triggers::classify_triggers(
        trigger_names(conn).iter().map(String::as_str),
        "pts",
        "geom",
    )
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type IN ('table','view') AND name = ?1",
        [name],
        |_| Ok(()),
    )
    .optional()
    .unwrap()
    .is_some()
}

#[test]
fn create_populates_index_matching_envelope_scan() {
    let (_dir, gpkg) = gpkg_with_points(&[(1, 10.0, 20.0), (2, -5.0, 7.0), (3, 100.0, 100.0)]);
    let layer = gpkg.layer("pts").unwrap();
    assert!(!layer.has_spatial_index().unwrap());

    layer.create_spatial_index().unwrap();

    let conn = gpkg.connection();
    // The index contents match a manual envelope scan of the source rows.
    assert_eq!(rtree_rows(conn), envelope_scan(conn));
    // The freshly built index is the 1.4 generation.
    assert!(layer.has_spatial_index().unwrap());
    assert_eq!(generation(conn), TriggerGeneration::V1_4);
}

#[test]
fn extension_row_matches_spec_strings() {
    let (_dir, gpkg) = gpkg_with_points(&[(1, 1.0, 1.0)]);
    let layer = gpkg.layer("pts").unwrap();
    layer.create_spatial_index().unwrap();

    let (table_name, column_name, extension_name, definition, scope): (
        String,
        String,
        String,
        String,
        String,
    ) = gpkg
        .connection()
        .query_row(
            "SELECT table_name, column_name, extension_name, definition, scope \
             FROM gpkg_extensions WHERE extension_name = 'gpkg_rtree_index'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(table_name, "pts");
    assert_eq!(column_name, "geom");
    // Spec Annex F.3 requirements 75/76: extension_name and scope are
    // prescribed verbatim; definition is a permalink to the extension section.
    assert_eq!(extension_name, "gpkg_rtree_index");
    assert_eq!(scope, "write-only");
    assert_eq!(
        definition,
        "http://www.geopackage.org/spec140/#extension_rtree"
    );
    // The constants used to write the row are the same spec strings.
    assert_eq!(extension_name, triggers::EXTENSION_NAME);
    assert_eq!(definition, triggers::EXTENSION_DEFINITION);
    assert_eq!(scope, triggers::EXTENSION_SCOPE);
}

#[test]
fn create_skips_null_and_empty_geometries() {
    let (_dir, gpkg) = gpkg_with_points(&[(1, 10.0, 20.0)]);
    let conn = gpkg.connection();
    // A NULL geometry and a raw empty-point geometry, alongside the real point.
    conn.execute(
        "INSERT INTO pts (fid, name, geom) VALUES (2, 'null', NULL)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO pts (fid, name, geom) VALUES (3, 'empty', ?1)",
        [gpb_empty_point(4326)],
    )
    .unwrap();

    let layer = gpkg.layer("pts").unwrap();
    layer.create_spatial_index().unwrap();

    // Only the non-empty, non-NULL geometry is indexed.
    let ids: Vec<i64> = rtree_rows(conn).into_iter().map(|r| r.0).collect();
    assert_eq!(ids, vec![1]);
    assert_eq!(rtree_rows(conn), envelope_scan(conn));
}

#[test]
fn features_in_uses_vtab_with_identical_results() {
    let (_dir, gpkg) = gpkg_with_points(&[(1, 0.0, 0.0), (2, 50.0, 50.0), (3, 100.0, 100.0)]);
    let layer = gpkg.layer("pts").unwrap();
    let bbox = BoundingBox::new(40.0, 40.0, 60.0, 60.0);

    layer.create_spatial_index().unwrap();
    assert!(layer.has_spatial_index().unwrap());

    // The query plan uses the rtree virtual table.
    let plan = query_plan(gpkg.connection(), &layer.features_in_sql().unwrap());
    assert!(
        plan.contains("VIRTUAL TABLE INDEX"),
        "expected rtree vtab in plan, got: {plan}"
    );

    let indexed: Vec<i64> = layer
        .features_in(bbox)
        .unwrap()
        .map(|f| f.unwrap().fid())
        .collect();
    assert_eq!(indexed, vec![2]);

    // Dropping the index changes the plan to a scan and yields identical rows.
    layer.drop_spatial_index().unwrap();
    assert!(!layer.has_spatial_index().unwrap());
    let scan_plan = query_plan(gpkg.connection(), &layer.features_in_sql().unwrap());
    assert!(
        !scan_plan.contains("VIRTUAL TABLE INDEX"),
        "expected a full scan after drop, got: {scan_plan}"
    );
    let scanned: Vec<i64> = layer
        .features_in(bbox)
        .unwrap()
        .map(|f| f.unwrap().fid())
        .collect();
    assert_eq!(indexed, scanned);
}

/// The `EXPLAIN QUERY PLAN` detail lines for `sql`, joined. The rtree form
/// carries four placeholders (the query box) and the full-scan form none; both
/// are bound with the right number of dummy values.
fn query_plan(conn: &Connection, sql: &str) -> String {
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let params = vec![0.0f64; stmt.parameter_count()];
    let details: Vec<String> = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |r| {
            r.get::<_, String>(3)
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    details.join("\n")
}

#[test]
fn upsert_through_new_index_maintains_rtree() {
    let (_dir, gpkg) = gpkg_with_points(&[(1, 1.0, 1.0)]);
    let layer = gpkg.layer("pts").unwrap();
    layer.create_spatial_index().unwrap();
    let conn = gpkg.connection();

    // An UPSERT that updates the existing row's geometry: the pre-1.4 update1
    // trigger corrupted this; the 1.4 update6/update7 pair handles it.
    conn.execute(
        "INSERT INTO pts (fid, name, geom) VALUES (1, 'b', ?1) \
         ON CONFLICT (fid) DO UPDATE SET geom = excluded.geom, name = excluded.name",
        [gpb_point(4326, 9.0, 9.0)],
    )
    .unwrap();

    let rows = rtree_rows(conn);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, 1);
    assert!((rows[0].1 - 9.0).abs() < 1e-6);
    assert_eq!(rtree_rows(conn), envelope_scan(conn));
}

#[test]
fn drop_removes_triggers_vtab_and_extension_row() {
    let (_dir, gpkg) = gpkg_with_points(&[(1, 1.0, 1.0), (2, 2.0, 2.0)]);
    let layer = gpkg.layer("pts").unwrap();
    layer.create_spatial_index().unwrap();
    let conn = gpkg.connection();
    assert!(table_exists(conn, "rtree_pts_geom"));
    assert!(!trigger_names(conn).is_empty());

    layer.drop_spatial_index().unwrap();

    // Triggers, the virtual table, and the extension row are gone.
    assert!(trigger_names(conn).is_empty());
    assert!(!table_exists(conn, "rtree_pts_geom"));
    let ext_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM gpkg_extensions WHERE extension_name = 'gpkg_rtree_index'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ext_rows, 0);
    // The gpkg_extensions table itself and the user table survive.
    assert!(table_exists(conn, "gpkg_extensions"));
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM pts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 2);

    // Drop is idempotent.
    layer.drop_spatial_index().unwrap();
    assert!(!table_exists(conn, "rtree_pts_geom"));
}

/// Install the GeoPackage 1.3.1 (pre-1.4) rtree trigger set for `pts.geom`,
/// verbatim from Annex F.3 of the 1.3.1 standard (with placeholders `<t>`=pts,
/// `<c>`=geom, `<i>`=fid substituted). The buggy `update1` and `update3`
/// triggers mark this as [`TriggerGeneration::PreV1_4`].
fn install_legacy_triggers(conn: &Connection) {
    conn.execute_batch(
        "CREATE TRIGGER \"rtree_pts_geom_insert\" AFTER INSERT ON \"pts\"
           WHEN (new.geom NOT NULL AND NOT ST_IsEmpty(NEW.geom))
         BEGIN
           INSERT OR REPLACE INTO rtree_pts_geom VALUES (
             NEW.fid,
             ST_MinX(NEW.geom), ST_MaxX(NEW.geom),
             ST_MinY(NEW.geom), ST_MaxY(NEW.geom)
           );
         END;
         CREATE TRIGGER \"rtree_pts_geom_update1\" AFTER UPDATE OF geom ON \"pts\"
           WHEN OLD.fid = NEW.fid AND
                (NEW.geom NOTNULL AND NOT ST_IsEmpty(NEW.geom))
         BEGIN
           INSERT OR REPLACE INTO rtree_pts_geom VALUES (
             NEW.fid,
             ST_MinX(NEW.geom), ST_MaxX(NEW.geom),
             ST_MinY(NEW.geom), ST_MaxY(NEW.geom)
           );
         END;
         CREATE TRIGGER \"rtree_pts_geom_update2\" AFTER UPDATE OF geom ON \"pts\"
           WHEN OLD.fid = NEW.fid AND
                (NEW.geom ISNULL OR ST_IsEmpty(NEW.geom))
         BEGIN
           DELETE FROM rtree_pts_geom WHERE id = OLD.fid;
         END;
         CREATE TRIGGER \"rtree_pts_geom_update3\" AFTER UPDATE ON \"pts\"
           WHEN OLD.fid != NEW.fid AND
                (NEW.geom NOTNULL AND NOT ST_IsEmpty(NEW.geom))
         BEGIN
           DELETE FROM rtree_pts_geom WHERE id = OLD.fid;
           INSERT OR REPLACE INTO rtree_pts_geom VALUES (
             NEW.fid,
             ST_MinX(NEW.geom), ST_MaxX(NEW.geom),
             ST_MinY(NEW.geom), ST_MaxY(NEW.geom)
           );
         END;
         CREATE TRIGGER \"rtree_pts_geom_update4\" AFTER UPDATE ON \"pts\"
           WHEN OLD.fid != NEW.fid AND
                (NEW.geom ISNULL OR ST_IsEmpty(NEW.geom))
         BEGIN
           DELETE FROM rtree_pts_geom WHERE id IN (OLD.fid, NEW.fid);
         END;
         CREATE TRIGGER \"rtree_pts_geom_delete\" AFTER DELETE ON \"pts\"
           WHEN old.geom NOT NULL
         BEGIN
           DELETE FROM rtree_pts_geom WHERE id = OLD.fid;
         END;",
    )
    .unwrap();
}

#[test]
fn repair_upgrades_legacy_triggers_to_v1_4() {
    // Rows exist before any triggers, so the legacy index below starts stale.
    let (_dir, gpkg) = gpkg_with_points(&[(1, 10.0, 20.0), (2, -5.0, 7.0), (3, 100.0, 100.0)]);
    let conn = gpkg.connection();

    // Hand-build a pre-1.4 index: the virtual table, the legacy trigger set,
    // and a deliberately wrong rtree row so the rebuild is observable.
    conn.execute_batch(&triggers::create_rtree_table_sql("pts", "geom").unwrap())
        .unwrap();
    install_legacy_triggers(conn);
    conn.execute(
        "INSERT INTO rtree_pts_geom VALUES (999, 1000.0, 1000.0, 1000.0, 1000.0)",
        [],
    )
    .unwrap();
    assert_eq!(generation(conn), TriggerGeneration::PreV1_4);

    let layer = gpkg.layer("pts").unwrap();
    layer.repair_spatial_index().unwrap();

    // The trigger set is now 1.4, and the index content matches the rows
    // (the bogus id 999 is gone, the real rows are present).
    assert_eq!(generation(conn), TriggerGeneration::V1_4);
    assert_eq!(rtree_rows(conn), envelope_scan(conn));
    let ids: Vec<i64> = rtree_rows(conn).into_iter().map(|r| r.0).collect();
    assert_eq!(ids, vec![1, 2, 3]);
}

#[test]
fn repair_on_v1_4_is_a_noop() {
    let (_dir, gpkg) = gpkg_with_points(&[(1, 10.0, 20.0), (2, -5.0, 7.0)]);
    let layer = gpkg.layer("pts").unwrap();
    layer.create_spatial_index().unwrap();
    let conn = gpkg.connection();

    let triggers_before = trigger_names(conn);
    let rtree_before = rtree_rows(conn);

    layer.repair_spatial_index().unwrap();

    assert_eq!(trigger_names(conn), triggers_before);
    assert_eq!(rtree_rows(conn), rtree_before);
    assert_eq!(generation(conn), TriggerGeneration::V1_4);
}

#[test]
fn repair_without_index_errors() {
    let (_dir, gpkg) = gpkg_with_points(&[(1, 1.0, 1.0)]);
    let layer = gpkg.layer("pts").unwrap();
    let err = layer.repair_spatial_index().unwrap_err();
    assert!(
        matches!(err, geopackage::Error::NoSpatialIndex { .. }),
        "expected NoSpatialIndex, got {err:?}"
    );
}

#[test]
fn create_on_already_indexed_errors() {
    let (_dir, gpkg) = gpkg_with_points(&[(1, 1.0, 1.0)]);
    let layer = gpkg.layer("pts").unwrap();
    layer.create_spatial_index().unwrap();
    let err = layer.create_spatial_index().unwrap_err();
    assert!(
        matches!(err, geopackage::Error::SpatialIndexExists { .. }),
        "expected SpatialIndexExists, got {err:?}"
    );
}

#[test]
fn create_on_attribute_layer_errors() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    let builder =
        TableSchemaBuilder::new("notes").column(ColumnSpec::new("body", ColumnType::Text(None)));
    let layer = gpkg.create_attributes_table(&builder).unwrap();
    let err = layer.create_spatial_index().unwrap_err();
    assert!(
        matches!(err, geopackage::Error::NoGeometryColumn { .. }),
        "expected NoGeometryColumn, got {err:?}"
    );
}

/// Draw a coordinate that is exactly representable in `f32`, widened to `f64`.
///
/// The RTree stores 32-bit bounds (minima rounded down, maxima rounded up), so
/// only for `f32`-exact coordinates does the stored bound equal the `ST_*`
/// envelope exactly, which is what lets these property tests compare the index
/// to the scan with `==` rather than a containment tolerance.
fn draw_coord(tc: &hegel::TestCase) -> f64 {
    f64::from(
        tc.draw(
            generators::floats::<f32>()
                .min_value(-1000.0)
                .max_value(1000.0),
        ),
    )
}

/// Upsert a point at `fid` via raw `ON CONFLICT` SQL (the case the pre-1.4
/// `update1` trigger corrupted; the 1.4 set must keep the index correct).
fn upsert_point(conn: &Connection, fid: i64, x: f64, y: f64) {
    conn.execute(
        "INSERT INTO pts (fid, geom) VALUES (?1, ?2) \
         ON CONFLICT(fid) DO UPDATE SET geom = excluded.geom",
        rusqlite::params![fid, gpb_point(4326, x, y)],
    )
    .unwrap();
}

/// Upsert a NULL geometry at `fid` (exercises the empty/NULL trigger guards).
fn upsert_null(conn: &Connection, fid: i64) {
    conn.execute(
        "INSERT INTO pts (fid, geom) VALUES (?1, NULL) \
         ON CONFLICT(fid) DO UPDATE SET geom = NULL",
        [fid],
    )
    .unwrap();
}

/// M2 acceptance criterion 3: the RTree contents provably match a full-scan
/// rebuild after an arbitrary insert/update/delete/upsert sequence, with the
/// initial index built through both the triggered and the bulk (D8) path.
///
/// The oracle is independent of the index: `envelope_scan` recomputes the
/// expected contents directly from the current table rows via the `ST_*`
/// functions with the same NULL/empty guard, so it can never agree with a buggy
/// trigger set by construction.
#[hegel::test]
fn rtree_tracks_full_scan_through_write_ops(tc: hegel::TestCase) {
    let (_dir, gpkg) = gpkg_with_points(&[]);
    let layer = gpkg.layer("pts").unwrap();
    let conn = gpkg.connection();

    // Seed some rows, then build the initial index through the drawn path.
    let seed = tc.draw(generators::integers::<usize>().min_value(0).max_value(10));
    for fid in 1..=(seed as i64) {
        upsert_point(conn, fid, draw_coord(&tc), draw_coord(&tc));
    }
    let options = if tc.draw(generators::booleans()) {
        BulkIndexOptions::always_bulk()
    } else {
        BulkIndexOptions::never_bulk()
    };
    layer.create_spatial_index_with(options).unwrap();
    assert_eq!(
        rtree_rows(conn),
        envelope_scan(conn),
        "index diverged from the scan right after the initial build"
    );

    // Apply an arbitrary op sequence over a small fid space (so upserts,
    // updates, and deletes collide with existing rows), checking the invariant
    // after every step.
    let n_ops = tc.draw(generators::integers::<usize>().min_value(0).max_value(40));
    for step in 0..n_ops {
        let fid = tc.draw(generators::integers::<i64>().min_value(1).max_value(8));
        match tc.draw(generators::integers::<u8>().min_value(0).max_value(3)) {
            0 => upsert_point(conn, fid, draw_coord(&tc), draw_coord(&tc)),
            1 => upsert_null(conn, fid),
            2 => {
                let (x, y) = (draw_coord(&tc), draw_coord(&tc));
                let mut writer = layer.writer().unwrap();
                writer
                    .update(fid, &Point::new(x, y), &[Value::Null])
                    .unwrap();
                writer.commit().unwrap();
            }
            _ => {
                let mut writer = layer.writer().unwrap();
                writer.delete(fid).unwrap();
                writer.commit().unwrap();
            }
        }
        assert_eq!(
            rtree_rows(conn),
            envelope_scan(conn),
            "index diverged from the scan after op {step}"
        );
    }
}

/// The bulk (D8) and triggered build paths produce byte-identical index
/// contents, and both equal the full-scan rebuild, for an arbitrary feature set.
#[hegel::test]
fn bulk_and_triggered_builds_agree(tc: hegel::TestCase) {
    // Above the 51-entry node capacity, so the generated trees span the
    // single-root-leaf case and multi-node cases with a real internal level.
    // At or below 51 the packed and triggered builders are structurally
    // indistinguishable, which made the earlier cap of 30 a much weaker claim
    // than it appeared.
    let n = tc.draw(generators::integers::<usize>().min_value(0).max_value(140));
    let points: Vec<(i64, f64, f64)> = (1..=(n as i64))
        .map(|fid| (fid, draw_coord(&tc), draw_coord(&tc)))
        .collect();

    let triggered = in_memory_with_points(&points);
    triggered
        .layer("pts")
        .unwrap()
        .create_spatial_index_with(BulkIndexOptions::never_bulk())
        .unwrap();

    let bulk = in_memory_with_points(&points);
    bulk.layer("pts")
        .unwrap()
        .create_spatial_index_with(BulkIndexOptions::always_bulk())
        .unwrap();

    // The two build paths agree with each other and with the scan.
    assert_eq!(
        rtree_rows(bulk.connection()),
        rtree_rows(triggered.connection()),
        "bulk and triggered index contents differ"
    );
    assert_eq!(
        rtree_rows(bulk.connection()),
        envelope_scan(bulk.connection()),
        "bulk index does not match the scan"
    );
}

/// The bulk `write_all` path reuses the envelopes computed while encoding each
/// geometry instead of re-deriving them with an `ST_*` scan. That reused set
/// must yield exactly the index a triggered build produces, for an arbitrary
/// feature set.
#[hegel::test]
fn write_all_bulk_envelopes_match_triggered_build(tc: hegel::TestCase) {
    // Above the 51-entry node capacity, so the generated trees span the
    // single-root-leaf case and multi-node cases with a real internal level.
    // At or below 51 the packed and triggered builders are structurally
    // indistinguishable, which made the earlier cap of 30 a much weaker claim
    // than it appeared.
    let n = tc.draw(generators::integers::<usize>().min_value(0).max_value(140));
    let points: Vec<(i64, f64, f64)> = (1..=(n as i64))
        .map(|fid| (fid, draw_coord(&tc), draw_coord(&tc)))
        .collect();

    // Reference: rows written first, then a triggered index built over them.
    let triggered = in_memory_with_points(&points);
    triggered
        .layer("pts")
        .unwrap()
        .create_spatial_index_with(BulkIndexOptions::never_bulk())
        .unwrap();

    // Under test: an empty indexed layer loaded with `write_all`, which takes
    // the bulk path and feeds it the encode-time envelopes.
    let gpkg = in_memory_with_points(&[]);
    let layer = gpkg.layer("pts").unwrap();
    layer.create_spatial_index().unwrap();
    let features: Vec<_> = points
        .iter()
        .map(|&(fid, x, y)| NewFeature::new(Point::new(x, y), vec![Value::Null]).with_fid(fid))
        .collect();
    layer
        .write_all_with(features, 0, BulkIndexOptions::always_bulk())
        .unwrap();

    assert_eq!(
        rtree_rows(gpkg.connection()),
        rtree_rows(triggered.connection()),
        "write_all bulk index differs from the triggered build"
    );
    assert_eq!(
        rtree_rows(gpkg.connection()),
        envelope_scan(gpkg.connection()),
        "write_all bulk index does not match the scan"
    );
}

/// Rows already in the table before a bulk `write_all` mean the encode-time
/// envelopes do not account for every indexable row, so the build must fall
/// back to deriving the entry set by scanning. Pre-existing NULL-geometry rows
/// leave the index legitimately empty, which is what makes the bulk path
/// eligible in the first place.
#[test]
fn write_all_bulk_with_preexisting_rows_indexes_every_row() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("m.gpkg")).unwrap();
    let builder = TableSchemaBuilder::new("pts")
        .column(ColumnSpec::new("name", ColumnType::Text(None)))
        .geometry(GeometrySpec::new(GeometryType::Point, 4326));
    let layer = gpkg.create_layer(&builder).unwrap();

    // Two NULL-geometry rows: indexable-row count stays zero.
    {
        let mut writer = layer.writer().unwrap();
        writer.insert_row(Some(1), &[Value::Null]).unwrap();
        writer.insert_row(Some(2), &[Value::Null]).unwrap();
        writer.commit().unwrap();
    }
    layer.create_spatial_index().unwrap();
    assert_eq!(rtree_rows(gpkg.connection()).len(), 0);

    let features = vec![
        NewFeature::new(Point::new(5.0, 6.0), vec![Value::Null]).with_fid(3),
        NewFeature::new(Point::new(7.0, 8.0), vec![Value::Null]).with_fid(4),
    ];
    layer
        .write_all_with(features, 0, BulkIndexOptions::always_bulk())
        .unwrap();

    let conn = gpkg.connection();
    assert_eq!(rtree_rows(conn), envelope_scan(conn));
    let ids: Vec<i64> = rtree_rows(conn).into_iter().map(|r| r.0).collect();
    assert_eq!(ids, vec![3, 4]);
}

/// The opt-in whole-database structural check still produces a correct index.
#[test]
fn bulk_build_with_full_database_check_is_correct() {
    let points: Vec<(i64, f64, f64)> = (1..=20).map(|i| (i, i as f64, -(i as f64))).collect();
    let (_dir, gpkg) = gpkg_with_points(&points);
    gpkg.layer("pts")
        .unwrap()
        .create_spatial_index_with(
            BulkIndexOptions::always_bulk().with_structural_check(StructuralCheck::FullDatabase),
        )
        .unwrap();

    let conn = gpkg.connection();
    assert_eq!(rtree_rows(conn), envelope_scan(conn));
    assert_eq!(rtree_rows(conn).len(), 20);
}

#[test]
fn feature_writer_maintains_new_index() {
    let (_dir, gpkg) = gpkg_with_points(&[(1, 1.0, 1.0), (2, 2.0, 2.0)]);
    let layer = gpkg.layer("pts").unwrap();
    layer.create_spatial_index().unwrap();

    // Writes through FeatureWriter fire the installed triggers.
    {
        let mut writer = layer.writer().unwrap();
        writer
            .insert(Some(3), &Point::new(30.0, 40.0), &[Value::Null])
            .unwrap();
        writer
            .update(1, &Point::new(-7.0, -8.0), &[Value::Null])
            .unwrap();
        writer.delete(2).unwrap();
        writer.commit().unwrap();
    }

    let conn = gpkg.connection();
    // The index tracks the insert (3), the update (1 moved), and the delete (2).
    assert_eq!(rtree_rows(conn), envelope_scan(conn));
    let ids: Vec<i64> = rtree_rows(conn).into_iter().map(|r| r.0).collect();
    assert_eq!(ids, vec![1, 3]);

    // The writer also kept gpkg_contents tracking (bounding box grew to cover
    // the new/updated points).
    let (min_x, min_y, max_x, max_y): (f64, f64, f64, f64) = conn
        .query_row(
            "SELECT min_x, min_y, max_x, max_y FROM gpkg_contents WHERE table_name = 'pts'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert!(min_x <= -7.0 && min_y <= -8.0 && max_x >= 30.0 && max_y >= 40.0);

    // And the index still serves features_in.
    let hits: Vec<i64> = layer
        .features_in(BoundingBox::new(25.0, 35.0, 35.0, 45.0))
        .unwrap()
        .map(|f| f.unwrap().fid())
        .collect();
    assert_eq!(hits, vec![3]);
}

/// A packed tree deep enough to have two internal levels must still match a
/// triggered build exactly, and satisfy SQLite's own structural check.
///
/// The property tests above top out at 140 entries, which is one internal level
/// (node capacity is 51). Three levels needs more than 51 * 51 entries, so this
/// case is deterministic rather than generated: it is the only coverage of the
/// packed builder's recursion beyond a single internal level, where the depth
/// field, the parent mappings and the root renumbering all have to line up.
#[test]
fn deep_packed_tree_matches_triggered_build() {
    // 3000 > 51 * 51, so the root has internal children which have leaf
    // children: depth 2.
    // Coordinates are multiples of 1/4 and 1/8 so they are exact in `f32`, which
    // is what lets the stored index bounds compare equal to the `ST_*` envelope
    // scan. The strides are coprime with the moduli, so the points spread over
    // the domain rather than collapsing onto a line.
    let points: Vec<(i64, f64, f64)> = (1..=3000)
        .map(|i| {
            let x = f64::from(u32::try_from((i * 7) % 1021).unwrap()) / 4.0;
            let y = f64::from(u32::try_from((i * 13) % 509).unwrap()) / 8.0;
            (i, x, y)
        })
        .collect();

    let triggered = in_memory_with_points(&points);
    triggered
        .layer("pts")
        .unwrap()
        .create_spatial_index_with(BulkIndexOptions::never_bulk())
        .unwrap();

    let packed = in_memory_with_points(&points);
    packed
        .layer("pts")
        .unwrap()
        .create_spatial_index_with(BulkIndexOptions::always_bulk())
        .unwrap();

    // The tree really is three levels deep: the root node stores its depth in
    // the first two bytes, big-endian, and depth 2 means root, internal, leaf.
    let root: Vec<u8> = packed
        .connection()
        .query_row(
            "SELECT data FROM rtree_pts_geom_node WHERE nodeno = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let depth = u16::from_be_bytes([root[0], root[1]]);
    assert_eq!(depth, 2, "expected a tree with two internal levels");

    // SQLite's own checker accepts the hand-written structure.
    let report: String = packed
        .connection()
        .query_row("SELECT rtreecheck('rtree_pts_geom')", [], |r| r.get(0))
        .unwrap();
    assert_eq!(report, "ok", "rtreecheck rejected the packed tree");

    assert_eq!(
        rtree_rows(packed.connection()),
        rtree_rows(triggered.connection()),
        "deep packed index differs from the triggered build"
    );
    assert_eq!(
        rtree_rows(packed.connection()),
        envelope_scan(packed.connection()),
        "deep packed index does not match the scan"
    );

    // And it answers queries identically through the index.
    let bbox = BoundingBox::new(10.0, 5.0, 120.0, 40.0);
    let layer = packed.layer("pts").unwrap();
    let reference = triggered.layer("pts").unwrap();
    let mut got: Vec<i64> = layer
        .features_in(bbox)
        .unwrap()
        .map(|f| f.unwrap().fid())
        .collect();
    let mut want: Vec<i64> = reference
        .features_in(bbox)
        .unwrap()
        .map(|f| f.unwrap().fid())
        .collect();
    got.sort_unstable();
    want.sort_unstable();
    assert_eq!(got, want, "deep packed index answers queries differently");
    assert!(!got.is_empty(), "the query box should match something");
}

/// A large `write_all` into an already-populated index rebuilds it in bulk
/// rather than letting the triggers append row by row, and the result still
/// matches a triggered build exactly.
///
/// Before this, `write_all` took the bulk path only into an empty index, so a
/// merge always paid the per-row triggered cost. Measured at 100k existing rows,
/// appending 10k triggered took 187ms against 149ms rebuilt, and the gap widens
/// with the size of the write.
#[test]
fn large_append_into_a_populated_index_rebuilds_it() {
    // Enough existing rows that the ratio rule is exercised rather than the
    // empty-index shortcut, and enough new rows to clear it.
    let existing: Vec<(i64, f64, f64)> = (1..=200).map(|i| (i, i as f64, -(i as f64))).collect();
    let gpkg = in_memory_with_points(&existing);
    let layer = gpkg.layer("pts").unwrap();
    layer.create_spatial_index().unwrap();
    assert_eq!(rtree_rows(gpkg.connection()).len(), 200);

    // 100 new rows against 200 existing is well over the 1-in-10 ratio.
    let new: Vec<NewFeature<Point<f64>>> = (201..=300)
        .map(|i| NewFeature::new(Point::new(i as f64, -(i as f64)), vec![Value::Null]).with_fid(i))
        .collect();
    layer
        .write_all_with(new, 0, BulkIndexOptions::with_threshold(1))
        .unwrap();

    let conn = gpkg.connection();
    assert_eq!(
        rtree_rows(conn).len(),
        300,
        "every row indexed after the merge"
    );
    assert_eq!(rtree_rows(conn), envelope_scan(conn));

    // Contents alone would look the same whichever path ran, so check the shape
    // too. A packed rebuild fills leaves to the 51-entry capacity, giving 6
    // leaves plus a root for 300 entries; letting the triggers append leaves
    // SQLite's half-filled split nodes, measured at 12 for the same rows.
    let nodes: i64 = conn
        .query_row("SELECT count(*) FROM rtree_pts_geom_node", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        nodes, 7,
        "expected a packed rebuild, not a triggered append"
    );

    assert_eq!(
        layer.spatial_index_status().unwrap(),
        geopackage::SpatialIndexStatus::Current,
        "the triggers must be back after a rebuild"
    );

    // A subsequent single write is still indexed, so the triggers really work.
    {
        let mut writer = layer.writer().unwrap();
        writer
            .insert(Some(301), &Point::new(301.0, -301.0), &[Value::Null])
            .unwrap();
        writer.commit().unwrap();
    }
    assert_eq!(rtree_rows(conn).len(), 301);
    assert_eq!(rtree_rows(conn), envelope_scan(conn));
}

/// A small append into a populated index keeps the triggered path: rebuilding
/// the whole index to add a handful of rows would cost more than it saves.
#[test]
fn small_append_into_a_populated_index_keeps_the_triggers() {
    let existing: Vec<(i64, f64, f64)> = (1..=200).map(|i| (i, i as f64, -(i as f64))).collect();
    let gpkg = in_memory_with_points(&existing);
    let layer = gpkg.layer("pts").unwrap();
    layer.create_spatial_index().unwrap();

    // 5 new rows against 200 existing is under the ratio.
    let new: Vec<NewFeature<Point<f64>>> = (201..=205)
        .map(|i| NewFeature::new(Point::new(i as f64, -(i as f64)), vec![Value::Null]).with_fid(i))
        .collect();
    layer
        .write_all_with(new, 0, BulkIndexOptions::with_threshold(1))
        .unwrap();

    let conn = gpkg.connection();
    assert_eq!(rtree_rows(conn).len(), 205);
    assert_eq!(rtree_rows(conn), envelope_scan(conn));
}

/// The bulk build must skip NULL and empty geometries exactly as the triggered
/// build does.
///
/// `create_skips_null_and_empty_geometries` covers this for the triggered path
/// only: it uses default options on a three-row table, which is far below the
/// bulk threshold. So the bulk path's own entry-set construction has never been
/// exercised against an empty or NULL geometry.
#[test]
fn bulk_build_skips_null_and_empty_geometries() {
    let gpkg = in_memory_with_points(&[(1, 10.0, 20.0), (4, -5.0, 7.0)]);
    let conn = gpkg.connection();
    conn.execute(
        "INSERT INTO pts (fid, name, geom) VALUES (2, 'null', NULL)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO pts (fid, name, geom) VALUES (3, 'empty', ?1)",
        [gpb_empty_point(4326)],
    )
    .unwrap();

    let layer = gpkg.layer("pts").unwrap();
    layer
        .create_spatial_index_with(BulkIndexOptions::always_bulk())
        .unwrap();

    let ids: Vec<i64> = rtree_rows(conn).into_iter().map(|r| r.0).collect();
    assert_eq!(
        ids,
        vec![1, 4],
        "NULL and empty geometries must not be indexed"
    );
    assert_eq!(rtree_rows(conn), envelope_scan(conn));
}

/// A geometry whose GPB header carries no envelope must still be indexed by the
/// bulk build, with bounds taken from the WKB body.
///
/// GDAL writes envelope-less point blobs, so this is the common shape of a
/// third-party file, and it is the case where the entry-set construction has to
/// fall back to a body traversal rather than reading the header.
#[test]
fn bulk_build_indexes_envelope_less_geometries() {
    let gpkg = in_memory_with_points(&[(1, 1.0, 1.0)]);
    let conn = gpkg.connection();
    // A point with a bare header: no envelope, so bounds come from the body.
    let mut blob = encode_header(4326, &Envelope::None, false, false);
    blob.push(1);
    blob.extend_from_slice(&1u32.to_le_bytes());
    blob.extend_from_slice(&12.5f64.to_le_bytes());
    blob.extend_from_slice(&(-3.25f64).to_le_bytes());
    conn.execute(
        "INSERT INTO pts (fid, name, geom) VALUES (2, 'bare', ?1)",
        [blob],
    )
    .unwrap();

    let layer = gpkg.layer("pts").unwrap();
    layer
        .create_spatial_index_with(BulkIndexOptions::always_bulk())
        .unwrap();

    assert_eq!(rtree_rows(conn), envelope_scan(conn));
    let row = rtree_rows(conn)
        .into_iter()
        .find(|r| r.0 == 2)
        .expect("envelope-less geometry should be indexed");
    assert!((row.1 - 12.5).abs() < 1e-6 && (row.3 - (-3.25)).abs() < 1e-6);
}
