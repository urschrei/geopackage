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

- [x] DDL for `gpkg_metadata` and `gpkg_metadata_reference` verbatim from
      Annex C. **1.4 defines no metadata reference triggers**: 1.2 had
      `gpkg_metadata_reference_column_name_insert` and `_update`, and 1.4's
      Annex D defines trigger SQL only for `gpkg_tile_matrix` and the two
      sample tables, with no mention of the metadata pair anywhere in the spec
      text. They went the way of the deprecated RTree trigger set. Nothing
      creates them; a file carrying them is older rather than wrong, and the
      DDL constant records this.
- [x] Typed `md_scope` and `reference_scope`. **Only one of them is a closed
      set**, contrary to how this item was written. `reference_scope` is closed:
      Requirement 96 gives five lowercase values and no escape hatch, so
      `ReferenceScope::parse` returns `Option`. `md_scope` is not: Requirement
      94 says SHALL, then SHOULD, then "however, this list is not exhaustive;
      new scopes are permitted", so `MetadataScope` carries `Other(String)`.
      Modelling it closed would have made a conformant file unreadable.
      Payloads stay strings, as planned.
- [x] Timestamps go through `geopackage_core::datetime` (Requirement 100 puts
      them in the same DATETIME form as everything else), so there is no second
      format path.
- [x] API: `metadata()`, `metadata_record()`, `add_metadata()`,
      `add_metadata_reference()`, `metadata_references()`, `metadata_for()`.
      A `MetadataTarget` states scope and target as one value, so the NULL
      pattern Requirements 97 to 99 impose cannot be got wrong at a call site.
      **Decided: enumeration hands back edges, and the walk is its own call.**
      `metadata_ancestors()` walks `md_parent_id` upwards and reports a cycle
      as a typed error. Requirement 102 forbids only a record being its own
      parent, so a longer cycle is a file to survive rather than a case to rule
      out, and that cost should not be paid by every enumeration.

Not done here: `validate()` checks over these tables belong to phase 7, which
is where every phase's checks collect.

## Phase 5: Related Tables

Still the largest of the extension items, but no longer the least certain: the
research item below was done first, and it found a single published spec version
and a single dominant producer to match, rather than the producer variation this
was originally sized for.

- [x] Read `gpkgext_relations` and walk a mapping table (`base_id`,
      `related_id`) for any relation type. `relations()`, `relations_from()`
      and `related_ids()`. Reading deliberately does not depend on recognising
      the relation type: a relationship is a base table, a related table and a
      mapping table, and that is all a walk needs.
- [x] Write, for every requirements class rather than two: `media`,
      `simple_attributes`, `features`, `attributes` and `tiles` are all defined
      classes, and the `x-<author>_<name>` form is accepted too, so there was
      no reason to start with a subset. `add_relation()` creates the mapping
      table, registers a `gpkg_extensions` row for it and for
      `gpkgext_relations`, and checks Requirements 5 and 6 (both ends in
      `gpkg_contents`) and Requirement 8 (the `relation_name` form).
      `add_mapping()` writes pairs.

      Cardinality stays unmodelled and duplicates are kept, per the research
      above. Mapping table columns are written `NOT NULL`, following Table 3
      rather than GDAL.
