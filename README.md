# geopackage

[![CI](https://github.com/urschrei/geopackage/actions/workflows/ci.yml/badge.svg)](https://github.com/urschrei/geopackage/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/geopackage.svg)](https://crates.io/crates/geopackage)
[![docs.rs](https://docs.rs/geopackage/badge.svg)](https://docs.rs/geopackage)

A Rust implementation of the
[OGC GeoPackage 1.4](https://www.geopackage.org/spec140/) format: vector
features, attribute tables, spatial indexing, tile pyramids, and columnar I/O
through Apache Arrow. Pre-1.0: the API will change without notice.

## Install

```toml
[dependencies]
geopackage = "0.7"
geo-types = "0.7"  # any geo-traits implementation works; this is the common one
```

Columnar read and write through Apache Arrow is behind an off-by-default
feature, which adds the `arrow-array` and `arrow-schema` dependencies:

```toml
geopackage = { version = "0.7", features = ["arrow"] }
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

### Tiles

A GeoPackage can also hold pre-rendered raster tiles, addressed by zoom level,
column and row. `GeoPackage::create_tile_pyramid` writes a pyramid,
`GeoPackage::tiles` opens one, and the payloads are stored and returned as they
are:

```rust
use geopackage::core::tiles::{TileCoord, TileMatrixSet, ZoomLadder};
use geopackage::{GeoPackage, TilePyramidBuilder};

let gpkg = GeoPackage::create("basemap.gpkg")?;
gpkg.add_epsg_srs(3857)?;

// The spec's default arrangement: each zoom level doubles the grid, with pixel
// sizes derived from the extent so they span it exactly.
let matrix_set = TileMatrixSet::web_mercator_quad();
let matrices = matrix_set.ladder(ZoomLadder::new(0, 12))?;
let tiles = gpkg
    .create_tile_pyramid(&TilePyramidBuilder::new("basemap", matrix_set).matrices(matrices))?;

tiles.put_tile(TileCoord::new(12, 2048, 1362), &png_bytes)?;
let tile = tiles.get_tile(TileCoord::new(12, 2048, 1362))?;
```

**This crate decodes no images.** It stores, indexes and validates tiles, and
depends on no image codec: what it reads of a payload is its header, which is
how a tile of the wrong pixel size, or in a format the table may not hold, is
rejected on write rather than stored. Turning tiles into pixels, or a source
raster into a pyramid, needs an image library or GDAL on top of this one.

Writing a tile checks its address against the zoom level's grid and its header
against the declared tile size; a WebP payload registers `gpkg_webp` as it
lands. Rows count from the top of the extent downwards (the WMTS and XYZ sense,
not TMS), and the indices are relative to the pyramid's own extent rather than
a global grid, so the XYZ conversion refuses on a pyramid that is not the
standard web mercator quad instead of quietly pointing somewhere else.

### Columnar read and write

With the `arrow` feature, `Layer::read_arrow` reads a layer as Arrow record
batches, on `min(4, available parallelism)` threads by default, and
`Layer::write_arrow` writes batches back through the same path as `write_all`.
Geometry is a GeoArrow WKB column whose metadata stores the CRS as PROJJSON.
`Layer::arrow_schema` and `TableSchemaBuilder::from_arrow_schema` give the two
directions of the type mapping, so a layer can be copied without restating its
schema.

### Extensions

`gpkg_extensions` is where a file declares what it uses beyond the core spec.
`GeoPackage::extensions` reads that catalogue, and every row says what this
crate can do with it: read and write it, identify it and leave it alone,
tolerate it as one of the two extensions OGC removed in 2016, or not recognise
it at all.

Writing to a table covered by an extension this crate cannot identify is
refused, because such an extension may constrain the rows, triggers or
encodings of the table it covers, and writing beside it could produce a file
its own producer can no longer read. Reading is never refused for this reason,
and `OpenOptions::allow_unsupported_extension_writes` overrides the refusal for
a caller who knows the extension is harmless.

Several extensions appear in the model rather than only as catalogue rows.
`gpkg_crs_wkt` puts a WKT2 definition and a coordinate epoch on `Srs`, which is
how a CRS with no WKT1 form is carried at all. `gpkg_metadata` stores documents
and attaches them to the file, a table, a column, a row or a cell, leaving the
payloads as written and interpreting no metadata profile. The Related Tables
Extension (OGC 18-000) relates two tables through a mapping table, readable for
any relation type and writable for the requirements classes the spec defines.
The non-linear geometry types register themselves as `gpkg_geom_<TYPE>` when a
layer declares one. And `gpkg_schema` describes columns and constrains their
values:

```rust
use geopackage::{ColumnConstraint, ConstraintKind, GeoPackage, OpenOptions};

let gpkg = GeoPackage::open("sites.gpkg")?;
gpkg.add_column_constraint(&ColumnConstraint {
    name: "years".into(),
    kind: ConstraintKind::Range {
        min: 1900.0,
        min_is_inclusive: true,
        max: 2000.0,
        max_is_inclusive: false,
    },
    description: None,
})?;

// Checking written values against the constraints their columns declare is
// asked for, not assumed: the format makes them advisory, so a conforming
// file may hold values its own constraints forbid.
let checked = OpenOptions::new()
    .enforce_column_constraints(true)
    .open("sites.gpkg")?;
```

A column's description reaches `Column::data_column`, so reading a layer's
schema shows it without a second lookup. Enforcement covers every write path,
the columnar one included, and costs about 31% on a write with two constrained
columns. The `glob` constraint form is evaluated by SQLite itself rather than
reimplemented here: its pattern language has no definition beyond what SQLite
does with it, and this crate bundles SQLite.

### Checking a file

`GeoPackage::validate` makes one pass over a file and returns what is wrong
with it: catalogue rows naming tables that are not there, spatial indexes that
no longer describe their rows, pre-1.4 index triggers, extensions this crate
cannot identify, tile pyramids that break the matrix rules, and metadata or
relation rows pointing at things that have gone. Each finding carries a
severity, and repair advice naming the method that performs it where one
exists. Nothing is modified.

Severity is about consequence: an error means a reader can get a wrong answer,
a warning means the file is out of step with the current spec but reads
correctly, and an advisory is a remark rather than a defect. A file with no
spatial index reads correctly, so it is the third of those.

This reports what this crate can see, not conformance in every respect the
spec defines; the OGC executable test suite remains the authority.

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
| Bulk index build: row threshold, self-verification, RTree node fill | `BulkIndexOptions` | 10,000 rows, `BulkVerification::None`, `1.0` |
| `DATETIME` parsing, and whether a value its declared type does not strictly permit is read or rejected | `ConversionOptions` | strict, lenient |
| Rows per Arrow batch, and threads the columnar read uses | `ArrowReadOptions` | 65,536 rows, `min(4, available parallelism)` |
| Geometry bytes per Arrow batch; a batch that would cross it is emitted with fewer rows | `ArrowReadOptions::max_batch_bytes` | `min(INT32_MAX, RAM / 4)`; the column's Arrow offsets are 32-bit, so 2 GB is a hard ceiling |

Each setting is documented in full on its type in the
[crate documentation](https://docs.rs/geopackage). Anything not covered is
reachable as SQL through `GeoPackage::connection()`.

## Command-line tool

The `geopackage-cli` crate builds `gpkg`, which reads, checks and repairs files
from a shell.

| Command | What it does |
|---|---|
| `gpkg info <file>` | Version, layers with their schemas and row counts, spatial reference systems, spatial-index state including its trigger generation, tile pyramids, and the extension catalogue with what this crate can do with each row. |
| `gpkg validate <file>` | The findings of `GeoPackage::validate`, most severe first, each with the repair advice that goes with it. Nothing is modified. |
| `gpkg index <file> <layer>` | Builds a spatial index on a layer that has none. A layer whose index is present but broken is refused rather than quietly repaired. |
| `gpkg repair <file> [layer]` | Rebuilds legacy and desynchronised indexes onto the 1.4 trigger set, for one named layer or for every layer that needs it. A layer with no index is left alone; `gpkg index` is how one is asked for. |
| `gpkg copy <src> <dst>` | Copies the feature and attribute layers of one file into a new one. |
| `gpkg tiles info <file> [pyramid]` | Each pyramid's extent, spatial reference system and zoom ladder, with the tiles stored at each level. |
| `gpkg tiles get <file> <pyramid> <zoom> <column> <row>` | Writes one tile's stored bytes to `--out` or to standard output. |

Every command opens the file leniently, so one with something wrong with it is
reported or repaired rather than refused: an inspection tool that will not open
the files worth inspecting is not much use, and the files worth repairing are by
definition ones something is wrong with.

`validate` exits non-zero when a finding is an error, the severity meaning a
reader can get a wrong answer from the file. Warnings and advisories exit zero,
and `--strict` promotes warnings to a failing exit as well; that is what makes
the command usable in a script or a CI job.

`copy` carries feature and attribute layers: their schemas, their spatial
reference systems, their rows, and a spatial index wherever the source had one.
Tiles and the extension tables are not carried, and whatever was left behind is
named at the end, so a copy is not mistaken for the whole file. Geometry crosses
as WKB rather than through `geo-types`, so the non-linear curve types survive a
copy byte for byte.

`tiles get` writes the bytes the file holds and decodes nothing, whatever
`--out` is named.

```console
$ gpkg info places.gpkg
places.gpkg
  version: 1.2

layer "points" (features)
  rows:     3
  geometry: geom (POINT, srs_id 4326)
  srs:      WGS 84 geodetic (EPSG:4326)
  index:    legacy trigger set  (repair with `gpkg repair`)
  columns:
    fid                  INTEGER PRIMARY KEY NOT NULL
    geom                 POINT
    name                 TEXT
    pop                  MEDIUMINT

extensions:
  gpkg_rtree_index                   points.geom            implemented [write-only]

$ gpkg validate --strict places.gpkg
places.gpkg
  warning: spatial index on "points" is maintained by a pre-1.4 or mixed trigger set
    repair: upgrade the trigger set with Layer::repair_spatial_index

  0 errors, 1 warning, 0 advisories
$ echo $?
1

$ gpkg repair places.gpkg
points: legacy trigger set index repaired to the 1.4 trigger set

$ gpkg validate --strict places.gpkg
places.gpkg
  no findings (this crate's checks; the OGC ETS is the authority)
```

## C ABI

The `geopackage-ffi` crate builds a `cdylib` and a `staticlib` over the same
library, for consumers that are not Rust. It is packaged with
[cargo-c](https://github.com/lu-zero/cargo-c), so

```sh
cargo cinstall -p geopackage-ffi --release --prefix=/usr/local
```

installs a versioned soname, the header and a pkg-config file, and a C program
compiles and links against it through `pkg-config --cflags --libs geopackage`.

The conventions:

- **Handles are opaque.** `gpkg_t` is an open file, `gpkg_layer_t` a feature
  or attribute layer (opened plain or projected to a column subset),
  `gpkg_tiles_t` a tile pyramid (opened by name or created, on the web
  mercator quad or any grid), `gpkg_tile_cursor_t` a scan over a pyramid's
  stored tiles, `gpkg_writer_t` a row-at-a-time write transaction, and
  `gpkg_findings_t` the result of validation. Each has one destructor,
  except the writer, whose two ends mean different things: commit keeps its
  work, free discards it.
- **Strings are NUL-terminated UTF-8 in both directions.** One this library
  returns is owned by the caller and released with `gpkg_string_free`; one the
  caller passes in is borrowed for the duration of the call.
- **Errors go through a `gpkg_error_t *` out-parameter**, carrying a code and a
  message, and released with `gpkg_error_clear`. Passing NULL means the caller
  does not want the detail. A function returning a pointer fails with NULL, and
  one returning a status with a value other than `GPKG_STATUS_OK`.
- **The data plane is the Arrow C Data Interface**, both ways.
  `gpkg_layer_read_arrow` fills in an `ArrowArrayStream` the caller owns,
  `gpkg_layer_read_arrow_filtered` does the same for a bounding box, a raw
  SQL `WHERE` clause with bound parameters, or both at once (one row is the
  clause `fid = ?1`), and `gpkg_layer_write_arrow` takes a stream.
  `gpkg_create_layer_from_arrow_schema` builds a layer from a stream's own
  schema, so a C consumer can copy a layer without describing its columns at
  all; the geometry column comes out declared as `GEOMETRY` rather than the
  source's specific type, because the GeoArrow WKB encoding does not carry one.
- **A file can be interrogated before it is touched.** Layer and pyramid
  enumeration, schema introspection, `gpkg_srs` for a coordinate reference
  system's definition, the `gpkg_extensions` catalogue with the support level
  this library claims per row, and `gpkg_validate` for everything the
  library's checks can find, with severities and repair advice.

Five worked C programs ship in
[`geopackage-ffi/examples/`](https://github.com/urschrei/geopackage/tree/main/geopackage-ffi/examples),
each compiled against the committed header and run by CI, so they cannot
drift from the ABI: first contact (`smoke.c`), the fail-fast inspection
pattern (`inspect.c`), the interactive read loop (`query.c`), a complete
layer copy through Arrow (`roundtrip.c`), and a tile pipeline built from
nothing (`tilepipe.c`).

```c
#include "geopackage.h"

gpkg_error_t error = {GPKG_STATUS_OK, NULL};
gpkg_t *gpkg = gpkg_open_read_only("places.gpkg", &error);
gpkg_layer_t *layer = gpkg_layer_open(gpkg, "points", &error);

struct ArrowArrayStream stream;
gpkg_layer_read_arrow(layer, &stream, &error);
/* pull batches with stream.get_next, then: */
stream.release(&stream);

gpkg_layer_free(layer);   /* before the close, which would otherwise refuse */
gpkg_close(gpkg, &error);
```

Two rules a C caller has to know:

**One handle per thread.** `GeoPackage` is `Send` but not `Sync`, because
`rusqlite::Connection` is, so a handle may be created on one thread and used on
another, but never used from two at once. Nothing here is internally locked: a
caller wanting concurrent access opens the file once per thread, which is also
what gives SQLite its own per-connection state.

**Closing a container is refused while any handle taken from it is alive.**
`gpkg_close` returns `GPKG_STATUS_HANDLE_IN_USE` and leaves the file open while
a layer handle, a tile handle, a tile cursor, a writer or an Arrow stream from
it is unfreed. Those handles hold a borrow of the container that C has no way
to express, so the count is checked at runtime rather than left to the caller
to keep track of; the Rust API gets the same guarantee from the borrow
checker, since `close` takes `self`. The one exemption is `gpkg_findings_t`,
which owns plain data, borrows nothing, and stays readable after a close.

Transactions are the caller's when wanted: `gpkg_begin`, `gpkg_commit`,
`gpkg_rollback` and `gpkg_in_transaction`, with every write path joining a
transaction the caller has open rather than fighting it. A program that never
calls `gpkg_begin` has each write durable when its call returns, with rows per
transaction set through the `batch_size` argument the bulk write calls take.

`geopackage-ffi` is the only crate in the workspace containing `unsafe`. Every
other crate sets `unsafe_code = "forbid"`.

## Workspace

| Crate | Purpose |
|---|---|
| [`geopackage-core`](geopackage-core) | Format primitives, no IO or SQLite: GeoPackage Binary (GPB) header codec, normative table DDL, version-aware RTree trigger SQL, identifier quoting, `application_id`/`user_version` handling. |
| [`geopackage`](geopackage) | The container: create/open over [rusqlite](https://github.com/rusqlite/rusqlite) with bundled SQLite, the feature and attribute read and write paths, columnar read and write through Apache Arrow (feature `arrow`), the RTree spatial-index lifecycle, and the `ST_*` SQL functions the index triggers require. |
| [`geopackage-cli`](geopackage-cli) | The `gpkg` binary: inspect, validate, index, repair and copy a file from a shell, plus the tile pyramid commands. |
| [`geopackage-ffi`](geopackage-ffi) | The C ABI: opaque handles over the same library, the Arrow C Data Interface as the data plane, and cargo-c packaging (header, pkg-config file, versioned soname). The one crate in the workspace containing `unsafe`. |
| `geopackage-core/fuzz` | cargo-fuzz targets for the GPB parser and the tile payload probe. |

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
  Both definitions are read back on `Srs`, and a caller can supply either.

## Performance

Measured over three public datasets. Apple M2 Pro, 12 cores, 16 GB, release build,
warm page cache, medians over repeated runs.

| | `buildings` | `rivers` | `admin` |
|---|---|---|---|
| source | [Microsoft Building Footprints](https://github.com/microsoft/USBuildingFootprints), California | [HydroRIVERS](https://www.hydrosheds.org/products/hydrorivers) v1.0, global | [GADM](https://gadm.org/data.html) 4.1, global |
| rows | 11,542,912 | 8,477,883 | 356,508 |
| geometry | Polygon | LineString | MultiPolygon |
| columns, including `fid` and geometry | 4 | 16 | 54 |
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

Reading is bound by bytes, not rows: the columnar path varies between 0.95 to 1.13 GB/s
across three layers whose rows differ by a factor of 37 in size. The index is
the other way round, tracking row count at 42,000 to 581,000 rows/s built and
about 40 bytes per row stored, whatever the geometry. Building it during the
write rather than afterwards between 20% and 47%, because the bulk path reuses the
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

Tiles, over a full web mercator ladder to zoom 7 (21,845 tiles of 4 KiB,
101 MB): about 146,000 tiles/s read by address, 108,000 streamed in matrix
order, and 131,000 written through `write_all`. GDAL reads the same file at
about 2,700 tiles/s, but that is a different operation and not a comparison:
GDAL returns pixels, decoding each PNG, and this crate returns the stored
bytes. See
[the tile write-up](https://github.com/urschrei/geopackage/blob/main/roadmap/benchmarks/2026-07-27-tiles.md).

## Conformance

Files written by this crate are checked against OGC
[ets-gpkg12](https://github.com/opengeospatial/ets-gpkg12): 40 passed, 1
failed, where the failing test's regex hard-codes the GeoPackage 1.2 trigger
set and rejects a correct 1.4 one (no 1.3/1.4 ETS exists). The
[PDOK validator](https://github.com/PDOK/geopackage-validator) reports two
advisory findings, both on deliberate test-file choices. A GDAL round-trip
byte-compares geometry WKB and attribute values after an `ogr2ogr` copy, and
the test corpus includes GDAL-written, QGIS-written and raw-SQLite files.

On the tile side, the ETS Tiles conformance class passes on a pyramid this
crate wrote (24 passed, 0 failed, alongside 17 in Core). A pyramid this crate
writes is read back by `gdalinfo`, a
`GoogleMapsCompatible` pyramid written by `gdal_translate` is read back here,
and the corpus sweep walks every tile of the GDAL- and NGA-written pyramids it
holds, probing each payload against the size its zoom level declares.

## Known limitations

- **Untrusted files can trigger a large allocation.** The `wkb` reader
  pre-allocates from element counts read out of the geometry blob without
  bounding them against the buffer, so a malformed 17-byte GPB blob declaring a
  0xFFFFFFFF-member collection drives a multi-gigabyte allocation. The fix
  belongs upstream in [georust/wkb](https://github.com/georust/wkb); do not
  parse untrusted GeoPackage files. It applies wherever that reader is used,
  which is every geometry but the non-linear ones: those are read by this
  crate's own walker, which allocates nothing and so fails on a bad count
  rather than reserving for it. Tracked in
  [#3](https://github.com/urschrei/geopackage/issues/3).
- **Non-linear curve types are bytes, not geometry.** `CIRCULARSTRING`,
  `COMPOUNDCURVE`, `CURVEPOLYGON`, `MULTICURVE` and `MULTISURFACE` can be
  written, indexed and queried by extent, because their envelopes are computed
  from the WKB directly, arc extents included. What they cannot do is come back
  as a geometry object: `geo-traits`, the interface this crate reads through,
  has no representation for an arc. Iteration itself is unaffected, so
  `Feature::geometry_bytes` hands back the WKB and `Feature::geometry` is the
  one call that fails.
- **Tiles are bytes, not images.** A tile pyramid can be created, read, written
  and validated, but no payload is ever decoded: there is no way to get pixels,
  reproject a pyramid, or build one from a source raster from here.
- **Tiled gridded coverage** (elevation and other gridded data, which stores
  values rather than pictures in its tiles) is not implemented: a TIFF payload
  is rejected on write, and such a pyramid reads as ordinary tiles whose bytes
  make no sense as images.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/urschrei/geopackage/blob/main/LICENSE-APACHE) or
[MIT license](https://github.com/urschrei/geopackage/blob/main/LICENSE-MIT) at your option.
