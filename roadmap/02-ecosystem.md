# Ecosystem: dependencies, coordination, code adaptation

## Dependency map (planned)

### Core (georust where possible)

| Crate | Role | When | Notes |
|---|---|---|---|
| [`geo-traits`](https://crates.io/crates/geo-traits) | Public geometry API bound | M1 | The API is generic over it; no concrete type forced |
| [`wkb`](https://github.com/georust/wkb) (georust) | WKB body encode/decode + envelope traversal | M1 | **Depend, don't copy.** Its no-alloc reader is exactly what the `ST_*` fallback needs |
| [`geo-types`](https://crates.io/crates/geo-types) | Convenience conversions | M1 | Default-on feature, not a hard dep |
| `rusqlite` | SQLite | now | `functions` + `limits`, system-linked by default with a `bundled` opt-in feature (D1, revisited 2026-08-06); `serialize` later for from/to-bytes (D5). Drives the workspace MSRV: libsqlite3-sys 0.38's build script uses `cfg_select!` (stable 1.95) without declaring a `rust-version`, so MSRV is 1.95 until upstream gates it; first CI run caught this |
| `thiserror` | errors | now | |
| [`jiff`](https://crates.io/crates/jiff) | calendar validation and epoch conversion for `DATE`/`DATETIME` | M3 (issue #24) | `default-features = false, features = ["std"]`: no `tz-system`, no `tzdb-*`. A GeoPackage `DATETIME` is UTC by definition and D3 keeps us out of transformation, so none of the timezone machinery is wanted. Measured at **+1.2 KB** on a release binary, because only the handful of entry points used survives dead-code elimination. No jiff type appears in this crate's API, so a jiff major version is not a breaking change here |
| [`serde_json`](https://crates.io/crates/serde_json) | corpus snapshot parsing | M1 | **dev-dependency of `geopackage` only.** Parses the committed `ogrinfo -json` expected-output snapshots in `geopackage/tests/corpus.rs`; not a runtime dependency |

### CRS definitions (decided, M1)

**Decision: vendored-subset + caller-supplied.** `geopackage-core::srs`
vendors WKT1 for 26 common EPSG codes (~17 KB, generated from GDAL by
`scripts/generate_epsg_wkt.py`, EPSG data (c) IOGP) and synthesises all 120
WGS 84 UTM zones (32601–32660/32701–32760) from a template, verified against
GDAL reference output in tests. Everything else is caller-supplied via
`GeoPackage::add_srs`; an unknown code is a typed `UnknownEpsgCode` error.
Lookup is one function: `srs::epsg_definition(code)`.

[`epsg-utils`](https://crates.io/crates/epsg-utils) (what rusqlite-gpkg uses)
backs everything outside that subset, resolving issue #23. It was rejected
initially on size, which measurement did not support: with both its
`wkt2-definitions` and `projjson-definitions` features it adds 1.1 MB to a
release binary, against 4.3 MB for
[`crs-definitions`](https://crates.io/crates/crs-definitions). It also supplies
the two forms we actually need, where `crs-definitions` supplies only WKT1:

- **WKT2**, for `definition_12_063`. This retires the note that geographic 3D
  CRSs such as EPSG:4979 are deliberately absent. They cannot be expressed in
  WKT1, so `add_epsg_srs` now writes them the way the spec and GDAL both do:
  `definition` contains the literal `undefined` and the real definition goes in
  the `gpkg_crs_wkt_1_1` extension column. Verified by round trip: GDAL reads
  our EPSG:4979 layer, resolves it as geographic 3D, and normalises our WKT2
  to a string identical to its own.
- **PROJJSON**, for GeoArrow's `crs` field metadata. The spec recommends it and
  says an authority code "should only be used as a last resort", since it
  leaves the reader to resolve the code against a registry it may not have. We
  emit an authority code only for codes outside the EPSG registry.

`miniproj` was considered and rejected on a different ground: it is a
transformation library, exposing neither WKT nor PROJJSON, so using it would
cross D3's "transform never" line without supplying what we needed.

### Later milestones

| Crate | Role | When |
|---|---|---|
| `arrow-array`/`arrow-schema` + [`geoarrow-array`](https://github.com/geoarrow/geoarrow-rs) | RecordBatch I/O, GeoArrow(WKB) columns | M3 |
| `cargo-c`/`cbindgen` | C ABI packaging | M3 |
| `clap` | CLI | M3 |
| [`hegeltest`](https://hegel.dev) (Hegel) | property tests | **landed M2** (dev-dep of `geopackage`, `0.28`) |
| `criterion` | benches | M2 (dev-dep) |
| `pyo3`+`maturin`, `napi-rs`, `uniffi` | bindings | post-v0.2, demand-driven |
| `sqlite-wasm-rs` | browser | parked (D5) |

`hegeltest` (the Hegel PBT library, Hypothesis-powered) is a dev-dependency of
`geopackage` only. It closes the property-test scope of **issue #2**: the RTree
contents provably match a full-scan rebuild after arbitrary
insert/update/delete/upsert sequences through both the triggered and the D8 bulk
build (`geopackage/tests/spatial_index.rs`), and the `features_in`
index-vs-full-scan equality (`geopackage/tests/features_in.rs`, ported from the
hand-rolled SplitMix64 generator). Its engine is compiled in from the
`hegeltest-c` crate at build time, so no server binary is fetched at test time and
no network is needed to *run* the tests, so it runs on plain `cargo test` /
`cargo nextest`, including on a CI runner that can fetch crates at build time. In
CI, Hegel disables its failure database and derandomises by default (no `.hegel/`
writes); `.hegel/` is gitignored regardless.

## rstar: why it is *not* in the plan (yet)

[`rstar`](https://github.com/georust/rstar) is georust's in-memory R\*-tree.
Three conceivable roles, assessed:

1. **Bulk-building the gpkg spatial index.** Tempting, but the on-disk format
   is SQLite's rtree shadow tables. Using rstar would mean serialising an
   rstar tree into `rtree_%_node` pages ourselves, reimplementing SQLite's
   node format, which is stable-in-practice but internal. GDAL's scratch-DB
   technique (D8) gets the same win using SQLite itself as the serialiser.
   **Parked**: only revisit if benchmarks show the scratch-DB build is a
   bottleneck *and* profiling points at SQLite's insert path, in which case an
   rstar bulk-load (STR packing) + direct shadow-table writes is the escalation
   with `PRAGMA integrity_check` gating in tests.
2. **Query-side acceleration.** No: queries go through SQL against the rtree
   vtab; an in-memory duplicate index would be a cache-coherence liability.
3. **`gpkg_contents` bbox maintenance / small utilities.** Overkill; a fold
   over envelopes suffices.

Thus: rstar stays out of the dependency tree for now.
Same logic applies to `geo` (algorithms): nothing in the container
layer needs planar predicates; anything that does (future `ST_*` extensions
beyond the required five) should be feature-gated.

## TIFF and GeoTIFF: assessed 2026-09-12, no dependency adopted

The tiled gridded coverage extension is the one place a TIFF can legally
appear in a GeoPackage, so "should we support GeoTIFF?" is three
questions, and the first finding is that only the third is about GeoTIFF at
all.

**The coverage payload is not a GeoTIFF.** Georeferencing comes from
`gpkg_tile_matrix_set`, and the extension does not use GeoKeys,
`ModelPixelScale` or `ModelTiepoint`. The
[migrated spec source](https://github.com/opengeospatial/geopackage/tree/master/spec/2d-gridded-coverage)
specifies a small profile of baseline TIFF:

| Requirement | Constraint |
|---|---|
| 15 | Conforms to the TIFF specification (baseline) |
| 16 | `SamplesPerPixel` = 1: "one sample per grid cell" |
| 17 | 32-bit float, **or** `SampleFormat` 1 (unsigned) or 2 (signed) with `BitsPerSample` 8, 16 or 32 |
| 18 | LZW compression *MAY* be used; clients "are expected to support" it |
| 19 | One image per file: "Multiple image files are not allowed" |
| 20 | *SHALL NOT* contain internal tiles (TIFF section 15) |
| 21 | Every pixel valid; `data_null` marks missing; "NaN and Inf SHALL NOT be used" |

with Requirement 13 sending `datatype` = *integer* to `image/png` (16-bit
greyscale) or `image/tiff`, and Requirement 14 sending *float* to
`image/tiff`. GDAL writes into the middle of that profile:
`gdalgeopackagerasterband.cpp` creates float tiles through the GTiff driver
with `COMPRESS=LZW` and a single strip for tiles up to 512x512, and its
float-tile *read* driver list is literally `{"GTiff"}`.

### Upstream status, which M5 asked us to re-assess

Answered, and the answer has changed since M4. The revision is finished:
`opengeospatial/geopackage-tiled-gridded-coverage` was archived on 2026-08-04
with its text migrated into
`opengeospatial/geopackage/spec/2d-gridded-coverage`, and **17-066r2**
(version 1.1) is published. r2 is *wider* than the r1 this workspace's
extension catalogue cites: it admits integer TIFF alongside float32, which is
the change [issue #680](https://github.com/opengeospatial/geopackage/issues/680)
asked for. So "the extension is under revision" is no longer a reason to keep
it out; the only remaining reason is that nobody has implemented it here.

### The crates, measured

Downloads and dates from crates.io on 2026-09-12.

| Crate | Latest | Updated | Recent dl | Owner | Read | Write | Notes |
|---|---|---|---|---|---|---|---|
| [`tiff`](https://github.com/image-rs/image-tiff) | 0.11.3 | 2026-02-10 | 31.3M | image-rs | yes | yes | The only load-bearing crate in the space. Pure Rust, MSRV 1.85, two `unsafe` blocks (slice casts in `bytecast.rs`) |
| [`geotiff`](https://github.com/georust/geotiff) | 0.1.0 | 2025-06-10 | 7.1k | georust | yes | no | "read GeoTIFFs, nothing else"; still on `tiff` 0.9, warns of breaking changes post-0.1 |
| [`async-tiff`](https://github.com/developmentseed/async-tiff) | 0.3.0 | 2026-04-01 | 10.7k | kylebarron | yes (async) | no | COG over `object_store`; default features pull `reqwest` and `object_store`; optional `jpeg2k`/`lerc`/`webp` bind C |
| [`georaster`](https://github.com/pka/georaster) | 0.2.0 | 2025-01-11 | 1.3k | pka | yes | no | Dormant about 20 months |
| `geotiff-reader`, `geotiff-writer`, `tiff-writer` ([roteiro-gis](https://github.com/roteiro-gis/geotiff-rust)) | 0.8.1 | 2026-08-13 | 5-9k | i-norden | yes | yes | Claims BigTIFF, COG, overviews, LERC/ZSTD, float predictor, GDAL-parity tests. First release 2026-03, 13 stars. Unaudited |
| [`oxigdal-geotiff`](https://github.com/cool-japan/oxigdal) | 0.1.7 | 2026-07-20 | 988 | cool-japan | yes | yes | "Pure Rust GDAL reimplementation". Very new, very low usage; unproven |
| [`wbgeotiff`](https://github.com/jblindsay/whitebox_next_gen) | 0.1.2 | 2026-05-07 | 175 | jblindsay | yes | yes | Internal engine for Whitebox's rewrite, not pitched as a general dependency |
| [`gdal`](https://github.com/georust/gdal) | 0.19.0 | 2025-12-23 | 419k | georust | yes | yes | C dependency, against D1's posture |

The state of the ecosystem in one line: **one mature TIFF codec, and no mature
pure-Rust GeoTIFF writer.** The georust crate is read-only by charter and a
`tiff` major behind; the three write-capable newcomers all appeared within six
months and have no adoption to speak of. Reading GeoTIFF *metadata* is the
small gap it looks like, because `tiff` already contains the tag constants
(`ModelPixelScaleTag` 33550, `ModelTiepointTag` 33922, `GeoKeyDirectoryTag`
34735 and its two companions) and `write_tag` emits arbitrary tags.

### What `tiff` 0.11.3 would give us, read from its source rather than its docs

- Encoder colour types include `Gray8`, `GrayI8`, `Gray16`, `GrayI16`,
  `Gray32`, `GrayI32` and `Gray32Float`: exactly Requirement 17's matrix.
- Encoder compressions: uncompressed, LZW, Deflate, PackBits. LZW is
  Requirement 18.
- The encoder is **strip-only**; there is no `TileWidth`/`TileLength` write
  path. Requirement 20 forbids internal tiles, so the missing feature is the
  required behaviour.
- Encoder predictors: `Horizontal` only, and the encoder rejects it for `IEEEFP`;
  `FloatingPoint` returns `UsageError::PredictorUnavailable`. Irrelevant on
  write, and the *decoder* handles both, so a GDAL-written tile reads either
  way.
- The decoder reads strip and tile chunks alike, so files in the wild read.
- Trimmable to `default-features = false, features = ["lzw"]`, which drops
  `flate2`, `fax` and `zune-jpeg`. MSRV 1.85 is under this workspace's 1.95,
  so it applies no MSRV pressure.

### Decision

1. **Coverage payload validation: no dependency.** Requirements 16 to 20 are
   answered by walking the first IFD, which is a header read of the kind
   `probe` already does and about 200 lines with no new crate. This keeps M4's
   "bytes in, bytes out, no decode in core" line intact, and is what a
   conformance check can assert. Planned as phase 1 of a coverage
   milestone.
2. **Pixel access: `tiff`, if and when the API needs to hand back values.**
   Off by default, and in `geopackage` or a `geopackage-coverage` crate rather
   than `-core`, whose stated line is header inspection and never a decode.
   Not scheduled.
3. **GeoTIFF ingest and export** (`gpkg tiles import dem.tif`) is a resampling
   and reprojection question, not a codec question, and reprojection is D3's
   line. If it is ever wanted, the CLI shelling out to `gdal_translate -of
   GPKG` costs no library surface and matches how `gdal_interop.rs` already
   works. `oxigdal-geotiff` and the roteiro-gis crates are not adoptable on
   current evidence; revisit if either grows users and an audit.
4. **`gdal` in the library: no**, for D1's reasons and because it would make
   this crate's purpose circular.

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
  Two upstream `wkb` items surfaced during M1 (both tracked in
  [03-m1-read-path.md](03-m1-read-path.md)): (1) `wkb` 0.9.2's reader
  pre-allocates from an untrusted element count
  (`Vec::with_capacity(num_geometries)` / `num_rings`) without bounding it
  against the buffer, so a malformed count drives an out-of-memory, found by
  our `gpb_geometry` fuzz target; (2) no reader support for the non-linear
  curve types. The second no longer blocks us: `geopackage-core::curve` reads
  those bodies itself for envelopes and passes the bytes through. Reading a
  curve back as a geometry object rather than as bytes would need a `geo-traits`
  representation for an arc as much as it needs a reader, and we are not waiting
  on either: `Feature::geometry_bytes` is the documented way to read a curve.
  Settled 2026-07-29, in
  [07-m5-extensions-and-1.0.md](07-m5-extensions-and-1.0.md).
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
  descriptions, since C++ internals don't transliterate usefully. Cite
  [gdal#7614](https://github.com/OSGeo/gdal/issues/7614) and RFC 86 at the
  implementation sites.
  For M3, the measurements to beat are in Even Rouault's
  [Paris meetup slides](https://download.osgeo.org/gdal/presentations/GDAL_%20integrating%20columnar%20formats%20into%20a%20row-oriented%20framework.pdf)
  (18 June 2026), reproduced and read in
  [05-m3-arrow-ffi.md](05-m3-arrow-ffi.md). He
  [notes](https://mastodon.social/@EvenRouault/116980004256229761) that the
  GeoPackage driver's Arrow path now performs "not very far away from Parquet
  using some tricks and multithreading"; on slide 10's benchmark it is in fact
  ahead of GeoParquet at four threads. What those tricks are is not in the
  slides, so the driver's `GetArrowStream` override is the place to look, under
  the technique-not-transliteration rule above.
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
