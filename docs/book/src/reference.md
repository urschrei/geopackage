# Reference

This page points to the reference documentation, which is generated from
the source.

## Crates

| Crate | Contents | Reference |
|---|---|---|
| `geopackage` | The main library: files, feature and attribute layers, spatial indexes, tile pyramids, tiled gridded coverages, Arrow I/O. | [docs.rs/geopackage](https://docs.rs/geopackage) |
| `geopackage-core` | Format code with no I/O or SQLite: the GPB geometry codec, table definitions, RTree triggers, identifier quoting, `application_id` and `user_version` handling. | [docs.rs/geopackage-core](https://docs.rs/geopackage-core) |
| `geopackage-ffi` | The C API: opaque handles over the same library, with data passed through the Arrow C Data Interface. Packaged with cargo-c. The only crate that uses `unsafe`. | [docs.rs/geopackage-ffi](https://docs.rs/geopackage-ffi) |
| `geopackage-cli` | The `gpkg` command-line tool. | `gpkg --help` |

## The C API

The header, `geopackage-ffi/include/geopackage.h`, is generated with cbindgen
and checked in. `cargo cinstall -p geopackage-ffi` installs it with a
versioned library and a pkg-config file named `geopackage`.

`geopackage-ffi/examples/` has five C programs. CI compiles each against the
committed header and runs it.

| Program | Contents |
|---|---|
| `smoke.c` | Open a file, list its layers, read one as an Arrow stream, and check that closing fails while the stream is open. |
| `inspect.c` | Open a file leniently and read-only, then check everything before writing: open warnings, layers and tile pyramids, extensions and their support levels, and validation findings. |
| `query.c` | Open a layer with only the columns it needs, resolve its CRS, and read by bounding box, by bounding box and `WHERE` clause with a bound parameter, and by feature ID. |
| `roundtrip.c` | Create a layer from another layer's Arrow schema, and copy the rows in one transaction. |
| `tilepipe.c` | Create a Web Mercator tile pyramid, fill it, and copy it tile by tile with a cursor. |

## `gpkg` commands

| Command | Description |
|---|---|
| `gpkg info <file>` | Version, layers with their schemas and row counts, spatial reference systems, spatial-index state including its trigger generation, tile pyramids, and the extension catalogue. |
| `gpkg validate <file>` | The findings of `GeoPackage::validate`, most severe first, each with its repair advice. Nothing is modified. `--strict` also exits non-zero for warnings. |
| `gpkg index <file> <layer>` | Builds a spatial index on a layer that has none. A layer whose index is present but broken exits non-zero. |
| `gpkg repair <file> [layer]` | Rebuilds legacy and desynchronised indexes onto the 1.4 trigger set, for one named layer or every layer that needs it. A layer with no index is left alone. |
| `gpkg copy <src> <dst>` | Copies the feature and attribute layers of one file into a new one. |
| `gpkg tiles info <file> [pyramid]` | Each pyramid's extent, spatial reference system and zoom ladder, with the tiles stored at each level. |
| `gpkg tiles get <file> <pyramid> <zoom> <column> <row>` | Writes one tile's stored bytes to `--out` or to standard output. |
| `gpkg coverage info <file> [coverage]` | Each tiled gridded coverage's sample type, field, scale and offset, null value, grid-cell convention, spatial reference system, extent and zoom levels, and the payload format of its first tile. |
| `gpkg coverage get <file> <coverage> <zoom> <column> <row>` | Writes one coverage tile's stored bytes to `--out` or to standard output, and prints the values its payload header and ancillary row declare. Samples are not decoded. |

Every command opens the file leniently. `gpkg validate` exits non-zero when a
finding is an error. Every other command exits non-zero only when it fails.

## Format and toolchain

- Specification: [OGC GeoPackage 1.4](https://www.geopackage.org/spec140/).
- Minimum supported Rust version: 1.95.
