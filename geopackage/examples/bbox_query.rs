//! Query a layer by bounding box, and by a SQL `WHERE` clause.
//!
//! `features_in` uses the RTree spatial index when the layer has one and falls
//! back to a full scan when it does not, with identical results either way.
//! `select` hands a `WHERE` clause straight to SQLite for anything a bounding
//! box cannot express.
//!
//! ```sh
//! cargo run --example bbox_query -- file.gpkg layer_name -10 50 2 56
//! ```

use geopackage::{BoundingBox, GeoPackage, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [path, layer_name, min_x, min_y, max_x, max_y] = args.as_slice() else {
        eprintln!("usage: bbox_query <file.gpkg> <layer> <min_x> <min_y> <max_x> <max_y>");
        std::process::exit(2);
    };

    let gpkg = GeoPackage::open_read_only(path)?;
    let layer = gpkg.layer(layer_name)?;

    let bbox = BoundingBox::new(
        min_x.parse()?,
        min_y.parse()?,
        max_x.parse()?,
        max_y.parse()?,
    );

    let indexed = layer.has_spatial_index()?;
    println!(
        "querying {layer_name} via {}",
        if indexed { "RTree index" } else { "full scan" }
    );

    let matches = layer.features_in(bbox)?;
    println!("{} features in bbox", matches.len());

    for feature in matches.into_iter().take(10) {
        let feature = feature?;
        // Geometry parsing is lazy: it happens here, not when the row was read.
        let kind = match feature.geometry()? {
            Some(geometry) => format!("{:?}", geometry.geometry_type()),
            None => "NULL".to_owned(),
        };
        let pk = layer.primary_key_column().unwrap_or("fid");
        let attributes: Vec<String> = feature
            .iter()
            .filter(|(name, value)| *name != pk && !matches!(value, Value::Null))
            .map(|(name, value)| format!("{name}={value:?}"))
            .collect();
        println!("  fid {} {kind} {}", feature.fid(), attributes.join(" "));
    }

    // Anything a bounding box cannot express goes through SQL. The clause is
    // appended to a generated `WHERE`, so it takes conditions rather than
    // trailing syntax such as `LIMIT`; values are bound, not interpolated.
    let with_geometry = layer.select("geom IS NOT NULL", &[])?;
    println!("{} features have a geometry", with_geometry.len());

    Ok(())
}
