//! Bulk-load a large point layer and build its spatial index the fast way.
//!
//! The order matters. Creating the (empty) spatial index *before* writing lets
//! `write_all` take the D8 bulk build: it drops the RTree triggers, inserts
//! every row without per-row index maintenance, then constructs the index in
//! one shadow-table copy. Writing first and indexing afterwards also works and
//! is bulk-built, but pays for the triggers to be installed and dropped.
//!
//! ```sh
//! cargo run --release --example bulk_load -- 200000 out.gpkg
//! ```

use std::time::Instant;

use geo_types::Point;
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{ColumnSpec, GeoPackage, GeometrySpec, NewFeature, TableSchemaBuilder, Value};

/// A deterministic, well-spread coordinate for row `i` over the WGS 84 domain.
fn coord(i: usize) -> (f64, f64) {
    let f = i as f64;
    (
        (f * 0.618_033_988_75).rem_euclid(360.0) - 180.0,
        (f * 0.314_159_265_36).rem_euclid(180.0) - 90.0,
    )
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let rows: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(200_000);
    let path = args.next().unwrap_or_else(|| "bulk_load.gpkg".to_owned());

    if std::fs::exists(&path)? {
        eprintln!("{path} already exists; refusing to overwrite");
        std::process::exit(1);
    }

    let gpkg = GeoPackage::create(&path)?;
    gpkg.create_layer(
        &TableSchemaBuilder::new("points")
            .column(ColumnSpec::new("label", ColumnType::Text(None)))
            .column(ColumnSpec::new("weight", ColumnType::Double))
            .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
    )?;
    // `create_layer` builds the index, and builds it empty, which is what lets
    // the write_all below fill it in one bulk pass instead of row by row
    // through the triggers.
    let layer = gpkg.layer("points")?;

    let features: Vec<_> = (0..rows)
        .map(|i| {
            let (x, y) = coord(i);
            NewFeature::new(
                Point::new(x, y),
                vec![Value::Text(format!("p{i}")), Value::Float(i as f64 / 7.0)],
            )
        })
        .collect();

    let started = Instant::now();
    // batch_size 0 puts the whole load in one transaction.
    let fids = layer.write_all(features, 0)?;
    let elapsed = started.elapsed();

    println!(
        "wrote {} features in {elapsed:?} ({:.0} rows/s)",
        fids.len(),
        fids.len() as f64 / elapsed.as_secs_f64()
    );
    println!("spatial index: {:?}", layer.spatial_index_status()?);

    // Interchange-first close: flushes and leaves a single .gpkg file behind.
    gpkg.close()?;
    println!("wrote {path}");
    Ok(())
}
