# The Arrow read path: where the time goes, and why criterion 1 was wrong

Apple M2 Pro, 12 cores, release build, 200,000 rows per shape, one thread.
`cargo bench -p geopackage --features arrow --bench arrow`.

M3 acceptance criterion 1 asked for `read_arrow` to be at least **3x** this
crate's own row-based full scan. It is not, on either workload shape, and the
profile below says no implementation could make it so. The criterion was
mis-calibrated rather than missed.

## Measurements

Two shapes, because what a columnar reader saves is per-row attribute handling
and the answer depends on how much of a row is attributes:

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

The GDAL-shaped workload is the *worse* of the two for us, not the better one:
bigger geometries put more of the row into a blob copy that both paths pay
alike.

## Where the time goes

Two floors in the same benchmark attribute the total. `step_only` steps the same
query touching nothing; `step_and_fetch` additionally fetches every value while
building nothing.

| | points_9attr | polygons_13attr |
|---|---|---|
| `sqlite/step_only` | 29.9 ms (23%) | 44.2 ms (23%) |
| per-value accessor dispatch | 75.2 ms (57%) | 101.7 ms (54%) |
| Arrow array building | 27.6 ms (21%) | 43.2 ms (23%) |
| total (`arrow/read_arrow`) | 132.7 ms | 189.1 ms |

The proportions barely move between shapes: roughly a quarter is SQLite walking
the rows, over half is fetching values one at a time through rusqlite's
accessor, and the remaining quarter is building the arrays.

Pre-sizing every builder to the batch size changed the total by -0.4%
(p = 0.29), which is noise. That is what ruled out allocation and pointed at
dispatch.

## Why 3x is unreachable here

On `polygons_13attr`, 3x against the cursor means 69 ms. Array building could be
free and the read would still cost 146 ms. Even eliminating accessor dispatch
entirely, which is the most an aggregate-function implementation could hope for,
leaves `step_only` plus array building: 44.2 + 43.2 = 87.4 ms, or **2.37x**.
The same arithmetic on `points_9attr` gives 2.66x.

So the target is out of reach for any implementation of this path, not for this
one.

## Why GDAL gets 3x and we do not

Their 3x is measured against `GetNextFeature`, which heap-allocates an
`OGRFeature` and copies field by field through virtual dispatch. Our `cursor`
does much less. The headroom is in their baseline, not in their columnar path,
and we do not have the same baseline to improve on.

Per-row rates make the point, with the usual caveat that the hardware and the
data differ, so these are indicative rather than a comparison:

| | rows/second |
|---|---|
| GDAL feature iteration (3.2M in 6.6 s) | 0.48 M/s |
| our `row/cursor`, polygons | 0.97 M/s |
| GDAL Arrow, 1 thread (3.2M in 2.2 s) | 1.45 M/s |
| our `arrow/read_arrow`, polygons | 1.06 M/s |

Two readings. Our row path is roughly twice as fast per row as theirs, which is
where their ratio comes from. And their columnar path is faster than ours in
absolute terms, which suggests the aggregate-function technique buys real speed
rather than merely a flattering ratio, and that it belongs to criterion 3
(competitive with GDAL) rather than criterion 1.

## What this changes

Criterion 1 was meant to catch one specific failure: a columnar path
accidentally layered over the row path, which is the shape that measured 0.61x
for GDAL's Shapefile driver. A ratio against our own row API turns out to be a
poor test of that, because our row API is fast enough to make any honest
implementation look unimpressive.

The floors are the better test, and they come from the same benchmark run: an
implementation layered over the row path could not have an array-building share
of a quarter, nor sit below `row/cursor` at all. Criterion 1 is restated in
those terms in [05-m3-arrow-ffi.md](../05-m3-arrow-ffi.md).

The performance weight moves to criterion 3, the GDAL comparison, which is the
number an outside reader can check, and to the parallel path, where GDAL's own
figures put the remaining 3.1x.

## Fixed while measuring

`read_arrow` selected the pagination key twice whenever it is also a table
column, which a comment called free. With per-value fetching at over half the
total, a twelfth column was not free: removing it gained 1.0% (p = 0.02).

## Array building, by column type (added after the aggregate landed)

With fetching cheap, building became the gap, so `bench_building_by_type`
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

**A limit of this decomposition, stated because it changes what the numbers
mean.** The fixtures do not hold bytes-per-row constant: twelve 14-byte strings
is a much larger row than twelve small integers, so SQLite reads more pages and
decodes more bytes. Part of what is charged to "text building" above is really
the cost of a bigger row. The ranking is therefore sound as a guide to where to
look, but the per-value figures are upper bounds on building cost, not building
cost. Separating the two needs fixtures with equal row bytes across types, which
is the next step if this is pursued.

The profiler was no help. Both with and without LTO, and with debug symbols,
macOS `sample` attributes essentially the whole read to `main`, because the path
inlines away entirely. That is why this is a benchmark decomposition rather than
a profile.

## Two building changes, and what they were worth

**The geometry column no longer parses a header it does not need.**
`gpb::parse_header` decodes the envelope's doubles to return them alongside the
body offset, but the offset follows from the envelope indicator in the flags
byte alone. Since our writer always emits an envelope (D6), that was four
discarded `f64` decodes per row. `gpb::body_offset` computes the offset without
them: **-4.3%** on the read (111.2 ms to 106.5 ms).

**Moving the column-name lookup off the happy path was worth nothing.** Each
value resolved its column name only to pass it to an error that was almost never
constructed. Removing that from the hot path measured +1.3% (p = 0.06), which is
noise. Kept, because paying for error formatting only on errors is the better
shape, but it is not a speed-up and is not claimed as one.

End to end against GDAL the ratio is unmoved: 1.39x before these changes, 1.41x
after, which is run-to-run variance on both sides. Closing to the 1.25x
criterion 3 asks for needs roughly 11% more, and text is the only line item big
enough to supply it.
