# The Arrow read path: where the time goes, and why criterion 1 was wrong

Apple M2 Pro, 12 cores, release build, 200,000 rows per shape, one thread.
`cargo bench -p geopackage --features arrow --bench arrow`.

M3 acceptance criterion 1 asked for `read_arrow` to be at least **3x** faster
than this crate's own row-based full scan. It is not, on either workload shape,
and the profile below shows that no implementation could reach it. The
criterion was mis-calibrated rather than missed.

## Measurements

A columnar reader saves per-row attribute handling, so the gain depends on how
much of each row is attributes. Two shapes are measured:

- `points_9attr`: point geometry, nine attributes, eleven columns.
- `polygons_13attr`: an 11-vertex footprint polygon, thirteen attributes,
  fifteen columns. This is the shape GDAL's published figures come from, so the
  ratios are comparable to theirs.

| | points_9attr | polygons_13attr |
|---|---|---|
| `row/features` (materialising) | 260.0 ms | 346.3 ms |
| `row/cursor` (streaming, the baseline) | 153.3 ms | 206.9 ms |
| `arrow/read_arrow` | **132.7 ms** | **189.1 ms** |
| speed-up against `row/cursor` | **1.16x** | **1.09x** |
| speed-up against `row/features` | 1.96x | 1.83x |

The GDAL-shaped workload is the worse of the two for us. Bigger geometries put
more of each row into a blob copy that both paths pay equally.

## Where the time goes

Two floors in the same benchmark split the total. `step_only` steps the same
query without reading anything. `step_and_fetch` also fetches every value but
builds nothing.

| | points_9attr | polygons_13attr |
|---|---|---|
| `sqlite/step_only` | 29.9 ms (23%) | 44.2 ms (23%) |
| per-value accessor dispatch | 75.2 ms (57%) | 101.7 ms (54%) |
| Arrow array building | 27.6 ms (21%) | 43.2 ms (23%) |
| total (`arrow/read_arrow`) | 132.7 ms | 189.1 ms |

The proportions barely change between shapes. Roughly a quarter is SQLite
walking the rows, over half is fetching values one at a time through rusqlite's
accessor, and the remaining quarter is building the arrays.

Pre-sizing every builder to the batch size changed the total by -0.4%
(p = 0.29), which is noise. This ruled out allocation and pointed at dispatch.

## Why 3x is unreachable here

On `polygons_13attr`, 3x against the cursor means 69 ms. With free array
building the read would still cost 146 ms. Removing accessor dispatch entirely,
the most an aggregate-function implementation could hope for, leaves
`step_only` plus array building: 44.2 + 43.2 = 87.4 ms, or **2.37x**. The same
arithmetic on `points_9attr` gives 2.66x.

The target is out of reach for any implementation of this path, not just this
one.

## Why GDAL gets 3x and we do not

GDAL's 3x is measured against `GetNextFeature`, which heap-allocates an
`OGRFeature` and copies field by field through virtual dispatch. Our `cursor`
does much less. The headroom is in their baseline, not their columnar path, and
our baseline does not have it.

Per-row rates show this. The hardware and the data differ, so these figures are
indicative rather than a comparison:

| | rows/second |
|---|---|
| GDAL feature iteration (3.2M in 6.6 s) | 0.48 M/s |
| our `row/cursor`, polygons | 0.97 M/s |
| GDAL Arrow, 1 thread (3.2M in 2.2 s) | 1.45 M/s |
| our `arrow/read_arrow`, polygons | 1.06 M/s |

Our row path is roughly twice as fast per row as theirs, which is where their
ratio comes from. Their columnar path is faster than ours in absolute terms.
This suggests the aggregate-function technique gives real speed, not just a
flattering ratio, and that it belongs under criterion 3 (competitive with GDAL)
rather than criterion 1.

## What this changes

Criterion 1 was meant to catch one specific failure: a columnar path
accidentally layered over the row path. That design measured 0.61x for GDAL's
Shapefile driver. A ratio against our own row API is a poor test for it,
because our row API is fast enough to make any properly columnar implementation
look unimpressive.

Beating `row/cursor` is a better test, and the same benchmark run provides it.
A path layered over the row API pays everything that API pays and then builds
arrays, so it must be slower than the API it wraps. Criterion 1 is restated in
those terms in [05-m3-arrow-ffi.md](../05-m3-arrow-ffi.md).

The restatement first also included a share test: building had to be under 30%
of the total. It was dropped later the same day. It duplicated the condition
above. It also measured building as a residual against a floor that moves with
the implementation, so when the aggregate function made the read faster, the
test read as a failure.

The performance emphasis moves to criterion 3, the GDAL comparison, which an
outside reader can check, and to the parallel path, where GDAL's own figures put
the remaining 3.1x.

