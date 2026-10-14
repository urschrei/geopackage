# How to copy features between files without decoding geometry

If you read a geometry into a `geo-types` value and write it back, every
coordinate is parsed for nothing, and the non-linear curve types are lost
entirely, because `geo-traits` has no representation for an arc. This guide
copies rows between files as WKB bytes instead. Nothing is re-encoded, and a
`CIRCULARSTRING` arrives unchanged.

## From the shell

For a whole file, `gpkg copy` already works this way:

```console
$ gpkg copy places.gpkg places-copy.gpkg
```

It copies feature and attribute layers, with their schemas, spatial reference
systems and rows, and a spatial index wherever the source had one. It does not
copy tile pyramids, tiled gridded coverages or the extension tables, and it
names whatever it left behind at the end.

## Mirror the source layer

In Rust, start with the source file open and the destination file created.
The destination's geometry column must match the source in three respects:
the geometry type, the `z` and `m` flags, and the SRS. These are easy to leave
at their defaults, and the copy then fails on the first row.

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use geopackage::core::types::ColumnType;
use geopackage::{ColumnSpec, GeoPackage, GeometrySpec, TableSchemaBuilder};

let src = GeoPackage::open_read_only("places.gpkg")?;
let dst = GeoPackage::create("subset.gpkg")?;

let source = src.layer("points")?;
let geom = source.geometry_column().expect("a feature layer");

// The SRS must exist in the destination before a layer can refer to it.
// `add_srs` does nothing if it is already there.
if let Some(srs) = src.srs(geom.srs_id)? {
    dst.add_srs(&srs)?;
}

let mut builder = TableSchemaBuilder::new("points").geometry(
    GeometrySpec::new(geom.geometry_type, geom.srs_id)
        .column_name(geom.column_name.clone())
        .z(geom.z)
        .m(geom.m),
);
// Copy every column except the geometry and the primary key. Those two are
// declared elsewhere, and `Feature::values` skips them.
for column in &source.schema().columns {
    if column.is_primary_key() || column.name == geom.column_name {
        continue;
    }
    // A declared type outside the spec vocabulary parses to `None`. BLOB is
    // the typeless case under SQLite's affinity rules.
    let column_type = column
        .column_type
        .clone()
        .unwrap_or(ColumnType::Blob(None));
    builder = builder.column(ColumnSpec::new(column.name.clone(), column_type));
}
dst.create_layer(&builder)?;
# Ok(()) }
```

If you leave `z` and `m` at their defaults, the dimension is declared
prohibited, and the first geometry with a Z fails. If the source's primary key
is not named `fid`, also pass its name to `TableSchemaBuilder::primary_key`.

## Copy the rows by their bytes

`Feature::geometry_bytes` returns the stored GPB blob, including its header.
`gpb::body_offset` reads the header to find where the ISO WKB body starts.
`FeatureWriter::insert_wkb` copies the body into the new blob without
re-serialising it. The geometry is never parsed.

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use geopackage::core::gpb;

# use geopackage::GeoPackage;
# let src = GeoPackage::open_read_only("places.gpkg")?;
# let dst = GeoPackage::create("subset.gpkg")?;
# let source = src.layer("points")?;
let target = dst.layer("points")?;
let mut writer = target.writer()?;
let mut cursor = source.cursor()?;

for feature in cursor.features()? {
    let feature = feature?;
    let values: Vec<_> = feature.values().collect();
    match feature.geometry_bytes() {
        Some(blob) => {
            let offset =
                gpb::body_offset(blob).map_err(|e| geopackage::Error::Core(e.into()))?;
            writer.insert_wkb(Some(feature.fid()), &blob[offset..], &values)?;
        }
        // A NULL geometry cell, or an attribute table.
        None => {
            writer.insert_row(Some(feature.fid()), &values)?;
        }
    }
}
writer.commit()?;
# Ok(()) }
```

`Feature::values` yields borrowed `ValueRef`s, so text and blob cells bind
into the destination statement without being copied.

To update rows in place instead of inserting them, use `update_wkb`. It gives
the same guarantee.

## For a long copy

A single writer is a single transaction, so the journal grows for the whole
length of the copy. Commit periodically and open a new writer, as `gpkg
copy` does every 10,000 rows. Add a counter to the loop above, and end each
batch with:

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# use geopackage::GeoPackage;
# let dst = GeoPackage::create("subset.gpkg")?;
# let target = dst.layer("points")?;
# let mut writer = target.writer()?;
# let mut in_batch = 0usize;
# const BATCH: usize = 10_000;
in_batch += 1;
if in_batch >= BATCH {
    writer.commit()?;
    writer = target.writer()?;
    in_batch = 0;
}
# Ok(()) }
```

If the copy then fails part-way, only the current batch is rolled back, not
the whole layer.

If you have opened your own transaction on `GeoPackage::connection`, these
commits only stage the rows, and you must issue the durable commit yourself. See
[Transactions](../explanation/transactions.md).

## Then

- [Geometry storage: GPB, WKB, and curve types](../explanation/geometry.md)
  explains what the GPB header contains, why the WKB body passes through
  untouched, and what `geo-traits` cannot represent.
- API reference:
  [`Feature::geometry_bytes`](https://docs.rs/geopackage/latest/geopackage/struct.Feature.html#method.geometry_bytes),
  [`FeatureWriter::insert_wkb`](https://docs.rs/geopackage/latest/geopackage/struct.FeatureWriter.html#method.insert_wkb),
  [`FeatureWriter::update_wkb`](https://docs.rs/geopackage/latest/geopackage/struct.FeatureWriter.html#method.update_wkb),
  [`gpb::body_offset`](https://docs.rs/geopackage-core/latest/geopackage_core/gpb/fn.body_offset.html).
