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
- [ ] Journal mode option on create/open (Delete default, Wal opt-in);
      checkpoint + reset to DELETE on close/Drop; `synchronous` exposure.
- [ ] Crash-safety test: kill mid-transaction (child process), reopen,
      verify integrity + no index desync.

### Bulk-build follow-ups (discovered during D8)
- [ ] Gate cost: the D8 gate runs a whole-database `PRAGMA integrity_check`,
      which is O(database) and dominates the gate on very large files; a benign
      pre-existing issue anywhere also forces the (still correct) triggered
      fallback. Consider scoping the structural check to `rtreecheck(<rtree>)`
      (available in bundled SQLite 3.53) with `integrity_check` behind an option,
      once benchmarks quantify the cost.
- [ ] Bulk-build atomicity: `ATTACH`/`DETACH` require autocommit, so the scratch
      build and detach sit outside the copy transaction. For `write_all` the row
      inserts commit before the index rebuild, so a crash between them can leave
      a stale index needing `repair_spatial_index()`. Fold into the D4
      journal/durability crash-safety work rather than solving separately.
- [ ] `write_all` bulk currently engages only for an empty target index; a
      merge-into-populated-index bulk path (re-index existing + new, or an rstar
      escalation) is deferred until benchmarks justify it (see 02-ecosystem
      rstar note).
- [ ] Read-path finding (from the hegel port of the `features_in` property
      test): for a coordinate of `f32` sub-normal magnitude (`|x| < ~1.2e-38`,
      e.g. `3e-39`), SQLite coerces the `f64` query-box constraint to `f32`
      non-conservatively, so the RTree candidate scan can drop a truly
      intersecting feature *before* `features_in`'s `f64` re-test runs — the
      indexed and full-scan paths then disagree. Fix in the M1 read path by
      expanding the bound each query passes to the vtab outward to the enclosing
      `f32` (min → next-lower `f32`, max → next-higher `f32`) so the vtab filter
      is provably conservative. The property test avoids the sub-normal band
      until this lands (it is outside the domain of geographic coordinates).

### Performance
- [ ] Criterion benches: 1M and 10M point/line/polygon writes, indexed and
      not, vs `gdal` crate as baseline; read-scan throughput vs M1 numbers.
- [ ] Target: bulk indexed write ≥ GDAL parity (its own rtree trick means
      parity is the honest goal, not a multiple).

## Acceptance criteria

1. Files produced here validate clean under OGC `ets-gpkg12` (aio jar) and
   the PDOK validator, and open correctly in QGIS and `ogrinfo` (manual 1.4
   checks: trigger names update5/6/7, user_version 10400).
2. GDAL round-trip: write here → read with ogr2ogr → byte-compare geometries
   (WKB) and values.
3. UPSERT + concurrent-reader tests pass on indexed tables; rtree contents
   provably match a full-scan rebuild after arbitrary write sequences
   (property test).
4. Benchmarks recorded in-repo with hardware notes.
5. Tag **v0.1.0**; publish `geopackage-core` + `geopackage` to crates.io.
