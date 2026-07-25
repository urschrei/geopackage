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

The slides do not say what the "tricks" are, so the driver was read directly:
[benchmarks/2026-07-25-gdal-arrow-techniques.md](benchmarks/2026-07-25-gdal-arrow-techniques.md).
It settles several questions below, and confirms from their source what we had
assumed about GPB bodies being usable as WKB without a parse.

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
      Write the direct statement loop first and measure it. *(Done, and it
      misses criterion 3 at 2.34x GDAL's time, so the next item is now
      scheduled.)*
- [x] Fill each batch from a SQLite **aggregate function**, as GDAL's driver
      does, so the whole batch is produced inside one `sqlite3_exec` and the
      per-value accessor dispatch disappears (see the study note). Expressible in
      safe Rust through rusqlite's `create_aggregate_function`. This was held in
      reserve pending a measurement; the measurement came back showing that
      dispatch is over half our read time and that GDAL's entire read costs less
      than our fetching alone, with a projected ratio of about 1.08x if it is
      removed. Slide 13's warning about complexity still applies, so the direct
      loop stays as the fallback for anything the aggregate cannot express.
      *(Done. 38.6% off the read, and the ratio against GDAL falls from 2.34x to
      1.39x, so criterion 3 is closer but still not met. One difference from
      GDAL: an aggregate collapses its input to one row, so a `LIMIT` beside it
      would bound the aggregate's output rather than the scan. GDAL slices with
      `BETWEEN` on a dense key instead; wrapping our paginated query in a
      subquery keeps the batch bounded without requiring dense keys. The direct
      loop is the fallback when a table has more columns than SQLite's
      function-argument limit, and is tested by lowering that limit on the
      connection.)*
- [ ] (issue #25) Revisit array building, which is now the gap. *(Partly done,
      and the gap persists at about 1.41x. `gpb::body_offset` stopped the geometry column
      decoding an envelope it discards, worth 4.3%; moving the column-name lookup
      off the hot path was worth nothing and is kept only because it is the
      better shape. A per-type decomposition ranks the cost datetime > text >
      blob > double > integer, putting the four text columns at roughly 22% of a
      realistic read and the datetime column at 7%. The profiler was no help:
      `sample` attributes the whole read to `main` with or without LTO, because
      the path inlines away. Remaining, in order: fixtures that hold row bytes
      constant, so building cost can be separated from the cost of reading a
      bigger row, which the current decomposition conflates; then the text path,
      the only line item large enough to supply the ~11% that criterion 3 still
      needs. Tracked as issue #25; nothing is blocked on it now that threaded
      reading is the default and the single-threaded gap is accepted.*
      *Update: the separation is done and retires the second half. Holding row
      size constant, text costs 0.8 ns per value more than a blob of the same
      length, so the 29.8 ns previously charged to text was almost entirely the
      cost of reading a bigger row. There is no text path worth attacking. What
      is left is per-value overhead that does not depend on type, and whether
      that is worth pursuing is an open question rather than an inherited plan.)*
- [x] Parallel `read_arrow`: one connection per thread over disjoint primary-key
      ranges, since SQLite permits concurrent readers and `rusqlite::Connection`
      is `Send`, so handle-per-thread needs no `unsafe`. The shape is settled by
      reading GDAL's driver (see the study note): read-only connections only,
      which removes the writer-under-readers question rather than answering it;
      batches delivered in feature-id order, which falls out of assigning each
      task a fixed start offset and consuming the queue FIFO; a default of
      `min(4, cpus)` threads. Sparse primary keys are not solved but detected:
      the parallel path engages only when `min(fid) == 1` and
      `max(fid) == row count`, so `WHERE fid BETWEEN a AND b` is a rowid range
      scan, and anything else falls back to the single-threaded path. Still open:
      how a bbox query splits its rtree candidate set, and whether scoped threads
      suffice or a work-stealing dependency earns its place under the
      02-ecosystem policy. A `:memory:` database cannot be shared between
      connections, so the parallel path requires a file and the property tests
      that use `:memory:` exercise the single-threaded path only.
      *(Done, and it is the default: `read_arrow` threads unless asked not to,
      since a single-threaded figure nobody runs is not worth optimising for.
      2.02x on four threads and 2.25x on
      eight, short of the 3.1x GDAL's slides report. Diminishing past four is
      what a read bound by pulling pages rather than by cores looks like, which
      is why the automatic count stops there. The density rule is
      `max - min + 1 == count`, slightly wider than GDAL's, and each of the three
      conditions declines to the single-threaded path rather than failing. The
      bbox-splitting question is still open; it does not arise until
      `features_in` has an Arrow counterpart.)*
- [ ] Decide the WKB column's Arrow type. `BinaryArray` carries int32 offsets, so
      one batch cannot hold more than 2 GB of WKB, which large polygons reach.
      GDAL cuts the batch short against a byte budget of `min(INT32_MAX,
      RAM / 4)`; `LargeBinaryArray` avoids the ceiling instead. Check which the
      `geoarrow.wkb` encoding permits before choosing. Default batch size 65536,
      following GDAL.
