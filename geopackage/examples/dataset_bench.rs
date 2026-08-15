//! Measurement tool for the published figures in the README: the read, write,
//! index-build and query paths timed over real third-party GeoPackages rather
//! than over generated fixtures. Driven by `scripts/bench_datasets.sh`.
//!
//! The generated-fixture benches in `geopackage/benches` answer "did this
//! change make the code faster", which wants a fixed, controlled shape. This
//! answers a different question: what a caller should expect from a file they
//! actually have. Real data differs from the fixtures in the ways that matter
//! for these paths, chiefly vertex counts per geometry and how unevenly the
//! features are spread, so it is measured rather than extrapolated.
//!
//! Every subcommand takes an explicit layer name and prints `<key>=<value>`
//! lines, including `elapsed_ms` measured inside the process.
//!
//! Subcommands:
//!
//! - `info <file> <layer>`: row count, column count, geometry type, whether a
//!   spatial index is present, and the layer's extent. No timing; this is what
//!   the dataset column of the README table is built from.
//! - `scan <file> <layer> [mode] [reps]`: full scalar read through
//!   [`Layer::cursor`], which streams rather than materialising the result set.
//!   `mode` selects how far each geometry is taken: `values` stops at the
//!   attributes and never touches the blob, `geom` parses the GPB header and
//!   validates the WKB body, and `geo` additionally builds an owned
//!   `geo_types::Geometry`. Default `geom`.
//! - `arrow <file> <layer> [reps] [threads]`: consume every Arrow batch.
//!   `threads` is passed to the reader as given, so `0` is its automatic choice
//!   and `1` pins it to this thread; default `0`.
//! - `index <file> <layer>`: build the spatial index, the timed operation. The
//!   layer must not already have one, so the script hands this a fresh copy.
//! - `bbox <file> <layer> <minx> <miny> <maxx> <maxy> [reps]`: a bounding-box
//!   query, counting the features it returns.
//! - `write <src> <srclayer> <dst> [index:yes|no]`: read the source's batches
//!   into memory (untimed), then time writing them into a fresh file. The read
//!   is outside the measurement deliberately: timing both would produce a
//!   figure that says nothing about either.
//! - `noop <file>`: open and close, the startup floor to subtract from an
//!   externally timed run.

use std::time::Instant;

use geopackage::arrow::ArrowReadOptions;
use geopackage::{BoundingBox, GeoPackage, TableSchemaBuilder};

/// Print the elapsed time of one repetition, given the total and the count.
fn report(elapsed: std::time::Duration, reps: usize) {
    println!(
        "elapsed_ms={:.3}",
        elapsed.as_secs_f64() * 1000.0 / reps as f64
    );
}

fn info(path: &str, layer_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let gpkg = GeoPackage::open(path)?;
    let layer = gpkg.layer(layer_name)?;

    // Counted with SQL rather than by iterating: the row count is a property of
    // the dataset being described, not something this tool is timing.
    let rows: i64 = gpkg.connection().query_row(
        &format!(
            "SELECT COUNT(*) FROM {}",
            geopackage::core::ident::quote(layer_name)?
        ),
        [],
        |row| row.get(0),
    )?;

    println!("rows={rows}");
    println!("columns={}", layer.schema().columns.len());
    match layer.geometry_column() {
        Some(column) => {
            println!("geometry_type={:?}", column.geometry_type);
            println!("srs_id={}", column.srs_id);
        }
        None => println!("geometry_type=none"),
    }
    println!("spatial_index={}", layer.has_spatial_index()?);
    println!("file_bytes={}", std::fs::metadata(path)?.len());

    // Informational in the spec, and only used here to pick query boxes that
    // fall inside the data, so a file that leaves it NULL is not an error.
    if let Some(entry) = gpkg
        .contents()?
        .into_iter()
        .find(|entry| entry.table_name == layer_name)
        && let (Some(min_x), Some(min_y), Some(max_x), Some(max_y)) =
            (entry.min_x, entry.min_y, entry.max_x, entry.max_y)
    {
        println!("extent={min_x} {min_y} {max_x} {max_y}");
    }
    Ok(())
}

fn scan(
    path: &str,
    layer_name: &str,
    mode: &str,
    reps: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let gpkg = GeoPackage::open(path)?;
    let layer = gpkg.layer(layer_name)?;
    let mut cursor = layer.cursor()?;

    let mut rows = 0usize;
    let mut geometries = 0usize;
    let start = Instant::now();
    for _ in 0..reps {
        rows = 0;
        geometries = 0;
        for feature in cursor.features()? {
            let feature = feature?;
            rows += 1;
            match mode {
                // Attributes only: the geometry blob is never looked at.
                "values" => {
                    std::hint::black_box(feature.values().len());
                }
                // Parse the GPB header and validate the WKB body.
                "geom" => {
                    if let Some(geometry) = feature.geometry()? {
                        geometries += 1;
                        std::hint::black_box(geometry.wkb_body().len());
                    }
                }
                // As `geom`, plus building an owned geo-types geometry.
                _ => {
                    if let Some(geometry) = feature.geometry()?
                        && let Some(geo) = geometry.to_geo()
                    {
                        geometries += 1;
                        std::hint::black_box(&geo);
                    }
                }
            }
        }
    }
    let elapsed = start.elapsed();

    report(elapsed, reps);
    println!("rows={rows}");
    println!("geometries={geometries}");
    println!("mode={mode}");
    Ok(())
}