- [x] Establish which OGC 18-000 version the ecosystem actually writes.
      **Answered 2026-07-27: there is only one.** 18-000 is version 1.0,
      approved 2019-03-26 and published 2019-05-08; the 0.1 and 0.2 entries in
      its Annex E revision history are pre-publication drafts, not releases.
      There is no 1.1. The item's premise, that a version has to be chosen,
      dissolves; what remains is matching what GDAL writes.

      **The name variance is an alias, not a version signal.** The spec says
      "Extension Name or Template: `related_tables`; upon adoption the alias
      `gpkg_related_tables` MAY be used", and its own abstract test suite
      queries `extension_name IN ('related_tables', 'gpkg_related_tables')`.
      `Extension::from_name` already accepts both. The open decision is which to
      *write*: GDAL 3.12.3 writes `gpkg_related_tables`, and our `name()`
      currently canonicalises to `related_tables`, so writing our canonical form
      would make us the odd producer out for no gain.

      **What GDAL 3.12.3 writes**, verified by driving `AddRelationship`
      through the Python bindings:

      - `gpkgext_relations(id, base_table_name, base_primary_column,
        related_table_name, related_primary_column, relation_name,
        mapping_table_name)`, with `relation_name` carrying the related-table
        type (`simple_attributes`, `media`, …).
      - Two `gpkg_extensions` rows, both `gpkg_related_tables`, scope
        `read-write`, `definition` `http://www.geopackage.org/18-000.html`,
        `column_name` NULL: one for `gpkgext_relations` itself and one per
        mapping table. This matches the spec's own tests, which require exactly
        one row for `gpkgext_relations` and at least one mapping-table row.
      - Mapping table as `CREATE TABLE "x" (id INTEGER PRIMARY KEY
        AUTOINCREMENT, base_id INTEGER, related_id INTEGER)`. The extra `id` is
        permitted: Requirement 9 says the table SHALL contain `base_id` and
        `related_id` and MAY contain other columns.
      - The mapping table also gets a `gpkg_contents` row as `attributes`.
        Requirements 5 and 6 mandate that only for the base and related tables;
        for the mapping table it is permitted rather than required.

      **One place GDAL is looser than the spec**: Table 3 gives `base_id` and
      `related_id` as Null "no", and GDAL's DDL leaves both nullable. Read
      permissively, write `NOT NULL`, and do not let `validate()` fail a
      GDAL-written file over it.

      **Cardinality is deliberately unconstrained.** The spec notes that a
      `UNIQUE` constraint could enforce one-to-many but is NOT RECOMMENDED,
      because SQLite does not expose such constraints in an easily queryable
      way. So the model should not try to infer or enforce cardinality.

      `ets-gpkg12` carries `RTETests`, so acceptance criterion 3 has a path for
      this extension without new harness work.
- [x] Fixture: `gdal_related.gpkg`. No GeoPackage in the committed fixtures or
      the fetched corpus carried `gpkgext_relations`, confirmed by sweeping
      both, so one is generated through GDAL's `osgeo` Python bindings, which
      is the only entry point offering `AddRelationship`: neither `ogr2ogr` nor
      the `gdal` CLI exposes it as of 3.12. The bindings are optional the way
      `qgis_process` is, so the generator warns and skips rather than failing
      where they are absent, and the committed fixture is what the tests read.
      Two traps are recorded at the builder, because they cost three attempts:
      the left and right table fields must be set (the FID column name is
      accepted), and naming the mapping table's own field names makes the call
      fail while still leaving `gpkgext_relations` behind, so a half-built file
      is the symptom rather than an error.

Corpus budget: the fixtures now total 247 KB of the 256 KB cap, so the next
one needs a trim first. The two curve and related fixtures cost 82 KB between
them, most of it RTree trigger SQL.

## Phase 6: non-linear geometry, passthrough with computed envelopes

Decided 2026-07-27: read the bytes through, do not compute envelopes for them,
do not linearise, and refuse to index them. **Revised the same day**, after
checking what the spec and the other implementations actually do:

- Annex F.3 Requirement 78 says the `ST_*` functions *shall* work on the
  non-linear types when that extension is implemented, and the extension
  applies to "any column specified in the `gpkg_geometry_columns` table". So
  supporting curves and refusing to index them diverges from the spec rather
  than conforming to it.
- PostGIS (`lw_arc_calculate_gbox_cartesian_2d`) and GDAL
  (`OGRCircularString::ExtendEnvelopeWithCircular`) both compute an exact arc
  envelope analytically. Neither refuses, and neither linearises to do it.
- The arc mathematics is about 150 lines and needs no dependency, and the
  envelope has to be right anyway: an arc bulges past its control points, so a
  control-point envelope in the GPB header is a silent correctness bug for any
  reader that trusts the header, which GDAL does.

So the escape hatch is not needed, and `gpkg_rtree_index` covers curve layers.

- [x] `geopackage-core::curve`: walk an ISO WKB body directly, computing exact
      arc extents. Removes the dependency on curve support landing in `wkb`.
- [x] Write: accept raw WKB carrying a curve type, register the matching
      `gpkg_geom_<TYPE>` row, and set the GPB header's extended flag.
      `FeatureWriter::insert_wkb` is the scalar entry point, so this does not
      need the non-default `arrow` feature.
- [x] Index: curve layers get an rtree like any other, with entries that bound
      the arc rather than its control points.
