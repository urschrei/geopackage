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

The read path carries an explicit constraint, from criterion 1: build Arrow
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

## Next

Bind directly from the Arrow arrays, without a `Value` per cell, mirroring what
the read path does. That is the change with a reason behind it rather than a
guess: the double copy of every text value is visible in the code, not inferred
from a measurement.

Until then criterion 3's write side is unmet at 1.55x, recorded as the number
reached rather than the bar moved.
