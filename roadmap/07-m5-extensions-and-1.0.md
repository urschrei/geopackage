# M5: extensions, bindings, hardening → 1.0 RFC

Goal: the extensions that real files actually carry, official language
bindings where demand exists, and an API freeze.

## Extensions (each: read support first, write behind explicit registration)

- [ ] **`gpkg_crs_wkt_1_1`**: `definition_12_063` (WKT2:2015) + `epoch`
      columns on `gpkg_spatial_ref_sys`; written automatically when a caller
      supplies WKT2 (D3).
- [ ] **`gpkg_metadata`**: `gpkg_metadata` + `gpkg_metadata_reference` models,
      typed scopes/reference targets; no XML/profile interpretation — payloads
      are strings.
- [ ] **`gpkg_schema`**: `gpkg_data_columns` (aliases, descriptions, mime) +
      `gpkg_data_column_constraints` (enum/range/glob); surfaced on
      `TableSchema` and *enforced on write* behind an option.
- [ ] **Non-linear geometry types** (CircularString, CompoundCurve,
      CurvePolygon, MultiCurve, MultiSurface): read-through as raw
      WKB (typed as unsupported-by-geo-traits passthrough), extension rows
      honoured; no linearization in core.
- [ ] **Related Tables** (OGC 18-000): read `gpkgext_relations` + mapping
      tables; write for `simple_attributes` and `media` first.
- [ ] Deprecated extensions (geometry type/srs triggers, legacy aspatial):
      tolerate on read, never write, `gpkg validate` flags them.
- [ ] Tiled gridded coverage: re-assess upstream status; implement if
      stabilised, otherwise document the decision and punt to post-1.0.

## Bindings (demand-gated, in this order of presumption)

- [ ] `geopackage-py`: PyO3 + maturin abi3 wheels; API surface =
      open/create + Arrow streams + geopandas `from_arrow`/`to_arrow`
      convenience; benchmark page vs pyogrio.
- [ ] Node via napi-rs or browser via wasm + `serialize` bytes API (D5) —
      pick based on who shows up asking.
- [ ] uniffi (Swift/Kotlin) — pursue when a mobile consumer materialises;
      NGA-maintenance-mode users are the audience.

## Hardening / 1.0 gate

- [ ] OSS-Fuzz onboarding (gpb, WKB fallback, `open()` on arbitrary SQLite
      files, tile matrix parsing).
- [ ] Full-corpus soak: every file in the corpus opened, fully read, indexes
      rebuilt and compared, weekly CI.
- [ ] API review: audit every `pub` item; `#[non_exhaustive]` where growth is
      plausible; error variants stabilised; rusqlite kept out of public API
      except documented escape hatches; MSRV policy written down.
- [ ] Docs: book-style guide (mdBook) — cookbook for the 10 common tasks,
      migration notes from gdal/gpkg-rs/rusqlite-gpkg, FFI integration guide.
- [ ] Performance regression CI (criterion + threshold alerts).
- [ ] 1.0 RFC issue in georust with the frozen API summary; two-release
      deprecation policy adopted.

## Standing items (never "done")

- Track spec changes (a 1.4.x errata or 1.5 draft would land here first:
  watch [opengeospatial/geopackage](https://github.com/opengeospatial/geopackage)).
- Track ETS releases — if an ets for 1.3/1.4 appears, wire it into CI
  alongside ets-gpkg12.
- Track Turso/limbo rtree + vtab parity for a possible second backend
  (explicitly not before 1.0).
