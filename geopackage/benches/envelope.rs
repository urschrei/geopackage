//! Envelope-computation microbenchmarks.
//!
//! Isolates the per-coordinate envelope work that `encode_gpb_from_wkb` and
//! the `ST_MinX` family perform, away from SQLite, so a change to the
//! coordinate walk is measurable on its own. Two paths over the same ISO WKB
//! bodies:
//!
//! - `scan`: [`geopackage::core::curve::xy_envelope`], the direct byte walk
//!   (the only reader for non-linear types, and a candidate for the linear
//!   ones).
//! - `wkb_reader`: `wkb::reader::Wkb` plus
//!   [`geopackage::core::geometry::write_envelope`], the visitor-based path
//!   the write path uses for linear bodies today.
//!
//! Bodies come from the `benchdata/` GeoPackages when present (buildings:
//! many small polygons; gadm: very large multipolygons; rivers: linestrings),
//! capped at a per-dataset byte budget (`GPKG_BENCH_ENVELOPE_BYTES`, default
//! 64 MiB). Synthetic datasets (a 100k-vertex linestring in both byte orders,
//! and many 5-vertex polygons) always run, so the bench works without the
//! local data files.
//!
//! Run: `cargo bench -p geopackage --bench envelope`.

use std::hint::black_box;
use std::path::Path;
use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use geopackage::core::curve;
use geopackage::core::geometry::write_envelope;
use geopackage::core::gpb;
use wkb::reader::Wkb;

/// Per-dataset cap on the total bytes of WKB loaded from a benchdata file.
/// 64 MiB by default; override with `GPKG_BENCH_ENVELOPE_BYTES`.
fn byte_budget() -> usize {
    std::env::var("GPKG_BENCH_ENVELOPE_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(64 * 1024 * 1024)
}

/// A set of ISO WKB bodies (GPB headers already stripped) to scan per
/// iteration.
struct Dataset {
    label: String,
    bodies: Vec<Vec<u8>>,
    bytes: u64,
}

/// Loads up to the byte budget of WKB bodies from one feature table of a
/// GeoPackage, stripping the GPB header from each blob. Returns `None`, with
/// a note on stderr, when the file is absent (benchdata is not committed).
fn load(dir: &Path, file: &str, table: &str, label: &str) -> Option<Dataset> {
    let path = dir.join(file);
    if !path.exists() {
        eprintln!("skipping {label}: {} not found", path.display());
        return None;
    }
    Some(read_dataset(&path, table, label))
}

/// Reads WKB bodies from an existing benchdata file, up to the byte budget.
fn read_dataset(path: &Path, table: &str, label: &str) -> Dataset {
    let conn =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open benchdata file");
    let mut stmt = conn
        .prepare(&format!("SELECT geom FROM {table}"))
        .expect("prepare blob query");
    let mut rows = stmt.query([]).expect("query blobs");
    let budget = byte_budget();
    let mut bodies = Vec::new();
    let mut bytes = 0usize;
    while bytes < budget {
        let Some(row) = rows.next().expect("read row") else {
            break;
        };
        let blob: Vec<u8> = row.get(0).expect("geometry blob");
        let offset = gpb::body_offset(&blob).expect("valid GPB header");
        let body = blob.get(offset..).expect("body follows header").to_vec();
        bytes += body.len();
        bodies.push(body);
    }
    Dataset {
        label: label.to_owned(),
        bodies,
        bytes: u64::try_from(bytes).expect("byte count fits u64"),
    }
}

/// Encodes a little- or big-endian ISO WKB linestring with `n` XY vertices.
fn synthetic_linestring(n: u32, little_endian: bool) -> Vec<u8> {
    let mut out = Vec::new();
    encode_linestring(&mut out, n, little_endian);
    out
}

/// Appends one WKB linestring to `out`. Coordinates are spread over the
/// WGS 84 domain, as the write bench does, so the min/max fold sees updates.
fn encode_linestring(out: &mut Vec<u8>, n: u32, little_endian: bool) {
    out.push(u8::from(little_endian));
    for word in [2u32, n] {
        if little_endian {
            out.extend_from_slice(&word.to_le_bytes());
        } else {
            out.extend_from_slice(&word.to_be_bytes());
        }
    }
    for i in 0..n {
        let f = f64::from(i);
        let x = (f * 0.618_033_988_75).rem_euclid(360.0) - 180.0;
        let y = (f * 0.314_159_265_36).rem_euclid(180.0) - 90.0;
        for v in [x, y] {
            if little_endian {
                out.extend_from_slice(&v.to_le_bytes());
            } else {
                out.extend_from_slice(&v.to_be_bytes());
            }
        }
    }
}

/// Encodes a little-endian WKB polygon with one closed 5-vertex ring.
fn synthetic_polygon(i: u32) -> Vec<u8> {
    let f = f64::from(i);
    let x = (f * 0.618_033_988_75).rem_euclid(360.0) - 180.0;
    let y = (f * 0.314_159_265_36).rem_euclid(180.0) - 90.0;
    let mut out = vec![1u8];
    for word in [3u32, 1, 5] {
        out.extend_from_slice(&word.to_le_bytes());
    }
    let ring = [
        (x, y),
        (x + 0.01, y),
        (x + 0.01, y + 0.01),
        (x, y + 0.01),
        (x, y),
    ];
    for (vx, vy) in ring {
        out.extend_from_slice(&vx.to_le_bytes());
        out.extend_from_slice(&vy.to_le_bytes());
    }
    out
}

/// The synthetic datasets: work the bench can always do, sized to exercise
/// both the long-sequence fast path and per-body overhead.
fn synthetic_datasets() -> Vec<Dataset> {
    let mut sets = Vec::new();
    for (label, little_endian) in [("linestring_100k_le", true), ("linestring_100k_be", false)] {
        let body = synthetic_linestring(100_000, little_endian);
        let bytes = u64::try_from(body.len()).expect("body length fits u64");
        sets.push(Dataset {
            label: format!("synthetic/{label}"),
            bodies: vec![body],
            bytes,
        });
    }
    let bodies: Vec<Vec<u8>> = (0..100_000).map(synthetic_polygon).collect();
    let bytes = bodies.iter().map(|b| b.len()).sum::<usize>();
    sets.push(Dataset {
        label: "synthetic/polygon_5v".to_owned(),
        bodies,
        bytes: u64::try_from(bytes).expect("byte count fits u64"),
    });
    sets
}

fn bench_envelopes(c: &mut Criterion) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../benchdata");
    let mut datasets = synthetic_datasets();
    for (file, table, label) in [
        ("ca_buildings.gpkg", "buildings", "buildings"),
        ("gadm_noidx.gpkg", "gadm", "gadm"),
        ("hydrorivers.gpkg", "rivers", "rivers"),
    ] {
        if let Some(dataset) = load(&dir, file, table, label) {
            datasets.push(dataset);
        }
    }

    let mut group = c.benchmark_group("envelope");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(5));

    for dataset in &datasets {
        group.throughput(Throughput::Bytes(dataset.bytes));
        group.bench_function(format!("scan/{}", dataset.label), |b| {
            b.iter(|| {
                for body in &dataset.bodies {
                    let env = curve::xy_envelope(black_box(body)).expect("valid body");
                    black_box(env);
                }
            });
        });
        group.bench_function(format!("wkb_reader/{}", dataset.label), |b| {
            b.iter(|| {
                for body in &dataset.bodies {
                    let geometry = Wkb::try_new(black_box(body)).expect("readable body");
                    black_box(write_envelope(&geometry));
                }
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_envelopes);
criterion_main!(benches);