- [ ] Read: a GPB whose body declares a curve type still fails at
      `GpbGeometry::parse`, so `Feature::geometry` errors and
      `Feature::geometry_bytes` is the way to get one. Fixing this needs a
      `geo-traits` representation for an arc, not just a reader, so it is an
      upstream question rather than a local one. Tracked in
      [02-ecosystem.md](02-ecosystem.md).
- [x] Fixture: `gdal_curves.gpkg`, one GDAL-written layer per non-linear type,
      all five spatially indexed. No `.expected.json`: `ogrinfo -json` has to
      emit GeoJSON, which has no curve type, so GDAL stroke-converts each arc on
      the way out and the snapshot would assert a linearisation. The RTree
      entries GDAL wrote are the oracle instead, checked in
      `geopackage/tests/curves.rs`, and they agree with ours to within a few
      ulps despite GDAL reaching them by a quadrant sweep rather than the
      chord-side test.
- [ ] Register the member types of a container geometry, not only the declared
      column type. Found by the fixture: GDAL's `multicurve` layer carries
      `gpkg_geom_CIRCULARSTRING` alongside `gpkg_geom_MULTICURVE`, and its
      `multisurface` layer carries `gpkg_geom_CURVEPOLYGON`. `create_layer`
      registers only what it is told, which is the column's declared type, so
      ours is the thinner registration. Doing this properly means noticing
      member types as geometries are written rather than at create time.
      `geopackage/tests/curves.rs` pins the current divergence.

Fixture budget note: the five indexed layers cost 57 KB of the 256 KB corpus
budget, because each carries the seven-trigger RTree schema. The total is now
223 KB. A sixth curve layer would not fit; trimming a layer or raising the
budget is the choice if one is wanted.

Issue #5 can close once the fixture lands; the envelope question it was open
for is answered.

## Phase 7: `validate()` in the library, extended by each phase after

The check surface M3 planned to put in `gpkg validate`. It lands in the library
because the CLI is now phase 8, and because a check only a binary can run is
not much use to an embedding caller.

- [x] `GeoPackage::validate()` returning typed findings with a severity, over
      everything the item listed plus the two catalogues phases 4 and 5 added:
      dangling `gpkg_metadata_reference` rows and relationships whose mapping
      table is gone. `Severity` is `Error` when a reader can get a wrong
      answer, `Warning` when the file is out of step with the current spec but
      reads correctly, and `Advisory` for a remark such as an unindexed layer.
      Findings come back most severe first.
- [x] Each finding carries repair advice where repair exists, naming the method
      that performs it. `repair()` is `None` where the fix needs the producing
      writer or a decision about data this crate should not take on the
      caller's behalf, which is most of the extension findings.
- [x] Run it over every committed fixture, with the findings pinned, so a
      change in what is detected is a diff. Two the sweep established rather
      than confirmed: `gdal_points_1_2.gpkg` reports
      `LegacySpatialIndexTriggers`, correctly, since a 1.2 file carries the
      pre-1.4 trigger set, and `gdal_multilayer_1_4.gpkg` reports three
      unindexed layers, which is how it was built.

The fetched corpus is pinned the same way, in the ignored
`corpus_external.rs`, as counts per finding kind rather than a list, since a
sixteen-layer file with no indexes reports sixteen identical findings. What the
four pinned files report: `gdal_sample_v1.0` has the GP10 identifier, the
pre-1.4 trigger set on fifteen indexed layers and one unindexed layer;
`gdal_sample_v1.2_no_extensions` has sixteen unindexed layers; `nga_rivers` has
GP10 and one unindexed layer; `ogc_sample1_2` has the pre-1.4 triggers on its
one indexed layer. No errors anywhere, so nothing in the corpus is unreadable.

Corrected while doing it: the soak's comment said some published samples carry
curve geometries the `wkb` reader cannot parse. Not true of the pinned corpus,
which declares no non-linear geometry type in any of the four files, so the
curve read limitation is not exercised there at all.

Noticed while writing the tests: a dangling `gpkg_metadata_reference` cannot be
created through SQL while foreign keys are enforced, since Annex C's DDL
declares them. Such a file arrives from a writer that had enforcement off,
which is the default in SQLite and what the common producers do, so the check
earns its place; the test turns them off to build the case.

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
- [ ] Ships as a bin crate.

