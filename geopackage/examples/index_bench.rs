//! Measurement tool for comparing this crate's RTree build against GDAL's on
//! the same operation and the same file. Driven by
//! `scripts/compare_gdal_index.sh`.
//!
//! The point is to remove the asymmetry in the original comparison, which timed
//! `ogr2ogr` copying a whole GeoPackage against our write-only path: GDAL was
//! reading a source file that we were not. GDAL exposes `CreateSpatialIndex` and
//! `DisableSpatialIndex` as SQL functions on an existing GeoPackage, so both
//! implementations can be asked to build an index over the same rows of the same
//! file, and nothing else.
//!
//! Subcommands:
//!
//! - `fixture <file> <rows> <dist>`: write an unindexed point layer. `dist` is
//!   `uniform` or `clustered`.
//! - `noop <file>`: open and close. The startup floor to subtract from a timed
//!   build, the counterpart of timing `ogrinfo -sql "SELECT 1"`.
//! - `build <file>`: build the spatial index, the timed operation.
//! - `stats <file>`: node count, tree depth and node bytes of an existing index.
//! - `query <file> <reps>`: median latency over a fixed set of boxes.
//! - `version`: the SQLite version this crate is linked against.
//!
//! Every subcommand prints `<key>=<value>` lines, including `elapsed_ms`
//! measured inside the process, so the script can report both internal time and
//! externally observed wall time.

use std::time::Instant;

use geo_types::{Geometry, Point};
use geopackage::core::types::GeometryType;
use geopackage::{BoundingBox, GeoPackage, GeometrySpec, NewFeature, TableSchemaBuilder};

/// Deterministic coordinates: `uniform` spreads over the domain, `clustered`
/// puts 90% into twelve dense blobs, which is closer to real feature data.
fn coord(i: usize, clustered: bool) -> (f64, f64) {
    let f = i as f64;
    if !clustered || i.is_multiple_of(10) {
        return (
            (f * 0.618_033_988_75).rem_euclid(360.0) - 180.0,
            (f * 0.314_159_265_36).rem_euclid(180.0) - 90.0,
        );
    }
    let cluster = i % 12;
    let cx = -150.0 + (cluster as f64) * 25.0;
    let cy = -60.0 + ((cluster * 7) % 11) as f64 * 10.0;
    (
        cx + ((f * 0.113).rem_euclid(1.0) - 0.5) * 1.5,
        cy + ((f * 0.271).rem_euclid(1.0) - 0.5) * 1.5,
    )
}

/// The boxes used by `query`, spanning three selectivities.
fn query_boxes() -> Vec<(&'static str, BoundingBox)> {
    vec![
        ("tiny", BoundingBox::new(-1.0, -1.0, 1.0, 1.0)),
        ("small", BoundingBox::new(-9.0, -4.5, 9.0, 4.5)),
        ("wide", BoundingBox::new(-160.0, -70.0, -60.0, 20.0)),
    ]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first().map(String::as_str) else {
        eprintln!("usage: index_bench <fixture|noop|build|stats|query> <file> [...]");
        std::process::exit(2);
    };
    if command == "version" {
        let conn = rusqlite::Connection::open_in_memory()?;
        let version: String = conn.query_row("SELECT sqlite_version()", [], |r| r.get(0))?;
        println!("sqlite={version}");
        return Ok(());
    }

    let path = args.get(1).ok_or("a file path is required")?.clone();

    match command {
        "fixture" => {
            let rows: usize = args.get(2).ok_or("row count required")?.parse()?;
            let clustered = args.get(3).map(String::as_str) == Some("clustered");
            let gpkg = GeoPackage::create(&path)?;
            let layer = gpkg.create_layer(
                &TableSchemaBuilder::new("pts")
                    .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
            )?;
            let features: Vec<NewFeature<Geometry<f64>>> = (0..rows)
                .map(|i| {
                    let (x, y) = coord(i, clustered);
                    NewFeature::new(Geometry::Point(Point::new(x, y)), Vec::new())
                })
                .collect();
            layer.write_all(features, 0)?;
            gpkg.close()?;
            println!("rows={rows}");
        }
        "noop" => {
            let started = Instant::now();
            let gpkg = GeoPackage::open(&path)?;
            let _ = gpkg.layer("pts")?;
            gpkg.close()?;
            println!("elapsed_ms={:.3}", started.elapsed().as_secs_f64() * 1000.0);
        }
        "build" => {
            let started = Instant::now();
            let gpkg = GeoPackage::open(&path)?;
            gpkg.layer("pts")?.create_spatial_index()?;
            gpkg.close()?;
            println!("elapsed_ms={:.3}", started.elapsed().as_secs_f64() * 1000.0);
        }
        "stats" => {
            let gpkg = GeoPackage::open(&path)?;
            let conn = gpkg.connection();
            let nodes: i64 =
                conn.query_row("SELECT count(*) FROM rtree_pts_geom_node", [], |r| r.get(0))?;
            let bytes: i64 = conn.query_row(
                "SELECT coalesce(sum(length(data)), 0) FROM rtree_pts_geom_node",
                [],
                |r| r.get(0),
            )?;
            let entries: i64 =
                conn.query_row("SELECT count(*) FROM rtree_pts_geom", [], |r| r.get(0))?;
            let root: Vec<u8> = conn.query_row(
                "SELECT data FROM rtree_pts_geom_node WHERE nodeno = 1",
                [],
                |r| r.get(0),
            )?;
            let depth = u16::from_be_bytes([
                root.first().copied().unwrap_or(0),
                root.get(1).copied().unwrap_or(0),
            ]);
            let report: String =
                conn.query_row("SELECT rtreecheck('rtree_pts_geom')", [], |r| r.get(0))?;
            println!("nodes={nodes}");
            println!("node_bytes={bytes}");
            println!("entries={entries}");
            println!("depth={depth}");
            println!("rtreecheck={report}");
        }
        "query" => {
            let reps: usize = args.get(2).map_or(Ok(20), |v| v.parse())?;
            let gpkg = GeoPackage::open(&path)?;
            let layer = gpkg.layer("pts")?;
            println!("indexed={}", layer.has_spatial_index()?);
            for (label, bbox) in query_boxes() {
                // One untimed pass, then the timed repetitions.
                let hits = layer.features_in(bbox)?.len();
                let mut samples: Vec<f64> = Vec::with_capacity(reps);
                for _ in 0..reps {
                    let started = Instant::now();
                    let found = layer.features_in(bbox)?.len();
                    samples.push(started.elapsed().as_secs_f64() * 1000.0);
                    if found != hits {
                        return Err(format!(
                            "hit count varied between repetitions: {found} then {hits}"
                        )
                        .into());
                    }
                }
                samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let median = samples.get(samples.len() / 2).copied().unwrap_or(f64::NAN);
                println!("query_{label}_median_ms={median:.4}");
                println!("query_{label}_hits={hits}");
            }
        }
        other => {
            eprintln!("unknown subcommand {other:?}");
            std::process::exit(2);
        }
    }
    Ok(())
}
