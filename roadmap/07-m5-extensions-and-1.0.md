# M5: extensions, then CLI and C ABI, then the API freeze

Goal: the extensions that files in the wild actually carry, the two consumer
surfaces M3 left unbuilt, and an API freeze.

## Shape of the milestone

Sequencing decided 2026-07-27: **extensions first, CLI and C ABI after**. Two
consequences worth stating before the task list, because they change how the
extension work is written rather than just when it happens.

- The M5 plan as originally drafted said "`gpkg validate` flags them" for
  deprecated extensions. There is no CLI to put that in, and there will not be
  one until phase 8, so every check lives in the library as
  `GeoPackage::validate()` and the CLI becomes a printer for it. That is the
  better division anyway: a check reachable only through a binary cannot be
  used by a caller embedding the crate.
- `gpkg_extensions` is entirely private today
  (`geopackage/src/extensions.rs` is three `pub(crate)` functions). Every
  extension below needs a public catalogue to hang off, so phase 1 is a
  prerequisite for the rest rather than one item among them.

This milestone also carries M3's unbuilt items. `geopackage-cli` and
`geopackage-ffi` do not exist, which leaves M3 acceptance criteria 6 and 7
unmet, M4's tile subcommands deferred, and design decision D12's single
`unsafe` carve-out hypothetical. They are phases 8 and 9 here.

No release is planned for any of this. The phases below are an order of work,
not a publication schedule; what ships, and when, is decided after M5.

## Phase 0: the Windows flake

- [x] Fix #44: cut the per-case work in
      `features_in_matches_full_scan_filter` so Hegel's `TooSlow` generation
      check stops firing on the slowest filesystem of the three. Option 1 in the
      issue, not the health-check suppression, and it wants doing before the
      tile property tests grow the same way. *(Done: the container goes in
      memory and the setup inserts share one transaction, following the
      reasoning already recorded on `in_memory_with_points` in
      `spatial_index.rs`. 3.401 s to 0.214 s locally, and the two tile
      properties and `rtree_tracks_full_scan_through_write_ops` got the same
      treatment, since they have the same shape. No generator draws fewer
      values than before.)*

## Phase 1: the extension catalogue as public API

- [x] Public `extensions` module: `ExtensionRow` (table, column, name,
      definition, scope) and `ExtensionScope`, with `GeoPackage::extensions()`,
      `Layer::extensions()` and `TilePyramid::extensions()`. Reading the
      catalogue is currently impossible from outside the crate.
      *(Done, plus `GeoPackage::table_extensions`, which the two handle methods
      call. Table names compare case-insensitively, as the note under
      Requirement 60 asks.)*
- [x] Classify each row: an enum covering what this crate implements
      (`gpkg_rtree_index`, `gpkg_zoom_other`, `gpkg_webp`, `gpkg_crs_wkt_1_1`,
      and each extension added below), the deprecated ones (geometry type and
      srs id triggers, legacy aspatial, the pre-rename elevation tiles name),
      and `Unknown(String)`. Support level per row: implemented, read only,
      deprecated, or unrecognised.
      *(Done as `geopackage_core::extensions::Extension` and
      `ExtensionSupport`. Two departures from the sketch. The names live in
      core, not in `geopackage`, because they are spec facts; `support()` went
      with them so its match is exhaustive, since across a crate boundary
      `#[non_exhaustive]` would force a wildcard and a new variant would
      classify itself silently. And "deprecated" is `Removed`: the SWG voted
      the two trigger extensions out of the standard on 2016-08-15 rather than
      deprecating them. Extensions with several historical spellings fold into
      one variant, which is where the pre-rename elevation tiles name went,
      along with `2d_gridded_coverage` and `gpkg_related_tables`.)*
