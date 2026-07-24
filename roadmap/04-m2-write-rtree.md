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
- [ ] `layer.create_spatial_index()`: vtab + 1.4 triggers + populate +
      `gpkg_extensions` row (creating `gpkg_extensions` on first use);
      `drop_spatial_index()`; `has_spatial_index()`.
- [ ] **Bulk build (D8)**: during `write_all` on an indexed layer (or
      `create_spatial_index` on a populated table): drop/defer triggers,
      accumulate `(fid, envelope)`, build rtree in scratch in-memory DB, copy
      `rtree_%_node/parent/rowid` shadow tables in one transaction, reinstall
      triggers. Gate with rtree integrity query + `PRAGMA integrity_check` in
      tests; automatic fallback to triggered path on any anomaly.
- [ ] `repair_spatial_index()`: drop legacy `update1`/`update3`, install 1.4
      set, rebuild if `TriggerGeneration::Mixed` (D7). Never automatic.

### Journal & durability (D4)
- [ ] Journal mode option on create/open (Delete default, Wal opt-in);
      checkpoint + reset to DELETE on close/Drop; `synchronous` exposure.
- [ ] Crash-safety test: kill mid-transaction (child process), reopen,
      verify integrity + no index desync.

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
