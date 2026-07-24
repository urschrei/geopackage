# Testing & conformance (cross-cutting)

The crate's pitch is "correct first, fast second, both can be demonstrated".
This file is the harness plan; milestone docs reference it.

## Layers of verification

1. **Spec-derived unit tests** (`geopackage-core`): testable requirements get
   tests named for their requirement number (`req11_srs_seeds`,
   `req20_gpb_magic`, …). The M0 suite starts this; keep the convention.
2. **Property tests** (Hegel/[`hegeltest`](https://hegel.dev), from M2): GPB
   round-trip over arbitrary geometries (all types × XY/Z/M/ZM × empty ×
   nested collections); envelope ⊇ geometry; `features_in(bbox)` ≡
   scan-filter; rtree state ≡ full rebuild after arbitrary
   write/update/delete/upsert sequences. Hegel is Hypothesis-powered
   (server-side shrinking, `#[hegel::test]` on the standard test runner);
   the hand-rolled SplitMix64 generator in `tests/features_in.rs` gets
   ported to it when the dev-dependency lands.
3. **Fuzzing** (cargo-fuzz, continuous once public via OSS-Fuzz):
   `gpb_parse` (M0 ✅), `gpb_geometry` (header+body via wkb, M1),
   `open_arbitrary` (arbitrary bytes as .gpkg file, M1), tile matrix
   consistency parser (M4). Corpus seeded from the fixture corpus.
4. **External validators** (CI, from M2, on files *we* write):
   - OGC [ets-gpkg12](https://github.com/opengeospatial/ets-gpkg12) all-in-one
     jar (Java in CI). Note: it validates 1.2 semantics — no 1.3/1.4 ETS
     exists as of 2026-07 — so supplement with:
   - **manual 1.4 checklist** (trigger names are update5/6/7 and no
     update1/update3; user_version 10400; strict DATETIME format),
   - [PDOK geopackage-validator](https://github.com/PDOK/geopackage-validator)
     (stricter naming/index rules; treat as advisory where it exceeds spec).
5. **Interop round-trips** (CI): write → `ogrinfo`/`ogr2ogr` read-back
   (geometry WKB byte-equality, value equality); read GDAL/QGIS/NGA-written
   files → our model → compare against `ogrinfo -json`. QGIS headless open
   in a scheduled (not per-PR) job.
6. **Corruption regressions**: fixtures for every historical failure mode we
   know – stale `update3` + UPSERT, mixed trigger generations, envelope
   disagreeing with geometry, wrong-endian headers, GP10 application_id,
   truncated GPB, `-wal` sidecar left by a crashed writer.
7. **Benchmarks** (criterion, from M2): tracked in-repo with hardware notes;
   regression thresholds in CI from M5. Baselines: `gdal` crate, pyogrio
   (via the Arrow path), and rusqlite-gpkg where APIs overlap.

## Fixture corpus

- `tests/data/` for small (<100 KB) committed files; `tests/corpus.toml` +
  fetch script for larger ones (NGA samples, Natural Earth extracts,
  GDAL-autotest gpkg files) pinned by sha256.
- Every corpus file records: producer + version, spec version, feature/tile
  counts, known quirks. Generation scripts live in the repo so fixtures are
  reproducible (`gpkg copy` becomes the generator from M3).
- Share fixtures with rusqlite-gpkg and geozero where useful (see
  [02-ecosystem.md](02-ecosystem.md)).

## CI structure (target state)

Per-PR: unit + property + integration tests (3 OSes), MSRV, clippy `-D
warnings`, fmt, docs, fuzz build, header-diff gate (M3+), validators on a
small file set. Scheduled (nightly/weekly): full corpus soak, long fuzz runs,
QGIS interop, benchmark tracking.
