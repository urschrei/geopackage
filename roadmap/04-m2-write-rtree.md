# M2 — write path + spatial index → v0.1

Goal: create valid GeoPackage 1.4 files that pass external validators, with
write performance competitive with GDAL's GPKG driver.

## Tasks

### Layer creation & DDL
- [x] `TableSchemaBuilder`: columns (gpkg types + constraints), geometry
      column (type, srs, z/m flags), pk. Emits user table DDL +
      `gpkg_contents` + `gpkg_geometry_columns` rows (creating
      `gpkg_geometry_columns` on first use).
- [x] `create_layer` / `create_attributes_table`; table/column name
      validation (reject `gpkg_` prefix per spec, SQL keywords quoted).
- [x] `gpkg_contents.last_change` maintenance; bbox (min_x…max_y) updated on
      write commit (cheap: fold envelopes during the transaction). *(The
      running fold and the commit-time `last_change`/bbox update live in the
      Group B `FeatureWriter::commit`, since they depend on the writer; create
      seeds the initial row with the DDL default `last_change` and a NULL
      bbox.)*

### Feature writes
- [x] `layer.writer()` prepared-statement writer: insert (with/without
      explicit fid), update, delete; accepts `impl GeometryTrait<T = f64>`;
      encodes GPB (always-envelope policy, D6) via core encoder + `wkb`
      writer; Z/M validated against column flags. *(The writer owns its
      transaction rather than borrowing a `tx` object, keeping
      `rusqlite::Transaction` out of the public API; statement reuse is via
      the per-connection `prepare_cached` cache.)*
- [x] Batched insert helper (`write_all(iter, batch_size)`) wrapping a
      transaction per batch, statement reuse, configurable batch size (`0`
      = one transaction for the whole iterator).
- [x] DATETIME serialization in the strict 1.4 format (via the core
      `datetime::DateTime` `Display`, bound through `Value`).

### Spatial index
- [x] `layer.create_spatial_index()`: vtab + 1.4 triggers + populate +
      `gpkg_extensions` row (creating `gpkg_extensions` on first use);
      `drop_spatial_index()`; `has_spatial_index()`. *(Population is the plain
      `INSERT INTO rtree SELECT` with the `ST_IsEmpty`/NULL guard — the D8 bulk
      build replaces it. `create` errors on an already-indexed layer
      (`SpatialIndexExists`), an attribute layer (`NoGeometryColumn`), or a
      table with no single-column primary key (`NoPrimaryKey`); `drop` is
      idempotent and removes any trigger generation, leaving the
      `gpkg_extensions` table. The extension row uses the spec Annex F.3
      requirement 75/76 strings, reusing the existing `triggers::EXTENSION_*`
      constants.)*
- [x] **Bulk build (D8)**: during `write_all` on an indexed layer (or
      `create_spatial_index` on a populated table): drop/defer triggers,
      accumulate `(fid, envelope)`, build rtree in scratch in-memory DB, copy
      `rtree_%_node/parent/rowid` shadow tables in one transaction, reinstall
      triggers. Gate with rtree integrity query + `PRAGMA integrity_check` in
      tests; automatic fallback to triggered path on any anomaly.
      *(Implemented in `geopackage/src/bulk.rs`. The scratch database is an
      `ATTACH`ed `:memory:` db built from an `ST_*` envelope scan; its
      `_node`/`_rowid`/`_parent` shadow tables are copied into the target inside
      one transaction that also (re)creates the vtab and — via an `after` hook —
      installs the triggers/`gpkg_extensions` row atomically. The gate is a
      bijection + containment check of the copied index against the accumulated
      envelopes plus `PRAGMA integrity_check`; any anomaly drops the copied
      result and rebuilds through `populate_rtree_sql`. Bulk-vs-triggered is
      chosen by `BulkIndexOptions` (default 10k rows; `create_spatial_index_with`
      / `write_all_with` override; `always_bulk`/`never_bulk` force it).
      `write_all` bulk engages only when the target index is empty (a fresh bulk
      load), so appends keep the per-row triggered path. A scratch-tamper test
      seam drives the fallback in a unit test. Full `integrity_check` cost and
      atomicity across the `ATTACH` boundary are noted below.)*
