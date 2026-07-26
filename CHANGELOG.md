# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the version is below 1.0 the API may change in any release.

## [Unreleased]

## [0.3.0] - 2026-07-26

### Changed

- **A layer's value columns no longer include the primary key.** The read path
  counted the key among a row's values while the write path excluded it, so the
  two sides of the API disagreed about what a row's values were: feeding a
  feature's values straight into a writer for an identically-shaped layer
  compiled and then failed at run time with `ValueCountMismatch`, and the caller
  had to know to strip the key by name. The key is reached through
  `Feature::fid`, as it always was, and GDAL draws the same line between a
  feature id and its fields. `Feature::value("fid")` is now `None`,
  `Feature::get` indices shift by one on a layer with a named primary key, and
  `Feature::columns` no longer lists it. As a side effect every query stopped
  selecting the key twice.
- **`Feature` hands out `ValueRef<'_>` rather than `&Value`.** A feature used to
  be a `Vec<Value>` plus a `String` or `Vec<u8>` for every text and blob cell:
  seven allocations a row on a thirteen-column layer with four text columns and
  a blob. Its geometry and every variable-length cell now sit end to end in one
  buffer with a range recorded per value, which is two allocations a row
  whatever the row's width, so there is no longer a `Value` inside a feature to
  lend out. `Feature::value`, `get`, `values` and `iter` return the new borrowed
  type; `values` returns an iterator rather than a slice. `ValueRef::to_value`,
  or `Value::from`, gives an owned value where one is needed.
- **The write path takes borrowed values.** `FeatureWriter::insert`,
  `insert_row`, `update` and `update_row`, and `Layer::select` and
  `cursor_select`, take `&[ValueRef<'_>]`. Nothing in the implementation needed
  ownership: bindings were already made by reference out of the `Value` they
  were given, so the owned signature only forced callers to build values they
  then handed straight over. A row read from one layer now binds into another
  without its text and blob cells being copied, and a literal parameter needs no
  allocation. `NewFeature` keeps `Vec<Value>`, because `write_all` consumes an
  iterator whose items must outlive any single call, which is the rule the API
  now follows: borrowed where a value need not outlive the call, owned where it
  must.

### Added

- **`ValueRef<'a>`**, the borrowed counterpart of `Value`, with `to_value`,
  `is_null`, and an accessor per variant (`as_str`, `as_blob`, `as_bool`,
  `as_i64`, `as_f64`, `as_date`, `as_datetime`). None of them converts between
  variants: what a cell reads as is driven by its column's declared type, not by
  its contents, so an `INTEGER` column holding `0` reads as `Integer` and stays
  that way under `as_bool`. `From` converts in both directions, and a `ValueRef`
  compares directly against a `Value`.
- **`gpb::header_len` and `gpb::encode_header_into`** in `geopackage-core`, so a
  caller can size a blob for a GPB header and its WKB body together and append
  both into one allocation. `encode_header` is unchanged.

### Performance

Allocation counts below are exact, from a counting global allocator over a
200,000-row fixture of thirteen attribute columns. Wall-clock effects on these
paths are mostly smaller than the run-to-run spread of the machine they were
measured on, so they are not quoted here; see
`roadmap/benchmarks/2026-07-25-real-datasets.md` for the dataset timings and
their caveats.

- **The materialising read is 40 allocations a row lighter.** `Layer::execute`
  rebuilt its per-row context inside the row loop, cloning the table name, the
  geometry column and the whole column list for every feature returned: 40
  allocations a row against 8 for the same work through `cursor`. It is built
  once per query, as the cursor path already did.
- **A feature is two allocations rather than seven** on that fixture, from the
  buffer described above, and one fewer again after `DATE` and `DATETIME` cells
  stopped being copied into a `String` only to be parsed and dropped.
