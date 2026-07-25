# M5: extensions, bindings, hardening → 1.0 RFC

Goal: the extensions that real files actually carry, official language
bindings where demand exists, and an API freeze.

## Extensions (each: read support first, write behind explicit registration)

- [x] **`gpkg_crs_wkt_1_1`**: `definition_12_063` + `epoch` columns on
      `gpkg_spatial_ref_sys`, added and registered on demand. Brought forward
      from M5 by issue #23: `add_epsg_srs` needs it for any code with no WKT1
      form. Write support only so far, and the definitions come from
      `epsg-utils` rather than from a caller supplying WKT2 (D3), so the read
      side and the caller-supplied path remain M5 work.
- [ ] **`gpkg_metadata`**: `gpkg_metadata` + `gpkg_metadata_reference` models,
      typed scopes/reference targets; no XML/profile interpretation; payloads
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
- [ ] Node via napi-rs or browser via wasm + `serialize` bytes API (D5):
      pick based on who shows up asking.
- [ ] uniffi (Swift/Kotlin): pursue when a mobile consumer materialises;
      NGA-maintenance-mode users are the audience.

## Hardening / 1.0 gate

- [ ] OSS-Fuzz onboarding (gpb, WKB fallback, `open()` on arbitrary SQLite
      files, tile matrix parsing).
- [ ] Full-corpus soak: every file in the corpus opened, fully read, indexes
      rebuilt and compared, weekly CI.
- [ ] API review: audit every `pub` item; `#[non_exhaustive]` where growth is
      plausible; error variants stabilised; rusqlite kept out of public API
      except documented escape hatches; MSRV policy written down.
- [ ] Docs: book-style guide (mdBook): cookbook for the 10 common tasks,
      migration notes from gdal/gpkg-rs/rusqlite-gpkg, FFI integration guide.
- [ ] Performance regression CI (criterion + threshold alerts).
- [ ] **Revisit the D8 bulk-build gate.** Every bulk index build verifies itself
      before it is trusted: a bijection and containment check of the written
      index against the accumulated envelopes, plus `rtreecheck` over the tree,
      with a fallback to the triggered build on any anomaly. That is about **45%
      of the build** (~745 ms of a ~1593 ms build at 1M points), and GDAL's
      builder runs no equivalent, so without it we would be faster than GDAL
      rather than level with it. The cost is the right call while
      `geopackage/src/packed.rs` is new: it writes an RTree by hand into a format
      SQLite does not document as an interface. The 1.0 question is whether the
      packer has enough history by then to make the gate opt-in, or to keep only
      the cheaper half. Decide it on the evidence at the time rather than on the
      benchmark alone, and if it is relaxed, keep a way to turn it back on. See
      [benchmarks/2026-07-24-gdal-like-for-like.md](benchmarks/2026-07-24-gdal-like-for-like.md).
- [ ] 1.0 RFC issue in georust with the frozen API summary; two-release
      deprecation policy adopted.

## Standing items (never "done")

- Track spec changes (a 1.4.x errata or 1.5 draft would land here first:
  watch [opengeospatial/geopackage](https://github.com/opengeospatial/geopackage)).
- Track ETS releases: if an ets for 1.3/1.4 appears, wire it into CI
  alongside ets-gpkg12.
- Track Turso/limbo rtree + vtab parity for a possible second backend
  (explicitly not before 1.0).