## Fixed while measuring

`read_arrow` selected the pagination key twice whenever it is also a table
column, and a comment called this free. With per-value fetching at over half
the total, a twelfth column was not free. Removing it gained 1.0% (p = 0.02).

## Array building, by column type (added after the aggregate landed)

Once fetching was cheap, building became the gap. `bench_building_by_type`
measures it directly: twelve attributes of one type plus a point geometry,
200,000 rows, `read_arrow` over each.

| attribute type | time | per value above `integer` |
|---|---|---|
| `integer` | 58.9 ms | |
| `double` | 71.3 ms | +5.2 ns |
| `blob` (4 bytes) | 100.9 ms | +17.5 ns |
| `text` (~14 bytes) | 130.5 ms | +29.8 ns |
| `datetime` | 148.4 ms | +37.3 ns |

On the realistic `polygons_13attr` fixture that puts the four text columns at
roughly 22% of the read and the single datetime column at roughly 7%.

This decomposition has a limitation that changes what the numbers mean. The
fixtures do not hold bytes per row constant. Twelve 14-byte strings make a much
larger row than twelve small integers, so SQLite reads more pages and decodes
more bytes. Part of what is charged to "text building" above is the cost of a
bigger row. The ranking is still a sound guide to where to look, but the
per-value figures are upper bounds on building cost, not building cost.
Separating the two needs fixtures with equal row bytes across types, which is
the next step if this is pursued.

The profiler did not help. With and without LTO, and with debug symbols, macOS
`sample` attributes essentially the whole read to `main`, because the path is
entirely inlined. That is why this is a benchmark decomposition rather than a
profile.

## Two changes to array building

**The geometry column no longer parses a header it does not need.**
`gpb::parse_header` decodes the envelope's doubles to return them alongside the
body offset, but the flags byte's envelope indicator alone determines the
offset. Our writer always emits an envelope (D6), so that was four discarded
`f64` decodes per row. `gpb::body_offset` computes the offset without them:
**-4.3%** on the read (111.2 ms to 106.5 ms).

**Moving the column-name lookup off the happy path gained nothing.** Each value
looked up its column name only to pass it to an error that was almost never
constructed. Removing that from the hot path measured +1.3% (p = 0.06), which
is noise. The change is kept, because paying for error formatting only on errors
is the better design, but it is not a speed-up.

End to end, the ratio against GDAL did not move: 1.39x before these changes and
1.41x after, which is run-to-run variance on both sides. Reaching the 1.25x that
criterion 3 asks for needs roughly 11% more, and text is the only line item big
enough to supply it.

## Correction: text is not expensive, big rows are (issue #25)

The by-type table above notes that its fixtures do not hold bytes per row
constant, so its per-value figures are upper bounds. Controlling for row size
shows how loose those bounds were.

`bench_text_and_bytes` varies one thing at a time. 200,000 rows, twelve columns
of one type, 2.4M values.

| | time |
|---|---|
| `text/4` | 105.8 ms |
| `text/16` | 109.3 ms |
| `blob/16` | 107.4 ms |
| `text/64` | 172.3 ms |
| `blob/64` | 163.0 ms |

Comparing text with blob at the same payload size keeps row size constant. The
difference that remains is UTF-8 validation and the difference between the two
array types:

| | difference | per value |
|---|---|---|
| `text/16` over `blob/16` | 1.9 ms | **0.8 ns** |
| `text/64` over `blob/64` | 9.3 ms | 3.9 ns |

The cost is under a nanosecond per value at 16 bytes and scales at roughly
0.06 ns per byte, which is UTF-8 validation running at about 16 GB/s. Text is
not intrinsically expensive to build.

The +29.8 ns per value charged to text in the by-type table was therefore
almost all row size. Twelve 14-byte strings make a far larger table than twelve
small integers, and the extra time was SQLite reading it. The text series shows
the same effect: going from 4 to 16 bytes costs 0.12 ns per byte and 16 to 64
costs 0.55, because at 64 bytes a row is 768 bytes and the file is ten times
larger.

**This retires the plan based on the earlier table.** Issue #25 was opened to
separate the two costs and then optimise the text path. The costs are now
separated, and there is no text path worth optimising. What looked like 22% of
a realistic read recoverable from text handling is mostly the unavoidable cost
of reading those bytes.

What remains is per-value overhead that does not depend on type. `text/4`, with
the smallest payload in the group, still costs 105.8 ms for 2.4M values, close
to what the fifteen-column polygon fixture costs for a similar number. That
points at the fixed work of appending a value (the two-level match in
`ColumnBuilder::append`, the null bitmap and the offset push) rather than at
anything type-specific. Whether that is worth optimising is a separate question
from the one this measurement answered. It should not assume, as the earlier
plan did, that it is.