- [x] `repair_spatial_index()`: drop legacy `update1`/`update3`, install 1.4
      set, rebuild if `TriggerGeneration::Mixed` (D7). Never automatic.
      *(Replaces every rtree trigger of a `PreV1_4`/`Mixed` generation with the
      1.4 set and rebuilds the index content; `V1_4` is a no-op, `None` is a
      typed `NoSpatialIndex` error directing to `create_spatial_index`. Shares
      the read-side `has_spatial_index` classification via a `pub(crate)`
      helper.)*

### Journal & durability (D4)
- [x] Journal mode option on create/open (Delete default, Wal opt-in);
      checkpoint + reset to DELETE on close/Drop; `synchronous` exposure.
      *(`OpenOptions` builder (mirrors `std::fs::OpenOptions`) with typed
      `JournalMode`/`Synchronous` enums — no rusqlite types in the public API;
      plain `create`/`open`/`open_read_only` are the default-options shortcuts.
      An unspecified journal mode leaves the file's mode untouched (a fresh file
      is DELETE, SQLite's default); WAL is opt-in. A handle that opted into WAL
      checkpoints (`wal_checkpoint(TRUNCATE)`) and resets the file to DELETE on
      the explicit consuming `close()` and on `Drop` (best-effort, never panics),
      so a handed-over `.gpkg` has no `-wal`/`-shm` sidecars. `into_connection()`
      and a plain `open` of a pre-existing WAL file both opt out of the finalise
      and leave the mode as found. The connection field is `Option<Connection>`
      so the handle can implement `Drop` and still hand the connection back.)*
- [x] Crash-safety test: kill mid-transaction (child process), reopen,
      verify integrity + no index desync.
      *(`geopackage/tests/crash_safety.rs`: an always-on parent test re-invokes
      this test binary to run only the ignored `crash_child` entry point, which
      commits one write, holds a second uncommitted, and signals readiness via a
      marker file before blocking; the parent kills it (`SIGKILL`) and reopens,
      asserting `integrity_check` ok, committed rows intact, the uncommitted row
      absent, and the rtree in step with the table. Runs for both DELETE and WAL
      modes. Synchronisation is on the marker file, not sleeps; verified reliable
      over repeated runs, so it is always-on rather than `#[ignore]`. The
      interrupted-bulk-build desync (below) is covered deterministically by a
      unit test in `index.rs` rather than by timing a kill mid-bulk. WAL
      sidecar/round-trip behaviour is covered by
      `geopackage/tests/wal_journal.rs`.)*

