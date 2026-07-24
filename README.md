# geopackage

A fast, robust, production-quality Rust implementation of the
[OGC GeoPackage 1.4](https://www.geopackage.org/spec140/) format, intended for
use from Rust and – via a C ABI with the Arrow C Data Interface as the bulk
data plane – from higher-level languages.

**Status: pre-alpha.** APIs will change without notice.

## Workspace

| Crate | Purpose |
|---|---|
| [`geopackage-core`](geopackage-core) | No-IO spec layer: GeoPackage Binary (GPB) header codec, normative table DDL, version-aware RTree trigger SQL, identifier quoting, `application_id`/`user_version` handling. Dependency-light by design so other implementations can share it. |
| [`geopackage`](geopackage) | The library: container create/open over [rusqlite](https://github.com/rusqlite/rusqlite) (`bundled` + `functions`), registration of the `ST_IsEmpty`/`ST_MinX`/… SQL functions required by the spatial index triggers, `gpkg_contents` introspection. Feature/attribute CRUD and the geo-traits API land in M1/M2. |
| `geopackage-core/fuzz` | cargo-fuzz targets (GPB parser). |

## Design notes

- **Sync core on rusqlite.** The RTree extension's triggers call `ST_*`
  functions that must be registered on every writing connection; sqlx-sqlite
  cannot register custom functions, and SQLite is synchronous anyway. Async
  wrappers can sit on top.
- **GeoPackage 1.4 trigger set** (`update5`/`update6`/`update7`) is emitted
  for new indexes; older generations are detected (and will be repairable)
  rather than silently mixed - mixed-generation triggers are a known source
  of file corruption (e.g. UPSERT against pre-1.4 triggers).
- **Escape hatches everywhere:** `GeoPackage::connection()` /
  `from_connection()` expose the underlying rusqlite connection. SQLite is
  the query engine; we do not wrap what we do not need to.

## Roadmap

M1: feature & attribute table read (scan, bbox via rtree, WHERE passthrough),
full WKB envelope computation via georust [`wkb`](https://github.com/georust/wkb).
M2: write path, layer creation, bulk rtree build (GDAL shadow-table
technique), trigger repair — v0.1. M3: GeoArrow `RecordBatch` I/O, C ABI
(`geopackage-ffi`), CLI — v0.2. M4: tiles. M5: extensions (CRS WKT2,
metadata, schema, related tables).

Conformance targets: OGC [ets-gpkg12](https://github.com/opengeospatial/ets-gpkg12),
[PDOK validator](https://github.com/PDOK/geopackage-validator), GDAL/QGIS
round-trip interop.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