**Corpus generation is cut from this phase**, and from the CLI's remit
generally. M3 assigned it here and
[08-testing-conformance.md](08-testing-conformance.md) carried the assumption,
but the role was filled meanwhile by `scripts/generate_fixtures.py`, which
builds the committed fixtures by driving GDAL, QGIS and raw `sqlite3` and
commits GDAL's own read beside each as the oracle. That is the right generator
and the CLI is not: the corpus exists to test against other implementations, so
a fixture this crate wrote and this crate reads proves nothing about interop.

Two things follow. `gpkg copy` no longer needs to be faithful enough to
reproduce arbitrary fixtures, so it can start at features and grow to tiles and
the extension tables only if something asks for it; M3 criterion 6 asks only
that a GDAL file copied through us comes out clean under the validators. And a
`--json` output mode loses its test-harness justification, since the corpus
oracle is GDAL's JSON rather than ours, leaving it an ergonomics question to
settle later.

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
      the Arrow C Data Interface.
- [ ] cbindgen header checked in, CI failing on an undocumented header diff,
      and a C program in CI reading a corpus file through the stream.
- [ ] SQLite thread model documented: handle per thread, or an external lock.

### Scope: the C API mirrors the Rust API

**Decided 2026-07-28.** Every Rust API call is exposed through the C ABI, within
reason. The earlier sketch above was a control plane plus Arrow streams, with
row-at-a-time accessors "deliberately omitted (Arrow is the data plane; revisit
only on concrete demand)". That is too narrow to be worth building: a C consumer
that cannot read a tile or run a bounding-box query is not a binding to this
crate, it is a binding to part of it.

Three consequences follow, none of them free:

- **A tile handle is needed.** The sketch names only `gpkg_t` and
  `gpkg_layer_t`, so `TilePyramid` had no C representation at all. It needs one,
  and on the measurements `get_tile` is the most call-heavy entry point in the
  crate, so it is the handle whose design costs most to get wrong.
- **`features_in` has no Arrow counterpart**, and it needs one. See the item
  below, which is library work rather than FFI work and precedes this phase.
- **Cursors and streams need C representations**, so `FeatureCursor`,
  `FeatureStream`, `TileCursor` and `TileStream` join the handle question rather
  than sitting outside it.

### Phase 8b: a bounding-box-filtered columnar read

`read_arrow` reads a whole layer; there is no columnar equivalent of
`features_in` (`geopackage/src/arrow.rs`). Under the scope decision above a C
consumer needs spatial queries, so the gap has to close one of two ways: build
the counterpart, or expose row-at-a-time accessors after all.

**Decided: build the counterpart.** Row accessors would reverse a deliberate
choice, and they cost a family of C entry points (per-value accessors, type
dispatch, string lifetimes, and the `unsafe` for each) to duplicate a data plane
that already exists. The filtered columnar read is one C entry point instead.
This is `geopackage` work, not `geopackage-ffi` work, and it is M3 debt that the
FFI merely surfaces, so it sits before phase 9 rather than inside it.

- [ ] `Layer::read_arrow_in(bbox, options)`. The SQL is the existing paginated
      query plus the `rtree_select` join and its four bound parameters. Without
      a spatial index it declines to a full scan carrying the exact filter,
      matching what `features_in_plan` already does.
- [ ] **The exact re-filter is required, not optional.** The rtree parameters
      are deliberately widened (`widen_up`/`widen_down`) because the index
      stores float32 envelopes, so its candidates are a superset.
      `FeatureStream` re-tests each candidate's blob before converting the row;
      the columnar path must do the same or it silently returns rows
      `features_in` would not. A batch of N candidates therefore yields at most
      N rows, and under-filling a batch is acceptable, as the byte ceiling
      already cuts batches short.
- [ ] **The aggregate and parallel paths decline to the direct loop when a
      bounding box is set**, rather than failing, which is the idiom the
      threaded read already uses for each of its three conditions. The
      aggregate builds columns inside a SQLite aggregate function, so the
      re-filter would have to move in there too. The parallel path assigns key
      *windows* to workers on a density rule (`max - min + 1 == count`), and a
      filter voids it: matching rows scatter, so equal key windows carry
      unequal work.