- [x] **Refusal policy for extensions we do not implement.** Settle what the
      spec requires of a client meeting an extension it does not understand,
      quoting the clause rather than reasoning from the scope strings, then
      implement that. Today this crate will happily write to a table carrying
      an unknown extension, which is the one place the catalogue being private
      has a correctness cost rather than an ergonomic one.
      *(Settled. The spec requires nothing of a client here: Requirements 58 to
      64 constrain the file, not the reader. What it offers is clause 2.3.2's
      purpose for the table, that an application can query it "to determine if
      it has the required capabilities to read or write to tables with
      extensions, and to 'fail fast' and return an error message if it does
      not", and Requirement 64's division, `read-write` affecting readers and
      writers, `write-only` affecting only writers. So: writes to a table
      covered by an extension we cannot identify are refused with
      `Error::UnsupportedExtension`, of either scope, since both affect
      writers; reads are never refused, since only `read-write` affects readers
      and a reader that turns a file away helps nobody. Extensions we can name
      never trigger it, so `gpkg_metadata` beside a feature table is no
      obstacle.* *This is stricter than GDAL, which warns on update and
      proceeds (`OGRGeoPackageTableLayer`, `GetUnknownExtensionsTableSpecific`),
      so `OpenOptions::allow_unsupported_extension_writes` exists for a caller
      who knows better than the catalogue does.)*
- [x] `OpenWarning::UnsupportedExtension { name, table, scope }` from
      `open_lenient`, alongside the existing three variants.
- [x] Corpus test: enumerate every `gpkg_extensions` row across the committed
      fixtures and the fetched corpus, and assert each classifies rather than
      falling through. This doubles as the inventory of what real files carry,
      which is the evidence the phase order below should be revisited against.
      *(Done in `tests/extensions.rs` for the fixtures, which also pins the
      inventory so a fixture gaining an extension shows as a diff, and in
      `tests/corpus_external.rs` for the fetched corpus. What the corpus
      carries today: `gpkg_rtree_index`, `gpkg_metadata`, `gpkg_schema`. That
      is thin evidence for the phase order below, and worth revisiting against
      a wider corpus rather than treating as settled.)*

## Phase 2: `gpkg_crs_wkt_1_1`, the read side

Write support landed early, brought forward by #23, but only for definitions
`epsg-utils` supplies. Two gaps remain.

- [x] `Srs` gains `definition_wkt2: Option<String>` and `epoch: Option<f64>`,
      populated by `srs()` and `srs_list()` when the columns exist. `Srs` has
      public fields, so this is a breaking change: take the
      `#[non_exhaustive]` decision here rather than in phase 10, since the
      freeze is the last chance to make it and this is the change that forces
      the question.
      *(Done, and the decision is **not** `#[non_exhaustive]`. The argument for
      it is that the struct might grow; with these two fields it now covers the
      whole of the CRS WKT 1.1 table definition, and the spec fixes that column
      set, so there is nothing left to grow into. Marking it would also break
      construction, which `add_srs` requires of callers, and would need a
      builder to replace. The same reasoning made `ExtensionRow` a plain struct
      in phase 1. `undefined`, the spec's value for a definition that could not
      be produced, reads back as `None` rather than as a definition.)*
- [x] `add_srs` accepts a caller-supplied WKT2 definition and epoch, enabling
      the extension on demand through the existing `enable_crs_wkt_extension`.
      D3 says users can supply arbitrary definitions; that is currently true of
      WKT1 only.
      *(Done. `add_epsg_srs_via_wkt2` now goes through `add_srs` rather than
      carrying its own insert.)*
- [x] Round trip: a file we write with a WKT2 definition reads back through
      GDAL with the same CRS, and a GDAL-written file with the extension reads
      back here with both definitions intact.
      *(Done as `crs_wkt_extension_round_trips_with_gdal` in `gdal_interop.rs`,
      both directions, against GDAL 3.12.3.)*

