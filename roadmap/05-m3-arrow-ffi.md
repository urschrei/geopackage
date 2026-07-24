# M3 — Arrow data plane + C ABI + CLI → v0.2

Goal: make the crate consumable from anything with an Arrow implementation,
and give it a face (CLI) for dogfooding and bug reports.

## Tasks

### Arrow / GeoArrow (feature `arrow`)
- [ ] `layer.read_arrow(opts) -> impl RecordBatchReader`: attribute columns to
      Arrow types (documented mapping, incl. DATETIME → timestamp semantics),
      geometry as GeoArrow **WKB-encoded** column (free: GPB bodies are WKB;
      native geoarrow encodings later), batch size option, bbox/WHERE options
      mirroring the scalar API.
- [ ] `layer.write_arrow(reader: impl RecordBatchReader)`: schema→TableSchema
      mapping, batched writes through the M2 bulk path (rtree shadow-table
      build included — this is the pyogrio-shaped fast path).
- [ ] GeoArrow metadata (extension name `geoarrow.wkb`, CRS as PROJJSON where
      we have it / user-supplied otherwise) on the geometry field.
- [ ] Interop test: batches round-trip through pyarrow (CI python job) and
      match pyogrio's read of the same file.

### `geopackage-ffi` (C ABI)
- [ ] New crate, `cdylib`+`staticlib`, packaged with cargo-c (pkg-config,
      versioned soname). Opaque handles `gpkg_t`, `gpkg_layer_t`; UTF-8;
      `gpkg_error_t` out-params (code + message + free fn). This is the sole
      crate exempt from the workspace `unsafe_code = "forbid"` lint (decision
      D12): it does not take `[lints] workspace = true`, documents the safety
      contract on every `unsafe` block (`undocumented_unsafe_blocks` applies),
      and needs sanitizer/miri CI gating before first release.
- [ ] Control plane: open/create/close, list layers, schema introspection,
      create layer, create/drop spatial index, begin/commit.
- [ ] Data plane: `gpkg_layer_read_arrow(layer, opts, ArrowArrayStream* out)`
      and `gpkg_layer_write_arrow(layer, ArrowArrayStream*)` per the
      [Arrow C Data Interface](https://arrow.apache.org/docs/format/CDataInterface.html).
      Row-at-a-time C accessors deliberately omitted (Arrow is the data
      plane; revisit only on concrete demand).
- [ ] cbindgen header checked in; CI fails on undocumented header diff
      (API-stability gate). Smoke test: a C program in CI reads a corpus file
      via the stream.
- [ ] SQLite thread-model documented (handle-per-thread or external lock).

### `geopackage-cli`
- [ ] `gpkg info <file>` (version, contents, srs, index status incl. trigger
      generation), `gpkg validate <file>` (our checks; prints repair advice),
      `gpkg copy <src> <dst>` (any supported read → our write; the dogfood
      command), `gpkg index <file> <layer>` / `repair`.
- [ ] Ships as bin crate; also the corpus-generation harness for tests.

## Acceptance criteria

1. Python (pyarrow + a 20-line ctypes shim, no bindings crate yet) can read
   a corpus file end-to-end zero-copy and geopandas can ingest via
   `from_arrow`; numbers vs pyogrio recorded.
2. `gpkg copy` GDAL-file → ours → validators clean (the full-circle test).
3. C header diff gate active; `cargo c-build` artifacts install and link in a
   CI job on all three OSes.
4. Tag **v0.2.0**.
