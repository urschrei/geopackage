# Introduction

A GeoPackage is an SQLite database with a standard schema for vector
features and raster tiles, defined by the
[OGC](https://www.geopackage.org/spec140/) and read by QGIS, GDAL and most
desktop GIS software. A single file holds the geometry, attributes, spatial
reference systems and spatial indexes, with no sidecar files and no server.

## What this workspace provides

- **`geopackage`**: The main library. Create and open files, read and write
  feature and attribute layers, manage RTree spatial indexes, read and write
  tile pyramids and tiled gridded coverages, and move data in and out as
  Apache Arrow record batches
- **`geopackage-core`**: Format code with no SQLite dependency: the
  GeoPackage Binary (GPB) geometry codec, table definitions, RTree triggers
  and identifier quoting
- **`geopackage-cli`**: The `gpkg` command-line tool, which inspects,
  validates, indexes, repairs and copies files, and reads tiles and coverages
- **`geopackage-ffi`**: A C API over the same library, with opaque handles and
  data passed through the Arrow C Data Interface

## Installation

Add the library to a Rust project:

```console
$ cargo add geopackage
```

Install the command-line tool:

```console
$ cargo install geopackage-cli
```

By default the library links the system SQLite, which must be built with
`SQLITE_ENABLE_RTREE`. Linux distributions and macOS both are. On Linux you
also need the development package (`libsqlite3-dev` on Debian and Ubuntu). To
compile and statically link a vendored SQLite instead, enable the `bundled`
feature. It needs a C compiler, and Windows requires it:

```console
$ cargo add geopackage --features bundled
```

The command-line tool does the opposite: it uses a vendored SQLite by
default, so `gpkg` builds on any host with a C compiler. Pass
`--no-default-features` to link the system library instead.

The minimum supported Rust version is 1.95.

## How this book is organised

- The **tutorial** is a lesson. Work through it once, in order, and you will
  have written and queried a file.
- The **how-to guides** assume you know what you want, and show how to do it.
- The **explanation** chapters describe why the format and this library
  behave as they do.
- The **reference** page points to the full documentation: the API on
  [docs.rs](https://docs.rs/geopackage), the C API in
  `geopackage-ffi/include/geopackage.h`, and the command-line tool in
  `gpkg --help`.

## Planned chapters

These aren't written yet, so they aren't in the table of contents:

- **Migration notes** for users of `gdal`, `gpkg-rs` and `rusqlite-gpkg`:
  the equivalent calls, and where the models differ.
- **A C API guide**: building and linking `geopackage-ffi`, its handle and
  error conventions, and passing data through the Arrow C Data Interface.
  Until then, see the five C programs listed on the
  [Reference](reference.md) page.
- **More how-to guides**, including tile pyramids, attribute tables, metadata
  and the Related Tables Extension.