One thing settled while reading the spec source here, recorded because it looks
like a conformance gap and is not one. The CRS WKT 1.1 document
(`spec/crs_wkt/clause_7_normative_text.adoc`) lists three `gpkg_extensions`
rows as required: `gpkg_crs_wkt` against `definition_12_063`, and
`gpkg_crs_wkt_1_1` against each of `definition_12_063` and `epoch`. This crate
writes two, both `gpkg_crs_wkt_1_1`. So does GDAL, which writes the
`gpkg_crs_wkt` row while a file has no epoch column and *renames* it on adding
one rather than keeping both. The ETS does not catch the difference, since its
`gpkg_crs_wkt` check is "not testable" against an empty result set. We follow
GDAL: interoperating with the files that exist beats matching a table no
widespread implementation produces.

## Phase 3: `gpkg_schema`

- [x] DDL for `gpkg_data_columns` and `gpkg_data_column_constraints` verbatim
      from Annex C into `geopackage-core::ddl`, as the tile tables were.
      *(Done, less the `//` comments the spec source writes them with, which
      SQLite does not accept.)*
- [x] Model: `DataColumn` (name, title, description, mime type, constraint
      name) and a typed `ColumnConstraint` for the enum, range and glob forms,
      with the range's inclusivity flags carried rather than flattened.
      *(Done in `geopackage_core::schema`. A constraint is assembled from the
      rows sharing its name rather than returned row by row, since an enum
      occupies one row per member and a range or glob exactly one. The enum's
      member order is the file's and means nothing: a round trip through GDAL
      reorders it, which the interop test compares as a set and the type's
      documentation now warns about.)*
- [x] Surfaced on `TableSchema`, so a caller asking for a layer's schema sees
      the aliases and constraints without a second lookup.
      *(Done as `Column::data_column`, filled by one query per schema read and
      none at all for a file without the extension. The constraint itself is
      resolved by name on demand rather than eagerly, since constraints are
      shared between columns.)*
- [x] **Enforcement on write behind an option.** Two things to settle while
      building it. Glob matching needs SQLite's semantics, and calling back
      into SQLite per value on a write path this heavily tuned is not viable,
      so the pattern language gets a Rust implementation with a property test
      asserting agreement with SQLite's own `GLOB` over generated patterns and
      inputs. And enforcement has to cover the Arrow and bulk write paths, not
      only `FeatureWriter`, or the option means "checked unless you used the
      fast path".
      *(Done as `OpenOptions::enforce_column_constraints`, off by default: the
      spec makes these constraints advisory, so a conforming file may hold
      values its own constraints forbid and refusing them by default would
      impose a rule the format does not. The check sits at the `FeatureWriter`
      boundary, where the three value representations meet, so the scalar,
      bulk, Arrow and partial-update paths are all covered by one
      implementation, and each has a test. NULL satisfies every constraint, and
      blob, date and datetime values are not checked, both documented on the
      option.)*

      *The glob form went the other way from the sketch above. It was first
      written here, following `patternCompare` in SQLite's `func.c`, with the
      property test the plan asks for. That was then deleted in favour of
      asking SQLite through a `SELECT ?1 GLOB ?2` prepared once per writer. The
      plan's premise, that calling into SQLite per value is not viable, was a
      judgement rather than a measurement, and measuring it found the opposite:
      the engine is 22% faster per call, because it walks UTF-8 bytes and
      allocates nothing where the matcher collected two `Vec<char>`. It is also
      the authority on its own pattern language, which has no definition beyond
      what it does, and this crate bundles it, so a copy of its rules could
      drift from the engine holding the file with nothing failing. What the
      matcher's property test proved is now structurally true. The awkward
      corners are kept as an end-to-end test so that reverting to a hand-rolled
      matcher would have to face them again.)*
- [x] Benchmark the enforcement cost against the unenforced write, and record
      it. The write path is the one place in this crate where a per-value check
      can be measured against a figure we already publish.
      *(**About 31%** on a 200,000-row write with two constrained columns,
      159 ms to 209 ms, recorded in
      [benchmarks/2026-07-27-constraint-enforcement.md](benchmarks/2026-07-27-constraint-enforcement.md),
      along with the per-call comparison of the two glob implementations. An
      earlier pair of figures from the same benchmark, 569 ms and 651 ms, was
      taken while the machine was running the test suite and is not comparable
      with anything; the note records that too rather than leaving the
      discrepancy to be found later.)*