- **The bulk index build no longer duplicates its entry set.** The gate built a
  `HashMap` of every `(fid, envelope)` pair to remove entries as it scanned;
  it now sorts and binary-searches the vector it already owns. The packer
  collected leaf cells and then drained them into a second keyed vector, and
  allocated a blob per node and a cell vector per leaf; those are one pass and
  two reused buffers. Transient bytes over 200,000 rows fall from 52 MB to
  25 MB, and peak resident memory over 11.5M rows from about 1246 MB to about
  1148 MB, medians of three runs each.
- **The write paths bind by reference.** Every insert and update deep-copied
  each text and blob cell before handing it to SQLite; each also collected a
  bindings vector per row, and the columnar path collected a second one. The
  scalar write is 4.08 allocations a row before and 2.04 after, the columnar
  7.08 and 5.04. Each GPB blob is now sized for its header and body together
  rather than being grown once per geometry, which removes a reallocation a row.

### Fixed

- **`Feature::geometry_bytes` and the value accessors no longer disagree with
  the writer about the primary key**, as described under Changed. The
  round-trip is now covered by a test.
- **The bulk index gate rejects an index whose stored bounds exclude the
  geometry they index.** The existing fault test deleted a row, which the row
  count caught before any bounds were compared, so the comparison itself was
  never exercised.

## [0.2.0] - 2026-07-25

### Added

- **A ceiling on the geometry bytes one Arrow batch may hold**
  (`ArrowReadOptions::max_batch_bytes`, with `DEFAULT_MAX_BATCH_BYTES` and
  `default_max_batch_bytes`). The geometry column is Arrow `Binary`, whose
  offsets are `i32`, so one batch cannot address more than 2 GB of WKB. At the
  default 65,536 rows that needs only 32 KB of geometry per feature, which large
  polygons exceed. A batch that would cross the ceiling is emitted short and the
  rows that did not fit begin the next one, on both the single-threaded and
  threaded paths. The default follows GDAL in taking `min(INT32_MAX, RAM / 4)`.
- **Columnar read and write through Apache Arrow, behind a new `arrow`
  feature.** `Layer::read_arrow` returns an `ArrowBatches`, which implements
  `RecordBatchReader`; `Layer::write_arrow` writes record batches back through
  the same batching, bulk-index decision and transaction handling as
  `write_all`. Attribute columns follow a documented type mapping, and the
  geometry column is WKB carrying the `geoarrow.wkb` extension name, which costs
  nothing because a GPB body already is ISO WKB.

  Reads are threaded by default: `min(4, available parallelism)` workers, each
  with its own read-only connection over a disjoint primary-key range, with
  batches still arriving in key order. 259.5ms at the default against 529.6ms
  pinned to one thread over the same file. The threaded path declines to the
  single-threaded one rather than failing when its conditions do not hold: an
  in-memory database, a primary key with gaps, or fewer than two batches of
  rows. `ArrowReadOptions` sets the rows per batch (`DEFAULT_BATCH_SIZE`,
  65,536) and the thread count, where `1` reads on the calling thread.

  Neither direction goes through `Feature` or `Value`. Arrays are built from the
  statement's column values inside a SQLite aggregate function, and a row being
  written binds straight out of the Arrow buffers. That is a constraint on the
  implementation rather than an optimisation of it: GDAL measured its generic
  Arrow path, which does materialise a row, as slower than the row API it wraps.

  `Layer::arrow_schema` and `TableSchemaBuilder::from_arrow_schema` are the two
  directions of the type mapping, so a layer can be copied into a new file
  without its schema being restated; `TableSchemaBuilder::primary_key_name` is
  public for the same reason. New error variants: `Error::Arrow`,
  `Error::ArrowValueMismatch` and `Error::UnsupportedArrowType`.

