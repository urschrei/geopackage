# Your first GeoPackage

In this tutorial we will start from an empty directory and finish with a
GeoPackage file containing four cities, which we will query by bounding box
from Rust and then inspect from the shell with the `gpkg` command-line tool.

Everything we need is a Rust toolchain of 1.95 or newer and a C compiler,
which the bundled SQLite needs.

## Create the project

First, we make a new binary project and change into it:

```console
$ cargo new cities
$ cd cities
```

Now we add the two dependencies. `geopackage` is the library; `geo-types` is
where our point geometries will come from:

```console
$ cargo add geopackage geo-types
```

The output ends with a summary of what was added:

```console
      Adding geopackage v0.7.1 to dependencies
      Adding geo-types v0.7.20 to dependencies
```

Notice that `geopackage` builds SQLite from source, so the first compile takes
a minute or so. Every later one is quick.

## Create the file

Let's open `src/main.rs` and replace it entirely with this:

```rust
use geopackage::GeoPackage;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let gpkg = GeoPackage::create("cities.gpkg")?;
    println!("created cities.gpkg");

    gpkg.close()?;
    Ok(())
}
```

`GeoPackage::create` fails rather than overwriting when the file already
exists, so each run of this program starts from nothing. We delete the
previous file as we run:

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

Notice that `cities.gpkg` is a single file, with no `-wal` or `-shm`
companions beside it. That is the default this library keeps to, so a file can
be handed to someone else as it stands.

## Define a layer

A file with no layers contains nothing. We will declare one: a table called
`cities` with a text column, an integer column and a point geometry column in
EPSG:4326.

Add the two new `use` lines and the `create_layer` call, so that `main.rs`
reads:

```rust
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

`create_layer` also built an empty RTree spatial index over the geometry
column, which we will use in a moment. Why it is built now, empty, rather than
after the rows arrive, is covered in
[The spatial index](../explanation/spatial-index.md).

## Write four cities

Now we put some data in. `write_all` takes an iterator of features and a batch
size, where `0` means one transaction for all of them.

Add the `geo_types::Point` import, extend the `geopackage` import, and append
the write:

```rust
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

The values in each `NewFeature` are given in the order the columns were
declared, and the geometry and the feature id are not among them: they have
their own places.

## Query by bounding box

Four cities is a small enough number to read back one at a time, so let's ask
a question instead. `features_in` returns the rows whose geometry intersects a
box, in the layer's own coordinates.

Add `BoundingBox` to the import list and the loop after the write:

```rust
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

Run it one more time:

```console
$ rm -f cities.gpkg && cargo run
created cities.gpkg
created layer "cities"
wrote 4 features
features in the box:
  Dublin: 592713
```

Notice that only Dublin came back, although we wrote four cities. The box we
passed spans one degree of longitude west of Greenwich and one degree of
latitude north of 53, and Dublin is the only city we wrote that falls inside
it. The query was served by the spatial index that `create_layer` built for
us; a layer without one returns exactly the same rows from a full scan.

## Look at the file from outside

Everything so far has been our own program describing its own work. Let's
check the file with a tool that knows nothing about it. Install the
command-line companion:

```console
$ cargo install geopackage-cli
```

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

Notice that `rows: 4` matches what our program reported, and that `index:
current` is the spatial index we never named: `create_layer` built it, and it
uses the GeoPackage 1.4 trigger set. Notice too the `fid` and
`geom` columns, which we never declared. `fid` is the default primary key and
`geom` the default geometry column name, both of which
[`TableSchemaBuilder`](https://docs.rs/geopackage/latest/geopackage/struct.TableSchemaBuilder.html)
can change.

Let's also check that the file is well formed:

```console
$ gpkg validate cities.gpkg
cities.gpkg
  no findings (this crate's checks; the OGC ETS is the authority)
```

## What we have built

We wrote a program that creates a GeoPackage, declares an indexed point layer,
writes features into it and answers a spatial query, and we confirmed the
result with a separate tool. `cities.gpkg` is an ordinary GeoPackage: QGIS,
GDAL and anything else that reads the format will open it.

From here:

- The [how-to guides](../how-to/add-spatial-index.md) cover specific tasks:
  indexing an existing layer, repairing a file, copying features between
  files, reading a layer as Arrow batches, and validating a file.
- The [explanation chapters](../explanation/extent.md) cover why the extent, the
  transactions, the spatial index and the geometry encoding work the way they
  do.
- The [reference page](../reference.md) points at the full API documentation.
