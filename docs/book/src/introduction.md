# Introduction

GeoPackage is an [OGC standard](https://www.geopackage.org/spec140/) for
storing geospatial data in a single SQLite database file. A GeoPackage can hold
vector features, attribute tables and raster tiles, together with the spatial
reference systems and spatial indexes that go with them. QGIS, GDAL and most
other desktop GIS software can read it, and because it is just a file, there is
no server to run.

This book shows you how to read and write GeoPackage files from Rust with the
`geopackage` crate, and how to use the `gpkg` command-line tool built on it.

## The crates

The project is split into four crates:

- `geopackage` is the library most people want. It creates and opens files,
  reads and writes feature and attribute layers, manages spatial indexes,
  handles tile pyramids and tiled gridded coverages, and can move data in and
  out as Apache Arrow record batches.
- `geopackage-core` contains the parts of the format that don't need a
  database, such as the geometry encoding and the table definitions. You only
  need it directly if you're working with the format without SQLite.
- `geopackage-cli` provides `gpkg`, a command-line tool for inspecting,
  validating, indexing, repairing and copying files, and for reading tiles and
  coverages.
- `geopackage-ffi` is a C library over the same code, which passes data
  through the Arrow C Data Interface.

## Installation

To use the library in a Rust project:

```console
$ cargo add geopackage
```

By default, the library links against your system's SQLite, which must include
the RTree module (`SQLITE_ENABLE_RTREE`). The SQLite that ships with macOS and
with Linux distributions has it. On Linux you also need the development
package, such as `libsqlite3-dev` on Debian and Ubuntu.

If you'd rather not depend on the system SQLite, or you're on Windows, which
doesn't have one, enable the `bundled` feature. It compiles its own copy of
SQLite, so you'll need a C compiler:

```console
$ cargo add geopackage --features bundled
```

To install the command-line tool:

```console
$ cargo install geopackage-cli
```

The tool uses the bundled SQLite by default. To link the system library
instead, pass `--no-default-features`.

All the crates need Rust 1.95 or newer.

## How this book is organised

The book has four parts:

- The [tutorial](tutorial/first-geopackage.md) walks you through creating and
  querying your first GeoPackage. Start there if you're new to the library.
- The [how-to guides](how-to/add-spatial-index.md) show you how to do specific
  tasks, such as adding a spatial index or copying features between files.
- The [explanation](explanation/extent.md) chapters describe how parts of the
  format work, and why the library behaves as it does.
- The [reference](reference.md) page links to the API documentation on
  [docs.rs](https://docs.rs/geopackage), the C header, and a summary of the
  `gpkg` commands.

## Still to come

These chapters haven't been written yet:

- Migration notes for people coming from `gdal`, `gpkg-rs` or `rusqlite-gpkg`,
  showing the equivalent calls and where the models differ.
- A guide to the C library: building and linking `geopackage-ffi`, its
  conventions for handles and errors, and passing data through the Arrow C
  Data Interface. Until then, the five example programs listed on the
  [reference](reference.md) page are the best place to start.
- More how-to guides, covering tile pyramids, attribute tables, metadata and
  the Related Tables Extension.
