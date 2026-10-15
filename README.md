# geopackage

[![CI](https://github.com/urschrei/geopackage/actions/workflows/ci.yml/badge.svg)](https://github.com/urschrei/geopackage/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/geopackage.svg)](https://crates.io/crates/geopackage)
[![docs.rs](https://docs.rs/geopackage/badge.svg)](https://docs.rs/geopackage)

Read and write [OGC GeoPackage 1.4](https://www.geopackage.org/spec140/)
files in Rust: feature and attribute tables, RTree spatial indexes, tile
pyramids, tiled gridded coverages, and Apache Arrow I/O. The workspace also
includes a command-line tool and a C API built on the same library.

Until 1.0, the API may change between minor versions.

- API reference: [docs.rs](https://docs.rs/geopackage)
- User guide, with a tutorial and how-to guides: [urschrei.github.io/geopackage](https://urschrei.github.io/geopackage/)
- [Changelog](CHANGELOG.md)

> [!NOTE]
> These crates are a testbed for LLM-assisted Rust development and have been
> produced with the substantial assistance of Claude's Fable and Opus 5 models.
> Issues will be answered by the author.

## Install

```toml
[dependencies]
geopackage = "0.11"
```

By default the crate links the system SQLite, which must include the RTree
module. Install `libsqlite3-dev` on Debian or Ubuntu, or `sqlite-devel` on
Fedora; macOS needs nothing extra. To use a vendored SQLite instead, enable the
`bundled` feature, which needs a C compiler. Windows requires `bundled`.

With `bundled`, set these SQLite compile flags in your `.cargo/config.toml`:

```toml
[env]
LIBSQLITE3_FLAGS = "-USQLITE_ENABLE_MEMORY_MANAGEMENT -DSQLITE_DEFAULT_MEMSTATUS=0"
```

Without them, all connections in a process share one SQLite page cache behind
one mutex, and the threads of a threaded Arrow read wait for each other.

The `arrow` feature adds Arrow I/O. The minimum supported Rust version is 1.95.

The command-line tool uses a vendored SQLite by default. Each GitHub release has
binaries for Linux, macOS and Windows, which `cargo binstall` installs:

```sh
cargo binstall geopackage-cli
```

To build it from crates.io instead, set the SQLite compile flags in the
environment:

```sh
LIBSQLITE3_FLAGS="-USQLITE_ENABLE_MEMORY_MANAGEMENT -DSQLITE_DEFAULT_MEMSTATUS=0" \
    cargo install geopackage-cli
```

## Example

Create a file, define a point layer, write two features, and query by
bounding box. Writes accept any `geo_traits::GeometryTrait` implementor,
including all `geo-types` geometries.

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

let layer = gpkg.layer("cities")?;
layer.write_all(
    vec![
        NewFeature::new(Point::new(-6.26, 53.35), vec![Value::Text("Dublin".into())]),
        NewFeature::new(Point::new(-0.13, 51.51), vec![Value::Text("London".into())]),
    ],
    0,
)?;

// Uses the RTree index that `create_layer` builds by default.
for feature in layer.features_in(BoundingBox::new(-7.0, 53.0, -6.0, 54.0))? {
    let feature = feature?;
    let geom = feature.geometry()?.and_then(|g| g.to_geo());
    println!("{:?} at {:?}", feature.value("name"), geom);
}
```

The [crate documentation](https://docs.rs/geopackage) covers tiles,
coverages, Arrow, extensions, validation and configuration.
[`geopackage/examples`](geopackage/examples) has runnable programs.

## Workspace

| Crate | Contents | Reference |
|---|---|---|
| [`geopackage`](geopackage) | The main library: files, layers, spatial indexes, tiles, coverages, Arrow I/O. | [docs.rs](https://docs.rs/geopackage) |
| [`geopackage-core`](geopackage-core) | Format code with no SQLite dependency: the geometry codec, table definitions, RTree triggers. | [docs.rs](https://docs.rs/geopackage-core) |
| [`geopackage-cli`](geopackage-cli) | The `gpkg` tool: `info`, `validate`, `index`, `repair`, `copy`, `tiles` and `coverage`. | `gpkg --help` |
| [`geopackage-ffi`](geopackage-ffi) | The C API: opaque handles, with data passed through the Arrow C Data Interface. Packaged with [cargo-c](https://github.com/lu-zero/cargo-c). CI builds and runs the C programs in [`examples`](geopackage-ffi/examples) against the committed header. | [docs.rs](https://docs.rs/geopackage-ffi), [`include/geopackage.h`](geopackage-ffi/include/geopackage.h) |

Only `geopackage-ffi` uses `unsafe`.

## Limitations

- Coordinate reference systems are stored but not applied: there is no PROJ
  dependency and no reprojection.
- Tiles are stored and validated as bytes. Images aren't decoded, so the
  crate can't render tiles, reproject a pyramid, or build one from a raster.
- Tiled gridded coverages (OGC 17-066r2) are fully supported, but their
  samples aren't decoded either. Payloads are checked against the extension's
  TIFF and PNG rules using their headers, and a coverage's scale, offset and
  null value can be applied to samples you decode yourself. Per-tile
  statistics are therefore whatever you supply, and Requirement 21 (no NaN or
  infinite values) can't be enforced.
- Curved geometry types (`CIRCULARSTRING` and others) can be written, indexed
  and queried by extent, but can't be read back as geometry objects.
  `Feature::geometry_bytes` returns their WKB.

## Conformance and performance

Files written by this crate pass the OGC
[ets-gpkg12](https://github.com/opengeospatial/ets-gpkg12) test suite,
including the whole Tiles conformance class, with one exception: a test whose
regex hard-codes the GeoPackage 1.2 triggers. Geometry and attributes survive
a round trip through GDAL byte for byte. See the [changelog](CHANGELOG.md) and
[`roadmap/08-testing-conformance.md`](roadmap/08-testing-conformance.md) for
details.

[The benchmarks](roadmap/benchmarks/README.md) compare reads, writes, index
builds, queries and `gpkg copy` with GDAL on subsets of three published
datasets.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
