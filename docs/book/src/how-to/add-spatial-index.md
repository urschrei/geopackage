# How to add a spatial index to an existing layer

A feature layer without an RTree index still answers bounding-box queries, by
full scan. This guide builds the index so those queries use it instead.

## From the shell

If the file is on disk and you have nothing else to do to it:

```console
$ gpkg index roads.gpkg roads
roads: index built over 1204553 rows
```

`gpkg index` builds an index only where there is none. If the layer already
has one, the command reports what it found and exits non-zero:

```console
$ gpkg index roads.gpkg roads
gpkg: roads already has an index (legacy trigger set); run `gpkg repair`
```

In that case see
[How to repair a file's spatial indexes](repair-spatial-indexes.md).

## From Rust

Open the file read-write and take the layer handle:

```rust,no_run
use geopackage::GeoPackage;

let gpkg = GeoPackage::open("roads.gpkg")?;
let layer = gpkg.layer("roads")?;
```

Check the status before building, because `create_spatial_index` fails with
`Error::SpatialIndexExists` when the `rtree_<table>_<column>` virtual table is
already there, whatever state it is in:

```rust,no_run
# use geopackage::{GeoPackage, SpatialIndexStatus};
# let gpkg = GeoPackage::open("roads.gpkg")?;
# let layer = gpkg.layer("roads")?;
match layer.spatial_index_status()? {
    SpatialIndexStatus::Absent => layer.create_spatial_index()?,
    SpatialIndexStatus::Current => {}
    // Present but not usable as it stands: repair, do not build over it.
    SpatialIndexStatus::Legacy | SpatialIndexStatus::Stale => {
        layer.repair_spatial_index()?
    }
    _ => {}
}
```

`create_spatial_index` creates the virtual table, installs the GeoPackage 1.4
trigger set, populates the index from the existing rows, and registers
`gpkg_rtree_index` in `gpkg_extensions`, all in one transaction. NULL and
empty geometries are skipped, as the triggers skip them.

## Choose the build path explicitly

Population takes one of two paths, chosen by row count against
`BulkIndexOptions::bulk_threshold` (10,000 rows by default). To override that
choice, use `create_spatial_index_with`:

```rust,no_run
use geopackage::{BulkIndexOptions, BulkVerification};

# use geopackage::GeoPackage;
# let gpkg = GeoPackage::open("roads.gpkg")?;
# let layer = gpkg.layer("roads")?;
// Force the bulk build, and check the result before trusting it.
layer.create_spatial_index_with(
    BulkIndexOptions::always_bulk().with_verification(BulkVerification::Structure),
)?;
```

Use the verification levels when it matters that a bad build is caught rather
than found later:

- `BulkVerification::None` is the default and checks nothing.
- `BulkVerification::Contents` reads every entry back and checks it against
  the envelopes accumulated while writing.
- `BulkVerification::Structure` adds `rtreecheck` over the tree.
- `BulkVerification::Database` adds a whole-database `PRAGMA integrity_check`,
  which is O(database) rather than O(index).

Every level above `None` arms an automatic fallback to the per-row triggered
build, so a failed check costs time rather than correctness.

If the layer will be appended to heavily after the build, lower the node fill
so the first append into a node does not split it immediately:

```rust,no_run
# use geopackage::{BulkIndexOptions, GeoPackage};
# let gpkg = GeoPackage::open("roads.gpkg")?;
# let layer = gpkg.layer("roads")?;
layer.create_spatial_index_with(BulkIndexOptions::default().with_fill_factor(0.7))?;
```

## Index a layer as it is written

If the rows are not in the file yet, do not build the index afterwards. Leave
`TableSchemaBuilder::spatial_index` at its default of `true` and let
`write_all` fill the empty index it left behind: the bulk path reuses the
envelopes it computed while encoding the geometries, which is between 20% and
47% cheaper than a separate build.

## Then

- Confirm the result with `gpkg info <file>`, whose `index:` line reports the
  status, or with `Layer::spatial_index_status`.
- [The spatial index: structure, contents, and the bulk build](../explanation/spatial-index.md)
  covers why the two questions about an index have different prices, and what
  the bulk build does.
- API reference:
  [`Layer::create_spatial_index`](https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.create_spatial_index),
  [`Layer::create_spatial_index_with`](https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.create_spatial_index_with),
  [`BulkIndexOptions`](https://docs.rs/geopackage/latest/geopackage/struct.BulkIndexOptions.html),
  [`BulkVerification`](https://docs.rs/geopackage/latest/geopackage/enum.BulkVerification.html).
