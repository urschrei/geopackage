# Development roadmap

Roadmap for the georust `geopackage` workspace: a production-quality, pure-Rust
(modulo bundled SQLite) implementation of [OGC GeoPackage 1.4](https://www.geopackage.org/spec140/),
usable from Rust and from higher-level languages via a C ABI with the Arrow C
Data Interface as the bulk data plane.

## Documents

| File | Contents |
|---|---|
| [00-completed.md](00-completed.md) | What exists today (M0) and how it was verified |
| [01-design-decisions.md](01-design-decisions.md) | Decision record: driver, API shape, CRS policy, WAL, Wasm posture — including lessons from prior art |
| [02-ecosystem.md](02-ecosystem.md) | Dependency map, georust coordination, code-adaptation policy (wkb, geozero, rstar, arrow, …) |
| [03-m1-read-path.md](03-m1-read-path.md) | M1: feature/attribute read path, full WKB envelopes |
| [04-m2-write-rtree.md](04-m2-write-rtree.md) | M2: write path, layer creation, bulk rtree build → **v0.1** |
| [05-m3-arrow-ffi.md](05-m3-arrow-ffi.md) | M3: GeoArrow batches, C ABI, CLI → **v0.2** |
| [06-m4-tiles.md](06-m4-tiles.md) | M4: tile pyramids → **v0.3** |
| [07-m5-extensions-and-1.0.md](07-m5-extensions-and-1.0.md) | M5: extensions, bindings, API freeze → **1.0 RFC** |
| [08-testing-conformance.md](08-testing-conformance.md) | Cross-cutting: conformance harness, fuzzing, benchmarks, corpus |

## Status snapshot (2026-07-24)

- **M0 complete**: workspace (`geopackage-core`, `geopackage`, fuzz), GPB header
  codec, normative DDL + 1.4 RTree trigger set (verbatim from spec source),
  container create/open with validation, `ST_*` function registration, 17 tests
  incl. end-to-end trigger/UPSERT proofs, CI (3 OSes, MSRV 1.85, clippy `-D
  warnings`, fmt, docs, fuzz-build).
- **M1 not started.**

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
