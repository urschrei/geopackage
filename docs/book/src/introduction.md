# Introduction

A GeoPackage is an SQLite database with a standardised schema for vector
features and raster tiles, defined by the
[OGC](https://www.geopackage.org/spec140/) and read by QGIS, GDAL and most of
the desktop GIS world. One file contains the geometry, the attributes, the
spatial reference system definitions and the spatial indexes, with no sidecar
files and no server.

## What this workspace provides

- **`geopackage`**: the container. Create and open files, read and write
  feature and attribute layers, manage the RTree spatial index, read and write
  tile pyramids, and read or write columnar data through Apache Arrow.
- **`geopackage-core`**: the format primitives, with no SQLite dependency: the
  GeoPackage Binary (GPB) header codec, the normative table DDL, the
  version-aware RTree trigger SQL, and identifier quoting.
- **`geopackage-cli`**: the `gpkg` binary, which inspects, validates, indexes,
  repairs and copies files from a shell.
- **`geopackage-ffi`**: the C ABI over the same library, with opaque handles
  and the Arrow C Data Interface as its data plane.

## Installation

Add the library to a Rust project:

```console
$ cargo add geopackage
```

Install the command-line tool:

```console
$ cargo install geopackage-cli
```

SQLite is bundled and built from source, so a C compiler is required and there
is no system SQLite dependency. The minimum supported Rust version is 1.95.

## How this book is organised

The four kinds of page here answer four different questions:
The **tutorial** is a lesson: work through it once, in
order, and you will have written and queried a file. The **how-to guides**
assume you already know what you want and show you how to get it. The
**explanation** chapters cover why parts of the format and this library behave as they do.
The **reference** page is a short signpost: the full API reference lives on
[docs.rs](https://docs.rs/geopackage), the C API in the committed header at
`geopackage-ffi/include/geopackage.h`, and the command-line surface behind
`gpkg --help`.

## Planned chapters

Not written yet, and so deliberately absent from the table of contents:

- **Migration notes** for callers arriving from `gdal`, `gpkg-rs` and
  `rusqlite-gpkg`: what the equivalent call is, and where the models differ.
- **An FFI integration guide**: building and linking `geopackage-ffi`, the
  handle and error conventions, and the Arrow C Data Interface as the data
  plane. Until it exists, the five compiled C programs listed on the
  [Reference](reference.md) page are the worked material.
- **More how-to guides**, including tile pyramids, attribute tables, metadata
  and the Related Tables Extension.
