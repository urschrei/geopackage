# geopackage

[![CI](https://github.com/urschrei/geopackage/actions/workflows/ci.yml/badge.svg)](https://github.com/urschrei/geopackage/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/geopackage.svg)](https://crates.io/crates/geopackage)
[![docs.rs](https://docs.rs/geopackage/badge.svg)](https://docs.rs/geopackage)

A Rust implementation of the
[OGC GeoPackage 1.4](https://www.geopackage.org/spec140/) format: vector
features, attribute tables, spatial indexing, and columnar I/O through Apache
Arrow. Pre-1.0: the API will change without notice.

## Install

```toml
[dependencies]
geopackage = "0.1"
geo-types = "0.7"  # any geo-traits implementation works; this is the common one
```

Columnar read and write through Apache Arrow is behind an off-by-default
feature, which adds the `arrow-array` and `arrow-schema` dependencies:

```toml
geopackage = { version = "0.1", features = ["arrow"] }
```

SQLite is bundled and built from source, so a C compiler is required and there
is no system SQLite dependency. The minimum supported Rust version is 1.95.

## Example

Create a file, declare a point layer, write features, and query by bounding
box.

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

With the `arrow` feature, `Layer::read_arrow` reads a layer as Arrow record
batches, on `min(4, available parallelism)` threads by default, and
`Layer::write_arrow` writes batches back through the same path as `write_all`.
Geometry is a GeoArrow WKB column whose metadata stores the CRS as PROJJSON.
`Layer::arrow_schema` and `TableSchemaBuilder::from_arrow_schema` give the two
directions of the type mapping, so a layer can be copied without restating its
schema.

### More examples

Runnable programs in [`geopackage/examples`](https://github.com/urschrei/geopackage/tree/main/geopackage/examples):

| Example | What it shows |
|---|---|
| `quickstart` | The example above. |
| `inspect` | Layers, schemas, SRS, feature counts and spatial-index health for a file, like `ogrinfo -al -so`. Uses `open_lenient`, so it reports problems rather than refusing to open. |
| `bulk_load` | Loading a large point layer with `write_all`, which engages the bulk index build. |
| `bbox_query` | `features_in` bounding-box queries and the `select` WHERE passthrough, with lazy geometry parsing. |
| `repair_index` | Detecting `Legacy` and `Stale` spatial indexes and repairing them. |

```sh
cargo run --release --example bulk_load -- 200000 out.gpkg
cargo run --example inspect -- out.gpkg
cargo run --example bbox_query -- out.gpkg points -10 -5 10 5
```

## Configuration

The defaults produce a single-file GeoPackage with an indexed feature layer.
What is settable:

| Setting | Where | Default |
|---|---|---|
| Journal mode and `synchronous` level | `OpenOptions` | unset: an existing file keeps its mode, a new file is `DELETE`, `synchronous` is SQLite's default; WAL is opt-in |
| Whether a new layer is indexed | `TableSchemaBuilder::spatial_index` | `true` |
| Primary-key and geometry column names | `TableSchemaBuilder`, `GeometrySpec` | `fid`, `geom` |
| Rows sharing a write transaction | the `batch_size` argument of `write_all` / `write_arrow` | caller-supplied; `0` writes all rows in one transaction |
| Bulk index build: row threshold, structural check, RTree node fill | `BulkIndexOptions` | 10,000 rows, `RtreeOnly`, `1.0` |
| `DATETIME` parsing, and whether a value its declared type does not strictly permit is read or rejected | `ConversionOptions` | strict, lenient |
| Rows per Arrow batch, and threads the columnar read uses | `ArrowReadOptions` | 65,536 rows, `min(4, available parallelism)` |
| Geometry bytes per Arrow batch; a batch that would cross it is emitted with fewer rows | `ArrowReadOptions::max_batch_bytes` | `min(INT32_MAX, RAM / 4)`; the column's Arrow offsets are 32-bit, so 2 GB is a hard ceiling |

Each setting is documented in full on its type in the
[crate documentation](https://docs.rs/geopackage). Anything not covered is
reachable as SQL through `GeoPackage::connection()`.

## Workspace

| Crate | Purpose |
|---|---|
| [`geopackage-core`](geopackage-core) | Format primitives, no IO or SQLite: GeoPackage Binary (GPB) header codec, normative table DDL, version-aware RTree trigger SQL, identifier quoting, `application_id`/`user_version` handling. |
| [`geopackage`](geopackage) | The container: create/open over [rusqlite](https://github.com/rusqlite/rusqlite) with bundled SQLite, the feature and attribute read and write paths, columnar read and write through Apache Arrow (feature `arrow`), the RTree spatial-index lifecycle, and the `ST_*` SQL functions the index triggers require. |
| `geopackage-core/fuzz` | cargo-fuzz targets for the GPB parser. |

## Design

- Synchronous API over rusqlite. `GeoPackage::connection()` and
  `from_connection()` expose the underlying connection; SQLite is the query
  engine, so anything the API does not cover is a query away.
- WAL is opt-in, and a handle that opted into it checkpoints and resets the
  file to `DELETE` on close, so a handed-over `.gpkg` is a single file with no
  sidecars.
- New indexes get the GeoPackage 1.4 trigger set. Older and mixed trigger
  generations, a known source of file corruption, are detected and repaired
  with `repair_spatial_index` rather than silently mixed.
- CRS definitions are stored, never transformed: there is no PROJ dependency
  and no coordinate transformation. `add_epsg_srs` writes WKT1 where the code
  has one, and WKT2 through the `gpkg_crs_wkt_1_1` extension otherwise (for
  codes with no WKT1 form, such as the geographic 3D EPSG:4979), matching GDAL.

## Performance

Measured over three published datasets rather than generated fixtures, since
what these paths cost depends on how many vertices a geometry carries and how
unevenly the features are spread. Apple M2 Pro, 12 cores, 16 GB, release build,
warm page cache, medians over repeated runs.

| | `buildings` | `rivers` | `admin` |
|---|---|---|---|
| source | [Microsoft Building Footprints](https://github.com/microsoft/USBuildingFootprints), California | [HydroRIVERS](https://www.hydrosheds.org/products/hydrorivers) v1.0, global | [GADM](https://gadm.org/data.html) 4.1, global |
| rows | 11,542,912 | 8,477,883 | 356,508 |
| geometry | Polygon | LineString | MultiPolygon |
| attribute columns | 4 | 16 | 54 |
| file | 2.37 GB | 2.03 GB | 2.74 GB |

| operation | `buildings` | `rivers` | `admin` |
|---|---|---|---|
| columnar read, `read_arrow` | 2.1 s | 1.9 s | ~2.9 s |
| scalar read, `cursor` | 4.3 s | 6.9 s | 2.4 s |
| write from Arrow batches | 12.7 s | 12.9 s | 8.4 s |
| the same write, index built as it goes | 31.0 s | 21.7 s | 8.9 s |
| `create_spatial_index` afterwards instead | 26.0 s | 14.6 s | 8.5 s |
| bounding-box query, indexed | 80 ms | 199 ms | 178 ms |
| the same query with no index | 1.7 s | 2.2 s | 1.3 s |
| features that query returned | 70,130 | 180,544 | 36,556 |

Reading is bound by bytes, not rows: the columnar path holds 0.95 to 1.13 GB/s
across three layers whose rows differ by a factor of 37 in size. The index is
the other way round, tracking row count at 42,000 to 581,000 rows/s built and
about 40 bytes per row stored, whatever the geometry. Building it during the
write rather than afterwards saves 20% to 47%, because the bulk path reuses the
envelopes it computed while encoding; that is why `create_layer` leaves an empty
index in place for `write_all` to fill.

The `admin` columnar read is given as approximate because it is: 7.7 kB rows
produce six batches of roughly 450 MB, and with the default four reader threads
holding one each, the figure follows host memory rather than the read path
(1.7 s to 5.1 s observed here). `ArrowReadOptions::with_batch_size` and
`with_max_batch_bytes` bound what is in flight.

Method, per-dataset detail and the rest of the caveats are in
[the benchmark write-up](https://github.com/urschrei/geopackage/blob/main/roadmap/benchmarks/2026-07-25-real-datasets.md);
[`scripts/bench_datasets.sh`](https://github.com/urschrei/geopackage/blob/main/scripts/bench_datasets.sh)
fetches the datasets and reproduces the table.

## Conformance

Files written by this crate are checked against OGC
[ets-gpkg12](https://github.com/opengeospatial/ets-gpkg12): 40 passed, 1
failed, where the failing test's regex hard-codes the GeoPackage 1.2 trigger
set and rejects a correct 1.4 one (no 1.3/1.4 ETS exists). The
[PDOK validator](https://github.com/PDOK/geopackage-validator) reports two
advisory findings, both on deliberate test-file choices. A GDAL round-trip
byte-compares geometry WKB and attribute values after an `ogr2ogr` copy, and
the test corpus includes GDAL-written, QGIS-written and raw-SQLite files.

## Known limitations

- **Untrusted files can trigger a large allocation.** The `wkb` reader
  pre-allocates from element counts read out of the geometry blob without
  bounding them against the buffer, so a malformed 17-byte GPB blob declaring a
  0xFFFFFFFF-member collection drives a multi-gigabyte allocation. The fix
  belongs upstream in [georust/wkb](https://github.com/georust/wkb); do not
  parse untrusted GeoPackage files. Tracked in
  [#3](https://github.com/urschrei/geopackage/issues/3).
- **Non-linear curve types** (`CIRCULARSTRING`, `COMPOUNDCURVE`, ...) cannot
  have their envelopes computed and so cannot be inserted into an indexed
  table. Tracked in [#5](https://github.com/urschrei/geopackage/issues/5).
- **Tile pyramids are not implemented.** A file's tile tables are visible
  through `contents()` but there is no read or write path for them.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/urschrei/geopackage/blob/main/LICENSE-APACHE) or
[MIT license](https://github.com/urschrei/geopackage/blob/main/LICENSE-MIT) at your option.