- [x] Interop: GDAL maps these to field domains, so a file written here should
      show its domains in `ogrinfo`, and a GDAL-written domain should read back
      as the equivalent constraint.
      *(Done as `column_constraints_round_trip_as_gdal_field_domains` in
      `gdal_interop.rs`, against GDAL 3.12.3: `ogrinfo -fielddomain` describes a
      range we wrote, and a constraint survives `ogr2ogr` copying the file,
      which means GDAL read ours and wrote its own from it.)*

## Phase 4: `gpkg_metadata`

- [ ] DDL for `gpkg_metadata` and `gpkg_metadata_reference` verbatim from
      Annex C. Check whether 1.4 still defines the metadata reference triggers
      or whether they went the way of the deprecated trigger set, and write
      what the current spec says rather than what 1.2 said.
- [ ] Typed `md_scope` and `reference_scope`, both closed sets in the spec.
      Payloads stay strings: no XML parse, no profile interpretation, which is
      the same posture as tile payloads staying opaque bytes.
- [ ] Timestamps go through the existing strict DATETIME handling rather than a
      second format path.
- [ ] API: enumerate metadata, add a record, attach a reference to a
      GeoPackage, table, column or row, and ask what is attached to a given
      target. Parent and child references (`md_parent_id`) are a graph, so
      decide whether the read API walks it or hands back the edges.

## Phase 5: Related Tables

The largest of the extension items, and the one with the most producer
variation to check against.

- [ ] Read `gpkgext_relations` and walk a mapping table (`base_id`,
      `related_id`) for any relation type.
- [ ] Write for `simple_attributes` and `media` first, each registering the
      extension rows the relation needs, including the row for the mapping
      table itself, and adding the related table to `gpkg_contents`.
- [ ] Establish which OGC 18-000 version the ecosystem actually writes, by
      reading the relations rows in the corpus and in GDAL-written and
      QGIS-written samples, before fixing the model. This is the item most
      likely to need a fixture we do not have yet.

## Phase 6: non-linear geometry, passthrough only

Decided 2026-07-27: read the bytes through, do not compute envelopes for them,
do not linearise. Issue #5 stays open for the envelope question.

- [ ] Read: a GPB whose body declares CircularString, CompoundCurve,
      CurvePolygon, MultiCurve or MultiSurface must not fail at
      `GpbGeometry::parse`, which today builds a `Wkb` eagerly and will reject
      the type code. The geometry surfaces as its type plus its raw WKB bytes,
      with the existing `wkb_geometry_type` doing the classification off the
      body directly.
- [ ] Write: accept raw WKB carrying a curve type, register the matching
      `gpkg_geom_<TYPE>` row, and set the GPB header's extended flag, which
      `encode_header` already takes as a parameter.
- [ ] Refuse insertion into an rtree-indexed table with a typed error naming
      the reason, since `encode_gpb_from_wkb` derives the envelope by parsing
      and cannot parse these. **Escape hatch to design:** a caller who knows
      the envelope can supply it, at which point the header and the index can
      both be written and the refusal does not apply. That keeps the crate
      useful for curve data without putting arc mathematics in core.
- [ ] Fixture: a curve-carrying file, written by GDAL, committed alongside the
      others and walked by the corpus tests.

## Phase 7: `validate()` in the library, extended by each phase after

The check surface M3 planned to put in `gpkg validate`. It lands in the library
because the CLI is now phase 8, and because a check only a binary can run is
not much use to an embedding caller.

- [ ] `GeoPackage::validate()` returning typed findings with a severity, over:
      the deprecated extensions from phase 1 (tolerated on read, never written,
      reported here), the spatial index audit that already exists as
      `SpatialIndexAudit`, the open warnings, `gpkg_contents` rows whose table
      is missing, and the tile matrix consistency rules that
      `TilePyramid::validate` already checks.
