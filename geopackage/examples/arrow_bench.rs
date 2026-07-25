//! Measurement tool for comparing this crate's `read_arrow` against GDAL's
//! Arrow read path on the same file (M3 acceptance criterion 3). Driven by
//! `scripts/compare_gdal_arrow.sh`.
//!
//! The counterpart of `scripts/gdal_arrow_read.c`, which does the same work
//! through GDAL. Both consume every batch of the whole layer and nothing else,
//! so the comparison is of the two read paths rather than of two programs.
//!
//! Subcommands:
//!
//! - `fixture <file> <rows>`: write the polygon layer with thirteen attributes,
//!   the shape GDAL's published benchmark uses.
//! - `noop <file>`: open and close. The startup floor to subtract, the
//!   counterpart of the C program's `noop`.
//! - `read <file> [reps] [threads]`: consume the whole Arrow stream, the timed
//!   operation. `threads` above one uses the parallel reader.
//!   `reps` repeats it, so the process runs long enough to attach a profiler to;
//!   the reported time is still for one pass.
//!
//! Every subcommand prints `<key>=<value>` lines, including `elapsed_ms`
//! measured inside the process.

use std::time::Instant;

use geo_types::{Geometry, LineString, Polygon};
use geopackage::arrow::ArrowReadOptions;
use geopackage::core::datetime::DateTime;
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{ColumnSpec, GeoPackage, GeometrySpec, NewFeature, TableSchemaBuilder, Value};

/// Deterministic coordinate for row `i` over the WGS 84 domain.
fn coord(i: usize) -> (f64, f64) {
    let f = i as f64;
    let x = (f * 0.618_033_988_75).rem_euclid(360.0) - 180.0;
    let y = (f * 0.314_159_265_36).rem_euclid(180.0) - 90.0;
    (x, y)
}

/// One of four category strings, indexed without panicking.
fn category(i: usize) -> String {
    ["alpha", "beta", "gamma", "delta"]
        .get(i % 4)
        .copied()
        .unwrap_or("alpha")
        .to_owned()
}

/// An 11-vertex closed ring about `(x, y)`, standing in for a building
/// footprint. Matches the `arrow` bench's geometry exactly.
fn footprint(x: f64, y: f64) -> Geometry<f64> {
    const VERTICES: usize = 10;
    let mut points: Vec<(f64, f64)> = (0..VERTICES)
        .map(|v| {
            let angle = (v as f64) * std::f64::consts::TAU / (VERTICES as f64);
            let radius = 0.000_5 + 0.000_2 * ((v % 3) as f64);
            (x + radius * angle.cos(), y + radius * angle.sin())
        })
        .collect();
    if let Some(&first) = points.first() {
        points.push(first);
    }
    Geometry::Polygon(Polygon::new(LineString::from(points), Vec::new()))
}

fn fixture(path: &str, rows: usize) -> Result<(), Box<dyn std::error::Error>> {
    let start = Instant::now();
    let gpkg = GeoPackage::create(path)?;
    let builder = TableSchemaBuilder::new("features")
        .column(ColumnSpec::new("name", ColumnType::Text(None)))
        .column(ColumnSpec::new("category", ColumnType::Text(None)))
        .column(ColumnSpec::new("source", ColumnType::Text(None)))
        .column(ColumnSpec::new("quality", ColumnType::Text(None)))
        .column(ColumnSpec::new("count", ColumnType::Integer))
        .column(ColumnSpec::new("code", ColumnType::MediumInt))
        .column(ColumnSpec::new("floors", ColumnType::SmallInt))
        .column(ColumnSpec::new("built_year", ColumnType::SmallInt))
        .column(ColumnSpec::new("area", ColumnType::Double))
        .column(ColumnSpec::new("height", ColumnType::Float))
        .column(ColumnSpec::new("active", ColumnType::Boolean))
        .column(ColumnSpec::new("surveyed", ColumnType::DateTime))
        .column(ColumnSpec::new("payload", ColumnType::Blob(None)))
        .geometry(GeometrySpec::new(GeometryType::Polygon, 4326));
    let layer = gpkg.create_layer(&builder)?;

    let stamp = DateTime::parse_strict("2026-07-24T12:34:56.789Z")?;
    let features: Vec<NewFeature<Geometry<f64>>> = (0..rows)
        .map(|i| {
            let (x, y) = coord(i);
            let f = i as f64;
            NewFeature::new(
                footprint(x, y),
                vec![
                    Value::Text(format!("building {i}")),
                    Value::Text(category(i)),
                    Value::Text("survey".to_owned()),
                    Value::Text(category(i + 1)),
                    Value::Integer(i as i64),
                    Value::Integer((i % 30_000) as i64),
                    Value::Integer((i % 40) as i64),
                    Value::Integer(1900 + (i % 125) as i64),
                    Value::Float(f * 1.5),
                    Value::Float(f * 0.25),
                    Value::Boolean(i % 2 == 0),
                    Value::DateTime(stamp),
                    Value::Blob(vec![0xde, 0xad, 0xbe, 0xef]),
                ],
            )
        })
        .collect();
    layer.write_all(features, 0)?;
    gpkg.close()?;
    println!("elapsed_ms={:.3}", start.elapsed().as_secs_f64() * 1000.0);
    println!("rows={rows}");
    Ok(())
}

fn noop(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let start = Instant::now();
    let gpkg = GeoPackage::open(path)?;
    drop(gpkg);
    println!("elapsed_ms={:.3}", start.elapsed().as_secs_f64() * 1000.0);
    Ok(())
}

fn read(path: &str, reps: usize, threads: usize) -> Result<(), Box<dyn std::error::Error>> {
    let gpkg = GeoPackage::open(path)?;
    let layer = gpkg.layer("features")?;
    let options = ArrowReadOptions::default().with_threads(threads);
    // Written out rather than behind a closure: the reader borrows the layer,
    // which a closure's return type cannot express.
    let start = Instant::now();
    for _ in 1..reps {
        let batches = if threads > 1 {
            layer.read_arrow_parallel(options)?
        } else {
            layer.read_arrow(options)?
        };
        for batch in batches {
            std::hint::black_box(batch?.num_rows());
        }
    }
    let batches = if threads > 1 {
        layer.read_arrow_parallel(options)?
    } else {
        layer.read_arrow(options)?
    };
    let mut rows = 0usize;
    let mut count = 0usize;
    // Taken from a batch rather than from the reader's schema, so this example
    // needs no direct dependency on `arrow-array` for the trait.
    let mut columns = 0usize;
    for batch in batches {
        let batch = batch?;
        rows += batch.num_rows();
        columns = batch.num_columns();
        count += 1;
    }
    let elapsed = start.elapsed();

    println!(
        "elapsed_ms={:.3}",
        elapsed.as_secs_f64() * 1000.0 / reps as f64
    );
    println!("rows={rows}");
    println!("batches={count}");
    println!("columns={columns}");
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let usage = "usage: arrow_bench <fixture <file> <rows>|noop <file>|read <file>>";
    let command = args.get(1).map(String::as_str).unwrap_or("");
    let path = args.get(2).map(String::as_str).unwrap_or("");
    match command {
        "fixture" => {
            let rows: usize = args.get(3).ok_or(usage)?.parse()?;
            fixture(path, rows)
        }
        "noop" => noop(path),
        "read" => {
            let reps: usize = args.get(3).map_or(Ok(1), |r| r.parse())?;
            let threads: usize = args.get(4).map_or(Ok(1), |t| t.parse())?;
            read(path, reps.max(1), threads.max(1))
        }
        _ => Err(usage.into()),
    }
}
