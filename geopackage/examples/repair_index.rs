//! Check every layer's spatial index and repair the ones that need it.
//!
//! Two states are worth repairing, and neither is fixed automatically:
//!
//! - `Legacy`: the index is maintained by a pre-1.4 trigger set. The pre-1.4
//!   `update1` trigger corrupts the index under `UPSERT`, so a file written by
//!   older software is worth upgrading before writing to it.
//! - `Stale`: the RTree table exists but its triggers do not, which is what a
//!   crash during a bulk build leaves behind. Queries stay correct because
//!   `features_in` declines a stale index and falls back to a full scan, but
//!   the index is dead weight until rebuilt.
//!
//! ```sh
//! cargo run --example repair_index -- file.gpkg
//! ```

use geopackage::{GeoPackage, SpatialIndexStatus};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: repair_index <file.gpkg>");
        std::process::exit(2);
    };

    let gpkg = GeoPackage::open(&path)?;
    let mut repaired = 0;

    for layer in gpkg.layers()? {
        let status = layer.spatial_index_status()?;
        let name = layer.table_name();
        match status {
            SpatialIndexStatus::Legacy | SpatialIndexStatus::Stale => {
                println!("{name}: {status:?}, repairing");
                layer.repair_spatial_index()?;
                println!("{name}: now {:?}", layer.spatial_index_status()?);
                repaired += 1;
            }
            SpatialIndexStatus::Current => println!("{name}: Current, nothing to do"),
            SpatialIndexStatus::Absent => {
                // Building one is a separate decision from repairing.
                println!("{name}: no spatial index (create_spatial_index would build one)");
            }
            other => println!("{name}: {other:?}"),
        }
    }

    println!("repaired {repaired} layer(s)");
    gpkg.close()?;
    Ok(())
}
