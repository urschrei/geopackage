# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the version is below 1.0 the API may change in any release.

## [Unreleased]

## [0.1.2] - 2026-07-24

Performance across all three index and read paths, plus a streaming read. No
API is removed or changed; the additions are `Layer::cursor`, `cursor_in`,
`cursor_select` and the `FeatureCursor` / `FeatureStream` types they return.

### Added

- **Streaming reads.** `Layer::cursor`, `Layer::cursor_in` and
  `Layer::cursor_select` return a `FeatureCursor` whose `features()` yields a
  `FeatureStream`, holding one row at a time instead of materialising the result
  set. Over 100k features this reads in 18.8ms against 29.5ms for the
  materialising methods, with peak memory bounded by a row rather than by the
  query.

  It is two calls rather than one because rusqlite's row cursor borrows its
  `Statement`, so an iterator owning both would be self-referential, which
  `#![forbid(unsafe_code)]` rules out without a helper crate. The cursor owns
  the statement and the stream borrows it, which is the shape rusqlite itself
  uses.

  `features`, `features_in` and `select` are unchanged and remain the right
  default for layers small enough that the result set is not a problem. Both
  paths build the same `Feature`s through the same code, so they never differ
  in results ([#4]).

### Changed

- **`create_spatial_index` is about 12% faster** (1504ms to 1330ms at 1M
  points). Its entry set was built by asking SQLite for `ST_IsEmpty` plus the
  four `ST_Min`/`ST_Max` functions per row: six user-function dispatches, each
  re-fetching the blob and re-parsing the GPB header. It now reads each blob
  once and computes the envelope in Rust, borrowing the blob rather than copying
  it. Emptiness is still decided exactly as `ST_IsEmpty` decides it and the
  bounds still come from the header envelope when present, so the accumulated
  set is identical rather than merely close ([#22], thanks to @sayrer).
- **A large `write_all` into an already-populated spatial index now rebuilds it
  in bulk** rather than letting the triggers append row by row. A rebuild costs
  roughly 1.5us per row of the whole table where a triggered append costs 18 to
  40us per new row, so the rebuild wins once the new rows are more than about a
  tenth of the existing ones, which is where the threshold now sits. Appending
  100k rows to a 1M-row indexed layer goes from 2938ms to 1783ms ([#17]).

### Fixed

- The bulk build path is now covered for NULL, empty and envelope-less
  geometries. The existing test for that behaviour used default options on a
  three-row table, which is far below the bulk threshold, so it only ever
  exercised the triggered path.

## [0.1.1] - 2026-07-24

A performance and durability release. No API is removed or changed: the only
public additions are `StructuralCheck`, `DEFAULT_FILL_FACTOR` and two builder
methods on `BulkIndexOptions`, so upgrading from 0.1.0 needs no code changes.

It also carries metadata fixes that 0.1.0 could not: that release's crates.io
and docs.rs pages point at a repository URL that does not exist, and show no
README, neither of which can be corrected in place.

### Added

- `StructuralCheck` and `BulkIndexOptions::with_structural_check`, choosing how
  thoroughly the bulk-build gate checks database structure: `RtreeOnly` (the new
  default, `rtreecheck()` over the index just built) or `FullDatabase`
  (additionally a whole-database `PRAGMA integrity_check`, which is what 0.1.0
  always did).
- `DEFAULT_FILL_FACTOR` and `BulkIndexOptions::with_fill_factor`, setting the
  fraction of each RTree node filled when packing. The default of 1.0 gives the
  smallest tree and the best queries; lower it when a freshly built index will
  be appended to heavily.
- Runnable examples: `inspect`, `bulk_load`, `bbox_query` and `repair_index`.

### Changed

- **Bulk spatial-index builds are about 3.5x faster.** A 1M-point indexed
  `write_all` goes from 7.31s to roughly 2.08s. Four changes got there, in
  decreasing order of effect: the RTree is now constructed directly and its
  shadow tables written, rather than every entry being inserted into a scratch
  index and copied ([#20]); `%_rowid` rows are inserted in feature-id order
  rather than tree order, since that table is keyed by feature id and inserting
  in Hilbert order paid a page split per row; the gate uses `rtreecheck()` on
  the new index instead of a whole-database `integrity_check` ([#16]); and
  `write_all` reuses the envelopes it computed while encoding instead of
  re-deriving them with an `ST_*` scan.
- Measured like for like, both implementations building an index over the same
  rows of the same file, this is level with GDAL: 8% slower on uniformly spread
  points and 9% faster on clustered points, while running a verification pass
  GDAL has no equivalent of and producing a tree a third smaller for the same
  query latency. Queries are unaffected or slightly faster than 0.1.0.
- **A bulk `write_all` is now atomic.** Dropping the RTree triggers, every row
  insert, the `gpkg_contents` flush, the index rebuild and reinstalling the
  triggers all commit together, so a crash or error part-way through leaves the
  file exactly as it was. In 0.1.0 the rows committed before the index was
  rebuilt, and a crash in between left a `Stale` index needing
  `repair_spatial_index()`. That split was forced by an `ATTACH`ed scratch
  database, which is now gone ([#15]).
- The bulk-build gate is a large share of a build: roughly 45%, about 745ms of a
  1593ms build at 1M points, split between checking the written index against
  the input and `rtreecheck`. It is why the build is level with GDAL rather than
  ahead of it. The check stays, because the RTree is written by hand into a
  format SQLite does not document as an interface; whether it should become
  optional is a question for 1.0.
- Peak memory during a bulk build is lower: the tree is streamed into the shadow
  tables as it is built rather than assembled whole in memory first.

### Fixed

- Both crates now render their README on crates.io. 0.1.0 showed none.
- The `repository` URL now points at the real repository rather than at
  `github.com/georust/geopackage`, which does not exist. Note that it still does
  not resolve for anyone else: the repository is private until the move to the
  georust org, so the link on crates.io and docs.rs remains a 404 for readers.
  An earlier wording of this entry claimed the link was fixed, which was wrong.
- The crate documentation now states the untrusted-input limitation ([#3]) that
  0.1.0 shipped with but did not mention: the `wkb` 0.9.2 reader pre-allocates
  from unbounded element counts, so a malformed geometry can drive a very large
  allocation. Still unfixed upstream, so it remains a known limitation here: do
  not parse GeoPackage files from untrusted sources.

## [0.1.0] - 2026-07-24

First release: the GeoPackage 1.4 read path (M1) and write path with
spec-correct spatial indexing (M2), across the `geopackage-core` and
`geopackage` crates.

### Added

- **Container.** Create and open with pragma and schema validation
  (`GeoPackage::create`, `open`, `open_read_only`, `from_connection`);
  `open_lenient` tolerates legacy and lightly malformed files, collecting typed
  `OpenWarning`s instead of failing. The `ST_*` SQL functions required by the
  spatial-index triggers are registered on every connection.
- **Read path.** Layer enumeration and typed `Layer` handles
  (`layers`, `layer`, `attributes`); `Layer::features` iterating owned
  `Feature`s with by-name and by-index value access and lazy geometry parsing;
  `Layer::features_in` bounding-box queries, RTree-accelerated where an index
  exists and property-tested identical to a full scan; `Layer::select` for a
  caller-supplied `WHERE` clause.
- **Write path.** `TableSchemaBuilder` with `create_layer` /
  `create_attributes_table` emitting user-table DDL and catalogue rows in one
  transaction; `Layer::writer` returning a transaction-owning `FeatureWriter`
  with `insert`/`update`/`delete` over any `impl GeometryTrait<T = f64>`;
  batched `Layer::write_all`; `gpkg_contents` `last_change` and bounding-box
  maintenance on commit; DATETIME serialisation in the strict 1.4 format.
- **Spatial index.** `create_spatial_index`, `drop_spatial_index`,
  `has_spatial_index` and `repair_spatial_index` over the GeoPackage 1.4
  trigger set (`update5`/`update6`/`update7`), with pre-1.4 and mixed
  generations detected and repairable rather than silently mixed. The D8 bulk
  shadow-table build (`BulkIndexOptions`) accelerates fresh index construction,
  gated by a bijection/containment check plus `PRAGMA integrity_check` with an
  automatic fallback to the triggered path on any anomaly.
  `Layer::spatial_index_status` reports `Absent`/`Current`/`Legacy`/`Stale`, and
  `repair_spatial_index` recovers a `Stale` index left by a crash mid-bulk-build.
- **Durability.** `OpenOptions` with typed `JournalMode` and `Synchronous`
  enums; WAL is opt-in, and a handle that opted into it checkpoints and resets
  the file to `DELETE` on `close` and on drop, so a handed-over `.gpkg` carries
  no `-wal`/`-shm` sidecars.
- **`geopackage-core`.** No-IO spec layer: GPB header codec, column and geometry
  type vocabulary, `DATE`/`DATETIME` parsing, normative DDL and
  `gpkg_spatial_ref_sys` seed rows, version-aware RTree trigger SQL,
  `application_id`/`user_version` handling, and SQL identifier quoting.

### Validation

- OGC `ets-gpkg12`: 40 passed, 71 not applicable, 1 failed. The failure's regex
  hard-codes the GeoPackage 1.2 `update1` trigger and rejects the correct 1.4
  set. No 1.3/1.4 ETS exists; the 1.4 trigger semantics are covered by a manual
  checklist.
- PDOK `geopackage-validator` 0.14.4: 21 checks, clean but for two advisory
  findings on deliberate choices (a mixed-SRS test file, an intentional Z layer).
- GDAL round-trip: geometry WKB bodies and every attribute value byte-identical
  after an `ogr2ogr` copy; `ogrinfo` reads all layers cleanly.
- Fixture corpus covering GDAL-written, QGIS-written and raw-SQLite files, plus
  property tests proving the RTree matches a full-scan rebuild after arbitrary
  write sequences on both build paths.

### Known limitations

- The `wkb` 0.9.2 reader pre-allocates from unbounded element counts, so a
  malformed geometry can drive a multi-gigabyte allocation; do not parse
  untrusted files with this release ([#3]).
- Bulk indexed writes are roughly 3-4x slower than an indexed `ogr2ogr` copy at
  1M rows ([#15], [#16]). The cost attributed here at release time was wrong:
  profiling afterwards put 4.61s of the 7.26s in the scratch RTree build, not in
  the gate's `integrity_check` (0.97s) or the `ST_*` envelope scan (0.26s). Much
  of this is addressed in Unreleased; the remainder is [#20].
- Non-linear curve types cannot have envelopes computed and so cannot be
  inserted into an indexed table ([#5]).
- Feature iteration materialises the result set rather than streaming ([#4]).

[Unreleased]: https://github.com/urschrei/geopackage/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/urschrei/geopackage/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/urschrei/geopackage/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/urschrei/geopackage/releases/tag/v0.1.0
[#3]: https://github.com/urschrei/geopackage/issues/3
[#4]: https://github.com/urschrei/geopackage/issues/4
[#4]: https://github.com/urschrei/geopackage/issues/4
[#5]: https://github.com/urschrei/geopackage/issues/5
[#15]: https://github.com/urschrei/geopackage/issues/15
[#16]: https://github.com/urschrei/geopackage/issues/16
[#17]: https://github.com/urschrei/geopackage/issues/17
[#20]: https://github.com/urschrei/geopackage/issues/20
[#22]: https://github.com/urschrei/geopackage/pull/22
