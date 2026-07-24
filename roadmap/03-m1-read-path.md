# M1 — container model + read path

Goal: read any reasonable GeoPackage's features and attributes, correctly and
fast, including files produced by GDAL, QGIS, and NGA tools.

## Tasks

### Schema model
- [x] `SrsDefinition` lookup module (vendored subset + synthesised WGS 84 UTM
      zones; decision recorded in [02-ecosystem.md](02-ecosystem.md)) +
      `srs()` / `srs_list()` / `add_srs()` / `add_epsg_srs()` on `GeoPackage`.
- [x] `gpkg_geometry_columns` model: geometry type name (incl. the non-linear
      extension type names, read-only for now), srs_id, z/m flags (0/1/2).
      `GeometryColumn` + `ZmFlag` (core) + `geometry_column()` /
      `geometry_columns()`; a missing table reads as no rows.
- [x] `TableSchema` introspection: column names, declared gpkg types
      (BOOLEAN, TINYINT, SMALLINT, MEDIUMINT, INT/INTEGER, FLOAT, DOUBLE/REAL,
      TEXT(maxlen), BLOB(maxlen), DATE, DATETIME, geometry types), pk column
      discovery (`fid` convention but never assumed — read `PRAGMA table_info`).
      `TableSchema` + `Column` + `table_schema()`; composite pks surfaced via
      `primary_key_columns()`, single-pk convenience via `primary_key()`.
- [x] `Value` enum mapping SQLite storage classes ↔ gpkg column types;
      strict DATETIME parsing (`YYYY-MM-DDTHH:MM:SS.SSSZ` — 1.4 kept the
      strict form) with a lenient read option (`ConversionOptions` /
      `DateTimeParsing`). Non-geometry only; storage/type mismatch is a typed
      error. Reachable via the `column_values()` building block.
- [ ] Revisit `Value` conversion leniencies once a validation option exists:
      `BOOLEAN` currently maps any non-zero INTEGER to `true`, and a whole
      number stored with integer affinity in a `FLOAT`/`DOUBLE` column is
      widened to `Value::Float` rather than rejected.
- [x] Fold the `column_values()` building block into the streaming feature/
      attribute read path. `Layer::features`/`features_in`/`select` drive
      `value::value_from_ref` from the layer schema's declared types with a
      `ConversionOptions` (carried on the `Layer`, default strict, overridable
      via `with_conversion_options`). `column_values()` itself is kept: it is a
      small, tested single-column convenience and shares the conversion core, so
      dropping it would only remove coverage.

### Geometry
- [x] `GpbGeometry<'a>` wrapper (`geopackage_core::geometry`): parsed header +
      WKB body slice, implementing `geo_traits::GeometryTrait` by delegating to
      `wkb::reader::Wkb`; `to_geo()` behind the `geo-types` feature; raw
      header/body accessors. Depends on georust `wkb` 0.9 and `geo-traits` 0.3.
- [x] Full WKB envelope traversal for the `ST_*` fallback (replaced the M0
      point-only fallback). Placement decision: the wrapper and traversal live
      in **`geopackage-core`** (`GpbGeometry::xy_envelope`/`is_empty`), keeping
      the fuzz workspace free of the SQLite dependency; coordinates are visited
      through the `geo-traits` interface rather than a hand-rolled WKB walker.
      `geopackage/src/functions.rs` now bounds envelope-less blobs of any type
      the `wkb` crate can read (all byte orders, any Z/M), and `ST_IsEmpty`
      reports emptiness (header flag, NaN-point convention, zero-element
      geometries). The eventual home is an upstreamed `gpb` feature in georust
      `wkb` itself (tracked in [02-ecosystem.md](02-ecosystem.md)); until then
      this is ours.
- [ ] Curve-type envelope support: a WKB body whose type the `wkb` crate
      cannot read — the non-linear curve types (`CIRCULARSTRING`,
      `CURVEPOLYGON`, `MULTICURVE`, …) and the abstract `CURVE`/`SURFACE` —
      makes the `ST_*` functions return a typed SQL error rather than an
      envelope, so such a geometry cannot be inserted into an rtree-indexed
      table. Needs curve support in georust `wkb` upstream.
- [ ] **wkb upstream (fuzz finding):** `wkb` 0.9.2 pre-allocates from an
      untrusted element count without bounding it against the remaining buffer
      (`Vec::with_capacity(num_geometries)` in `reader::GeometryCollection`,
      `Vec::with_capacity(num_rings)` in `reader::Polygon`). A 17-byte GPB blob
      whose body is a `GEOMETRYCOLLECTION` declaring `0xFFFFFFFF` members drives
      a ~240 GB allocation (out-of-memory), found by the `gpb_geometry` fuzz
      target. This is a `wkb` bug, not the wrapper (our code never panics); fix
      upstream by growing the vec on demand or capping capacity by the bytes
      left. Until fixed, the `gpb_geometry` fuzz target exposes this OOM class.
