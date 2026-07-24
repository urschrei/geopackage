# Completed work

## Pre-M0: ecosystem survey and planning (2026-07-24)

- Surveyed all known GeoPackage-related Rust crates. Conclusion: no maintained,
  widely used, high-quality pure-Rust implementation exists; full spec coverage
  is GDAL-bindings-or-nothing. The live pure-Rust contenders are
  [`rusqlite-gpkg`](https://github.com/yutannihilation/rusqlite-gpkg)
  (pre-1.0, active, one maintainer) and geozero's `with-gpkg` blob codec
  (deliberately not a container implementation; see
  [geozero#185](https://github.com/georust/geozero/issues/185)).
- Decision: independent georust crate, loose coordination with rusqlite-gpkg
  and geozero; v0.1 scope = features + attributes + RTree; FFI via Arrow C
  Data Interface + plain C ABI; Rust API generic over `geo-traits`.
- Full survey and plan live outside the repo (georust discussion); key facts
  are restated in these roadmap docs where decisions depend on them.

## M0: workspace skeleton ✅

### `geopackage-core` (no-IO spec layer)

- [x] **GPB header codec** (`src/gpb.rs`): magic/version/flags parsing, both
  byte orders, envelope indicators 0–4 (5–7 rejected), empty + extended flags,
  reserved bits tolerated on read / zeroed on write, truncation-safe. Encoder
  always emits little-endian, version 0. Round-trip + garbage-rejection tests.
- [x] **Normative DDL** (`src/ddl.rs`): `gpkg_spatial_ref_sys`, `gpkg_contents`,
  `gpkg_geometry_columns`, `gpkg_extensions` verbatim from
  [Annex C source](https://github.com/opengeospatial/geopackage/blob/master/spec/core/annexes/ddl.adoc);
  the three Requirement-11 SRS seed rows (EPSG:4326 with full WKT1, srs_id 0
  and −1 as `undefined`).
- [x] **RTree trigger SQL** (`src/triggers.rs`): the seven-trigger GeoPackage
  1.4 set (insert, update2, update4, update5, update6, update7, delete)
  verbatim from
  [Annex F.3 source](https://github.com/opengeospatial/geopackage/blob/master/spec/core/annexes/extension_spatialindex.adoc),
  modulo identifier quoting; vtab SQL emitted in the exact double-quoted form
  the OGC ATS string-compares; `TriggerGeneration` classifier
  (V1_4 / PreV1_4 / Mixed / None) + legacy `update1`/`update3` drop statements;
  populate-index statement; `gpkg_extensions` row constants.
- [x] **Identity** (`src/version.rs`): `application_id` GPKG/GP10/GP11,
  `user_version` 102xx/103xx/104xx classification (future minors read as
  newest-known); `src/ident.rs` identifier quoting.
- [x] `#![forbid(unsafe_code)]`, `#![warn(missing_docs)]`, only dep: thiserror.

### `geopackage` (the library)

- [x] `GeoPackage::create`: pragmas (`application_id` 0x47504B47,
  `user_version` 10400, `foreign_keys` ON), core tables + SRS seeds in one
  transaction; refuses to overwrite non-empty files.
- [x] `GeoPackage::open` / `open_read_only` / `from_connection`: pragma
  classification, required-table check, typed `NotAGeoPackage` errors.
- [x] **`ST_*` SQL function registration** on every connection
  (`src/functions.rs`): `ST_IsEmpty`, `ST_MinX/MaxX/MinY/MaxY` from the GPB
  envelope, with a point-only WKB fallback for envelope-less blobs (GDAL
  writes points without envelopes); NULL→NULL; non-point envelope-less blobs
  raise a typed error (full traversal is M1).
- [x] `contents()` introspection (`ContentsEntry`, `ContentsDataType`);
  `connection()` / `into_connection()` escape hatches.

### Verification (all in CI)

- [x] 17 tests across 6 suites, incl. two end-to-end proofs against real
  SQLite: `triggers_maintain_index` (every trigger path: insert, geometry
  update, →NULL, NULL→, rowid move, delete, empty-geometry exclusion) and
  `upsert_works_with_1_4_triggers` (`INSERT … ON CONFLICT DO UPDATE`, the
  exact case that corrupted pre-1.4 GeoPackages), plus an rtree bbox query
  test.
- [x] cargo-fuzz target `gpb_parse` (never-panic + parse→encode→parse
  fidelity with NaN-safe comparison).
- [x] CI: test on ubuntu/macos/windows stable; MSRV 1.85 check; rustfmt;
  clippy `-D warnings` all targets; docs `-D warnings`; fuzz target build.
- [x] Verified green on rust 1.95 stable before first commit.

### Deliberate M0 choices (rationale in [01-design-decisions.md](01-design-decisions.md))

- Crate names `geopackage` / `geopackage-core` (`gpkg` and `gpkg-core` on
  crates.io are owned by cjriley9's dormant project).
- `gpkg_geometry_columns` and `gpkg_extensions` are **not** created at
  `create()` time; they are created lazily with the first feature table /
  extension registration (spec makes them conditional).
- MIT OR Apache-2.0, edition 2024, MSRV 1.85.

# M1: container model + read path (completed 2026-07-24)

Verified by CI run 30101537044 (urschrei/geopackage): 3-OS tests, MSRV 1.95,
clippy `-D warnings` (strict lint set per D12-adjacent config), fmt, docs,
fuzz-build, all green. Detail and per-item history in
[03-m1-read-path.md](03-m1-read-path.md); decisions D3 (CRS vendored subset),
D12 (unsafe policy) recorded during the milestone.

- [x] `srs` module: vendored EPSG WKT1 subset (26 codes + 120 synthesised
      WGS 84 UTM zones) + `srs()`/`srs_list()`/`add_srs()`/`add_epsg_srs()`.
- [x] Schema model: `ColumnType`/`GeometryType`/`ZmFlag` vocabulary,
      `DATE`/`DATETIME` strict + lenient parsing, `GeometryColumn`,
      `TableSchema`, `Value` conversion.
- [x] `GpbGeometry` over georust `wkb` + geo-traits; full WKB envelope
      traversal replacing the M0 point-only `ST_*` fallback; opt-in
      declared-type validation; `gpb_geometry` fuzz target (which found an
      upstream `wkb` OOM, issue #3).
- [x] Read path: `layers()`/`layer()`/`attributes()`, owned-`Feature`
      iteration, rtree-accelerated `features_in(bbox)` with f64 re-filtering
      (property-tested identical to full scan), `select()` passthrough,
      `open_lenient()` with typed warnings.
- [x] Corpus: six committed byte-deterministic fixtures (GDAL 1.2/1.4, QGIS
      4.0.2, raw SQLite) with `ogrinfo -json` snapshot comparison; pinned
      external soak (6740 features, zero errors).
- MSRV raised to 1.95 during closing (libsqlite3-sys `cfg_select!`; see
  [02-ecosystem.md](02-ecosystem.md)).
