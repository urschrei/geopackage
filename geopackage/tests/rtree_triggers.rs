//! End-to-end proof of the M0 stack: GPB encoding (core) + registered ST_*
//! functions + the 1.4 trigger set maintaining a real SQLite rtree,
//! including the UPSERT case that corrupted pre-1.4 GeoPackages.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geopackage::GeoPackage;
use geopackage::core::gpb::{Envelope, encode_header};
use geopackage::core::triggers;

/// GPB blob for an XY point with an XY envelope.
fn gpb_point(srs_id: i32, x: f64, y: f64) -> Vec<u8> {
    let mut blob = encode_header(srs_id, &Envelope::Xy([x, x, y, y]), false, false);
    blob.push(1); // WKB little-endian
    blob.extend_from_slice(&1u32.to_le_bytes()); // Point
    blob.extend_from_slice(&x.to_le_bytes());
    blob.extend_from_slice(&y.to_le_bytes());
    blob
}

/// GPB blob for an empty point (empty flag + NaN coordinates, no envelope).
fn gpb_empty_point(srs_id: i32) -> Vec<u8> {
    let mut blob = encode_header(srs_id, &Envelope::None, true, false);
    blob.push(1);
    blob.extend_from_slice(&1u32.to_le_bytes());
    blob.extend_from_slice(&f64::NAN.to_le_bytes());
    blob.extend_from_slice(&f64::NAN.to_le_bytes());
    blob
}

/// GPB blob for a LINESTRING with no header envelope (little-endian WKB).
///
/// M0 could not index this: its `ST_*` fallback handled points only, so the
/// insert trigger's `ST_MinX`/… calls failed. M1's full traversal fixes it.
fn gpb_linestring_no_envelope(srs_id: i32, pts: &[(f64, f64)]) -> Vec<u8> {
    let mut blob = encode_header(srs_id, &Envelope::None, false, false);
    blob.push(1); // WKB little-endian
    blob.extend_from_slice(&2u32.to_le_bytes()); // LineString
    blob.extend_from_slice(&(pts.len() as u32).to_le_bytes());
    for (x, y) in pts {
        blob.extend_from_slice(&x.to_le_bytes());
        blob.extend_from_slice(&y.to_le_bytes());
    }
    blob
}

fn setup() -> (tempfile::TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    let conn = gpkg.connection();
    conn.execute_batch("CREATE TABLE pts (fid INTEGER PRIMARY KEY, geom BLOB, name TEXT);")
        .unwrap();
    conn.execute_batch(&triggers::create_rtree_table_sql("pts", "geom").unwrap())
        .unwrap();
    for sql in triggers::create_triggers_sql("pts", "geom", "fid").unwrap() {
        conn.execute_batch(&sql).unwrap();
    }
    (dir, gpkg)
}