- [ ] **Only then decide whether a threaded filtered read pays.** M3 chose the
      design already (one thread runs the rtree scan and hands candidate ids to
      workers in blocks, so the scan happens once and no feature is returned
      twice) and recorded the open question with it: whether bbox results are
      typically large enough for any of it to pay, given that fetching an
      arbitrary id list is index lookups rather than a rowid range scan. Answer
      that with a measurement against the single-threaded path, not in advance.

### The handle-lifetime question, and what it costs

`Layer<'a>` and `TilePyramid<'a>` borrow the `GeoPackage` they came from, so a C
ABI handle cannot hold either: a C caller keeping a layer handle past its
container handle is exactly the use-after-free the lifetime prevents. This has
to be settled before the crate is written, and before phase 10 freezes whatever
it settles on.

Three facts, all established rather than assumed.

**The performance side is decided.** Rebuilding a borrowed handle inside every
FFI call costs a near-constant 37 to 51 µs, so the overhead is inversely
proportional to how cheap the call is: 778% on `get_tile`, 675% on `extent`,
131% to 150% on the small `features_in` results an interactive map issues, and
13% only at a thousand rows. Streaming paths are unaffected, since their handle
is built once per scan. An owned handle would add 3.77 ns per construction, and
nothing builds a handle per row or per batch. See
[benchmarks/2026-07-28-handle-construction.md](benchmarks/2026-07-28-handle-construction.md).

These figures bear on the C ABI only because of the scope decision above. Under
the earlier narrow sketch, `get_tile` and `features_in` did not cross the
boundary at all, and the only exposed data-plane path was the streaming one that
amortises a rebuild over a whole scan; on that reading the performance argument
was close to moot. Widening the surface to mirror the Rust API is what makes the
per-call figures the deciding ones.

**Owning costs `GeoPackage` its `Send`.** `Arc<T>` is `Send` only when `T` is
`Sync`, and `Connection` is `Send` but not `Sync`, so a `GeoPackage` built as
`Arc<Inner>` is neither. `GeoPackage` is `Send` today (verified by compiling the
assertion), and D1 records the intended async story as "a `spawn_blocking`
wrapper crate", which requires it. This applies to options 1, 2 and 3 alike and
is the strongest argument against all three. `Layer` itself loses nothing, since
`&GeoPackage` is already not `Send`, but the container is what async and
thread-handoff callers hold.

**The remaining cost of owning is a compile-time guarantee.** `close` takes
`self`, and for a handle that opted into WAL it checkpoints, resets the journal
mode to `DELETE` and drops the connection, so a handed-over file is a single
file with no sidecars. Today the borrow *encodes* that contract: a live `Layer`
makes `gpkg.close()` a compile error, because `close` consumes what the layer
borrows. Under an owned handle that becomes a runtime error or a silent
breakage. Weigh this at its real size, though: `Drop` already does the same work
best-effort, `into_connection` already opts out of it deliberately, and
`mem::forget` already defeats it with no compile error. What the borrow buys is
the observable error and the determinism of when, not the guarantee itself.

The options:

1. **Owned handles, close fails when handles are outstanding.** `GeoPackage`
   becomes a thin handle over `Arc<Inner>`; `close` checks the strong count and
   returns an error naming the count if it is not one. There is no race in that
   check: `Arc<Inner>` over a non-`Sync` inner is itself neither `Send` nor
   `Sync`, so every clone is confined to one thread and the count is exact. The
   residual hole is liveness rather than soundness, since a leaked handle wedges
   `close` into erroring forever. Also breaks `into_connection`, which cannot
   move a `Connection` out of an `Arc` with clones outstanding. Costs
   `GeoPackage: Send`.
2. **Owned handles, `close(&self)` marks the handle closed.** Interior
   mutability takes the connection out; surviving handles error afterwards.
   Deterministic close whatever is outstanding, but it breaks
   `GeoPackage::connection() -> &Connection`, the documented escape hatch the
   README points at for anything the API does not cover, since a guard type
   cannot be handed out as a plain reference. Costs `GeoPackage: Send`.