fn arrow(
    path: &str,
    layer_name: &str,
    reps: usize,
    threads: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let gpkg = GeoPackage::open(path)?;
    let layer = gpkg.layer(layer_name)?;
    let options = ArrowReadOptions::default().with_threads(threads);

    let mut rows = 0usize;
    let mut batches = 0usize;
    let mut columns = 0usize;
    let start = Instant::now();
    for _ in 0..reps {
        rows = 0;
        batches = 0;
        for batch in layer.read_arrow(options)? {
            let batch = batch?;
            rows += batch.num_rows();
            columns = batch.num_columns();
            batches += 1;
        }
    }
    let elapsed = start.elapsed();

    report(elapsed, reps);
    println!("rows={rows}");
    println!("batches={batches}");
    println!("columns={columns}");
    println!("threads={threads}");
    Ok(())
}

fn index(path: &str, layer_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let gpkg = GeoPackage::open(path)?;
    let layer = gpkg.layer(layer_name)?;
    if layer.has_spatial_index()? {
        return Err("layer already has a spatial index; hand this a fresh copy".into());
    }

    let start = Instant::now();
    layer.create_spatial_index()?;
    let elapsed = start.elapsed();

    report(elapsed, 1);
    println!("spatial_index={}", layer.has_spatial_index()?);
    Ok(())
}

fn bbox(
    path: &str,
    layer_name: &str,
    bounds: BoundingBox,
    reps: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let gpkg = GeoPackage::open(path)?;
    let layer = gpkg.layer(layer_name)?;
    let indexed = layer.has_spatial_index()?;
    let mut cursor = layer.cursor_in(bounds)?;

    let mut hits = 0usize;
    let start = Instant::now();
    for _ in 0..reps {
        hits = 0;
        for feature in cursor.features()? {
            std::hint::black_box(feature?.fid());
            hits += 1;
        }
    }
    let elapsed = start.elapsed();

    report(elapsed, reps);
    println!("hits={hits}");
    println!("spatial_index={indexed}");
    Ok(())
}

fn write(
    source: &str,
    source_layer: &str,
    target: &str,
    with_index: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Untimed: pull the whole source into memory as batches, so the timed
    // section is the write path and nothing else.
    let src = GeoPackage::open(source)?;
    let layer = src.layer(source_layer)?;
    let schema = layer.arrow_schema()?;
    let batches: Vec<_> = layer
        .read_arrow(ArrowReadOptions::default())?
        .collect::<Result<Vec<_>, _>>()?;
    let batch_count = batches.len();

    let dst = GeoPackage::create(target)?;
    let created = dst.create_layer(
        &TableSchemaBuilder::new(source_layer)
            .from_arrow_schema(&schema)?
            // The argument decides, so the comparison can run both ways.
            .spatial_index(false),
    )?;
    if with_index {
        // Created empty and before the write, which is what lets the bulk path
        // build it in one pass rather than through the triggers.
        created.create_spatial_index()?;
    }
    drop(created);

    let start = Instant::now();
    let written = dst
        .layer(source_layer)?
        .write_arrow(batches.into_iter().map(Ok), 0)?;
    dst.close()?;
    let elapsed = start.elapsed();

    report(elapsed, 1);
    println!("rows={}", written.len());
    println!("batches={batch_count}");
    println!("index={}", if with_index { "yes" } else { "no" });
    Ok(())
}

fn noop(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let start = Instant::now();
    let gpkg = GeoPackage::open(path)?;
    drop(gpkg);
    report(start.elapsed(), 1);
    Ok(())
}

// A noop unless the `hotpath` feature is enabled; then it prints a profiling
// report when `main` returns.
#[hotpath::main]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let usage = "usage: dataset_bench <info|scan|arrow|index|bbox|write|noop> <file> <layer> [...]";
    let command = args.get(1).map(String::as_str).unwrap_or("");
    let path = args.get(2).map(String::as_str).unwrap_or("");
    let layer = args.get(3).map(String::as_str).unwrap_or("");

    match command {
        "info" => info(path, layer),
        "scan" => {
            let mode = args.get(4).map(String::as_str).unwrap_or("geom");
            let reps: usize = args.get(5).map_or(Ok(1), |r| r.parse())?;
            scan(path, layer, mode, reps.max(1))
        }
        "arrow" => {
            let reps: usize = args.get(4).map_or(Ok(1), |r| r.parse())?;
            // Passed through unclamped: 0 is the reader's automatic choice.
            let threads: usize = args.get(5).map_or(Ok(0), |t| t.parse())?;
            arrow(path, layer, reps.max(1), threads)
        }
        "index" => index(path, layer),
        "bbox" => {
            let value = |i: usize| -> Result<f64, Box<dyn std::error::Error>> {
                Ok(args.get(i).ok_or(usage)?.parse()?)
            };
            let bounds = BoundingBox::new(value(4)?, value(5)?, value(6)?, value(7)?);
            let reps: usize = args.get(8).map_or(Ok(1), |r| r.parse())?;
            bbox(path, layer, bounds, reps.max(1))
        }
        "write" => {
            let target = args.get(4).ok_or(usage)?;
            let with_index = args.get(5).map(String::as_str) == Some("yes");
            write(path, layer, target, with_index)
        }
        "noop" => noop(path),
        _ => Err(usage.into()),
    }
}
