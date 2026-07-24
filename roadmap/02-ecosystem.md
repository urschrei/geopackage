# Ecosystem: dependencies, coordination, code adaptation

## Dependency map (planned)

### Core (georust where possible)

| Crate | Role | When | Notes |
|---|---|---|---|
| [`geo-traits`](https://crates.io/crates/geo-traits) | Public geometry API bound | M1 | The API is generic over it; no concrete type forced |
| [`wkb`](https://github.com/georust/wkb) (georust) | WKB body encode/decode + envelope traversal | M1 | **Depend, don't copy.** Its no-alloc reader is exactly what the `ST_*` fallback needs |
| [`geo-types`](https://crates.io/crates/geo-types) | Convenience conversions | M1 | Default-on feature, not a hard dep |
| `rusqlite` | SQLite | now | `bundled` + `functions`; `serialize` later for from/to-bytes (D5) |
| `thiserror` | errors | now | |

### CRS definitions (decision needed early in M1)

Options for `srs_id → WKT` seeding (D3):
[`crs-definitions`](https://crates.io/crates/crs-definitions) (proj4/WKT for
all EPSG codes, georust adjacent, adds ~MBs),
[`epsg-utils`](https://crates.io/crates/epsg-utils) (what rusqlite-gpkg uses),
or a small vendored table of the ~30 codes that cover real-world gpkg traffic
(4326, 3857, national grids…) with everything else caller-supplied.
**Leaning: vendored-subset + caller-supplied**, keeping binary size honest;
revisit if users push back. Whatever we pick, the lookup lives behind one
function in one module.

### Later milestones

| Crate | Role | When |
|---|---|---|
| `arrow-array`/`arrow-schema` + [`geoarrow-array`](https://github.com/geoarrow/geoarrow-rs) | RecordBatch I/O, GeoArrow(WKB) columns | M3 |
| `cargo-c`/`cbindgen` | C ABI packaging | M3 |
| `clap` | CLI | M3 |
| `criterion`, `proptest` | benches, property tests | M2 |
| `pyo3`+`maturin`, `napi-rs`, `uniffi` | bindings | post-v0.2, demand-driven |
| `sqlite-wasm-rs` | browser | parked (D5) |

## rstar: why it is *not* in the plan (yet)

[`rstar`](https://github.com/georust/rstar) is georust's in-memory R\*-tree.
Three conceivable roles, assessed:

1. **Bulk-building the gpkg spatial index.** Tempting, but the on-disk format
   is SQLite's rtree shadow tables. Using rstar would mean serialising an
   rstar tree into `rtree_%_node` pages ourselves — reimplementing SQLite's
   node format, which is stable-in-practice but internal. GDAL's scratch-DB
   technique (D8) gets the same win using SQLite itself as the serialiser.
   **Parked**: only revisit if benchmarks show the scratch-DB build is a
   bottleneck *and* profiling points at SQLite's insert path, in which case an
   rstar bulk-load (STR packing) + direct shadow-table writes is the escalation
   — with `PRAGMA integrity_check` gating in tests.
2. **Query-side acceleration.** No: queries go through SQL against the rtree
   vtab; an in-memory duplicate index would be a cache-coherence liability.
3. **`gpkg_contents` bbox maintenance / small utilities.** Overkill; a fold
   over envelopes suffices.

Thus: rstar stays out of the dependency tree for now.
Same logic applies to `geo` (algorithms): nothing in the container
layer needs planar predicates; anything that does (future `ST_*` extensions
beyond the required five) should be feature-gated.

## Code adaptation policy (geozero, gpkg-rs, GDAL, …)

Licences: geozero, wkb, gpkg-rs are MIT OR Apache-2.0; GDAL is MIT. So
adaptation is legally clean everywhere relevant; the policy below is about
engineering.

- **georust `wkb`**: dependency, not adaptation. Anything it lacks (e.g. an
  envelope-accumulating visitor that avoids materialising geometries) gets
  **upstreamed to wkb**, not forked here. We maintain wkb, so this is cheap –
  and it's the right home: rusqlite-gpkg strips GPB headers and delegates to
  wkb already; a shared `gpb` feature there (header parse + body delegate)
  would let rusqlite-gpkg, geozero and us converge on one codec. Track as an
  explicit M1 task; until it lands, `geopackage-core::gpb` is ours.
- **geozero**: adapt with attribution, don't depend (geozero's gpkg support
  drags in sqlx). Worth lifting: the `WkbDialect::Geopackage` test cases and
  fixtures (battle-tested against real files since 2020), the empty-flag and
  envelope-indicator edge-case handling in `wkb_reader.rs` as a checklist
  against our codec, and its GPKG test .gpkg files for the corpus. As geozero
  maintainers we should also, post-v0.1, offer geozero a path off sqlx-gpkg:
  geozero keeps the codec trait story, points container users here
  (resolves the [geozero#185](https://github.com/georust/geozero/issues/185) /
  [#38](https://github.com/georust/geozero/issues/38) stall).
- **gpkg-rs (cjriley9)**: no code worth lifting (2D-only, pre-geo-traits, old
  wkb fork), but its derive-macro UX informs the eventual `geopackage-derive`,
  and its integration tests have a few good real-file cases. Credit in README
  as prior art; ask about the crates.io `gpkg` name redirect when we publish.
- **rusqlite-gpkg (yutannihilation)**: coordinate, don't copy. Specifically align
  on: the wkb `gpb` extraction (above), `ST_*` function semantics, and shared
  conformance fixtures. His [design write-up](https://dev.to/yutannihilation/how-it-feels-to-write-a-gpkg-library-in-2026-in-rust-52mg)
  is already baked into [01-design-decisions.md](01-design-decisions.md).
- **GDAL**: adapt *techniques* (shadow-table rtree build, WAL
  checkpoint-on-close, Arrow batch reads), reimplemented from issue/RFC
  descriptions — C++ internals don't transliterate usefully. Cite
  [gdal#7614](https://github.com/OSGeo/gdal/issues/7614) and RFC 86 at the
  implementation sites.
- **NGA geopackage-java**: mine its test suite and published sample/corrupt
  .gpkg files for the conformance corpus; its layered core/platform split
  already shaped D10.

## Org-level coordination checklist

- [ ] Verify `geopackage` is still free on crates.io; publish a 0.0.1
      placeholder to reserve it (with README pointing at the repo).
- [ ] georust/meta RFC issue: attach survey + this roadmap; ping
      yutannihilation, cjriley9, pka, kylebarron.
- [ ] Transfer repo to github.com/georust once the RFC has assent.
- [ ] wkb issue: propose `gpb` feature (GPB header + dialect over wkb reader);
      link from geozero#185.
- [ ] Post-v0.1: geozero release note pointing gpkg container users here.
