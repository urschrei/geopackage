# M2: write path + spatial index → v0.1

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
      `INSERT INTO rtree SELECT` with the `ST_IsEmpty`/NULL guard; the D8 bulk
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
      *(Implemented in `geopackage/src/bulk.rs`. The scratch database this item
      describes was the first shape and is gone: `geopackage/src/packed.rs` now
      builds the `_node`/`_rowid`/`_parent` contents outright from the entry set
      and `bulk.rs` writes them, so no entry passes through the RTree module
      (#20). The entry set comes from an `ST_*` envelope scan, or from the
      envelopes `write_all` already computed while encoding when it can prove
      they cover every indexable row. The gate is a bijection + containment check
      of the written index against those envelopes plus a structural check
      (`rtreecheck` by default, `PRAGMA integrity_check` opt-in, #16); any
      anomaly discards the result and rebuilds through `populate_rtree_sql`.
      Bulk-vs-triggered is chosen by `BulkIndexOptions` (default 10k rows;
      `create_spatial_index_with` / `write_all_with` override;
      `always_bulk`/`never_bulk` force it). `write_all` bulk engages only when
      the target index is empty (a fresh bulk load), so appends keep the per-row
      triggered path. A tamper test seam drives the fallback in a unit test.
      Dropping the scratch database removed the `ATTACH`, so the whole build,
      and for `write_all` the row inserts too, is now one transaction.)*
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
      `JournalMode`/`Synchronous` enums, so no rusqlite types in the public API;
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
- [x] (issue #16) Gate cost: the D8 gate ran a whole-database `PRAGMA integrity_check`,
      which is O(database) and dominated the gate on very large files; a benign
      pre-existing issue anywhere also forced the (still correct) triggered
      fallback. *(Done. The gate now runs `rtreecheck(<rtree>)` on the index just
      built, 0.97 s to 0.50 s at 1M rows; `rtreecheck` has existed since SQLite
      3.26, so the bundled 3.51.3 has it (the earlier note saying 3.53 was wrong).
      The whole-database check stays available as
      `StructuralCheck::FullDatabase`. Removing it from the default also exposed
      that the read benchmark was measuring a connection warmed by
      `integrity_check` rather than the query; see
      [benchmarks/2026-07-24-bulk-build.md](benchmarks/2026-07-24-bulk-build.md).)*
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
      orphaned) index by rebuilding + reinstalling the 1.4 triggers; previously
      the file was stuck between `create_spatial_index` (`SpatialIndexExists`)
      and `repair` (`NoSpatialIndex`). Superseded by the atomicity work below:
      the bulk path no longer produces a `Stale` index at all, though the status
      and the repair remain, since a file can arrive from anywhere.)*
- [x] (issue #15) Close the bulk-build crash window so the whole `write_all` is
      atomic and the `Stale` window disappears. *(Done, though not the way this
      item proposed. It called for building the scratch RTree in a separate
      in-memory `Connection`; packing the tree directly (#20) removed the scratch
      database altogether, and with it the `ATTACH` that required autocommit and
      forced the rebuild into its own transaction. `write_all_bulk` now drops the
      triggers, inserts the rows, flushes `gpkg_contents`, rebuilds the index and
      reinstalls the triggers in one transaction: `FeatureWriter::flush` hands
      back the still-open transaction and `bulk::fill_index_in_transaction` joins
      it. `restore_index_after_failed_bulk` was deleted, since the rollback now
      restores the dropped triggers by itself. Pinned by
      `writer::tests::failed_bulk_write_rolls_back_rows_and_index`, which forces
      the index build to fail after the rows are staged and asserts nothing
      survives; it fails against the old arrangement. A mid-bulk `SIGKILL` test
      was written alongside it and then removed: the kill lands during the row
      inserts, which the old code also rolled back cleanly, so it passed against
      the arrangement it was meant to catch and was earning nothing against the
      cost of spawning processes.)*
- [x] (issue #17) `write_all` bulk engages for a merge into a populated index,
      and for an iterator that does not advertise its length.
      *(Two parts. The merge landed first: a write of at least
      `indexed / MERGE_REBUILD_RATIO` new entries rebuilds the index rather than
      letting the triggers append, the ratio taken from the measurements in the
      issue. The second part removed the up-front guessing that fed it. The size
      of a write used to come from `Iterator::size_hint`, whose lower bound is 0
      for most iterators not backed by a collection, so a streaming source never
      reached the threshold however large it turned out to be. The engagement
      question is now settled by buffering up to `bulk_threshold` rows when the
      hint cannot settle it, and the rebuild-or-append question is deferred until
      after the write, where both counts are exact. That also gave the path a
      branch it lacked: a write that clears the threshold but not the ratio now
      adds its entries to the existing index directly, running the `_insert`
      trigger's own statement over the encode-time envelopes, instead of falling
      back to per-row trigger maintenance. Recorded as a refinement of D8 in
      [01-design-decisions.md](01-design-decisions.md).)*
      Not done, and not needed for either: the rstar escalation (see 02-ecosystem
      rstar note). `write_all` also still holds one `(fid, envelope)` pair per
      written row, so it is not a streaming-memory path; that is a separate
      question from this one.
- [x] Read-path finding (from the hegel port of the `features_in` property
      test): for a coordinate of `f32` sub-normal magnitude (`|x| < ~1.2e-38`,
      e.g. `3e-39`), SQLite coerces the `f64` query-box constraint to `f32`
      non-conservatively, so the RTree candidate scan can drop a truly
      intersecting feature *before* `features_in`'s `f64` re-test runs, so the
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
      run in the matrix, since the triggered path is ~18 s/1M and criterion's 10-sample
      floor makes a 10M matrix run for hours with no new ratio information. The
      baseline is the `ogr2ogr` CLI, not the `gdal` crate: the crate needs system
      bindings, and the CLI timing is the honest, documented baseline (it includes
      GDAL's source read, so it is conservative for a pure write). Benches compile
      in CI via clippy `--all-targets`; they carry `test = false` so `cargo test`
      never runs them.)*
- [x] Target: bulk indexed write >= GDAL parity (its own rtree trick means
      parity is the goal, not a multiple). *(**Met**, on a like-for-like
      measurement, after one false start. It was first ticked on the strength of
      an `ogr2ogr` comparison whose figure also included GDAL reading a source
      file; that was withdrawn. Asking both implementations to build an index
      over the same rows of the same file, via GDAL's `CreateSpatialIndex` SQL
      function, our build is 8% slower on uniform points and 9% faster on
      clustered points at 1M rows, while running a verification gate GDAL does
      not and producing a tree a third smaller for equivalent query latency.
      Getting there needed a phase profile: tree construction turned out to be
      5% of the build, while `%_rowid` inserts in Hilbert order were 556 ms
      (uniform) to 1.76 s (clustered) of it, since that table is keyed by
      feature id. Buffering and inserting in key order costs 16 bytes per entry
      and removes both the cost and the distribution sensitivity. See
      [benchmarks/2026-07-24-gdal-like-for-like.md](benchmarks/2026-07-24-gdal-like-for-like.md)
      and `scripts/compare_gdal_index.sh`.)*

- [x] (issue #20) Build the RTree without the module: pack `%_node`/`%_rowid`/
      `%_parent` directly from the entry set rather than inserting row by row
      into a scratch index. *(Done in `geopackage/src/packed.rs`. Node format and
      invariants taken from SQLite's `rtree.c` and its `rtreecheck`; entries are
      Hilbert-ordered and packed full, internal levels built bottom-up. Lee and
      Lee's OMT partitioning was implemented and measured against this: ~15%
      slower to build on uniform data with no query benefit, within noise on
      clustered data, so it was dropped. Removing the scratch database also
      removed the `ATTACH`, so the build is now a single transaction.)*

## Acceptance criteria

1. Files produced here validate clean under OGC `ets-gpkg12` (aio jar) and
   the PDOK validator, and open correctly in QGIS and `ogrinfo` (manual 1.4
   checks: trigger names update5/6/7, user_version 10400).
   *(**Largely verified, 2026-07-24.** A representative file (four indexed
   feature layers (2D point, Z point, linestring in EPSG:3857, polygon), a
   non-spatial attributes table, every attribute type) was written by the new
   API and checked (`geopackage/tests/gdal_interop.rs`, `#[ignore]`d; run
   locally):
   - **ogrinfo** (`-al`) full read: clean, all five layers, `beacons` reported
     3D Point.
   - **manual 1.4 checklist**: `user_version` 10400, 28 RTree triggers all the
     1.4 generation (`update5`/`update6`/`update7` present, no `update1`/
     `update3`).
   - **ets-gpkg12 1.3** (`scripts/run_ets_gpkg12.sh`, jar sha256-pinned): 40
     passed, 71 skipped (not applicable), **1 failed**:
     `RTreeIndexTests::extensionIndexImplementation`, whose regex hard-codes the
     GeoPackage **1.2** `update1` trigger and rejects our correct 1.4 set. This
     is the documented 1.2-vs-1.4 gap (no 1.3/1.4 ETS exists); the 1.4 trigger
     semantics are covered by the manual checklist above, so this is not a defect
     in the file.
   - **PDOK** `pdok-geopackage-validator` 0.14.4
     (`scripts/run_pdok_validator.sh`): 21 checks; the only findings are RQ13
     "single SRS across geometry tables" (our file deliberately mixes 4326+3857,
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
   query paths agree, all green in CI. A **dedicated concurrent-reader**
   (read-during-write) test is not present; WAL round-trip/durability is covered
   by `wal_journal.rs`/`crash_safety.rs`. The concurrent-reader sub-item stays
   open.)*
4. Benchmarks recorded in-repo with hardware notes.
   *(**Verified, 2026-07-24.** `roadmap/benchmarks/2026-07-24-m2.md` (Apple M2
   Pro, macOS 15.6.1, SQLite 3.51.3, GDAL 3.12.3): write matrix (point/line/
   polygon × unindexed/triggered/bulk), read matrix (full scan, `features_in`
   index vs full-scan), and the `ogr2ogr` baseline, with exact commands.)*
5. Tag **v0.1.0**; publish `geopackage-core` + `geopackage` to crates.io.
   *(**Done, 2026-07-24.** Tagged `v0.1.0` at `b3649cc` with a GitHub release;
   `geopackage-core` 0.1.0 and `geopackage` 0.1.0 published to crates.io, both
   building on docs.rs. Post-release follow-ups landed separately: the crates'
   `repository` metadata pointed at the not-yet-existing `georust/geopackage`
   and is now the live URL (GitHub preserves the redirect through a transfer),
   both crates now carry the README on crates.io, and the release is recorded in
   `CHANGELOG.md`. The org transfer itself remains open in issue #18.)*
