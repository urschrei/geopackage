# Reference

This page points at the reference material rather than reproducing it. The
authoritative API documentation is generated from the source.

## Crates

| Crate | Contents | Reference |
|---|---|---|
| `geopackage` | The container: create and open files, feature and attribute layers, the spatial-index lifecycle, tile pyramids, columnar I/O through Apache Arrow. | [docs.rs/geopackage](https://docs.rs/geopackage) |
| `geopackage-core` | Format primitives with no I/O or SQLite: the GPB header codec, the normative table DDL, version-aware RTree trigger SQL, identifier quoting, `application_id` and `user_version` handling. | [docs.rs/geopackage-core](https://docs.rs/geopackage-core) |
| `geopackage-ffi` | The C ABI: opaque handles over the same library, the Arrow C Data Interface as the data plane, cargo-c packaging. The only crate in the workspace containing `unsafe`. | [docs.rs/geopackage-ffi](https://docs.rs/geopackage-ffi) |
| `geopackage-cli` | The `gpkg` binary. | `gpkg --help` |

## The C API

The header is committed at `geopackage-ffi/include/geopackage.h` and generated
from the crate with cbindgen. `cargo cinstall -p geopackage-ffi` installs it
alongside a versioned library and a pkg-config file named `geopackage`.

Five C programs ship in `geopackage-ffi/examples/`. Each is compiled against
the committed header and run by CI.

| Program | Contents |
|---|---|
| `smoke.c` | Open a file, walk its layers, pull a layer through the Arrow stream, observe the close-while-in-use failure. |
| `inspect.c` | Tolerant read-only open, the warnings it collected, layer and pyramid enumeration, the extensions catalogue with support levels, and validation. |
| `query.c` | A projected open, the CRS resolved to a definition, and three shapes of filtered read: a bounding box, the box narrowed by a `WHERE` clause with a bound parameter, and one feature by id. |
| `roundtrip.c` | A destination layer created from the source stream's own Arrow schema, filled inside one transaction. |
| `tilepipe.c` | A pyramid created from nothing on the web mercator quad, filled, and copied tile by tile through the lending cursor. |

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

Every command opens the file leniently. `gpkg validate` exits non-zero when a
finding is an error; all other commands exit non-zero on failure only.

## Format and toolchain

- Specification: [OGC GeoPackage 1.4](https://www.geopackage.org/spec140/).
- Minimum supported Rust version: 1.95.
