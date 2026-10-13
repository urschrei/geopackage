# How to repair a file's spatial indexes

An index in a file you receive may be maintained by a pre-1.4 trigger set, or
may have lost its triggers entirely. This library trusts neither for queries,
so bounding-box reads fall back to a full scan until the index is repaired.
This guide shows you how to repair it.

Repair is never automatic. The library rewrites an existing trigger set only
when you ask it to.

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

If nothing needs repairing, the command says so and exits zero:

```console
$ gpkg repair places.gpkg
nothing to repair
```

`gpkg repair` leaves a layer with no index alone, because an unindexed layer
reads correctly. If you want an index built, see
[How to add a spatial index to an existing layer](add-spatial-index.md).

## From Rust

Open the file read-write, preferably with the lenient open. A file that needs
repair already has something wrong with it, and a strict open can fail on a
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

`spatial_index_status` checks only the structure. It cannot tell whether the
entries agree with the geometries. An index can be `Current` and still be
wrong if rows were written while the triggers were absent, or if another tool
populated it incompletely. To check the contents, audit the index:

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

The audit reads every geometry in the layer, so run it as a deliberate check,
not before each query. It writes nothing. To fix what it finds, call
`rebuild_spatial_index`. It does unconditionally what the repair does only when
the structure is wrong.

`gpkg validate` runs the same audit across the whole file. This is the cheaper
way to find out whether any layer needs rebuilding. See
[How to validate a GeoPackage and act on the findings](validate.md).

## Then

- [The spatial index: structure, contents, and the bulk build](../explanation/spatial-index.md)
  explains why repair is never automatic, and why a legacy trigger set is
  worth replacing.
- API reference:
  [`Layer::spatial_index_status`](https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.spatial_index_status),
  [`Layer::repair_spatial_index`](https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.repair_spatial_index),
  [`Layer::audit_spatial_index`](https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.audit_spatial_index),
  [`Layer::rebuild_spatial_index`](https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.rebuild_spatial_index),
  [`SpatialIndexAudit`](https://docs.rs/geopackage/latest/geopackage/struct.SpatialIndexAudit.html).
