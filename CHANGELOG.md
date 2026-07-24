# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the version is below 1.0 the API may change in any release.

## [Unreleased]

### Added

- `StructuralCheck` and `BulkIndexOptions::with_structural_check`, selecting how
  thoroughly the bulk-build gate checks database structure after copying the
  shadow tables: `RtreeOnly` (the new default, `rtreecheck()` on the index just
  built) or `FullDatabase` (additionally a whole-database
  `PRAGMA integrity_check`, the previous behaviour).

### Changed

- Bulk spatial-index builds are roughly a third faster: 1M-point indexed
  `write_all` goes from 7.31s to 4.95s (criterion, point -32.3%, linestring
  -33.3%, polygon -35.6%, all p < 0.05). Three fixes: the scratch RTree inserts
  now run in one transaction rather than one implicit transaction per row; the
  gate uses `rtreecheck()` instead of a whole-database `integrity_check` ([#16]);
  and `write_all` reuses the envelopes it computes while encoding instead of
  re-deriving them with an `ST_*` scan. The index produced is byte-identical.
- The read benchmark now closes and reopens its fixture before measuring, rather
  than querying through the connection that built it. Its figures are therefore
  slower than, and not comparable to, the v0.1.0 set. See
  `roadmap/benchmarks/2026-07-24-bulk-build.md`.

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

- OGC `ets-gpkg12`: 40 passed, 71 not applicable, 1 failed — the failure's regex
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

[Unreleased]: https://github.com/urschrei/geopackage/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/urschrei/geopackage/releases/tag/v0.1.0
[#3]: https://github.com/urschrei/geopackage/issues/3
[#4]: https://github.com/urschrei/geopackage/issues/4
[#5]: https://github.com/urschrei/geopackage/issues/5
[#15]: https://github.com/urschrei/geopackage/issues/15
[#16]: https://github.com/urschrei/geopackage/issues/16
[#20]: https://github.com/urschrei/geopackage/issues/20
