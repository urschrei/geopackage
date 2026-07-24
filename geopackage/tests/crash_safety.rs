//! Crash safety (design decision D4): a child process opens the GeoPackage,
//! commits one write, holds a second write uncommitted, and is `SIGKILL`ed
//! mid-flight; the parent reopens and asserts `PRAGMA integrity_check` is `ok`,
//! the committed rows survive, the uncommitted row is gone, and the RTree index
//! is not desynchronised.
//!
//! The child is this same test binary, re-invoked to run only the ignored
//! `crash_child` entry point with the role/path/marker passed through the
//! environment. Parent and child synchronise on a marker file the child writes
//! once its transactions are in place, not on a timed sleep, so the kill lands
//! deterministically after the committed write is durable and the uncommitted
//! write is open.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-unwrap-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use geo_types::Point;
use geopackage::core::types::GeometryType;
use geopackage::{
    GeoPackage, GeometrySpec, JournalMode, NewFeature, OpenOptions, TableSchemaBuilder,
};

const ROLE_ENV: &str = "GEOPACKAGE_CRASH_ROLE";
const PATH_ENV: &str = "GEOPACKAGE_CRASH_PATH";
const MARKER_ENV: &str = "GEOPACKAGE_CRASH_MARKER";

/// Rows the bulk-write child attempts: enough that a kill lands inside the
/// write rather than after it has finished.
const BULK_ROWS: i64 = 400_000;

/// The child entry point, re-invoked as a separate process. It only acts when
/// [`ROLE_ENV`] is set (the parent sets it); the `#[ignore]` keeps it out of an
/// ordinary test run.
#[test]
#[ignore = "re-invoked as a child process by the crash-safety parent tests"]
fn crash_child() {
    let Ok(role) = std::env::var(ROLE_ENV) else {
        // Not a child invocation.
        return;
    };
    let path = std::env::var(PATH_ENV).expect("child needs a database path");
    let marker = std::env::var(MARKER_ENV).expect("child needs a marker path");
    let journal = match role.as_str() {
        "delete" => None,
        "wal" => Some(JournalMode::Wal),
        "bulk" => {
            bulk_child_body(&path, &marker);
        }
        other => panic!("unknown crash role {other:?}"),
    };
    child_body(&path, &marker, journal);
}