- **EPSG codes outside the vendored WKT1 subset now register.**
  `GeoPackage::add_epsg_srs` refused anything the vendored subset did not carry.
  It now falls back to the EPSG registry, and a code with no WKT1 form at all,
  such as the geographic 3D EPSG:4979, is written as WKT2 into the
  `definition_12_063` column of the `gpkg_crs_wkt_1_1` extension, with
  `definition` holding the literal `undefined`. That is what the spec and GDAL
  both do for these codes; GDAL reads such a layer back as geographic 3D and
  normalises the WKT2 to a string identical to its own.

  Adding the extension columns, backfilling WKT2 for rows already present and
  inserting the new row share one transaction, so a failure leaves the file as it
  was rather than half-carrying an extension. Only a code in neither the subset
  nor the registry is still `Error::UnknownEpsgCode` ([#23]).

- **`geopackage-core` primitives the columnar paths needed, useful on their
  own.** `gpb::body_offset` gives a blob's WKB body offset without decoding the
  envelope. `geometry::encode_gpb_from_wkb` and `EncodedGpb` put a GPB header in
  front of bytes that are already ISO WKB, rather than parsing a geometry and
  serialising it straight back out; the body is still parsed, because the
  envelope has to be computed and because parsing is what rejects a body that is
  not ISO WKB, such as PostGIS EWKB. `Date::days_since_epoch`,
  `DateTime::micros_since_epoch` and their inverses give callers the boundary
  conversion the `datetime` module docs recommend without their having to take a
  datetime crate ([#24]).

- **`StorageStrictness`, controlling the two `Value` conversion leniencies.** A
  `BOOLEAN` column holding an integer other than 0 or 1, and an integer reaching
  a `FLOAT`/`DOUBLE` column, are both readable as their declared type and both
  non-conformant. `ConversionOptions::storage` now decides which of those facts
  wins. `StorageStrictness::Lenient` is the default and behaves as before;
  `Strict` reports `Error::NonBooleanInteger` (a new variant) for the first and
  `Error::ValueTypeMismatch` for the second. `ConversionOptions::with_datetime`
  and `with_storage` set the two axes independently ([#1]).

### Changed

- **GeoArrow CRS metadata is PROJJSON**, which is the form that specification
  prefers; it says an authority code "should only be used as a last resort",
  because it leaves the reader to resolve the code against a registry it may not
  have. An `EPSG:<code>` string remains the fallback for a code the registry
  does not know.

  Reading it back is a JSON parse rather than a scan for the first code. A CRS
  object nests identifiers for its coordinate system, datum and ellipsoid, and
  in EPSG:4326 the first to appear is 6422, the ellipsoidal coordinate system;
  only the top-level identifier names the CRS. The reader therefore also accepts
  PROJJSON written by other producers, which the earlier substring match could
  not ([#23]).

- **Calendar arithmetic is deferred to [jiff](https://docs.rs/jiff).** `Date`
  validates against jiff's calendar instead of carrying its own
  `days_in_month`/`is_leap_year`, and the new epoch conversions replace four
  hand-rolled implementations that had accumulated in the columnar path. The
  0-9999 year bound stays, since it comes from the spec's four-digit text form
  rather than from the calendar.

  jiff is configured with no timezone database at all: a GeoPackage `DATETIME`
  is UTC by definition and this workspace transforms neither coordinates nor
  times, so none of the `tz-*` or `tzdb-*` features are wanted. Measured at
  about 1.2 KB on a release binary, because only the entry points used survive
  dead-code elimination. No jiff type appears in any signature, so a jiff major
  version is not a breaking change here, and which text forms are accepted on
  read and written back is unchanged ([#24]).

- **The scalar write path is faster.** `FeatureWriter` composed its `INSERT`
  statement on every call: for a fifteen-column table roughly seventeen
  allocations per row, to produce one of four fixed strings. The four are now
  built once per writer and indexed by whether the row carries an explicit id
  and whether it carries a geometry. 22.6% off an unindexed point write and
  16.0% off a bulk one, measured at the same row count. This cost has been in
  the released write path since that path existed; it was found by asking what
  remained once the columnar-specific candidates were gone.

- **`create_layer` now builds a spatial index by default** ([#26]). Previously a
  feature layer came back unindexed and the caller asked for an index
  separately. Decline it with `TableSchemaBuilder::spatial_index(false)`.

  This changes the bytes a file gets: an indexed layer carries the
  `rtree_<table>_<column>` virtual table, its shadow tables, the GeoPackage 1.4
  trigger set and a `gpkg_extensions` row. All of that is spec-legal and is what
  `ogr2ogr` produces, but it is a change rather than an improvement in disguise.

  The reasoning: every other implementation indexes by default, and without an
  index `Layer::features_in` still answers correctly by falling back to a full
  scan, so the absence is invisible until someone profiles it. Building it at
  layer-creation time also makes it empty, which is the state that lets a
  subsequent large `write_all` or `write_arrow` fill it in one bulk pass instead
  of row by row through the triggers, so the default arrangement is also the fast
  one.

  Attribute tables are unaffected, having no geometry column.

  The table and its index are created in one transaction, so a failure building
  the index leaves nothing behind rather than a registered feature table with no
  index for the caller to notice and clean up.

- **`ConversionOptions::strict()` is now strict on both axes**, so it is no
  longer the same value as `ConversionOptions::default()`. `default()` is
  unchanged in behaviour: strict `DATETIME` parsing with lenient value reading.
  A caller who wants only strict `DATETIME` parsing wants `default()`. `Layer`
  seeds itself from `default()`, so feature reads are unaffected ([#1]).

- **`write_all` reaches the bulk index path for streaming sources.** The size of
  a write was taken from `Iterator::size_hint`, whose lower bound is 0 for most
  iterators not backed by a collection, so such a write stayed on the per-row
  triggered path however large it turned out to be unless the caller passed
  `always_bulk`. Where the hint cannot settle the question, rows are now buffered
  up to `bulk_threshold` to settle it, which is bounded by the threshold and
  never by the length of the input. Sized sources are unaffected ([#17]).

- **A bulk `write_all` normalises the trigger set it reinstalls**, which it has
  always done but now does in more cases. The path drops whatever RTree triggers
  a file carries and reinstalls the GeoPackage 1.4 set, so a file written against
  a pre-1.4 trigger generation comes out with 1.4 triggers. That was previously
  reachable only for a fresh bulk load into an empty index; it now also follows a
  large write into a populated index, or one from a source that does not
  advertise its length ([#17]).

- **The index is rebuilt or appended to, decided after the write.** A bulk
  `write_all` used to choose between rebuilding the index and leaving it to the
  triggers before writing a row, from that same lower bound. It now writes the
  rows first and chooses with both counts exact. A write too small to be worth a
  rebuild adds its entries to the existing index directly, running the statement
  the `_insert` trigger would have run over the envelopes computed during
  encoding, rather than falling back to per-row trigger maintenance. The
  resulting index is identical to a triggered write's ([#17]).

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
  `github.com/georust/geopackage`, which does not exist.
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
  generations detected and repairable rather than silently mixed. The bulk
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
  of this is addressed in 0.1.1; the remainder is [#20].
- Non-linear curve types cannot have envelopes computed and so cannot be
  inserted into an indexed table ([#5]).
- Feature iteration materialises the result set rather than streaming ([#4]).

[Unreleased]: https://github.com/urschrei/geopackage/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/urschrei/geopackage/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/urschrei/geopackage/compare/v0.1.2...v0.2.0
[0.1.2]: https://github.com/urschrei/geopackage/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/urschrei/geopackage/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/urschrei/geopackage/releases/tag/v0.1.0
[#1]: https://github.com/urschrei/geopackage/issues/1
[#26]: https://github.com/urschrei/geopackage/issues/26
[#3]: https://github.com/urschrei/geopackage/issues/3
[#4]: https://github.com/urschrei/geopackage/issues/4
[#5]: https://github.com/urschrei/geopackage/issues/5
[#15]: https://github.com/urschrei/geopackage/issues/15
[#16]: https://github.com/urschrei/geopackage/issues/16
[#17]: https://github.com/urschrei/geopackage/issues/17
[#20]: https://github.com/urschrei/geopackage/issues/20
[#22]: https://github.com/urschrei/geopackage/pull/22
[#23]: https://github.com/urschrei/geopackage/issues/23
[#24]: https://github.com/urschrei/geopackage/issues/24
