# M3: Arrow data plane + C ABI + CLI → v0.2

Goal: make the crate consumable from anything with an Arrow implementation,
and give it a face (CLI) for dogfooding and bug reports.

Throughput is a goal of this milestone, not a hope attached to it. An Arrow API
that is merely present buys nothing: see the evidence note below, where GDAL's
generic implementation makes one driver *slower* than the row-based API it
wraps. The acceptance criteria are written as measurements for that reason.

### Evidence: what the target looks like

Even Rouault, "GDAL: integrating columnar formats into a row-oriented
framework", Paris Arrow/Parquet meetup, 18 June 2026
([slides](https://download.osgeo.org/gdal/presentations/GDAL_%20integrating%20columnar%20formats%20into%20a%20row-oriented%20framework.pdf)),
slide 10. Loading 3.2M footprint polygons of 13 attributes each:

| | GeoParquet | GeoPackage | FlatGeoBuf | Shapefile |
|---|---|---|---|---|
| File size | 0.43 GB | 1.67 GB | 1.8 GB | 2.92 GB |
| Feature iteration | 6.2 s | 6.6 s | 5.0 s | 10.3 s |
| Arrow, 1 thread | 1.6 s (3.9x) | 2.2 s (3.0x) | 3.0 s (1.7x) | 16.8 s (0.61x) |
| Arrow, 4 threads | 1.0 s (6.2x) | 0.7 s (9.4x) | | |

Three readings that shape the work below. The GeoPackage container reaches 0.7 s
against GeoParquet's 1.0 s at four threads, on a file four times the size, so
the format is not the ceiling. Roughly half the total gain is threading (3.0x
columnar, then a further 3.1x from one thread to four). And Shapefile's 0.61x is
what the generic path costs: GDAL gives every driver a `GetArrowStream()` built
on `GetNextFeature()` (slide 9), and for a driver that does not override it the
result is slower than plain row iteration. The gain comes from not materialising
a row, not from the shape of the API.

Slide 11 lists specialised `WriteArrowBatch` implementations for GeoParquet and
GeoArrow only, so GDAL's GeoPackage Arrow **write** goes through its generic
`CreateFeature` path. That is the one place where being ahead, rather than
level, is a reasonable target.

Slide 13 records what adopting these APIs cost GDAL's developers: significantly
more complex code than feature-based equivalents, impedance mismatches between
native and Arrow types, and Arrow types being a moving target. The DATETIME
mapping below is exactly that kind of mismatch.

## Tasks

### Arrow / GeoArrow (feature `arrow`)
- [ ] `layer.read_arrow(opts) -> impl RecordBatchReader`: attribute columns to
      Arrow types (documented mapping, incl. DATETIME → timestamp semantics),
      geometry as GeoArrow **WKB-encoded** column (free: GPB bodies are WKB;
      native geoarrow encodings later), batch size option, bbox/WHERE options
      mirroring the scalar API.
- [ ] **Constraint on that implementation:** it must not be layered over the
      feature or cursor read path. Arrow arrays are built directly from the
      statement's column values; no `Feature` and no `Value` is constructed per
      row. Reusing the row path is the obvious way to write this and is the
      shape that measured 0.61x above. This is a correctness-of-approach item,
      so it is pinned by the criteria rather than by a unit test.
- [ ] Parallel `read_arrow`: one connection per thread over disjoint primary-key
      ranges, since SQLite permits concurrent readers and `rusqlite::Connection`
      is `Send`, so handle-per-thread needs no `unsafe`. Open questions to
      settle when it is built: whether batches are delivered in key order or
      the order is documented as unspecified; how ranges are chosen when keys are
      sparse (an even split of the key space is not an even split of the rows);
      how a bbox query splits its rtree candidate set; the default thread count;
      and whether a scoped-thread implementation suffices or a work-stealing
      dependency earns its place under the 02-ecosystem policy. A `:memory:`
      database cannot be shared between connections, so the parallel path
      requires a file and the property tests that use `:memory:` exercise the
      single-threaded path only.
- [ ] `layer.write_arrow(reader: impl RecordBatchReader)`: schema→TableSchema
      mapping, batched writes through the M2 bulk path (rtree shadow-table
      build included; this is the pyogrio-shaped fast path). A
      `RecordBatchReader` is an unsized source, which is what issue #17 was
      for: the bulk path engages for it by buffering to the threshold rather
      than trusting a size hint.
- [ ] Parallel `write_arrow`, within what SQLite allows: **one writer, always.**
      SQLite takes a single write lock per database, so this means moving CPU
      work off the writing thread, not concurrent inserts. Candidates are
      geometry encoding to GPB, envelope computation, and Arrow array decoding
      to bind values, with a single thread doing the SQLite calls. Worth stating
      plainly in the docs, because "parallel write" invites the other reading.
- [ ] GeoArrow metadata (extension name `geoarrow.wkb`, CRS as PROJJSON where
      we have it / user-supplied otherwise) on the geometry field.
- [ ] Interop test: batches round-trip through pyarrow (CI python job) and
      match pyogrio's read of the same file.
- [ ] Benchmark harness comparing against GDAL like for like: the same file, the
      same rows, the same work, and the thread count recorded for both sides. M2
      had to withdraw a GDAL-parity claim once because the figure it rested on
      also included GDAL reading a source file
      ([benchmarks/2026-07-24-gdal-like-for-like.md](benchmarks/2026-07-24-gdal-like-for-like.md)),
      and a threaded comparison has more ways to go wrong than that one did.

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

Performance first, and every figure like for like: same file, same rows, same
work, thread counts stated for both sides. A number that cannot be reproduced
from the recorded methodology does not count, which is the M2 lesson.

1. **Columnar beats row, on our own code.** `read_arrow` on one thread reads a
   large file at least **3x** faster than this crate's own feature-based
   full-scan of the same file. That is the ratio GDAL reports for its GeoPackage
   driver (6.6 s to 2.2 s), and it is the criterion that fails loudly if the
   implementation ends up layered over the row path.
2. **Threads scale.** Parallel `read_arrow` is at least **2.5x** faster on four
   threads than on one, over the same file. GDAL measures 3.1x (2.2 s to 0.7 s);
   2.5x leaves room for our own contention without letting a token
   implementation pass.
3. **Competitive with GDAL at equal thread count.** Reading, no worse than
   **1.25x** GDAL's time. Writing, **at or ahead of** GDAL: its GeoPackage
   driver has no specialised Arrow write path, so parity is the floor here, not
   the target. Both figures recorded with the comparison method, per the note
   above.
4. **No regression to the row path.** The scalar `features`/`cursor` reads stay
   within measurement noise of their 0.1.2 numbers. The Arrow work must not be
   paid for by the API most callers use.
5. Python (pyarrow + a 20-line ctypes shim, no bindings crate yet) can read a
   corpus file end-to-end zero-copy and geopandas can ingest via `from_arrow`;
   numbers against pyogrio recorded, with pyogrio's thread count stated, since
   it drives this same GDAL driver.
6. `gpkg copy` GDAL-file → ours → validators clean (the full-circle test).
7. C header diff gate active; `cargo c-build` artifacts install and link in a
   CI job on all three OSes.
8. Tag **v0.2.0**.

Criteria 1 to 3 are the ones that can fail on a working implementation, so they
are where the risk is. If any of them cannot be met, the number that was reached
is recorded along with why, rather than the criterion being quietly softened:
the M2 GDAL-parity item is the model for how that is written up.
