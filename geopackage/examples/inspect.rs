//! Print a summary of a GeoPackage: its layers, their schemas, spatial
//! reference systems, feature counts, and spatial-index health.
//!
//! A small stand-in for `ogrinfo -al -so`, showing the read-side introspection
//! API.
//!
//! ```sh
//! cargo run --example inspect -- path/to/file.gpkg
//! ```

use geopackage::{GeoPackage, SpatialIndexStatus};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: inspect <file.gpkg>");
        std::process::exit(2);
    };

    // `open_lenient` reports problems as warnings rather than refusing to open,
    // which is what an inspection tool wants: show the file, and say what is
    // wrong with it.
    let gpkg = GeoPackage::open_lenient(&path)?;
    println!("{path}");
    println!("  version: {:?}", gpkg.version());
    for warning in gpkg.open_warnings() {
        println!("  warning: {warning:?}");
    }

    let layers = gpkg.layers()?;
    if layers.is_empty() {
        println!("  no feature layers");
    }

    for layer in &layers {
        let count = layer.features()?.len();
        println!("\nlayer {:?} ({:?})", layer.table_name(), layer.kind());
        println!("  features: {count}");

        if let Some(geom) = layer.geometry_column() {
            println!(
                "  geometry: {} ({:?}, srs_id {})",
                geom.column_name, geom.geometry_type, geom.srs_id
            );
            if let Some(srs) = gpkg.srs(geom.srs_id)? {
                println!(
                    "  srs:      {} ({}:{})",
                    srs.name, srs.organization, srs.organization_coordsys_id
                );
            }

            // `spatial_index_status` distinguishes a healthy index from a legacy
            // trigger set or one left desynchronised by an interrupted build.
            let status = layer.spatial_index_status()?;
            print!("  index:    {status:?}");
            match status {
                SpatialIndexStatus::Legacy | SpatialIndexStatus::Stale => {
                    println!("  (run repair_spatial_index)");
                }
                _ => println!(),
            }
        }

        println!("  columns:");
        for column in &layer.schema().columns {
            let pk = if column.is_primary_key() {
                " PRIMARY KEY"
            } else {
                ""
            };
            let not_null = if column.not_null { " NOT NULL" } else { "" };
            let declared = if column.declared_type.is_empty() {
                "(untyped)"
            } else {
                &column.declared_type
            };
            println!("    {:<20} {declared}{pk}{not_null}", column.name);
        }
    }

    Ok(())
}
