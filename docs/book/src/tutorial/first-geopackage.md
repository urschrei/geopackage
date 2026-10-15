# Your first GeoPackage

In this tutorial we start from an empty directory and build a GeoPackage file
containing four cities. We query it by bounding box from Rust, then inspect it
from the shell with the `gpkg` command-line tool.

You need a Rust toolchain, version 1.95 or newer, and an SQLite to link
against. On Linux (install `libsqlite3-dev` or your distribution's equivalent)
and macOS that is the system library. On Windows, use the vendored copy via the
`bundled` feature (see the [introduction](../introduction.md#installation)).

## Create the project

First, we make a new binary project and change into it:

```console
$ cargo new cities
$ cd cities
```

Now we add two dependencies. `geopackage` is the library, and `geo-types`
provides our point geometries:

```console
$ cargo add geopackage geo-types
```

The output ends with a summary of what was added:

```console
      Adding geopackage v0.11.1 to dependencies
      Adding geo-types v0.7.20 to dependencies
```

The first build compiles every dependency, so it takes a minute or so. Later
builds are quick.

## Create the file

Open `src/main.rs` and replace its contents with this:

```rust,no_run
use geopackage::GeoPackage;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let gpkg = GeoPackage::create("cities.gpkg")?;
    println!("created cities.gpkg");

    gpkg.close()?;
    Ok(())
}
```

`GeoPackage::create` fails rather than overwrite an existing file, so we delete
the previous file before each run. That way every run starts from nothing:

```console
$ rm -f cities.gpkg && cargo run
```

The output should end with:

```console
created cities.gpkg
```

Let's check the directory:

```console
$ ls
Cargo.lock  Cargo.toml  cities.gpkg  src  target
```

`cities.gpkg` is a single file, with no `-wal` or `-shm` files beside it. This
is the library's default, so you can hand the file to someone else as it is.

## Define a layer

The file has no layers yet, so it contains nothing. Next we declare a table
called `cities` with a text column, an integer column and a point geometry
column in EPSG:4326.

Add the two new `use` lines and the `create_layer` call, so that `main.rs`
reads:

```rust,no_run
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{ColumnSpec, GeoPackage, GeometrySpec, TableSchemaBuilder};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let gpkg = GeoPackage::create("cities.gpkg")?;
    println!("created cities.gpkg");

    gpkg.create_layer(
        &TableSchemaBuilder::new("cities")
            .column(ColumnSpec::new("name", ColumnType::Text(None)))
            .column(ColumnSpec::new("population", ColumnType::Integer))
            .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
    )?;
    println!("created layer \"cities\"");

    gpkg.close()?;
    Ok(())
}
```

Run it again:

```console
$ rm -f cities.gpkg && cargo run
created cities.gpkg
created layer "cities"
```

`create_layer` also built an empty RTree spatial index on the geometry column,
which we will use shortly. [The spatial index](../explanation/spatial-index.md)
explains why the index is built now, while empty, rather than after the rows
arrive.

## Write four cities

Now we add some data. `write_all` takes an iterator of features and a batch
size. A batch size of `0` writes all of them in one transaction.

Add the `geo_types::Point` import, extend the `geopackage` import, and append
the write:

```rust,no_run
use geo_types::Point;
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{ColumnSpec, GeoPackage, GeometrySpec, NewFeature, TableSchemaBuilder, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let gpkg = GeoPackage::create("cities.gpkg")?;
    println!("created cities.gpkg");

    gpkg.create_layer(
        &TableSchemaBuilder::new("cities")
            .column(ColumnSpec::new("name", ColumnType::Text(None)))
            .column(ColumnSpec::new("population", ColumnType::Integer))
            .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
    )?;
    println!("created layer \"cities\"");

    let layer = gpkg.layer("cities")?;
    layer.write_all(
        vec![
            NewFeature::new(
                Point::new(-6.26, 53.35),
                vec![Value::Text("Dublin".into()), Value::Integer(592_713)],
            ),
            NewFeature::new(
                Point::new(-0.13, 51.51),
                vec![Value::Text("London".into()), Value::Integer(8_866_180)],
            ),
            NewFeature::new(
                Point::new(2.35, 48.86),
                vec![Value::Text("Paris".into()), Value::Integer(2_048_472)],
            ),
            NewFeature::new(
                Point::new(13.40, 52.52),
                vec![Value::Text("Berlin".into()), Value::Integer(3_662_381)],
            ),
        ],
        0,
    )?;
    println!("wrote {} features", layer.features()?.len());

    gpkg.close()?;
    Ok(())
}
```

Run it:

```console
$ rm -f cities.gpkg && cargo run
created cities.gpkg
created layer "cities"
wrote 4 features
```

The values in each `NewFeature` follow the order in which the columns were
declared. The geometry and the feature id are not in that list. Each is handled
separately.

## Query by bounding box

Rather than read every city back, let's ask a spatial question. `features_in`
returns the rows whose geometry intersects a box given in the layer's own
coordinates.

Add `BoundingBox` to the import list, and add the loop after the write:

```rust,no_run
use geo_types::Point;
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{
    BoundingBox, ColumnSpec, GeoPackage, GeometrySpec, NewFeature, TableSchemaBuilder, Value,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let gpkg = GeoPackage::create("cities.gpkg")?;
    println!("created cities.gpkg");

    gpkg.create_layer(
        &TableSchemaBuilder::new("cities")
            .column(ColumnSpec::new("name", ColumnType::Text(None)))
            .column(ColumnSpec::new("population", ColumnType::Integer))
            .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
    )?;
    println!("created layer \"cities\"");

    let layer = gpkg.layer("cities")?;
    layer.write_all(
        vec![
            NewFeature::new(
                Point::new(-6.26, 53.35),
                vec![Value::Text("Dublin".into()), Value::Integer(592_713)],
            ),
            NewFeature::new(
                Point::new(-0.13, 51.51),
                vec![Value::Text("London".into()), Value::Integer(8_866_180)],
            ),
            NewFeature::new(
                Point::new(2.35, 48.86),
                vec![Value::Text("Paris".into()), Value::Integer(2_048_472)],
            ),
            NewFeature::new(
                Point::new(13.40, 52.52),
                vec![Value::Text("Berlin".into()), Value::Integer(3_662_381)],
            ),
        ],
        0,
    )?;
    println!("wrote {} features", layer.features()?.len());

    println!("features in the box:");
    for feature in layer.features_in(BoundingBox::new(-7.0, 53.0, -6.0, 54.0))? {
        let feature = feature?;
        let name = feature.value("name").and_then(|v| v.as_str()).unwrap_or("");
        let population = feature
            .value("population")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        println!("  {name}: {population}");
    }

    gpkg.close()?;
    Ok(())
}
```

Run it again:

```console
$ rm -f cities.gpkg && cargo run
created cities.gpkg
created layer "cities"
wrote 4 features
features in the box:
  Dublin: 592713
```

Only Dublin came back, although we wrote four cities. The box spans one degree
of longitude west of Greenwich and one degree of latitude north of 53, and
Dublin is the only one of our cities inside it. The spatial index that
`create_layer` built answered the query. A layer without an index returns
exactly the same rows from a full scan.

## Read the geometries back

So far we have read back the name and population, but not the point itself. A
feature returns its geometry lazily. `feature.geometry()` parses the stored
blob into a wrapper over its bytes, and `to_geo` on that wrapper converts it to
an owned
[`geo_types::Geometry`](https://docs.rs/geo-types/latest/geo_types/geometry/enum.Geometry.html).
This mirrors the write side, which accepted our `geo_types::Point` values
directly. `to_geo` returns an `Option` because a GeoPackage can hold curve
types that `geo-types` cannot represent. Our points are not among them.

Extend the loop body to fetch the location:

```rust,no_run
use geo_types::Point;
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{
    BoundingBox, ColumnSpec, GeoPackage, GeometrySpec, NewFeature, TableSchemaBuilder, Value,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let gpkg = GeoPackage::create("cities.gpkg")?;
    println!("created cities.gpkg");

    gpkg.create_layer(
        &TableSchemaBuilder::new("cities")
            .column(ColumnSpec::new("name", ColumnType::Text(None)))
            .column(ColumnSpec::new("population", ColumnType::Integer))
            .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
    )?;
    println!("created layer \"cities\"");

    let layer = gpkg.layer("cities")?;
    layer.write_all(
        vec![
            NewFeature::new(
                Point::new(-6.26, 53.35),
                vec![Value::Text("Dublin".into()), Value::Integer(592_713)],
            ),
            NewFeature::new(
                Point::new(-0.13, 51.51),
                vec![Value::Text("London".into()), Value::Integer(8_866_180)],
            ),
            NewFeature::new(
                Point::new(2.35, 48.86),
                vec![Value::Text("Paris".into()), Value::Integer(2_048_472)],
            ),
            NewFeature::new(
                Point::new(13.40, 52.52),
                vec![Value::Text("Berlin".into()), Value::Integer(3_662_381)],
            ),
        ],
        0,
    )?;
    println!("wrote {} features", layer.features()?.len());

    println!("features in the box:");
    for feature in layer.features_in(BoundingBox::new(-7.0, 53.0, -6.0, 54.0))? {
        let feature = feature?;
        let name = feature.value("name").and_then(|v| v.as_str()).unwrap_or("");
        let population = feature
            .value("population")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let location = match feature.geometry()?.and_then(|g| g.to_geo()) {
            Some(geo_types::Geometry::Point(p)) => format!("{}, {}", p.x(), p.y()),
            _ => "no location".into(),
        };
        println!("  {name}: {population} at {location}");
    }

    gpkg.close()?;
    Ok(())
}
```

Run it again:

```console
$ rm -f cities.gpkg && cargo run
created cities.gpkg
created layer "cities"
wrote 4 features
features in the box:
  Dublin: 592713 at -6.26, 53.35
```

The point comes back exactly as we wrote it. You don't have to convert,
though. An algorithm written against the `geo-traits` interfaces can read
coordinates directly from the wrapper, without ever creating an owned geometry.
The [geometry chapter](../explanation/geometry.md) covers this.

## Look at the file from outside

So far only our own program has reported on the file. Let's check it with a
separate tool. Install the command-line tool, which downloads a binary from the
project's GitHub release:

```console
$ cargo binstall geopackage-cli
```

If you don't have `cargo binstall`, run `cargo install geopackage-cli` to build
the tool from source instead.

Then ask it what our file contains:

```console
$ gpkg info cities.gpkg
cities.gpkg
  version: 1.4

layer "cities" (features)
  rows:     4
  geometry: geom (POINT, srs_id 4326)
  srs:      WGS 84 geodetic (EPSG:4326)
  index:    current
  columns:
    fid                  INTEGER PRIMARY KEY
    name                 TEXT
    population           INTEGER
    geom                 POINT

extensions:
  gpkg_rtree_index                   cities.geom            implemented [write-only]
```

`rows: 4` matches what our program reported, and `index:
current` is the spatial index we never named: `create_layer` built it, and it
uses the GeoPackage 1.4 trigger set. We never declared the `fid` and `geom`
columns either. `fid` is the default primary key and `geom` the default
geometry column name. You can change both with
[`TableSchemaBuilder`](https://docs.rs/geopackage/latest/geopackage/struct.TableSchemaBuilder.html).

Let's also check that the file is well formed:

```console
$ gpkg validate cities.gpkg
cities.gpkg
  no findings (this crate's checks; the OGC ETS is the authority)
```

## What we have built

We wrote a program that creates a GeoPackage, declares an indexed point layer,
writes features into it, answers a spatial query and reads the geometries back
as `geo-types` values. We then confirmed the result with a separate tool.
`cities.gpkg` is an ordinary GeoPackage, so QGIS, GDAL and anything else that
reads the format will open it.

From here:

- The [how-to guides](../how-to/add-spatial-index.md) cover specific tasks:
  indexing an existing layer, repairing a file, copying features between
  files, reading a layer as Arrow batches, and validating a file.
- The [explanation chapters](../explanation/extent.md) cover why the extent, the
  transactions, the spatial index, the geometry encoding and tiled gridded
  coverages work the way they do.
- The [reference page](../reference.md) points at the full API documentation.
