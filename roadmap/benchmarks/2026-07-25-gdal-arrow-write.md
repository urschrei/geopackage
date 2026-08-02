# Arrow write against GDAL: 1.55x slower

For a reason the read path already knew.

Apple M2 Pro, macOS, GDAL 3.12.3, release build, 200,000 polygon rows with
thirteen attributes. `scripts/compare_gdal_arrow_write.sh 200000 5`.

M3 acceptance criterion 3 asks for the write to be **at or ahead of** GDAL,
because its GeoPackage driver has no specialised `WriteArrowBatch`: slide 11 of
the reference talk lists only GeoParquet and GeoArrow, so a GeoPackage write
goes
through its generic `CreateFeature` path. Being ahead was the expectation rather
than an achievement. We are behind.

## Method

Both arms read the same source file's Arrow stream into memory first, untimed,
then time only the writing of those batches into a fresh file. Timing a read
plus
a write would give a figure that says nothing about either, which is the mistake
the M2 comparison had to withdraw.

Ours is `arrow_bench write`; GDAL's is `scripts/gdal_arrow_write.c` against the
OGR C API. Repetitions alternate arm order and figures are medians.

## The spatial index is not a detail

The first version of this comparison had our arm write no spatial index and
GDAL's write one, because their driver creates one by default and our
`create_layer` does not. That put a write that builds an index against one that
does not, and reported **1.17x**, flattering us. The script now measures both
configurations and cross-checks afterwards that the rtree tables are present or
absent as asked, rather than trusting the flag.

| | ours | GDAL | ratio |
|---|---|---|---|
| no spatial index | 671.2 ms | 433.6 ms | **1.54x** |
| with spatial index | 839.7 ms | 539.7 ms | **1.55x** |

Building the index costs us 168 ms and GDAL 106 ms at this size. Our M2 work
measured index-build parity at 1M rows; at 200k the bulk build's fixed costs,
the gate in particular, are a larger share, which is consistent with that.

## Why we are slower, and it is our own lesson

The read path has an explicit constraint, from criterion 1: build Arrow
arrays straight from the statement, never through a `Feature` or a `Value`,
because a columnar path layered over the row path is slower than the row path it
wraps. That is measured, and it is why `read_arrow` is what it is.

The write path does not honour the same constraint. Every row currently becomes
an `ArrowRow` holding an owned `Vec<Value>` and an owned copy of its WKB, which
is then bound through `value_to_sql`, and that clones each `String` and
`Vec<u8>`
a second time. So a text value is copied twice per row before it reaches SQLite,
and a geometry once, on top of the per-row `Vec` allocations.

In other words the columnar write is built on the row write, which is exactly
the
shape the read side forbids. That it is 1.55x slower than a generic
`CreateFeature` implementation is what that costs.

## What was fixed here, and what it was worth

The first implementation collected every row into a `Vec<ArrowRow>` before
writing anything. That held the whole input in memory, and it also made the
source *sized*, so the bulk index path saw a known length and the unsized-source
handling from issue #17 was never exercised despite being the reason it exists.

Rows are now produced one batch at a time and handed on lazily, with a failure
travelling as a row of its own so it still reaches the write path and rolls the
transaction back. Peak memory is a batch, and the unsized property is real
again.

It made no measurable difference to the time (1.54x to 1.55x, noise). It was a
memory and correctness fix, and is not claimed as anything else.

## First change: move values into the bindings instead of cloning them

`value_to_sql` took `&Value` and cloned, so every string and blob was copied a
second time on the way to SQLite. The columnar path now owns its values and
moves them, through `value_into_sql`. The scalar path keeps the borrowing form,
since it is handed a slice it does not own.

Measured over the same fixture: **1.54x to 1.51x** without an index, **1.55x to
1.50x** with. About 3%, which is roughly what removing one of the two copies of
four short strings per row should be worth.

Note the absolute figures rose on both sides between runs (671 to 699 ms for us,
434 to 461 ms for GDAL), which is machine drift rather than a regression; the
ratio is the figure to read.

## Second change: bind straight from the Arrow arrays

A row now stores `Arc<RecordBatch>`, a shared column layout and an index, rather
than owned values. Strings and blobs are bound as slices into the Arrow buffers
through `ToSqlOutput::Borrowed`, and only `DATE` and `DATETIME` are owned,
because a GeoPackage stores them as text that has to be produced.
`ToSqlOutput` carrying both cases is what made this straightforward; an earlier
sketch with a separate buffer of owned strings could not satisfy the borrow
checker without two passes.

**1.51x to 1.32x** without an index, **1.50x to 1.30x** with.

## Third change: stop rebuilding the INSERT statement for every row

`insert_sql` composed the statement on each call: a `Vec` of column names, a
`String` per placeholder, and two joins. For a fifteen-column table that is
roughly seventeen allocations per row, to produce one of four fixed strings. The
four are now built once per writer and indexed by whether the row has an
explicit id and whether it has a geometry.

**1.30x to 1.04x** with an index and **1.32x to 0.93x** without, so the columnar
write is now ahead of GDAL in the unindexed case.

This was the largest of the three by some way, and it was not an Arrow problem
at
all. The scalar write path was paying it too, measured against a baseline at the
same row count:

| | before | after | change |
|---|---|---|---|
| `write/point/unindexed` | 168.1 ms | 130.1 ms | **-22.6%** |
| `write/point/bulk` | 347.6 ms | 291.9 ms | **-16.0%** |

Worth noting how it was found. It was invisible to the columnar-versus-row
comparison, because both sides paid it. It turned up only from asking what was
left after the two Arrow-specific changes, which is an argument for continuing
to
look after the obvious candidates are gone.

## Where this leaves criterion 3

| | ours | GDAL | ratio |
|---|---|---|---|
| no spatial index | 454.0 ms | 487.0 ms | **0.93x** |
| with spatial index | 603.4 ms | 579.1 ms | **1.04x** |

Met without an index, missed by 4% with one. The difference between the two is
the index build: about 149 ms for us against 92 ms for GDAL at this size. Our M2
work measured index-build parity at 1M rows, and at 200k the bulk build's fixed
costs, the verification gate in particular, are a larger share of a smaller
total. Whether that closes at 1M is worth measuring before treating it as a gap
to fix.

## Next

The remaining per-row allocations are the two `Vec`s of bindings and the GPB
blob
itself. The blob is unavoidable, since it is a new buffer by construction. The
binding vectors could be reused across rows if the write loop held a scratch,
but
that means threading one through `WritableRow`, and there is no measurement yet
saying it would be worth the shape.
