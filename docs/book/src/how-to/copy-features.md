# How to copy features between files without decoding geometry

Reading a geometry into a `geo-types` value and writing it back parses every
coordinate for no purpose, and loses the non-linear curve types entirely,
because `geo-traits` has no representation for an arc. This guide moves rows
between files by their WKB bytes instead, so a `CIRCULARSTRING` survives the
copy unchanged and nothing is re-encoded.

## From the shell

For a whole file, `gpkg copy` already works this way:

```console
$ gpkg copy places.gpkg places-copy.gpkg
```

It copies feature and attribute layers: their schemas, their spatial reference
systems, their rows, and a spatial index wherever the source had one. Tiles
and the extension tables are not copied, and whatever was left behind is named
at the end.

## Mirror the source layer

In Rust, start from an open source and a destination you have created. The
destination column definition has to match the source in three respects that
are easy to leave at their defaults and then fail on the first row: the
geometry type, the `z` and `m` flags, and the SRS.

```rust,no_run
use geopackage::core::types::ColumnType;
use geopackage::{ColumnSpec, GeoPackage, GeometrySpec, TableSchemaBuilder};

let src = GeoPackage::open_read_only("places.gpkg")?;
let dst = GeoPackage::create("subset.gpkg")?;

let source = src.layer("points")?;
let geom = source.geometry_column().expect("a feature layer");

// The definition has to exist in the destination before a layer names it.
// `add_srs` does nothing for one already present.
if let Some(srs) = src.srs(geom.srs_id)? {
    dst.add_srs(&srs)?;
}

let mut builder = TableSchemaBuilder::new("points").geometry(
    GeometrySpec::new(geom.geometry_type, geom.srs_id)
        .column_name(geom.column_name.clone())
        .z(geom.z)
        .m(geom.m),
);
// Every column except the geometry and the primary key: those two are
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
```

Leaving `z` and `m` at their defaults declares the dimension prohibited, and
the first geometry with a Z fails against it. If the source names its primary
key something other than `fid`, pass that name to
`TableSchemaBuilder::primary_key` as well.

## Copy the rows by their bytes

`Feature::geometry_bytes` returns the stored GPB blob, header included.
`gpb::body_offset` reads that header to find where the ISO WKB body starts,
and `FeatureWriter::insert_wkb` copies the body into the new blob rather than
re-serialising it. Nothing parses the geometry at any point.

```rust,no_run
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
```

`Feature::values` yields borrowed `ValueRef`s, so text and blob cells bind
into the destination statement without being copied.

If you are updating rows in place rather than inserting them, `update_wkb` is
the counterpart and keeps the same guarantee.

## For a long copy

A single writer means a single transaction, which grows the journal for the
length of the copy. Commit periodically and open a fresh writer, as `gpkg
copy` does every 10,000 rows. Add a counter to the loop above and end each
batch with:

```rust,no_run
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
```

A failure part-way then rolls back only the batch it was in, rather than the
whole layer.

Note that if you have opened your own transaction on
`GeoPackage::connection`, these commits stage rather than commit, and the
durable commit is yours to issue. See
[Transactions, and who commits](../explanation/transactions.md).

## Then

- [Geometry storage: GPB, WKB, and curve types](../explanation/geometry.md)
  covers what the GPB header contains, why the WKB body passes through
  untouched, and what `geo-traits` cannot represent.
- API reference:
  [`Feature::geometry_bytes`](https://docs.rs/geopackage/latest/geopackage/struct.Feature.html#method.geometry_bytes),
  [`FeatureWriter::insert_wkb`](https://docs.rs/geopackage/latest/geopackage/struct.FeatureWriter.html#method.insert_wkb),
  [`FeatureWriter::update_wkb`](https://docs.rs/geopackage/latest/geopackage/struct.FeatureWriter.html#method.update_wkb),
  [`gpb::body_offset`](https://docs.rs/geopackage-core/latest/geopackage_core/gpb/fn.body_offset.html).