/// The bulk-write child: signal readiness, then start a large bulk `write_all`
/// and be killed part-way through it. Never returns.
///
/// The whole bulk path is one transaction (triggers dropped, rows inserted,
/// index rebuilt, triggers reinstalled), so wherever the kill lands the parent
/// must find either none of the write or all of it, and never an index left
/// desynchronised from the table.
fn bulk_child_body(path: &str, marker: &str) -> ! {
    let gpkg = GeoPackage::open(path).expect("child opens the gpkg");
    let layer = gpkg.layer("pts").expect("child opens the layer");

    let features: Vec<NewFeature<Point<f64>>> = (1..=BULK_ROWS)
        .map(|i| {
            let f = i as f64;
            NewFeature::new(Point::new(f % 180.0, f % 90.0), Vec::new()).with_fid(i)
        })
        .collect();

    signal_ready(marker);
    // Large enough that the kill lands inside the write rather than after it.
    // The result is irrelevant: this process is about to be killed, and if it
    // somehow survives to return, the loop below blocks anyway.
    drop(layer.write_all(features, 0));

    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Open the layer, commit fid 2 (must survive the kill), then open and hold an
/// uncommitted insert of fid 3 (must not survive). Signal readiness, then block
/// until killed. Never returns.
fn child_body(path: &str, marker: &str, journal: Option<JournalMode>) -> ! {
    let options = OpenOptions::new();
    let options = match journal {
        Some(mode) => options.journal_mode(mode),
        None => options,
    };
    let gpkg = options.open(path).expect("child opens the gpkg");
    let layer = gpkg.layer("pts").expect("child opens the layer");

    // A fully committed write: durable, must survive the kill.
    let mut committed = layer.writer().expect("child begins a writer");
    committed
        .insert(Some(2), &Point::new(2.0, 2.0), &[])
        .expect("child inserts fid 2");
    committed.commit().expect("child commits fid 2");

    // An uncommitted write held open across the kill: must roll back. The
    // writer owns its transaction and is never committed nor dropped (the loop
    // below diverges), so on SIGKILL the transaction is abandoned.
    let mut pending = layer.writer().expect("child begins a second writer");
    pending
        .insert(Some(3), &Point::new(3.0, 3.0), &[])
        .expect("child inserts fid 3 (uncommitted)");

    signal_ready(marker);

    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Write the readiness marker the parent polls for. Full write + flush so the
/// file is present and non-empty the instant the parent sees it.
fn signal_ready(marker: &str) {
    let mut file = std::fs::File::create(marker).expect("child creates the marker");
    file.write_all(b"ready").expect("child writes the marker");
    file.flush().expect("child flushes the marker");
}

/// Build an indexed point layer with one committed feature (fid 1), then run the
/// crash scenario for `role` and assert the post-crash invariants.
fn run_crash_case(role: &str) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crash.gpkg");
    let marker = dir.path().join("child.ready");

    {
        let gpkg = GeoPackage::create(&path).unwrap();
        let layer = gpkg
            .create_layer(
                &TableSchemaBuilder::new("pts")
                    .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
            )
            .unwrap();
        layer.create_spatial_index().unwrap();
        let mut writer = layer.writer().unwrap();
        writer.insert(Some(1), &Point::new(1.0, 1.0), &[]).unwrap();
        writer.commit().unwrap();
    }

    let exe = std::env::current_exe().unwrap();
    let mut child = Command::new(exe)
        .args(["--exact", "crash_child", "--ignored"])
        .env(ROLE_ENV, role)
        .env(PATH_ENV, &path)
        .env(MARKER_ENV, &marker)
        .spawn()
        .unwrap();

    // Deterministic: wait for the child's marker, then kill it mid-flight.
    wait_for_marker(&marker, Duration::from_secs(30));
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(
        !status.success(),
        "child should have been killed, not exited"
    );

    // Reopen and check durability.
    let gpkg = GeoPackage::open(&path).unwrap();
    let integrity: String = gpkg
        .connection()
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(integrity, "ok", "integrity_check after crash ({role})");

    let fids = table_fids(&gpkg);
    assert!(fids.contains(&1), "committed parent row (1) lost ({role})");
    assert!(fids.contains(&2), "committed child row (2) lost ({role})");
    assert!(
        !fids.contains(&3),
        "uncommitted child row (3) survived ({role})"
    );

    // The RTree must agree exactly with the table: a committed write maintains
    // the index atomically, so a crash cannot desync it.
    assert_eq!(
        rtree_ids(&gpkg),
        fids,
        "rtree desynced from the table after crash ({role})"
    );

    // The layer is a healthy, current index, so no repair is needed for this path.
    let layer = gpkg.layer("pts").unwrap();
    assert_eq!(
        layer.spatial_index_status().unwrap(),
        geopackage::SpatialIndexStatus::Current,
        "index should be Current after a triggered-write crash ({role})"
    );
}

fn wait_for_marker(marker: &Path, timeout: Duration) {
    let start = Instant::now();
    while !marker.exists() {
        assert!(
            start.elapsed() < timeout,
            "child never signalled readiness within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The sorted `fid` values in the `pts` table.
fn table_fids(gpkg: &GeoPackage) -> Vec<i64> {
    let conn = gpkg.connection();
    let mut stmt = conn.prepare("SELECT fid FROM pts ORDER BY fid").unwrap();
    let mut ids: Vec<i64> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    ids.sort_unstable();
    ids
}

/// The sorted `id` values in the RTree shadow table.
fn rtree_ids(gpkg: &GeoPackage) -> Vec<i64> {
    let conn = gpkg.connection();
    let mut stmt = conn
        .prepare("SELECT id FROM rtree_pts_geom ORDER BY id")
        .unwrap();
    let mut ids: Vec<i64> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    ids.sort_unstable();
    ids
}

#[test]
fn crash_mid_transaction_delete_mode() {
    run_crash_case("delete");
}

#[test]
fn crash_mid_transaction_wal_mode() {
    run_crash_case("wal");
}

/// Killing a process part-way through a bulk `write_all` leaves either none of
/// the write or all of it, and never an index desynchronised from the table.
///
/// This is an end-to-end check against a real `SIGKILL`, not a proof that the
/// bulk path is atomic. The kill lands wherever it lands, and in practice that
/// is during the row inserts, which even the old two-transaction arrangement
/// rolled back cleanly. The atomicity itself is pinned deterministically by
/// `writer::tests::failed_bulk_write_rolls_back_rows_and_index`, which forces a
/// failure in the index build specifically, after the rows are staged: that is
/// the window this path used to have.
#[test]
fn crash_mid_bulk_write_leaves_no_stale_index() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bulk_crash.gpkg");
    let marker = dir.path().join("child.ready");

    // An indexed layer with an empty table, which is what makes `write_all`
    // take the bulk path.
    {
        let gpkg = GeoPackage::create(&path).unwrap();
        let layer = gpkg
            .create_layer(
                &TableSchemaBuilder::new("pts")
                    .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
            )
            .unwrap();
        layer.create_spatial_index().unwrap();
        gpkg.close().unwrap();
    }

    let exe = std::env::current_exe().unwrap();
    let mut child = Command::new(exe)
        .args(["--exact", "crash_child", "--ignored"])
        .env(ROLE_ENV, "bulk")
        .env(PATH_ENV, &path)
        .env(MARKER_ENV, &marker)
        .spawn()
        .unwrap();

    wait_for_marker(&marker, Duration::from_secs(30));
    child.kill().unwrap();
    let status = child.wait().unwrap();
    // Terminated by the signal, not exited on its own. Without this the test
    // could pass vacuously: a child that failed to start the write at all would
    // also leave a consistent file.
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            status.signal(),
            Some(9),
            "child exited on its own ({:?}) instead of being killed mid-write",
            status.code()
        );
    }
    assert!(
        !status.success(),
        "child should have been killed, not exited"
    );

    let gpkg = GeoPackage::open(&path).unwrap();
    let integrity: String = gpkg
        .connection()
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(integrity, "ok", "integrity_check after a mid-bulk crash");

    // All or nothing: the transaction either committed or rolled back whole.
    let fids = table_fids(&gpkg);
    assert!(
        fids.is_empty() || fids.len() == usize::try_from(BULK_ROWS).unwrap(),
        "partial bulk write survived: {} rows",
        fids.len()
    );

    // Whatever landed, the index agrees with the table and is usable without
    // a repair. A `Stale` result here is the regression this guards against.
    let layer = gpkg.layer("pts").unwrap();
    assert_eq!(
        layer.spatial_index_status().unwrap(),
        geopackage::SpatialIndexStatus::Current,
        "index left desynchronised by a mid-bulk crash"
    );
    assert_eq!(
        rtree_ids(&gpkg),
        fids,
        "rtree disagrees with the table after a mid-bulk crash"
    );
}
