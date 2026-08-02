# How to repair a file's spatial indexes

An index that arrives with a file may be maintained by a pre-1.4 trigger set,
or may have lost its triggers entirely. Neither is trusted for queries here,
so bounding-box reads fall back to a full scan until the index is put right.
This guide restores them.

Repairing is never automatic. Nothing in this library rewrites an existing
trigger set unless you call for it.

## From the shell

To repair every layer in a file that needs it:

```console
$ gpkg repair places.gpkg
points: legacy trigger set index repaired to the 1.4 trigger set
```

To repair one named layer, add its name:

```console
$ gpkg repair places.gpkg points
```

When nothing needs doing the command says so and exits zero:

```console
$ gpkg repair places.gpkg
nothing to repair
```

A layer with no index at all is left alone by `gpkg repair`, because an
unindexed layer reads correctly. See
[How to add a spatial index to an existing layer](add-spatial-index.md) if you
want one built.

## From Rust

Open read-write, and prefer the lenient open: a file worth repairing is by
definition one with something wrong with it, and a strict open can fail on a
legacy `application_id` before you reach the index.

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use geopackage::{GeoPackage, SpatialIndexStatus};

let gpkg = GeoPackage::open_lenient("places.gpkg")?;

for layer in gpkg.layers()? {
    if layer.geometry_column().is_none() {
        continue;
    }
    match layer.spatial_index_status()? {
        SpatialIndexStatus::Legacy | SpatialIndexStatus::Stale => {
            layer.repair_spatial_index()?;
        }
        // Current needs nothing; Absent is a choice, not a defect.
        _ => {}
    }
}
# Ok(()) }
```

`repair_spatial_index` drops every RTree trigger on the table, installs the
1.4 set, and rebuilds the index content, in one transaction. It returns
without doing anything when the index is already current, and fails with
`Error::NoSpatialIndex` when there is no index to repair.

## If the structure is right but the contents are in doubt

`spatial_index_status` is a structural question and cannot tell whether the
entries agree with the geometries. An index can be `Current` and still be
wrong, in a file where rows were written while the triggers were absent, or
that another tool populated incompletely. To answer that, audit it:

```rust,no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# use geopackage::GeoPackage;
# let gpkg = GeoPackage::open_lenient("places.gpkg")?;
# let layer = gpkg.layer("points")?;
let audit = layer.audit_spatial_index()?;
if !audit.is_consistent() {
    eprintln!(
        "{} missing, {} extra, {} not covering",
        audit.missing, audit.extra, audit.not_covering
    );
    layer.rebuild_spatial_index()?;
}
# Ok(()) }
```

The audit reads every geometry in the layer, so price it as a deliberate
check rather than something to run before each query. It writes nothing.
`rebuild_spatial_index` is the remedy, and does unconditionally what the
repair does only when the structure is wrong.

`gpkg validate` runs the same audit across the whole file, which is the
cheaper way to find out whether any layer needs it: see
[How to validate a GeoPackage and act on the findings](validate.md).

## Then

- [The spatial index: structure, contents, and the bulk build](../explanation/spatial-index.md)
  covers why repair is never automatic, and what makes a legacy trigger set
  worth replacing.
- API reference:
  [`Layer::spatial_index_status`](https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.spatial_index_status),
  [`Layer::repair_spatial_index`](https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.repair_spatial_index),
  [`Layer::audit_spatial_index`](https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.audit_spatial_index),
  [`Layer::rebuild_spatial_index`](https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.rebuild_spatial_index),
  [`SpatialIndexAudit`](https://docs.rs/geopackage/latest/geopackage/struct.SpatialIndexAudit.html).
