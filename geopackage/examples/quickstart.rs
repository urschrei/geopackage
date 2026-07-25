//! The README quickstart, kept compilable: create a file, declare a point
//! layer, write features, index it, and query by bounding box.

use geo_types::Point;
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{
    BoundingBox, ColumnSpec, GeoPackage, GeometrySpec, NewFeature, TableSchemaBuilder, Value,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("cities.gpkg");

    let gpkg = GeoPackage::create(path)?;

    gpkg.create_layer(
        &TableSchemaBuilder::new("cities")
            .column(ColumnSpec::new("name", ColumnType::Text(None)))
            .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
    )?;

    // `create_layer` builds the spatial index; `.spatial_index(false)` on the
    // builder declines it.
    let layer = gpkg.layer("cities")?;

    layer.write_all(
        vec![
            NewFeature::new(Point::new(-6.26, 53.35), vec![Value::Text("Dublin".into())]),
            NewFeature::new(Point::new(-0.13, 51.51), vec![Value::Text("London".into())]),
        ],
        1000,
    )?;

    // Uses the RTree index when one is present, a full scan otherwise.
    for feature in layer.features_in(BoundingBox::new(-7.0, 53.0, -6.0, 54.0))? {
        println!("{:?}", feature?.value("name"));
    }

    Ok(())
}
