# M1 — container model + read path

Goal: read any reasonable GeoPackage's features and attributes, correctly and
fast, including files produced by GDAL, QGIS, and NGA tools.

## Tasks

### Schema model
- [x] `SrsDefinition` lookup module (vendored subset + synthesised WGS 84 UTM
      zones; decision recorded in [02-ecosystem.md](02-ecosystem.md)) +
      `srs()` / `srs_list()` / `add_srs()` / `add_epsg_srs()` on `GeoPackage`.
- [ ] `gpkg_geometry_columns` model: geometry type name (incl. the non-linear
      extension type names, read-only for now), srs_id, z/m flags (0/1/2).
- [ ] `TableSchema` introspection: column names, declared gpkg types
      (BOOLEAN, TINYINT, SMALLINT, MEDIUMINT, INT/INTEGER, FLOAT, DOUBLE/REAL,
      TEXT(maxlen), BLOB(maxlen), DATE, DATETIME, geometry types), pk column
      discovery (`fid` convention but never assumed — read `PRAGMA table_info`).
- [ ] `Value` enum mapping SQLite storage classes ↔ gpkg column types;
      strict DATETIME parsing (`YYYY-MM-DDTHH:MM:SS.SSSZ` — 1.4 kept the
      strict form) with a lenient read option.

### Geometry
- [ ] `GpbGeometry<'a>` wrapper: parsed header + WKB body slice, implementing
      `geo_traits::GeometryTrait` by delegating to `wkb::reader`; `to_geo()`
      behind the `geo-types` feature; raw header/body accessors.
- [ ] Full WKB envelope traversal for the `ST_*` fallback (replace the M0
      point-only fallback). Preferred: upstream an envelope visitor to
      georust `wkb`; interim: local traversal in `geopackage`.
- [ ] Reject/flag geometry column type mismatches (declared POINT, blob says
      LINESTRING) behind a validation option.

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