fn rtree_rows(gpkg: &GeoPackage) -> Vec<(i64, f64, f64)> {
    let conn = gpkg.connection();
    let mut stmt = conn
        .prepare("SELECT id, minx, maxy FROM rtree_pts_geom ORDER BY id")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn triggers_maintain_index() {
    let (_dir, gpkg) = setup();
    let conn = gpkg.connection();

    // insert trigger
    conn.execute(
        "INSERT INTO pts (fid, geom, name) VALUES (1, ?1, 'a')",
        [gpb_point(4326, 10.0, 20.0)],
    )
    .unwrap();
    assert_eq!(rtree_rows(&gpkg).len(), 1);
    let (id, minx, _) = rtree_rows(&gpkg)[0];
    assert_eq!(id, 1);
    assert!((minx - 10.0).abs() < 1e-6);

    // update6: non-empty -> non-empty updates the entry
    conn.execute(
        "UPDATE pts SET geom = ?1 WHERE fid = 1",
        [gpb_point(4326, -5.0, 7.0)],
    )
    .unwrap();
    let (_, minx, _) = rtree_rows(&gpkg)[0];
    assert!((minx + 5.0).abs() < 1e-6);

    // update2: non-empty -> NULL removes the entry
    conn.execute("UPDATE pts SET geom = NULL WHERE fid = 1", [])
        .unwrap();
    assert!(rtree_rows(&gpkg).is_empty());

    // update7: NULL -> non-empty inserts the entry
    conn.execute(
        "UPDATE pts SET geom = ?1 WHERE fid = 1",
        [gpb_point(4326, 1.0, 2.0)],
    )
    .unwrap();
    assert_eq!(rtree_rows(&gpkg).len(), 1);

    // update5: rowid change with non-empty geometry moves the entry
    conn.execute("UPDATE pts SET fid = 42 WHERE fid = 1", [])
        .unwrap();
    assert_eq!(rtree_rows(&gpkg)[0].0, 42);

    // delete trigger
    conn.execute("DELETE FROM pts WHERE fid = 42", []).unwrap();
    assert!(rtree_rows(&gpkg).is_empty());

    // empty geometry is never indexed
    conn.execute(
        "INSERT INTO pts (fid, geom) VALUES (7, ?1)",
        [gpb_empty_point(4326)],
    )
    .unwrap();
    assert!(rtree_rows(&gpkg).is_empty());
}

#[test]
fn upsert_works_with_1_4_triggers() {
    // The pre-1.4 update1 trigger made "INSERT ... ON CONFLICT DO UPDATE"
    // fail; the update6/update7 replacements fixed it (spec 1.4, Annex F.3).
    let (_dir, gpkg) = setup();
    let conn = gpkg.connection();
    conn.execute(
        "INSERT INTO pts (fid, geom, name) VALUES (1, ?1, 'a')",
        [gpb_point(4326, 1.0, 1.0)],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO pts (fid, geom, name) VALUES (1, ?1, 'b') \
         ON CONFLICT (fid) DO UPDATE SET geom = excluded.geom, name = excluded.name",
        [gpb_point(4326, 9.0, 9.0)],
    )
    .unwrap();
    let rows = rtree_rows(&gpkg);
    assert_eq!(rows.len(), 1);
    assert!((rows[0].1 - 9.0).abs() < 1e-6);

    let name: String = conn
        .query_row("SELECT name FROM pts WHERE fid = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(name, "b");
}

#[test]
fn spatial_query_via_rtree() {
    let (_dir, gpkg) = setup();
    let conn = gpkg.connection();
    for (fid, x) in [(1i64, 0.0f64), (2, 50.0), (3, 100.0)] {
        conn.execute(
            "INSERT INTO pts (fid, geom) VALUES (?1, ?2)",
            rusqlite::params![fid, gpb_point(4326, x, x)],
        )
        .unwrap();
    }
    let hits: Vec<i64> = {
        let mut stmt = conn
            .prepare(
                "SELECT id FROM rtree_pts_geom \
                 WHERE minx <= 60 AND maxx >= 40 AND miny <= 60 AND maxy >= 40",
            )
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert_eq!(hits, vec![2]);
}

#[test]
fn envelopeless_non_point_insert_indexes_via_traversal() {
    // The M0 acceptance gap: an envelope-less non-point geometry inserted into
    // an rtree-indexed table. The insert trigger calls ST_MinX/ST_MaxX/…,
    // which now traverse the WKB body to bound it. End-to-end proof.
    let (_dir, gpkg) = setup();
    let conn = gpkg.connection();
    conn.execute(
        "INSERT INTO pts (fid, geom, name) VALUES (5, ?1, 'line')",
        [gpb_linestring_no_envelope(
            4326,
            &[(2.0, 3.0), (8.0, -1.0), (5.0, 9.0)],
        )],
    )
    .unwrap();
    let rows = rtree_rows(&gpkg);
    assert_eq!(rows.len(), 1);
    let (id, minx, maxy) = rows[0];
    assert_eq!(id, 5);
    assert!((minx - 2.0).abs() < 1e-6, "minx from traversal");
    assert!((maxy - 9.0).abs() < 1e-6, "maxy from traversal");
}
