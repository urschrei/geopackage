# geopackage

[![CI](https://github.com/urschrei/geopackage/actions/workflows/ci.yml/badge.svg)](https://github.com/urschrei/geopackage/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/geopackage.svg)](https://crates.io/crates/geopackage)
[![docs.rs](https://docs.rs/geopackage/badge.svg)](https://docs.rs/geopackage)

Read and write [OGC GeoPackage 1.4](https://www.geopackage.org/spec140/)
files from Rust: feature and attribute tables, RTree spatial indexes, tile
pyramids, and columnar I/O through Apache Arrow. A command-line tool and a
C ABI are built over the same library.

Pre-1.0: the API changes between minor versions.

- API reference: [docs.rs](https://docs.rs/geopackage)
- Book, with tutorial, how-to guides and explanation: [urschrei.github.io/geopackage](https://urschrei.github.io/geopackage/)
- [Changelog](CHANGELOG.md)

> [!NOTE]
> These crates are a testbed for LLM-assisted Rust development and have been
> produced with the substantial assistance of Claude's Fable and Opus 5 models.
> Issues will be answered by the author.

## Install

```toml
[dependencies]
geopackage = "0.9"
```

The default build links the system SQLite, which must include the RTree
module (`libsqlite3-dev` on Debian/Ubuntu, `sqlite-devel` on Fedora; macOS
needs nothing beyond the SDK). The `bundled` feature compiles and links a
vendored SQLite instead, needs a C compiler, and is the only option on
Windows. The `arrow` feature adds the columnar read and write paths. The
minimum supported Rust version is 1.95.

The command-line tool vendors SQLite by default:

```sh
cargo install geopackage-cli
```

## Example

Create a file, declare a point layer, write two features, and query by
bounding box. Writing accepts any `geo_traits::GeometryTrait` implementor,
which includes every `geo-types` geometry.

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

// Served by the RTree index that `create_layer` builds.
for feature in layer.features_in(BoundingBox::new(-7.0, 53.0, -6.0, 54.0))? {
    let feature = feature?;
    let geom = feature.geometry()?.and_then(|g| g.to_geo());
    println!("{:?} at {:?}", feature.value("name"), geom);
}
```

Tiles, Arrow, extensions, validation and configuration are covered in the
[crate documentation](https://docs.rs/geopackage). Runnable programs are in
[`geopackage/examples`](geopackage/examples).

## Workspace

| Crate | Contents | Reference |
|---|---|---|
| [`geopackage`](geopackage) | The container: files, layers, spatial indexes, tile pyramids, Arrow I/O. | [docs.rs](https://docs.rs/geopackage) |
| [`geopackage-core`](geopackage-core) | Format primitives with no SQLite dependency: the GPB codec, table DDL, RTree trigger SQL. | [docs.rs](https://docs.rs/geopackage-core) |
| [`geopackage-cli`](geopackage-cli) | `gpkg`: `info`, `validate`, `index`, `repair`, `copy` and `tiles`. | `gpkg --help` |
| [`geopackage-ffi`](geopackage-ffi) | The C ABI: opaque handles, the Arrow C Data Interface as data plane, packaged with [cargo-c](https://github.com/lu-zero/cargo-c). Five C programs in [`examples`](geopackage-ffi/examples) are compiled against the committed header by CI. | [docs.rs](https://docs.rs/geopackage-ffi), [`include/geopackage.h`](geopackage-ffi/include/geopackage.h) |

`geopackage-ffi` is the only crate that contains `unsafe`.

## Scope

- CRS definitions are stored, not applied: there is no PROJ dependency and no
  coordinate transformation.
- Tiles are stored and validated as bytes. The crate does not decode images,
  reproject a pyramid, or build one from a raster. Tiled gridded coverage is
  not currently implemented.
- Non-linear curve types (`CIRCULARSTRING` and the rest) can be written,
  indexed and queried by extent, but do not read back as geometry objects;
  `Feature::geometry_bytes` returns their WKB.

## Conformance and performance

Files written by this crate pass the OGC
[ets-gpkg12](https://github.com/opengeospatial/ets-gpkg12) suite except one
test whose regex hard-codes the 1.2 trigger set, and the Tiles conformance
class in full. Round trips through GDAL byte-compare geometry and attributes.
Details are in the [changelog](CHANGELOG.md) and
[`roadmap/08-testing-conformance.md`](roadmap/08-testing-conformance.md).

Benchmarks on three multi-gigabyte datasets, with method and caveats, are in
[`roadmap/benchmarks`](roadmap/benchmarks); the scripts that produce them are
in [`scripts`](scripts).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