3. **`Weak` in the handles, strong in the iteration state.** `Layer` and
   `TilePyramid` hold a `Weak<Inner>` and upgrade per call, so `close` and
   `Drop` keep their exact present semantics and outstanding handles go inert.
   The iteration state is where this fails. `FeatureCursor` owns a
   `rusqlite::Statement<'a>` borrowing the connection, so a cursor holding both
   an upgraded `Arc<Inner>` and a statement borrowing into it is
   self-referential, which `forbid(unsafe_code)` rules out here. The safe shape
   is a separate guard object the cursor borrows, turning every cursor, stream
   and writer into a three-step construction. `TilePyramid::gpkg() -> &'a
   GeoPackage` also cannot be expressed over a temporary upgrade. Costs
   `GeoPackage: Send`. Not a contender.
4. **Keep the borrow, make the rebuild cheap.** Handles stay borrowed and every
   current guarantee stands, including the compile-time one. The library grows a
   constructor that builds a `Layer` from a cached, shared schema rather than
   re-querying, and the FFI holds the cache and rebuilds per call. Removes the
   performance objection without touching the ownership model. Requires
   `Layer`'s owned fields (`schema`, `value_columns`) to become shared for the
   rebuild to be cheap, for which `value_column_names: Arc<[String]>` is already
   the precedent; the rebuild cost then needs measuring rather than assuming.
   The cache has to hold everything `build_layer` derives, not just the schema:
   the resolved physical table name, the contents data type, the geometry
   column, the primary key and the value columns. As public API it also has a
   trust problem, since a caller can pass a cache built from another file and
   the constructor must either re-validate, defeating the point, or document the
   mismatch as the caller's fault. Staleness under `ALTER TABLE` is *not* a mark
   against it: a long-lived `Layer` already snapshots its schema at
   construction, and `TilePyramid` documents the same for its write block, so
   every option here is equally stale.
5. **Solve it inside `geopackage-ffi` with `unsafe`, leaving the library
   alone.** The C handle owns the `GeoPackage` (boxed, so its address is stable)
   alongside a lifetime-erased `Layer` or `TilePyramid`, and the FFI's close
   function refuses while child handles are outstanding. No rebuild cost, no
   library change, no `Send` regression, and the Rust API keeps its
   compile-time close guarantee intact. The runtime check is paid only by C
   callers, which is where a runtime check is the only kind available anyway: a
   C caller can never be handed a compile-time lifetime, so demoting the Rust
   API's static guarantee buys C nothing it can use. D12 exists precisely to
   license this, and the planned `undocumented_unsafe_blocks`, sanitizer and
   miri gating are the machinery for reviewing it.

**Recommended: option 5**, with option 4 as the fallback if the lifetime erasure
proves harder to justify than expected. Options 1 to 3 are ruled out by the
`Send` regression alone; 2 and 3 have independent blockers on top of it.

Two things support 5 beyond the reasoning above. It is the only option
indifferent to how the C surface grows, which matters now that the scope is
"mirror the Rust API": nothing above needs revisiting if a tile handle, a cursor
handle or a row accessor is added later. And the FFI has equivalent lifetime
work to do regardless, because `FFI_ArrowArrayStream::new` takes
`Box<dyn RecordBatchReader + Send>`, meaning `Send + 'static`, while
`ArrowBatches<'a>` is neither and stays neither under every option here. The one
qualifier is that `ParallelBatches` is `'static` and opens its own worker
connections, so the stream export may be expressible safely by always taking the
parallel path; that would confine the FFI to file-backed databases, and it needs
confirming in the prototype rather than assuming either way.

If option 5 is taken, an owned tier for Rust callers stays available as a purely
additive `GeoPackage::into_shared()` after the freeze, so it does not need
deciding now. That also corrects the framing this section opened with: keeping
the borrow is the status quo, and the only options the freeze forecloses are the
breaking ones.

Whichever is chosen applies to `TilePyramid` as well as `Layer`, and on the
measurements `TilePyramid` is the more urgent of the two. The second-tier
borrowed types (`FeatureCursor`, `FeatureStream`, `TileCursor`, `TileStream`,
`ArrowBatches`, `FeatureWriter`, `TileWriter`) are iteration state built once per
scan, so they can stay borrowed as long as the FFI iterator object owns its
parent. `ValueRef<'a>` and `GpbGeometry<'a>` borrow row data rather than a
handle and should not change at all.

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
- **Curve envelopes** (#5). No longer a non-goal: phase 6 was revised on
  2026-07-27 and arc extrema are computed exactly, so curve geometries index
  like any other. What stays out is reading a curve back as a geometry object,
  which needs a `geo-traits` representation for an arc rather than anything
  local.
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