- [ ] Each finding carries repair advice where repair exists, since that is
      what the CLI will print.
- [ ] Run it over every corpus file in CI. Known findings on known files become
      the expected output, so a change in what we detect shows up as a diff.

## Phase 8: `geopackage-cli`

Closes M3's CLI item, M3 acceptance criterion 6, and M4's deferred tile
commands.

- [ ] `gpkg info <file>`: version, contents, srs, index status including
      trigger generation, extensions with their support level.
- [ ] `gpkg validate <file>`: prints phase 7's findings and their repair
      advice.
- [ ] `gpkg copy <src> <dst>`: any supported read to our write. The dogfood
      command, and the full-circle test in M3 criterion 6.
- [ ] `gpkg index <file> <layer>` and `gpkg repair`.
- [ ] `gpkg tiles info` and `gpkg tiles get z/x/y --out tile.png`, deferred
      from M4 for want of this crate.
- [ ] Ships as a bin crate, and becomes the corpus generation harness the
      testing plan assumes from M3 onwards.

## Phase 9: `geopackage-ffi`

Closes M3's C ABI item and acceptance criterion 7, and makes D12's carve-out
concrete.

- [ ] New crate, `cdylib` plus `staticlib`, packaged with cargo-c
      (pkg-config, versioned soname). Opaque `gpkg_t` and `gpkg_layer_t`,
      UTF-8, `gpkg_error_t` out-params with code, message and a free function.
- [ ] The sole crate not taking `[lints] workspace = true`: `unsafe` confined
      to the C ABI surface, `undocumented_unsafe_blocks` applied, sanitizer and
      miri gating in CI before anything outside the workspace links against it.
- [ ] Control plane: open, create, close, list layers, schema introspection,
      create layer, create and drop spatial index, begin and commit.
- [ ] Data plane: `gpkg_layer_read_arrow` and `gpkg_layer_write_arrow` through
      the Arrow C Data Interface. Row-at-a-time C accessors stay omitted.
- [ ] cbindgen header checked in, CI failing on an undocumented header diff,
      and a C program in CI reading a corpus file through the stream.
- [ ] SQLite thread model documented: handle per thread, or an external lock.

## Phase 10: hardening and the API freeze

- [ ] **API review.** Audit every `pub` item; `#[non_exhaustive]` where growth
      is plausible; error variants stabilised; rusqlite kept out of the public
      API except through documented escape hatches; MSRV policy written down.
      Mechanise it with a `cargo public-api` diff gate in CI so the freeze is
      enforced continuously rather than asserted once.
- [ ] **Settle #29, the lending cursor.** The recommendation from the evidence
      in #30 is to close it as work for after the freeze: column projection shipped in
      0.5.0 and captured most of the benefit on geometry-heavy layers, and a
      borrowed cursor is purely additive API, so deferring it costs nothing at
      the freeze. Record the decision either way, since the issue has been open
      across three milestones.
- [ ] **Revisit the D8 bulk-build gate.** Every bulk index build verifies itself
      before it is trusted: a bijection and containment check of the written
      index against the accumulated envelopes, plus `rtreecheck` over the tree,
      with a fallback to the triggered build on any anomaly. That is about **45%
      of the build** (~745 ms of a ~1593 ms build at 1M points), and GDAL's
      builder runs no equivalent, so without it we would be faster than GDAL
      rather than level with it. The cost is the right call while
      `geopackage/src/packed.rs` is new: it writes an RTree by hand into a format
      SQLite does not document as an interface. The question at the freeze is
      whether the packer has enough history by then to make the gate opt-in, or to keep only
      the cheaper half. Decide it on the evidence at the time rather than on the
      benchmark alone, and if it is relaxed, keep a way to turn it back on. See
      [benchmarks/2026-07-24-gdal-like-for-like.md](benchmarks/2026-07-24-gdal-like-for-like.md).
- [ ] OSS-Fuzz onboarding (gpb, WKB fallback, `open()` on arbitrary SQLite
      files, tile matrix parsing, and the new parsers from phases 3 to 6).