- [x] Reject/flag geometry column type mismatches (declared POINT, blob says
      LINESTRING) behind a validation option. The primitive
      (`geometry::geometry_type_matches` + `wkb_geometry_type`, and
      `GpbGeometry::matches_declared`) models the spec's instantiable-type
      rules (exact match; `GEOMETRY` accepts anything; `GEOMETRYCOLLECTION`
      accepts only collection types) and classifies curve-type bodies the
      `wkb` reader cannot parse. Wired as the opt-in
      `Layer::with_geometry_type_validation()`: a mismatching row surfaces as
      a per-row `Error::GeometryTypeMismatch` without stopping iteration; off
      by default.

### Read API
- [x] `gpkg.layer(name) -> Layer` (features) / `gpkg.attributes(name)` +
      `gpkg.layers()`. Feature enumeration joins `gpkg_contents`
      (`data_type = 'features'`) to `gpkg_geometry_columns` **case-insensitively**
      (`COLLATE NOCASE`), so a wrong-case catalogue still enumerates. A name not
      in `gpkg_contents` is `NoSuchLayer`; the wrong `data_type` is
      `WrongDataType`. A `Layer` caches the introspected `TableSchema`, its
      resolved geometry column and single-column pk.
- [x] `layer.features()` yields owned `Feature { fid, geometry: Option<Vec<u8>>,
      values }` with by-name (`value`) and by-index (`get`) access and a lazy
      `geometry() -> Result<Option<GpbGeometry>>`. The iterator is
      `Iterator<Item = Result<Feature>>` (per-row fallible). **Deviation:** the
      result set is materialised into owned features rather than streamed
      lazily. rusqlite's `Rows` borrows its `Statement` and resets it on drop,
      so a lazy iterator owning both is a self-referential struct, which
      `#![forbid(unsafe_code)]` cannot express without a helper crate
      (`self_cell`/`ouroboros`). The `Feature` ownership shape is what the
      original note anticipated; only peak memory differs. New item below.
- [x] `layer.features_in(bbox)`: uses `rtree_<t>_<c>` when
      `classify_triggers ≠ None` **and** the vtab exists **and** the layer has a
      single-column pk (the rtree id is joined back on it; rowid-only tables
      fall back to full scan), else full scan with an envelope filter. RTree
      candidates are re-filtered against the true `f64` envelope (header
      envelope preferred, WKB traversal fallback) because the vtab stores
      f32-widened bounds — so both paths return exactly the same rows. Asserted
      by `EXPLAIN QUERY PLAN` (rtree path shows `VIRTUAL TABLE`; full scan shows
      `SCAN`) and a seeded property test.
- [x] `layer.select(where_clause, params)` passthrough. `params: &[Value]`
      (our enum) convert internally (`Date`/`DateTime` bind as canonical text);
      no rusqlite type in the signature. The clause is raw, caller-trusted SQL
      (D9).
- [x] `open_lenient()`: tolerates GP10/GP11 `application_id`s
      (`LegacyApplicationId`), a missing `gpkg_geometry_columns` table
      (`MissingGeometryColumns`), and `gpkg_contents` table names that match a
      real SQLite table only case-insensitively (`TableNameCaseMismatch`,
      resolved to the physical table). Warnings are collected on the handle
      (`open_warnings()`); strict `open()` is unchanged (it already accepts
      GP10/GP11 via `version.rs`, so leniency here adds the diagnostic, not the
      accept decision).
- [x] Declared-type validation on read (`geometry_type_matches`) was
      deliberately not wired in the read-API chunk; now wired as the opt-in
      `Layer::with_geometry_type_validation()` (see the ticked item under
      "Geometry" above).
- [ ] **Lazy/streaming feature iterator.** Replace the eager materialisation in
      `Layer::features`/`features_in`/`select` with a truly lazy cursor that
      owns both the prepared `Statement` and its `Rows`. Needs a safe
      self-referential holder (`self_cell`/`ouroboros`) or an owning-cursor
      addition upstream in rusqlite; blocked today by `#![forbid(unsafe_code)]`.
      Relevant once very large layers are read outside the GeoArrow bulk plane.

