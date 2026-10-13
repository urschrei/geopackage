# Arrow write against GDAL: 1.55x slower

Apple M2 Pro, macOS, GDAL 3.12.3, release build, 200,000 polygon rows with
thirteen attributes. `scripts/compare_gdal_arrow_write.sh 200000 5`.

M3 acceptance criterion 3 asks for the write to be at or ahead of GDAL,
because GDAL's GeoPackage driver has no specialised `WriteArrowBatch`. Slide 11
of the reference talk lists only GeoParquet and GeoArrow, so a GeoPackage write
goes through its generic `CreateFeature` path. Being ahead was the expectation,
not an achievement. We are behind.

## Method

Both arms first read the same source file's Arrow stream into memory, untimed,
and then time only the writing of those batches into a fresh file. Timing a
read plus a write gives a figure that says nothing about either; the M2
comparison was withdrawn for that mistake.

Our arm is `arrow_bench write`; GDAL's is `scripts/gdal_arrow_write.c`, using
the OGR C API. Repetitions alternate arm order, and figures are medians.

## Matching the spatial index

The first version of this comparison had our arm write no spatial index and
GDAL's write one, because their driver creates one by default and our
`create_layer` does not. It compared a write that builds an index with one
that does not, and reported 1.17x, which flattered us. The script now measures
both configurations and afterwards checks that the rtree tables are present or
absent as requested, rather than trusting the flag.

| | ours | GDAL | ratio |
|---|---|---|---|
| no spatial index | 671.2 ms | 433.6 ms | **1.54x** |
| with spatial index | 839.7 ms | 539.7 ms | **1.55x** |

Building the index costs us 168 ms and GDAL 106 ms at this size. Our M2 work
measured index-build parity at 1M rows. At 200k the bulk build's fixed costs,
the gate in particular, are a larger share, which is consistent with that.

## Why we are slower

The read path has an explicit constraint from criterion 1: build Arrow arrays
straight from the statement, never through a `Feature` or a `Value`, because a
columnar path layered over the row path is slower than the row path it wraps.
That was measured, and it is why `read_arrow` is built the way it is.

The write path does not follow the same constraint. Every row currently
becomes an `ArrowRow` holding an owned `Vec<Value>` and an owned copy of its
WKB. That row is then bound through `value_to_sql`, which clones each `String`
and `Vec<u8>` a second time. A text value is therefore copied twice per row
before it reaches SQLite, and a geometry once, on top of the per-row `Vec`
allocations.

The columnar write is built on the row write, which is the shape the read side
forbids. Being 1.55x slower than a generic `CreateFeature` implementation is
the cost.

## Streaming the rows

The first implementation collected every row into a `Vec<ArrowRow>` before
writing anything. That held the whole input in memory. It also made the source
*sized*, so the bulk index path saw a known length and never exercised the
unsized-source handling from issue #17. This path is the reason that handling
exists.

Rows are now produced one batch at a time and passed on lazily. A failure
travels as a row of its own, so it still reaches the write path and rolls the
transaction back. Peak memory is one batch, and the source is genuinely
unsized again.

This made no measurable difference to the time (1.54x to 1.55x, noise). It was
a memory and correctness fix only.

## First change: move values into the bindings instead of cloning them

`value_to_sql` took `&Value` and cloned it, so every string and blob was copied
a second time on the way to SQLite. The columnar path now owns its values and
moves them through `value_into_sql`. The scalar path keeps the borrowing form,
since it receives a slice it does not own.

Over the same fixture: **1.54x to 1.51x** without an index, **1.55x to
1.50x** with. That is about 3%, roughly what removing one of the two copies of
four short strings per row should be worth.

The absolute figures rose on both sides between runs (671 to 699 ms for us,
434 to 461 ms for GDAL). That is machine drift, not a regression. Compare the
ratios.

## Second change: bind straight from the Arrow arrays

A row now stores `Arc<RecordBatch>`, a shared column layout and an index,
instead of owned values. Strings and blobs are bound as slices into the Arrow
buffers through `ToSqlOutput::Borrowed`. Only `DATE` and `DATETIME` are owned,
because a GeoPackage stores them as text that has to be produced. Because
`ToSqlOutput` carries both cases, this was straightforward. An earlier sketch
with a separate buffer of owned strings could not satisfy the borrow checker
without two passes.

**1.51x to 1.32x** without an index, **1.50x to 1.30x** with.

## Third change: stop rebuilding the INSERT statement for every row

`insert_sql` composed the statement on each call: a `Vec` of column names, a
`String` per placeholder, and two joins. For a fifteen-column table that is
roughly seventeen allocations per row to produce one of four fixed strings.
The four are now built once per writer and selected by whether the row has an
explicit id and whether it has a geometry.

**1.30x to 1.04x** with an index and **1.32x to 0.93x** without. The columnar
write is now ahead of GDAL in the unindexed case.

This was the largest of the three changes by a wide margin, and it was not an
Arrow problem. The scalar write path paid it too, measured against a baseline
at the same row count:

| | before | after | change |
|---|---|---|---|
| `write/point/unindexed` | 168.1 ms | 130.1 ms | **-22.6%** |
| `write/point/bulk` | 347.6 ms | 291.9 ms | **-16.0%** |

The columnar-versus-row comparison could not show this cost, because both
sides paid it. It turned up only when we asked what was left after the two
Arrow-specific changes. That argues for continuing to look after the obvious
candidates are gone.

## Criterion 3 status

| | ours | GDAL | ratio |
|---|---|---|---|
| no spatial index | 454.0 ms | 487.0 ms | **0.93x** |
| with spatial index | 603.4 ms | 579.1 ms | **1.04x** |

Met without an index, missed by 4% with one. The difference is the index
build: about 149 ms for us against 92 ms for GDAL at this size. Our M2 work
measured index-build parity at 1M rows, and at 200k the bulk build's fixed
costs, the verification gate in particular, are a larger share of a smaller
total. Measure whether the gap closes at 1M before treating it as one to fix.

## Next

The remaining per-row allocations are the two `Vec`s of bindings and the GPB
blob itself. The blob is unavoidable, since it is a new buffer by
construction. The binding vectors could be reused across rows if the write
loop held a scratch buffer. That means threading one through `WritableRow`,
and no measurement yet shows it would be worth the change in shape.