- [ ] Full-corpus soak: every file opened, fully read, indexes rebuilt and
      compared, weekly CI.
- [ ] Performance regression CI (criterion plus threshold alerts), and the
      allocation benches with it: #28 for the index build and #31 for the tile
      paths, both needing a Linux runner because Valgrind has no macOS arm64
      port. Deciding where that runner comes from is the blocking part of both
      issues, not the benches themselves.
- [ ] The QGIS interop job (#6), which also carries M4's undelivered acceptance
      criterion 2: a pyramid written here rendering correctly in QGIS.
- [ ] Docs: book-style guide (mdBook), cookbook for the ten common tasks,
      migration notes from gdal, gpkg-rs and rusqlite-gpkg, FFI integration
      guide.
- [ ] The M3 Arrow items still unticked, if they are still wanted at the
      freeze: parallel `write_arrow` (one writer, CPU work moved off the
      writing thread), GeoArrow field metadata with CRS as PROJJSON, and the
      pyarrow and pyogrio interop test.

## Bindings: a slot, unscheduled

Sized here, not scheduled: no consumer has asked. Revisit before the freeze,
since a binding is easier to add against a frozen API than to keep in step with
a moving one.

- `geopackage-py`: PyO3 plus maturin abi3 wheels; open and create, Arrow
  streams, geopandas `from_arrow`/`to_arrow` convenience, benchmark page
  against pyogrio. The C ABI from phase 9 plus a ctypes shim already covers the
  read case, which is what M3 criterion 5 exercises, so this is an ergonomics
  and packaging piece rather than a capability one.
- Node via napi-rs, or browser via wasm plus the D5 `serialize` bytes API:
  pick on who asks.
- uniffi (Swift and Kotlin): when a mobile consumer materialises.

## Acceptance criteria

1. **Nothing in the corpus is silently ignored.** Every `gpkg_extensions` row
   across the committed fixtures and the fetched corpus classifies through the
   public API, and the test fails on an unrecognised name rather than skipping
   it.
2. **An extension we do not implement blocks the writes the spec says it
   should.** A file carrying such a row cannot be written through this crate,
   and the error names the extension.
3. **ets-gpkg12 passes on files carrying each extension we write**, as it does
   for tiles, with the classes it skips recorded and explained as M4 did for
   `Tiles Encoding WebP`.
4. **GDAL round trip for the extensions GDAL implements**: schema as field
   domains, metadata, related tables where its support reaches, with anything
   it does not support recorded rather than quietly untested.
5. **`gpkg copy` GDAL file to ours, validators clean** (M3 criterion 6).
6. **C header diff gate active, `cargo c-build` artifacts install and link on
   all three OSes** (M3 criterion 7).
7. **The API is frozen and the freeze is mechanised**: `cargo public-api` diff
   gate in CI, MSRV policy written down, two-release deprecation policy
   adopted.

## Explicit non-goals

- **Tiled gridded coverage.** Re-assess upstream status once during this
  milestone and record the answer. If it is still under revision, it stays out
  and the TIFF rejection M4 added stands.
- **Curve envelopes** (#5). Phase 6 takes the passthrough, so an arc's exact
  extrema stay unimplemented and curve geometries stay out of the rtree unless
  the caller supplies an envelope. Reconsider if georust/wkb grows a curve
  reader.
- **Linearisation** of any curve type, in core or elsewhere.
- **A second backend**, as the standing items below already record.

## Standing items (never "done")

- Track spec changes (a 1.4.x errata or 1.5 draft would land here first:
  watch [opengeospatial/geopackage](https://github.com/opengeospatial/geopackage)).
- Track ETS releases: if an ets for 1.3/1.4 appears, wire it into CI
  alongside ets-gpkg12.
- Track Turso/limbo rtree + vtab parity for a possible second backend
  (explicitly not before 1.0).
- Org coordination (#18) is tracked in its own issue and is not planned around
  here.
