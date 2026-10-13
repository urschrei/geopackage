# How to add a spatial index to an existing layer

A feature layer without an RTree index answers bounding-box queries by a full
scan. This guide shows you how to build the index so those queries can use it.

## From the shell

If the file is on disk and you don't need to do anything else to it:

```console
$ gpkg index roads.gpkg roads
roads: index built over 1204553 rows
```

`gpkg index` only builds an index where none exists. If the layer already has
one, the command reports what it found and exits non-zero:

```console
$ gpkg index roads.gpkg roads
gpkg: roads already has an index (legacy trigger set); run `gpkg repair`
```

In that case, see
[How to repair a file's spatial indexes](repair-spatial-indexes.md).

## From Rust

Open the file read-write and take the layer handle:

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use geopackage::GeoPackage;

let gpkg = GeoPackage::open("roads.gpkg")?;
let layer = gpkg.layer("roads")?;
# Ok(()) }
```

Check the status first. `create_spatial_index` fails with
`Error::SpatialIndexExists` if the `rtree_<table>_<column>` virtual table
already exists, whatever state it is in:

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# use geopackage::{GeoPackage, SpatialIndexStatus};
# let gpkg = GeoPackage::open("roads.gpkg")?;
# let layer = gpkg.layer("roads")?;
match layer.spatial_index_status()? {
    SpatialIndexStatus::Absent => layer.create_spatial_index()?,
    SpatialIndexStatus::Current => {}
    // Present but not usable: repair it instead of building over it.
    SpatialIndexStatus::Legacy | SpatialIndexStatus::Stale => {
        layer.repair_spatial_index()?
    }
    _ => {}
}
# Ok(()) }
```

`create_spatial_index` creates the virtual table, installs the GeoPackage 1.4
trigger set, populates the index from the existing rows, and registers
`gpkg_rtree_index` in `gpkg_extensions`, all in one transaction. Like the
triggers, it skips NULL and empty geometries.

## Choose the build path explicitly

The index is populated by one of two paths, chosen by comparing the row count
with `BulkIndexOptions::bulk_threshold` (10,000 rows by default). To choose the
path yourself, use `create_spatial_index_with`:

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use geopackage::{BulkIndexOptions, BulkVerification};

# use geopackage::GeoPackage;
# let gpkg = GeoPackage::open("roads.gpkg")?;
# let layer = gpkg.layer("roads")?;
// Force the bulk build, and verify the result.
layer.create_spatial_index_with(
    BulkIndexOptions::always_bulk().with_verification(BulkVerification::Structure),
)?;
# Ok(()) }
```

Set a verification level if a bad build must be caught at build time rather
than found later:

- `BulkVerification::None` is the default and checks nothing.
- `BulkVerification::Contents` reads every entry back and checks it against
  the envelopes accumulated while writing.
- `BulkVerification::Structure` adds `rtreecheck` over the tree.
- `BulkVerification::Database` adds a whole-database `PRAGMA integrity_check`,
  which is O(database) rather than O(index).

Every level above `None` enables an automatic fallback to the per-row
triggered build. A failed check makes the build slower, not wrong.

If you will append many rows to the layer after the build, lower the node fill
so that the first append into a node doesn't split it immediately:

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# use geopackage::{BulkIndexOptions, GeoPackage};
# let gpkg = GeoPackage::open("roads.gpkg")?;
# let layer = gpkg.layer("roads")?;
layer.create_spatial_index_with(BulkIndexOptions::default().with_fill_factor(0.7))?;
# Ok(()) }
```

## Index a layer as it is written

If the rows are not in the file yet, don't build the index afterwards. Leave
`TableSchemaBuilder::spatial_index` at its default of `true`, which creates an
empty index, and let `write_all` fill it. The bulk path reuses the envelopes
computed while encoding the geometries, which is between 20% and 47% cheaper
than a separate build.

## Then

- Confirm the result with `gpkg info <file>`, whose `index:` line reports the
  status, or with `Layer::spatial_index_status`.
- [The spatial index: structure, contents, and the bulk build](../explanation/spatial-index.md)
  explains why checking an index's structure and checking its contents differ
  in cost, and what the bulk build does.
- API reference:
  [`Layer::create_spatial_index`](https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.create_spatial_index),
  [`Layer::create_spatial_index_with`](https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.create_spatial_index_with),
  [`BulkIndexOptions`](https://docs.rs/geopackage/latest/geopackage/struct.BulkIndexOptions.html),
  [`BulkVerification`](https://docs.rs/geopackage/latest/geopackage/enum.BulkVerification.html).