### Corpus & verification (details in [08-testing-conformance.md](08-testing-conformance.md))
- [x] Fixture corpus: five small GDAL-written / `sqlite3`-built fixtures
      committed under `geopackage/tests/fixtures/`, each with an
      `ogrinfo`-derived JSON snapshot, generated deterministically by
      `scripts/generate_fixtures.py` (GDAL 3.12.3, spec 1.4 trigger set):
      GDAL points without GPB envelopes plus lines/polygons with envelopes,
      XYZ and empty geometries, indexed and non-indexed layers, GPKG 1.2 and
      1.4, the full attribute-type spread (`BOOLEAN`, `TINYINT`…`INTEGER`,
      `FLOAT`/`DOUBLE`, non-ASCII `TEXT`, `BLOB`, `DATE`, `DATETIME`, NULL in
      each), an attribute-only table, and legacy-`application_id` /
      case-mismatch files for the lenient open path. Larger third-party samples
      (GDAL 1.0/1.2, OGC, NGA) are script-fetched (`scripts/fetch_corpus.sh`,
      sha256-pinned) into a git-ignored `corpus/`. A QGIS-written fixture
      (`qgis_lines.gpkg`, QGIS 4.0.2 via headless `qgis_process`
      `native:savefeatures`) is committed alongside the ogr2ogr ones; the
      generator discovers `qgis_process` (env var, PATH, macOS app bundle) and
      skips that fixture with a warning where QGIS is absent. Regeneration is
      byte-deterministic: content timestamps (`gpkg_contents.last_change`,
      `gpkg_metadata_reference.timestamp`) and the SQLite header change
      counter are pinned. The scheduled QGIS interop job in
      [08-testing-conformance.md](08-testing-conformance.md) is still to be
      wired.
- [x] Round-trip comparison test vs `ogrinfo -json` output for each corpus file
      (`geopackage/tests/corpus.rs`): per-layer feature count, fid sequence,
      per-feature geometry type + coordinates, and every attribute value. Each
      GDAL-vs-ours representation gap is normalised and documented at the
      comparison site (GDAL's `YYYY/MM/DD` date and `YYYY/MM/DD HH:MM:SS+00`
      datetime spelling vs our ISO form, float print precision handled with an
      epsilon, JSON-omitted binary recovered via GDAL's `hex()`, empty-vs-NULL
      geometry, Z dropped by `to_geo`) — our read was **not** weakened to pass.
      Snapshots are committed so the suite runs without GDAL; an `#[ignore]`d
      test regenerates-and-diffs live. Cross-implementation index compatibility
      is proven here: a GDAL-built RTree serves `features_in` correctly through
      our reader. An `#[ignore]`d external soak (`corpus_external.rs`) sweeps
      the fetched corpus (locally: 6740 features across four files, zero read
      errors).
- [x] Property test: `features_in(bbox)` ≡ full-scan filter
      (`geopackage/tests/features_in.rs`). Both the rtree and the full-scan
      paths are checked against an independent oracle (envelopes computed from
      the generated coordinates), over randomised points and linestrings and
      query boxes — including full-mantissa `f64` coordinates that are not
      representable in `f32` and boxes whose edges sit exactly on a stored
      coordinate, to stress the vtab's f32 boundary rounding. **Generator
      decision:** a hand-rolled seeded SplitMix64 rather than adding `proptest`
      — the property is a set equality with an independent oracle (little value
      from shrinking) and a zero-dependency seeded generator keeps the
      dependency tree honest and CI reproducible (failing seeds print). So
      no property-testing dependency was added in M1. The M2 property tests
      use Hegel (`hegeltest`, see [02-ecosystem.md](02-ecosystem.md) and
      [08-testing-conformance.md](08-testing-conformance.md)); this
      hand-rolled generator gets ported to it then.

## Acceptance criteria

Status as of 2026-07-24. All four hold **locally**; the milestone's formal bar
is "pass in CI", and CI has not yet been exercised on GitHub (see
[README.md](README.md)), so the milestone stays open until that runs green.

1. Every corpus file opens and iterates fully; geometry + attribute values
   match GDAL's read of the same file.
   → **Met (this chunk).** `corpus.rs` opens all six committed fixtures
   (five ogr2ogr/raw-sqlite, one QGIS-written), iterates every feature and
   compares against the `ogrinfo` snapshots (counts, fid sequence, geometry
   type + coordinates, every attribute value); the `corpus_external.rs` soak
   reads 6740 features across four larger third-party files with zero errors.
   "Files produced by GDAL, QGIS, and NGA tools" from the M1 goal are all
   represented.
2. bbox queries use the rtree when present (assert via `EXPLAIN QUERY PLAN`)
   and return provably identical results either way.
   → **Met (held before this chunk).** `features_in.rs` asserts the plan and
   the seeded property equivalence; the corpus adds a cross-implementation
   check that our `features_in` reads correctly from a **GDAL-built** RTree.
3. `ST_MinX` & co. work on envelope-less non-point blobs (fallback complete).
   → **Met (held before this chunk).** `functions.rs` over the full WKB
   envelope traversal in `geopackage-core::geometry`; the GDAL points fixture
   (no header envelope) exercises the same fallback end-to-end.
4. No `unsafe`, clippy/fmt/docs clean, fuzz target extended to the new
   surface (`GpbGeometry` over arbitrary bodies must never panic).
   → **Met (held before this chunk).** `#![forbid(unsafe_code)]`; `cargo
   nextest`, `clippy --all-targets -D warnings`, `fmt --check`, `doc`, and
   `check --no-default-features` all green locally; the `gpb_geometry` fuzz
   target covers arbitrary bodies (the one open finding is an upstream `wkb`
   allocation bug, tracked above, not a panic in our code).