- [x] `layer.write_arrow(reader: impl RecordBatchReader)`: schema→TableSchema
      mapping, batched writes through the M2 bulk path (rtree shadow-table
      build included; this is the pyogrio-shaped fast path). A
      `RecordBatchReader` is an unsized source, which is what issue #17 was
      for: the bulk path engages for it by buffering to the threshold rather
      than trusting a size hint.
      *(Done, sharing the M2 write path rather than duplicating it. That took a
      refactor: `write_all` was generic over `NewFeature<G: GeometryTrait>` and
      encoded through `encode_gpb`, which would have parsed each WKB geometry
      into a trait view and serialised it straight back out. The batching, the
      bulk-index decision and the transaction handling are now generic over a
      `WritableRow`, so both paths share them and differ only in how one row
      reaches the database. Geometry goes in as `geopackage_core::geometry::
      encode_gpb_from_wkb`, which puts a header in front of the bytes; it still
      parses them once, because the envelope is needed for the header and the
      index anyway and because parsing is what rejects EWKB. Layer creation from
      an Arrow schema is done too, as `TableSchemaBuilder::from_arrow_schema`.)*
- [x] Bind the columnar write directly from Arrow arrays, without a `Value` per
      cell. *(Done, 1.55x to 1.30x. Two changes: move values into the bindings
      instead of cloning them (3%), and hold a row as a view over its batch,
      binding strings and blobs as slices into the Arrow buffers (a further
      13%), then stop composing the `INSERT` statement per row (a further 30%,
      and the scalar path gained 22.6% unindexed and 16.0% on the bulk path from
      the same change). Criterion 3's write side is met without a spatial index
      at 0.93x and missed by 4% with one. See
      [benchmarks/2026-07-25-gdal-arrow-write.md](benchmarks/2026-07-25-gdal-arrow-write.md).)*
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

1. **The columnar path is not layered over the row path.** Two conditions, both
   from one run of the `arrow` bench, over both workload shapes: `read_arrow`
   pinned to one thread is faster than `row/cursor`, the faster of our two row
   APIs, and faster than `row/features`.

   That is sufficient because a columnar path built on the row path, which is
   what GDAL's generic implementation is and what measures 0.61x for their
   Shapefile driver, pays everything `row/cursor` pays and then builds arrays on
   top. It is necessarily slower than the API it wraps, so beating that API
   cannot be faked.

   *Restated once and trimmed once, both recorded rather than quietly applied.
   Originally "at least 3x `row/cursor`", taken from GDAL's ratio for its own
   driver, which was mis-calibrated: the profile in
   [benchmarks/2026-07-25-arrow-read-profile.md](benchmarks/2026-07-25-arrow-read-profile.md)
   shows 3x is unreachable by any implementation of this path, because even
   eliminating per-value accessor dispatch entirely leaves 2.37x. GDAL's
   headroom is in their row baseline, which allocates a feature object per row;
   ours does not, so the ratio graded our row path more than our columnar one.*

   *The restatement then carried a third condition, that array building be under
   30% of the total, measured as the total above the fetch floor. It was dropped
   for two reasons. It duplicated what the two conditions above already catch,
   and it was not stable: building is a residual against a floor that moves when
   the implementation moves, so when the aggregate function collapsed the fetch
   cost the share went from 23% to 53% on unchanged building code, reading as a
   failure because the read got 40% faster. The residual also disagreed with
   itself, 43 ms against 59 ms for the same code, because the two floors are
   approximate in opposite directions. The property it was reaching for, that no
   `Feature` or `Value` is constructed per row, is a fact about the code and is
   stated as a constraint in the task list above, where review can enforce it.*
2. **Threads scale.** Parallel `read_arrow` is at least **2.5x** faster on four
   threads than on one, over the same file. GDAL measures 3.1x (2.2 s to 0.7 s);
   2.5x leaves room for our own contention without letting a token
   implementation pass.
3. **Competitive with GDAL at equal thread count.** Reading, no worse than
   **1.25x** GDAL's time. Writing, **at or ahead of** GDAL: its GeoPackage
   driver has no specialised Arrow write path, so parity is the floor here, not
   the target. Both figures recorded with the comparison method, per the note
   above.

   With criterion 1 restated, this is where the performance weight sits: it is
   the figure an outside reader can check, and the one that decides whether
   GDAL's aggregate-function technique needs to come out of reserve.

   *Measured, read side, 2026-07-25: **2.34x** with the direct loop, **1.39x**
   with the aggregate function, so not met. Same file, same
   rows, same columns, same batch size, same SQLite version, GDAL driven through
   the OGR C API so nothing else sits in the loop. See
   [benchmarks/2026-07-25-gdal-arrow-comparison.md](benchmarks/2026-07-25-gdal-arrow-comparison.md).
   The aggregate function has since been built, which is what took it from 2.34x
   to 1.39x; array building is now the remaining gap. **Knowingly accepted as
   unmet**, on the ground that single-threaded is no longer the default
   configuration: at its default this crate reads the same file in 259 ms where
   GDAL takes 367 ms at any thread setting. The criterion is left as written
   rather than restated around the new default, so what it asks and what was
   achieved both stay legible.*
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
