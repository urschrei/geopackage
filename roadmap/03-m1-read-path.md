# M1 — container model + read path

Goal: read any reasonable GeoPackage's features and attributes, correctly and
fast, including files produced by GDAL, QGIS, and NGA tools.

## Tasks

### Schema model
- [x] `SrsDefinition` lookup module (vendored subset + synthesised WGS 84 UTM
      zones; decision recorded in [02-ecosystem.md](02-ecosystem.md)) +
      `srs()` / `srs_list()` / `add_srs()` / `add_epsg_srs()` on `GeoPackage`.
- [x] `gpkg_geometry_columns` model: geometry type name (incl. the non-linear
      extension type names, read-only for now), srs_id, z/m flags (0/1/2).
      `GeometryColumn` + `ZmFlag` (core) + `geometry_column()` /
      `geometry_columns()`; a missing table reads as no rows.
- [x] `TableSchema` introspection: column names, declared gpkg types
      (BOOLEAN, TINYINT, SMALLINT, MEDIUMINT, INT/INTEGER, FLOAT, DOUBLE/REAL,
      TEXT(maxlen), BLOB(maxlen), DATE, DATETIME, geometry types), pk column
      discovery (`fid` convention but never assumed — read `PRAGMA table_info`).
      `TableSchema` + `Column` + `table_schema()`; composite pks surfaced via
      `primary_key_columns()`, single-pk convenience via `primary_key()`.
- [x] `Value` enum mapping SQLite storage classes ↔ gpkg column types;
      strict DATETIME parsing (`YYYY-MM-DDTHH:MM:SS.SSSZ` — 1.4 kept the
      strict form) with a lenient read option (`ConversionOptions` /
      `DateTimeParsing`). Non-geometry only; storage/type mismatch is a typed
      error. Reachable via the `column_values()` building block.
- [ ] Revisit `Value` conversion leniencies once a validation option exists:
      `BOOLEAN` currently maps any non-zero INTEGER to `true`, and a whole
      number stored with integer affinity in a `FLOAT`/`DOUBLE` column is
      widened to `Value::Float` rather than rejected.
- [ ] Fold the `column_values()` building block into the streaming feature/
      attribute read path when it lands (it was added here only to make `Value`
      conversion reachable and testable ahead of `layer()`/`features()`).

### Geometry
- [x] `GpbGeometry<'a>` wrapper (`geopackage_core::geometry`): parsed header +
      WKB body slice, implementing `geo_traits::GeometryTrait` by delegating to
      `wkb::reader::Wkb`; `to_geo()` behind the `geo-types` feature; raw
      header/body accessors. Depends on georust `wkb` 0.9 and `geo-traits` 0.3.
- [x] Full WKB envelope traversal for the `ST_*` fallback (replaced the M0
      point-only fallback). Placement decision: the wrapper and traversal live
      in **`geopackage-core`** (`GpbGeometry::xy_envelope`/`is_empty`), keeping
      the fuzz workspace free of the SQLite dependency; coordinates are visited
      through the `geo-traits` interface rather than a hand-rolled WKB walker.
      `geopackage/src/functions.rs` now bounds envelope-less blobs of any type
      the `wkb` crate can read (all byte orders, any Z/M), and `ST_IsEmpty`
      reports emptiness (header flag, NaN-point convention, zero-element
      geometries). The eventual home is an upstreamed `gpb` feature in georust
      `wkb` itself (tracked in [02-ecosystem.md](02-ecosystem.md)); until then
      this is ours.
- [ ] Curve-type envelope support: a WKB body whose type the `wkb` crate
      cannot read — the non-linear curve types (`CIRCULARSTRING`,
      `CURVEPOLYGON`, `MULTICURVE`, …) and the abstract `CURVE`/`SURFACE` —
      makes the `ST_*` functions return a typed SQL error rather than an
      envelope, so such a geometry cannot be inserted into an rtree-indexed
      table. Needs curve support in georust `wkb` upstream.
- [ ] **wkb upstream (fuzz finding):** `wkb` 0.9.2 pre-allocates from an
      untrusted element count without bounding it against the remaining buffer
      (`Vec::with_capacity(num_geometries)` in `reader::GeometryCollection`,
      `Vec::with_capacity(num_rings)` in `reader::Polygon`). A 17-byte GPB blob
      whose body is a `GEOMETRYCOLLECTION` declaring `0xFFFFFFFF` members drives
      a ~240 GB allocation (out-of-memory), found by the `gpb_geometry` fuzz
      target. This is a `wkb` bug, not the wrapper (our code never panics); fix
      upstream by growing the vec on demand or capping capacity by the bytes
      left. Until fixed, the `gpb_geometry` fuzz target exposes this OOM class.
- [ ] Reject/flag geometry column type mismatches (declared POINT, blob says
      LINESTRING) behind a validation option. The primitive exists
      (`geometry::geometry_type_matches` + `wkb_geometry_type`, and
      `GpbGeometry::matches_declared`), modelling the spec's instantiable-type
      rules (exact match; `GEOMETRY` accepts anything; `GEOMETRYCOLLECTION`
      accepts only collection types) and classifying curve-type bodies the
      `wkb` reader cannot parse. Still needs wiring into an open/read
      validation option (part of the read-API chunk).

### Read API
- [ ] `gpkg.layer(name) -> Layer` (features) / `gpkg.attributes(name)`;
      layers enumerate from `gpkg_contents` joined with
      `gpkg_geometry_columns`.
- [ ] `layer.features()` streaming iterator over prepared statement;
      `Feature { fid, geometry, values }` with by-name and by-index access.
- [ ] `layer.features_in(bbox)`: uses `rtree_<t>_<c>` when present
      (`classify_triggers` ≠ None + vtab exists), falls back to full scan with
      envelope filter; identical results property-tested.
- [ ] `layer.select(where_clause, params)` passthrough.
- [ ] `open_lenient()`: tolerate GP10/GP11 files, missing
      `gpkg_geometry_columns` for attribute-only files, wrong-case table
      names; collect warnings on the handle rather than failing.

### Corpus & verification (details in [08-testing-conformance.md](08-testing-conformance.md))
- [ ] Fixture corpus: files written by GDAL (≥3.6 and 3.11+/1.4), QGIS, NGA
      sample data; commit small ones, script-fetch big ones.
- [ ] Round-trip comparison test vs `ogrinfo -json` output for each corpus
      file (feature counts, geometry types, first/last feature equality).
- [ ] Property test: `features_in(bbox)` ≡ full-scan filter.

## Acceptance criteria

1. Every corpus file opens and iterates fully; geometry + attribute values
   match GDAL's read of the same file.
2. bbox queries use the rtree when present (assert via `EXPLAIN QUERY PLAN`)
   and return provably identical results either way.
3. `ST_MinX` & co. work on envelope-less non-point blobs (fallback complete).
4. No `unsafe`, clippy/fmt/docs clean, fuzz target extended to the new
   surface (`GpbGeometry` over arbitrary bodies must never panic).
