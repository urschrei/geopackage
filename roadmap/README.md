# Development roadmap

Roadmap for the georust `geopackage` workspace: a production-quality, pure-Rust
(modulo SQLite, system-linked or bundled) implementation of [OGC GeoPackage 1.4](https://www.geopackage.org/spec140/),
usable from Rust and from higher-level languages via a C ABI with the Arrow C
Data Interface as the bulk data plane.

## Documents

| File | Contents |
|---|---|
| [00-completed.md](00-completed.md) | What exists today (M0) and how it was verified |
| [01-design-decisions.md](01-design-decisions.md) | Decision record: driver, API shape, CRS policy, WAL, Wasm posture, including lessons from prior art |
| [02-ecosystem.md](02-ecosystem.md) | Dependency map, georust coordination, code-adaptation policy (wkb, geozero, rstar, arrow, …) |
| [03-m1-read-path.md](03-m1-read-path.md) | M1: feature/attribute read path, full WKB envelopes |
| [04-m2-write-rtree.md](04-m2-write-rtree.md) | M2: write path, layer creation, bulk rtree build → **v0.1** |
| [05-m3-arrow-ffi.md](05-m3-arrow-ffi.md) | M3: GeoArrow batches, C ABI, CLI → **v0.2** |
| [06-m4-tiles.md](06-m4-tiles.md) | M4: tile pyramids → **v0.6** |
| [07-m5-extensions-and-1.0.md](07-m5-extensions-and-1.0.md) | M5: extensions, then the CLI and C ABI M3 left unbuilt, then the API freeze |
| [08-testing-conformance.md](08-testing-conformance.md) | Cross-cutting: conformance harness, fuzzing, benchmarks, corpus |
| [09-c-api-sense-check.md](09-c-api-sense-check.md) | The C surface compared against GDAL's C API and QGIS's provider needs, with findings and the decision |

## Status snapshot (2026-08-02)

| Milestone | State |
|---|---|
| M0 workspace and codec | Complete |
| M1 read path | Complete |
| M2 write path and RTree | Complete, released as v0.1.0 |
| M3 Arrow, C ABI, CLI | Complete. Arrow landed in v0.2.0; the C ABI and CLI were built as M5 phases 8 and 9 and released in v0.6.0, which is what closes acceptance criteria 6 and 7. |
| M4 tiles | Complete, released as v0.6.0. |
| M5 extensions, then CLI and C ABI, then the freeze | **In progress.** Phases 0 to 9 done; phase 10, the API freeze, is what remains. |

Released: v0.1.0, v0.1.1, v0.1.2 (2026-07-24), v0.2.0 (2026-07-25), v0.3.0,
v0.4.0, v0.5.0 (2026-07-26), v0.6.0 (2026-07-29), v0.7.0 and v0.7.1
(2026-08-02), v0.8.0 (2026-08-06). Workspace version is 0.8.0.
No release is planned for the rest of M5: its phases are an order of work, not a
publication schedule.

**601 tests pass** locally across the workspace with all features, 573 on the
system-linked default (`geopackage-cli` excluded), plus 44 doctests, with
clippy clean under the strict lint set. CI runs the same across 3 OSes at MSRV
1.95.

### Current focus: M5

- **Done.** Phase 0 (the Windows flake), phase 1 (the extension catalogue as
  public API, a prerequisite for the rest), phase 2 (`gpkg_crs_wkt_1_1` read
  side), phase 3 (`gpkg_schema`), phase 4 (`gpkg_metadata`), phase 5 (Related
  Tables), phase 6 (non-linear geometry), phase 7 (`GeoPackage::validate`),
  phase 8 (`geopackage-cli`) and phase 9 (`geopackage-ffi`).
- **Phase 6 was revised mid-milestone.** It was planned as passthrough with no
  envelopes and no indexing. Requirement 78 says the `ST_*` functions shall work
  on these types, and PostGIS and GDAL both compute exact arc envelopes, so the
  plan changed: `geopackage-core::curve` walks WKB directly and computes arc
  extrema exactly, and curve layers index like any other. The originally
  planned "caller supplies the envelope" escape hatch is not needed. Both of its
  trailing items are now settled: member types are registered as GDAL registers
  them, and reading a curve back as a geometry object is closed rather than
  deferred, since `geo-traits` has no representation for an arc and
  `Feature::geometry_bytes` is the answer rather than a stopgap.
- **Phase 10, the API freeze, is what remains**, and it is last by construction:
  freezing before the CLI and FFI had exercised the surface would have frozen
  something nothing had used from outside. Its largest item is the reading half
  of the API review. `scripts/public_api.sh` records every exported item and CI
  diffs it, which is the mechanised half; deciding which of those items should
  stay public, and which want `#[non_exhaustive]`, has not started.
- **The C API sense-check is done** (2026-08-02, in
  [09-c-api-sense-check.md](09-c-api-sense-check.md)): the C surface compared
  against GDAL's C API and against what QGIS would need to sit on it, since
  it had only ever been validated against the Rust API it mirrors. The
  headline is that nothing found blocks the freeze: every finding is
  additive. Five items follow from it (an attribute-filtered Arrow read with
  its C entry point, projection, the SRS definition, the fail-fast pair,
  a pyramid cursor); schema evolution and layer deletion are deferred past
  1.0; capability probing, `ExecuteSQL`, row reads and decoded pixels are
  recorded omissions.

### Milestone history

- **M0 complete**: workspace (`geopackage-core`, `geopackage`, fuzz), GPB header
  codec, normative DDL + 1.4 RTree trigger set (verbatim from spec source),
  container create/open with validation, `ST_*` function registration, CI
  (3 OSes, clippy `-D warnings`, fmt, docs, fuzz-build).
- **M1 complete** (criteria verified in CI, 2026-07-24): schema model, geometry
  wrapper, and the feature/attribute **read path**: `layers()`/`layer()`/
  `attributes()` handles, `features()`/`features_in(bbox)` (rtree-accelerated
  with a full-scan fallback, results property-tested identical) and the
  `select()` WHERE passthrough, plus `open_lenient()` with typed open warnings.
  The corpus chunk adds the committed fixture corpus
  (`geopackage/tests/fixtures/`, generated by `scripts/generate_fixtures.py`)
  and the `ogrinfo`-comparison suite (`geopackage/tests/corpus.rs`), plus a
  sha256-pinned external soak. It covers GDAL-written, QGIS-written,
  raw-SQLite, and fetched third-party files. MSRV was raised 1.85 to 1.95 by the
  first CI run: libsqlite3-sys 0.38 (via rusqlite 0.40) uses `cfg_select!`,
  stable only from 1.95, and declares no `rust-version`. All four acceptance
  criteria pass in CI (run 30101537044, 2026-07-24); the GitHub milestone is
  closed and deferred follow-ups are issues #1 to #6.
- **M2 complete, released as v0.1.0** (2026-07-24): the full write path (layer
  creation + DDL, `FeatureWriter`/`write_all`, DATETIME serialisation), the
  RTree spatial-index lifecycle, the D8 bulk shadow-table index build with its
  gate + triggered fallback, the D4 journal/durability work, the Hegel property
  tests, criterion benchmarks, and the external-validation harness (ets-gpkg12,
  PDOK, ogrinfo, GDAL round-trip). All five acceptance criteria are annotated in
  [04-m2-write-rtree.md](04-m2-write-rtree.md).
- **M3 partial** (Arrow released as v0.2.0): GeoArrow batches read and write
  through `arrow-array`/`arrow-schema` behind the non-default `arrow` feature.
  The C ABI and CLI never started; see M5 phases 8 and 9.
- **M4 complete in code, unreleased**: tile pyramids, the second GeoPackage data
  type. Payloads stay opaque, so no image codec is pulled in; what is read is
  each payload's header, which is how a tile of the wrong size or format is
  rejected on write. See [06-m4-tiles.md](06-m4-tiles.md).

### Live caveats

- **GDAL-parity performance target**: met, on a like-for-like measurement, after
  being ticked once in error and withdrawn. The first tick rested on timing
  `ogr2ogr`, whose figure also covered reading a source file. Asking both
  implementations to build an index over the same rows of the same file, our
  build is 8% slower on uniform points and 9% faster on clustered points at 1M
  rows, while running a verification gate GDAL does not and producing a tree a
  third smaller for the same query latency. Tree construction is 5% of our build
  time, so porting GDAL's R*-tree bulk loader cannot be where the difference
  lives. See
  [benchmarks/2026-07-24-gdal-like-for-like.md](benchmarks/2026-07-24-gdal-like-for-like.md).
- **Shipped known limitation**: the `wkb` 0.9.2 untrusted-count OOM (#3) was
  scoped "before v0.1" but no upstream fix has been released, so releases ship
  with it, documented in the README and both crates' docs. The dependency bump
  follows upstream.
- **The repository is still private** until the georust move (#18), so the
  `repository` link in the crates.io metadata 404s for readers.
- Smaller open items: a dedicated concurrent-reader test, a QGIS re-check,
  merge-into-populated-index (#17), and the full-scan read path being about 25%
  slower than `rusqlite-gpkg` on the same SQLite (#21).

## Working conventions

- Milestone docs are checklists: tick items as they land, add discovered work
  as new unticked items rather than silently expanding scope.
- Every milestone has **acceptance criteria**; a milestone is done when those
  pass in CI, not when the code exists.
- Spec references use requirement numbers from
  [spec140](https://www.geopackage.org/spec140/) and file paths in the
  [spec source repo](https://github.com/opengeospatial/geopackage) so claims
  are checkable.
- SQL that the spec gives verbatim is copied verbatim (see
  `geopackage-core/src/{ddl,triggers}.rs`); do not "improve" normative text.
