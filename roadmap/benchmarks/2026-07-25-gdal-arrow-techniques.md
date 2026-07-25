# How GDAL's GeoPackage driver makes its Arrow read path fast

Study of GDAL 3.12.3, `ogr/ogrsf_frmts/gpkg/ogrgeopackagetablelayer.cpp` and
`ogr/ogrsf_frmts/generic/ograrrowarrayhelper.cpp`, read at tag `v3.12.3` to
match the locally installed GDAL. Written because the slides behind
[05-m3-arrow-ffi.md](../05-m3-arrow-ffi.md) say the driver performs "not very
far away from Parquet using some tricks and multithreading" without saying what
the tricks are.

Techniques only, per the adaptation policy in
[02-ecosystem.md](../02-ecosystem.md): nothing here is transliterated, and the
C++ does not carry over anyway.

## 1. The batch is filled by a SQLite aggregate function

This is the central trick, and it is not obvious. Rather than stepping a
statement and reading columns row by row, the driver registers an aggregate SQL
function and runs one statement per batch
(`GetNextArrowArrayInternal`, line 8935):

```sql
SELECT OGR_GPKG_FillArrowArray_INTERNAL(-1, "fid", "geom", "attr1", "attr2", ...)
FROM "table" WHERE "fid" BETWEEN <start + 1> AND <start + batch_size>
```

The step callback (line 7847) fires once per row *inside SQLite's own loop*,
receives every column as a `sqlite3_value*`, and writes straight into the Arrow
buffers. One `sqlite3_exec` produces a whole batch. What it removes is the
per-row return into application code and the per-column accessor dispatch on top
of it.

Two wrinkles worth knowing before copying it. The first argument is a field
start index, because a table with more columns than `SQLITE_LIMIT_FUNCTION_ARG`
cannot be passed in one call: the SELECT then contains several calls of the same
aggregate, each covering a slice of the columns, and each is told where its
slice begins. And an over-large batch is cut short from inside the callback by
calling `sqlite3_interrupt` on the connection.

**For us.** rusqlite exposes `create_aggregate_function` and the whole thing is
expressible in safe Rust, so `unsafe_code = "forbid"` is not in the way. The
builders would live behind shared mutable state that the step callback appends
to.

Whether it is *needed* is a separate question and should be measured, not
assumed. The overhead this removes is largest in a C++ path with virtual
dispatch per field; a plain `stmt.query()` loop with `row.get_ref()` in Rust is
already much leaner than the thing GDAL is routing around. The order of work
should therefore be: write the direct loop, measure it against criterion 1, and
reach for the aggregate only if the direct loop falls short. Slide 13's warning
about Arrow code being significantly more complex than feature-based code
applies squarely to this technique.

## 2. Parallelism is only attempted when feature ids are dense

The optimised path engages only if `min(fid) == 1` and
`max(fid) == total feature count`, that is, a gap-free 1-based key
(`GetNextArrowArray`, line 8660). The test costs two aggregate queries and is
cached in a tri-state member. If it fails, the driver falls back to a
single-background-thread path instead.

That precondition is what makes `WHERE fid BETWEEN a AND b` a correct and cheap
way to cut batches: no `OFFSET`, no window function, just a rowid range scan.

**For us.** This answers the open question in the M3 doc about sparse primary
keys, and the answer is: do not solve it. Check for density, take the parallel
range path when it holds, and fall back otherwise. Our `fid` is an integer
primary key and therefore a rowid alias, so `BETWEEN` is the same rowid range
scan it is for GDAL. Deletions are what break density in practice.

## 3. One connection per thread, batches still delivered in order

When the dense-key test passes, the driver opens N *additional dataset handles*
on the same file, each with its own SQLite connection and its own layer object,
and each prefetches one batch at a fixed start offset (line 8810 onwards).
Consumption is FIFO, and the consumer asserts that the task it pops starts at
the row it expects. A consumed task is then recycled: its start advances by
`n_tasks * batch_size` and it is pushed back, giving a rolling pipeline with N
batches in flight.

So batches arrive in feature-id order. The ordering falls out of assigning each
task a deterministic offset and popping the queue in order; it costs nothing
extra.

Conditions checked before any thread is spawned, all of them worth copying:

- the dataset is read-only (`GA_ReadOnly`), which sidesteps a writer mutating
  under the readers entirely;
- at least two batches remain;
- `sqlite3_threadsafe() != 0`;
- at least two threads are available;
- usable physical RAM is above 1 GB.

Thread count defaults to `min(4, num_cpus)`, overridable by
`OGR_GPKG_NUM_THREADS`, which also accepts `ALL_CPUS`.

**For us.** Two open questions in the M3 doc are answered by this: batches are
delivered in order, and the default thread count is `min(4, cpus)`. The
read-only restriction is the most valuable part to copy, because it removes a
whole class of concurrency question rather than answering it. Our equivalent is
opening additional read-only connections to the same path, which is also why the
parallel path cannot work on a `:memory:` database.

## 4. Batch size, and the 2 GB offset ceiling

`MAX_FEATURES_IN_BATCH` defaults to **65536**
(`ograrrowarrayhelper.cpp`, line 1947 of `ogrlayerarrow.cpp`).

The per-array memory limit is `min(INT32_MAX, usable_RAM / 4)`
(`GetMemLimit`, `ograrrowarrayhelper.cpp` line 25). The `INT32_MAX` is not
arbitrary caution: Arrow's `binary` type carries **int32 offsets**, so a WKB
column cannot exceed 2 GB within a single batch. When the running batch would
cross the limit the step callback submits early, logging "premature notification
of N features to consumer due to too big array".

**For us.** This is a real constraint we would otherwise have met at runtime
with large polygons. arrow-rs has the same split: `BinaryArray` is int32-offset,
`LargeBinaryArray` is int64. So M3 needs a decision, either cut batches short on
a byte budget as GDAL does, or emit large_binary, and the GeoArrow
`geoarrow.wkb` encoding needs checking for which it permits. 65536 is a
reasonable default batch size to start from.

## 5. The geometry column is a pointer slice, not a parse

In the ordinary case the driver reads the GPB header only for its length, then
takes the WKB body as a pointer into the SQLite blob and copies it into the
Arrow buffer (line 8003):

```c
pabyWkb = pabyBlob + oHeader.nHeaderLen;
nWKBSize = nBlobSize - oHeader.nHeaderLen;
```

No geometry object is constructed. This confirms the "free: GPB bodies are WKB"
assumption in the M3 doc from the other implementation's source rather than from
our reading of the spec. The exceptions are coordinate-precision rounding
(`m_bUndoDiscardCoordLSBOnReading`) and SpatiaLite-format blobs, both of which
do parse; neither applies to us.

A spatial filter, when present, is applied inside the same callback and uses the
GPB header envelope when the header carries one, so the common case still does
not parse the body. That matches what `bulk::envelope_of` already does on the
write side.

## What this changes in M3

- Direct-loop implementation first, measured against criterion 1, with the
  aggregate-function technique held in reserve rather than adopted up front.
- Parallel reads: dense-key precondition, read-only connections, ordered
  delivery, `min(4, cpus)` default. These were open questions and are now
  settled.
- New decision needed: int32-offset binary with byte-budgeted batches, or
  large_binary.
- Default batch size 65536.