### Bulk-build follow-ups (discovered during D8)
- [ ] (issue #16) Gate cost: the D8 gate runs a whole-database `PRAGMA integrity_check`,
      which is O(database) and dominates the gate on very large files; a benign
      pre-existing issue anywhere also forces the (still correct) triggered
      fallback. Consider scoping the structural check to `rtreecheck(<rtree>)`
      (available in bundled SQLite 3.53) with `integrity_check` behind an option,
      once benchmarks quantify the cost.
- [x] Bulk-build atomicity: `ATTACH`/`DETACH` require autocommit, so the scratch
      build and detach sit outside the copy transaction. For `write_all` the row
      inserts commit before the index rebuild, so a crash between them can leave
      a stale index needing `repair_spatial_index()`. Fold into the D4
      journal/durability crash-safety work rather than solving separately.
      *(Addressed by detect-and-direct, not by claiming atomicity across the
      `ATTACH` boundary. The crash leaves the rtree virtual table present with
      its triggers dropped; `has_spatial_index` already declines that state, so
      `features_in` stays correct via a full scan. Added `SpatialIndexStatus`
      { `Absent`, `Current`, `Legacy`, `Stale` } and `Layer::spatial_index_status`
      to name it, and taught `repair_spatial_index` to recover a `Stale` (or
      orphaned) index by rebuilding + reinstalling the 1.4 triggers — previously
      the file was stuck between `create_spatial_index` (`SpatialIndexExists`)
      and `repair` (`NoSpatialIndex`). A clean (non-crash) error on the bulk path
      is still restored in-process, so the `Stale` window is only reachable by an
      actual crash/kill.)*
- [ ] (issue #15) Narrow the bulk-build crash window further: build the scratch RTree in a
      **separate in-memory `Connection`** (not an `ATTACH`ed database) and copy
      its shadow-table rows out and into the target inside the same transaction
      as the `write_all` row inserts, so the whole `write_all` becomes atomic and
      the `Stale` window closes entirely. Deferred from this pass: it reworks the
      gated `bulk::fill_index` copy mechanism (cross-connection blob copy instead
      of `INSERT ... SELECT`), so it wants its own change with the existing gate
      and fallback re-proven. Detect-and-direct (above) covers correctness
      meanwhile.
- [ ] (issue #17) `write_all` bulk currently engages only for an empty target index; a
      merge-into-populated-index bulk path (re-index existing + new, or an rstar
      escalation) is deferred until benchmarks justify it (see 02-ecosystem
      rstar note).
- [x] Read-path finding (from the hegel port of the `features_in` property
      test): for a coordinate of `f32` sub-normal magnitude (`|x| < ~1.2e-38`,
      e.g. `3e-39`), SQLite coerces the `f64` query-box constraint to `f32`
      non-conservatively, so the RTree candidate scan can drop a truly
      intersecting feature *before* `features_in`'s `f64` re-test runs — the
      indexed and full-scan paths then disagree. Fix in the M1 read path by
      expanding the bound each query passes to the vtab outward to the enclosing
      `f32` (min → next-lower `f32`, max → next-higher `f32`) so the vtab filter
      is provably conservative. *(Fixed in PR #13, closing issue #12: bounds
      are widened one `f32` ULP outward before binding, and the property test
      generator is un-constrained back across the sub-normal band.)*

### Performance
- [x] Criterion benches: 1M and 10M point/line/polygon writes, indexed and
      not, vs `gdal` crate as baseline; read-scan throughput vs M1 numbers.
      *(`geopackage/benches/{write,read}.rs`, criterion `0.8`. Recorded at 1M in
      `roadmap/benchmarks/2026-07-24-m2.md` (Apple M2 Pro); 10M extrapolated, not
      run in the matrix — the triggered path is ~18 s/1M and criterion's 10-sample
      floor makes a 10M matrix run for hours with no new ratio information. The
      baseline is the `ogr2ogr` CLI, not the `gdal` crate: the crate needs system
      bindings, and the CLI timing is the honest, documented baseline (it includes
      GDAL's source read, so it is conservative for a pure write). Benches compile
      in CI via clippy `--all-targets`; they carry `test = false` so `cargo test`
      never runs them.)*
- [ ] Target: bulk indexed write ≥ GDAL parity (its own rtree trick means
      parity is the honest goal, not a multiple). *(**Not yet met** — the
      benchmark is the evidence. Unindexed writes are competitive (our write-only
      point load 0.81 s vs GDAL's 1.22 s read+write); the D8 bulk indexed write is
      ~3-4x slower than GDAL's indexed `ogr2ogr` copy (7.31 s vs 1.89 s for 1M
      points). The overhead beyond the raw insert is the gate's whole-database
      `PRAGMA integrity_check` (O(database)) and the per-row `ST_*` envelope scan
      (4M function calls) — exactly the two open D8 follow-ups above (scope the
      structural check to `rtreecheck`; build the scratch RTree in a separate
      connection). Stays open pending those.)*

## Acceptance criteria

1. Files produced here validate clean under OGC `ets-gpkg12` (aio jar) and
   the PDOK validator, and open correctly in QGIS and `ogrinfo` (manual 1.4
   checks: trigger names update5/6/7, user_version 10400).
   *(**Largely verified, 2026-07-24.** A representative file (four indexed
   feature layers — 2D point, Z point, linestring in EPSG:3857, polygon — a
   non-spatial attributes table, every attribute type) was written by the new
   API and checked (`geopackage/tests/gdal_interop.rs`, `#[ignore]`d; run
   locally):
   - **ogrinfo** (`-al`) full read: clean, all five layers, `beacons` reported
     3D Point.
   - **manual 1.4 checklist**: `user_version` 10400, 28 RTree triggers all the
     1.4 generation (`update5`/`update6`/`update7` present, no `update1`/
     `update3`).
   - **ets-gpkg12 1.3** (`scripts/run_ets_gpkg12.sh`, jar sha256-pinned): 40
     passed, 71 skipped (not applicable), **1 failed** —
     `RTreeIndexTests::extensionIndexImplementation`, whose regex hard-codes the
     GeoPackage **1.2** `update1` trigger and rejects our correct 1.4 set. This
     is the documented 1.2-vs-1.4 gap (no 1.3/1.4 ETS exists); the 1.4 trigger
     semantics are covered by the manual checklist above, so this is not a defect
     in the file.
   - **PDOK** `pdok-geopackage-validator` 0.14.4
     (`scripts/run_pdok_validator.sh`): 21 checks; the only findings are RQ13
     "single SRS across geometry tables" (our file deliberately mixes 4326+3857 —
     spec-legal; PDOK convention, advisory) and RC19 (the intentional Z layer, a
     recommendation). All other RQ/RC checks pass.
   - **QGIS**: not re-exercised in this pass (covered in the M1 corpus via
     headless `qgis_process`); stays open here.)*
2. GDAL round-trip: write here → read with ogr2ogr → byte-compare geometries
   (WKB) and values.
   *(**Verified, 2026-07-24.** `gdal_interop.rs::gdal_roundtrip_wkb_and_values`
   (`#[ignore]`d): write a point/line/polygon layer with TEXT/INTEGER/DOUBLE/
   BOOLEAN attributes → `ogr2ogr` GPKG copy → read the copy back with this crate
   → the geometry WKB bodies (GPB header stripped) and every attribute value are
   byte-identical for all three shapes.)*
3. UPSERT + concurrent-reader tests pass on indexed tables; rtree contents
   provably match a full-scan rebuild after arbitrary write sequences
   (property test).
   *(**Verified (UPSERT + property), 2026-07-24.** `upsert_through_new_index_`
   `maintains_rtree` and `upsert_works_with_1_4_triggers` cover UPSERT on 1.4
   indexed tables; the Hegel property tests `rtree_tracks_full_scan_through_`
   `write_ops` and `bulk_and_triggered_builds_agree` prove the rtree equals a
   full-scan rebuild after arbitrary insert/update/delete/upsert sequences
   (both build paths), and `features_in_matches_full_scan_filter` proves the
   query paths agree — all green in CI. A **dedicated concurrent-reader**
   (read-during-write) test is not present; WAL round-trip/durability is covered
   by `wal_journal.rs`/`crash_safety.rs`. The concurrent-reader sub-item stays
   open.)*
4. Benchmarks recorded in-repo with hardware notes.
   *(**Verified, 2026-07-24.** `roadmap/benchmarks/2026-07-24-m2.md` (Apple M2
   Pro, macOS 15.6.1, SQLite 3.51.3, GDAL 3.12.3): write matrix (point/line/
   polygon × unindexed/triggered/bulk), read matrix (full scan, `features_in`
   index vs full-scan), and the `ogr2ogr` baseline, with exact commands.)*
5. Tag **v0.1.0**; publish `geopackage-core` + `geopackage` to crates.io.
   *(**Open — maintainer's act.** The workspace version is bumped to `0.1.0`;
   tagging and publishing are deliberately left to the maintainer and not done
   here.)*
