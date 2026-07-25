# geopackage

[![CI](https://github.com/urschrei/geopackage/actions/workflows/ci.yml/badge.svg)](https://github.com/urschrei/geopackage/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/geopackage.svg)](https://crates.io/crates/geopackage)
[![docs.rs](https://docs.rs/geopackage/badge.svg)](https://docs.rs/geopackage)

A fast, robust, Rust implementation of the
[OGC GeoPackage 1.4](https://www.geopackage.org/spec140/) format, intended for
use from Rust and via a C ABI which implements the Arrow C Data Interface.

**Status: pre-alpha (0.1.x).** The read and write paths are complete and
validated against external tooling (see [Conformance](#conformance)), but the
API will change without notice before 1.0.

## Install

```toml
[dependencies]
geopackage = "0.1"
geo-types = "0.7"  # any geo-traits implementation works; this is the common one
```

Columnar read and write through Apache Arrow is behind an off-by-default
feature, since it pulls in the `arrow-array` and `arrow-schema` crates:

```toml
geopackage = { version = "0.1", features = ["arrow"] }
```

SQLite is bundled and built from source, so a C compiler is required; there is
no system SQLite dependency. The minimum supported Rust version is 1.95.

## Example

Create a file, declare a point layer, write features, and query by bounding
box. The layer is indexed: `create_layer` builds a spatial index unless the
builder declines it.

```rust
use geo_types::Point;
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{
    BoundingBox, ColumnSpec, GeoPackage, GeometrySpec, NewFeature, TableSchemaBuilder, Value,
};

let gpkg = GeoPackage::create("cities.gpkg")?;

gpkg.create_layer(
    &TableSchemaBuilder::new("cities")
        .column(ColumnSpec::new("name", ColumnType::Text(None)))
        .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
)?;

// `create_layer` builds a spatial index; decline it with
// `.spatial_index(false)` on the builder.
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
```

### Columnar read and write

With the `arrow` feature,
[`read_arrow`](https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.read_arrow)
reads a layer as Arrow record batches, on `min(4, available parallelism)`
threads by default, and
[`write_arrow`](https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.write_arrow)
writes them back through the same path as `write_all`. Geometry is a GeoArrow
WKB column carrying its CRS as PROJJSON, and
[`arrow_schema`](https://docs.rs/geopackage/latest/geopackage/struct.Layer.html#method.arrow_schema)
with `TableSchemaBuilder::from_arrow_schema` give the two directions of the
type mapping, so a layer can be copied without its schema being restated.

### More examples

Runnable programs in [`geopackage/examples`](https://github.com/urschrei/geopackage/tree/main/geopackage/examples):

| Example | What it shows |
|---|---|
| `quickstart` | The snippet above, kept compiling. |
| `inspect` | Layers, schemas, SRS, feature counts and spatial-index health for a file, in the manner of `ogrinfo -al -so`. Uses `open_lenient`, so it reports problems rather than refusing to open. |
| `bulk_load` | Loading a large point layer with `write_all` into the empty index `create_layer` leaves behind, which is what engages the bulk shadow-table build. |
| `bbox_query` | `features_in` bounding-box queries (RTree-accelerated or full-scan) and the `select` WHERE passthrough, with lazy geometry parsing. |
| `repair_index` | Detecting `Legacy` and `Stale` spatial indexes and repairing them. |

```sh
cargo run --release --example bulk_load -- 200000 out.gpkg
cargo run --example inspect -- out.gpkg
cargo run --example bbox_query -- out.gpkg points -10 -5 10 5
```

## Configuration

The defaults suit the common case: a single-file GeoPackage, an indexed feature
layer, and values read the way other implementations read them. What people
usually change:

| Setting | Where | Default |
|---|---|---|
| Journal mode (WAL is opt-in) and `synchronous` level | `OpenOptions` | leave the file's own mode; a new file is `DELETE`, with SQLite's own `synchronous` |
| Whether a new layer is indexed | `TableSchemaBuilder::spatial_index` | `true` |
| Primary-key and geometry column names | `TableSchemaBuilder`, `GeometrySpec` | `fid`, `geom` |
| Rows sharing a write transaction | the `batch_size` argument of `write_all` / `write_arrow` | caller's; `0` writes all of them in one |
| The row count at which an index is built in bulk, how thoroughly that build then checks itself, and how full each RTree node is packed | `BulkIndexOptions` | 10,000 rows, `RtreeOnly`, `1.0` |
| `DATETIME` strictness, and whether a value its declared type does not strictly permit is read or rejected | `ConversionOptions` | strict, lenient |
| Rows per Arrow batch, and threads the columnar read uses | `ArrowReadOptions` | 65,536 rows, `min(4, available parallelism)` |
| Geometry bytes one Arrow batch may hold, above which it is emitted short | `ArrowReadOptions::max_batch_bytes` | `min(INT32_MAX, RAM / 4)`; the column's Arrow offsets are 32-bit, so 2 GB is a hard ceiling |

The crate documentation's
[Configuration](https://docs.rs/geopackage/latest/geopackage/#configuration)
section links each of these to the type that documents it in full, with the
reasoning behind each default. Anything not covered is reachable as SQL through
`GeoPackage::connection()`.

## Workspace

| Crate | Purpose |
|---|---|
| [`geopackage-core`](https://github.com/urschrei/geopackage/tree/main/geopackage-core) | No-IO spec layer: GeoPackage Binary (GPB) header codec, normative table DDL, version-aware RTree trigger SQL, identifier quoting, `application_id`/`user_version` handling. Dependency-light by design so other implementations can share it. |
| [`geopackage`](https://github.com/urschrei/geopackage/tree/main/geopackage) | The library: container create/open over [rusqlite](https://github.com/rusqlite/rusqlite) (`bundled` + `functions`), the feature/attribute read and write paths, columnar read and write through Apache Arrow (feature `arrow`), the RTree spatial-index lifecycle, and registration of the `ST_IsEmpty`/`ST_MinX`/… SQL functions required by the spatial index triggers. |
| `geopackage-core/fuzz` | cargo-fuzz targets (GPB parser). |

## Design notes

- **Sync core on rusqlite.** The RTree extension's triggers call `ST_*`
  functions that must be registered on every writing connection; sqlx-sqlite
  cannot register custom functions, and SQLite is synchronous anyway. Async
  wrappers can sit on top.
- **GeoPackage 1.4 trigger set** (`update5`/`update6`/`update7`) is emitted
  for new indexes; older generations are detected and repairable
  (`repair_spatial_index`) rather than silently mixed - mixed-generation
  triggers are a known source of file corruption (e.g. UPSERT against pre-1.4
  triggers).
- **Escape hatches everywhere:** `GeoPackage::connection()` /
  `from_connection()` expose the underlying rusqlite connection. SQLite is
  the query engine; we do not wrap what we do not need to.
- **Interchange-first close.** WAL is opt-in, and a handle that opted into it
  checkpoints and resets the file to `DELETE` on close, so a handed-over
  `.gpkg` is a single file with no sidecars.
- **CRS stored faithfully, never transformed.** There is no PROJ dependency and
  no coordinate transformation. `add_epsg_srs` writes a WKT1 definition from a
  vendored subset where the code has one and from the EPSG registry otherwise;
  a code with no WKT1 form at all, such as the geographic 3D EPSG:4979, is
  written as WKT2 through the `gpkg_crs_wkt_1_1` extension column, which is
  what GDAL does with the same codes.

## Conformance

Files written by this crate are checked against OGC
[ets-gpkg12](https://github.com/opengeospatial/ets-gpkg12) (40 passed, 1
failure whose regex hard-codes the GeoPackage 1.2 trigger set and rejects a
correct 1.4 one; no 1.3/1.4 ETS exists), the
[PDOK validator](https://github.com/PDOK/geopackage-validator) (clean but for
two advisory findings on deliberate choices), `ogrinfo`, and a GDAL round-trip
that byte-compares geometry WKB and attribute values. The test corpus includes
GDAL-written, QGIS-written and raw-SQLite files.

## Known limitations

- **Untrusted files can trigger a large allocation.** The `wkb` 0.9.2 reader
  pre-allocates from element counts read out of the geometry blob without
  bounding them against the buffer, so a malformed 17-byte GPB blob declaring a
  0xFFFFFFFF-member collection drives a multi-gigabyte allocation. Found by the
  `gpb_geometry` fuzz target. The fix belongs upstream in
  [georust/wkb](https://github.com/georust/wkb); do not parse untrusted
  GeoPackage files with 0.1.x. Tracked in
  [#3](https://github.com/urschrei/geopackage/issues/3).
- **Non-linear curve types** (`CIRCULARSTRING`, `COMPOUNDCURVE`, …) cannot have
  their envelopes computed and so cannot be inserted into an indexed table.
  Tracked in [#5](https://github.com/urschrei/geopackage/issues/5).
- **Tile pyramids are not implemented.** This is a vector-feature and
  attribute-table library so far; a file's tile tables are visible through
  `contents()` but there is no read or write path for them.

## Status

The read path (scan, bounding-box query through the RTree, `WHERE`
passthrough, full WKB envelopes) and the write path (layer creation,
`FeatureWriter`, bulk RTree build, trigger repair) are released as 0.1.x.
Landed since 0.1.2 and not yet released: columnar read and write through
Apache Arrow behind the `arrow` feature, a spatial index on every new feature
layer by default, and EPSG coverage beyond the vendored WKT1 subset. See the
[changelog](CHANGELOG.md).

Next, for 0.2: a C ABI (`geopackage-ffi`) exposing the Arrow C Data Interface,
and a CLI. After that, tile pyramids, then the remaining extensions and an API
freeze for 1.0.

## License

Licensed under either of
[Apache License, Version 2.0](https://github.com/urschrei/geopackage/blob/main/LICENSE-APACHE)
or [MIT license](https://github.com/urschrei/geopackage/blob/main/LICENSE-MIT)
at your option.
