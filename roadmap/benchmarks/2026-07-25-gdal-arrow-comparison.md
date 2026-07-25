# Arrow read against GDAL, like for like: criterion 3 is not met

Apple M2 Pro, 12 cores, macOS, GDAL 3.12.3, release build.
`scripts/compare_gdal_arrow.sh <rows> 5`.

M3 acceptance criterion 3 asks for a single-threaded read no worse than
**1.25x** GDAL's time. Measured: **2.34x**. The direct-loop implementation does
not meet it, and the aggregate-function technique held in reserve now has a
quantified case rather than a projected one.

## Method

Both arms are handed the same file and asked for the same thing: consume every
Arrow batch of the whole layer, and nothing else.

- Ours: `geopackage/examples/arrow_bench.rs`, `read` subcommand.
- GDAL: `scripts/gdal_arrow_read.c`, a small C program against the OGR C API.
  Written in C rather than driven from Python so that nothing but GDAL is inside
  the measured loop.

Repetitions alternate which arm runs first, timings are medians, and each arm's
own startup floor (an open and close) is subtracted from wall time. Internal and
adjusted-wall figures agree to within 1%, so only the internal ones are quoted
below.

The fixture is polygon features with thirteen attributes, the shape GDAL's
published benchmark uses.

## Cross-checks, before the numbers mean anything

| | ours | GDAL |
|---|---|---|
| rows read | 1,000,000 | 1,000,000 |
| columns in the stream | 15 | 15 |
| batches | 16 | 16 |
| SQLite version linked | 3.53.2 | 3.53.2 |

Same rows, same columns, same batch size, same SQLite. The SQLite check matters:
we bundle ours through `libsqlite3-sys` and GDAL uses the system library, so a
version difference would have put part of any gap outside either read path. It
does not.

## Result

| rows | ours | GDAL, 1 thread | ratio |
|---|---|---|---|
| 200,000 | 177.0 ms | 80.8 ms | 2.19x |
| 1,000,000 | 904.1 ms | 385.1 ms | 2.34x |

The gap widens slightly with size. Criterion 3 wants 1.25x.

## Where the gap is, from our own profile

From [2026-07-25-arrow-read-profile.md](2026-07-25-arrow-read-profile.md), at
200,000 rows of the same shape:

| | ours |
|---|---|
| SQLite stepping alone | 44.2 ms |
| plus per-value accessor dispatch | 145.9 ms |
| plus array building (the whole read) | 189.1 ms |

GDAL does the entire job in 80.8 ms, which is **below our `step_and_fetch`
floor** and only 1.8x our bare stepping cost. So their per-value handling plus
array building together cost roughly 36 ms where ours cost roughly 145 ms.

That is what the aggregate-function technique buys. Their step callback receives
every column as a `sqlite3_value*` inside SQLite's own loop, so the ~102 ms we
spend on `get_ref` dispatch has no counterpart on their side. Their array
building also appears leaner than ours, since 36 ms covers both of their phases
where our building alone is 43 ms; they write into raw buffers through an
internal NanoArrow-like helper rather than through `arrow-rs` builders.

**The technique measured, before building it.** Rather than project, the bench
gained an arm that fetches every column of every row through a SQLite aggregate
function, doing the same work as `step_and_fetch` by the means GDAL uses. Over
200,000 polygon rows of fifteen columns:

| | time | per-value fetching |
|---|---|---|
| `sqlite/step_only` | 41.7 ms | |
| `sqlite/step_and_fetch` (row loop) | 142.3 ms | 100.6 ms |
| `sqlite/aggregate_fetch` (aggregate) | **51.7 ms** | **10.0 ms** |

Fetching costs a tenth as much. That is far more than the one saved FFI call
per value the source reading suggested (`Context::get_raw` is a slice index plus
two FFI calls where `Row::get_ref` makes three, since it asks SQLite for the
column count to bounds-check every index). The rest is the per-row return into
application code disappearing: the whole loop stays inside SQLite's VDBE.

**The projection this justifies.** Our array building currently costs 39.0 ms
(181.3 - 142.3). On top of `aggregate_fetch` that is about **91 ms**, against
GDAL's 80.8 ms at the same row count: a ratio of **1.12x**, inside criterion 3.
Arithmetic over measurements rather than a guess, and the case for building it.

## What this run does not establish

GDAL's parallel prefetch **never engaged**, in any thread setting:

| `OGR_GPKG_NUM_THREADS` | time | its "Using N threads" debug line |
|---|---|---|
| unset (defaults to `min(4, cpus)`) | 522.8 ms (cold) | absent |
| `1` | 376.4 ms | absent |
| `4` | 372.2 ms | absent |
| `ALL_CPUS` | 371.0 ms | absent |

With `CPL_DEBUG=ON` the driver prints other GPKG-category lines, so debug
output is reaching us; the threading line simply never fires. The preconditions
we can check from outside are all satisfied: the dataset is opened read-only,
the feature ids are dense (`min(fid) = 1`, `max(fid) = count = 1,000,000`),
there are 16 batches so more than two remain, and the machine has ample RAM and
cores. Why it does not engage is unresolved.

So this run says nothing about what threading is worth, and the 3.1x in GDAL's
published figures remains the only evidence for it. The `gdal, 4 threads` line
the comparison script prints should be read as "not engaged" rather than as
"threading gains nothing" until this is understood.

The first run being slower (522.8 ms) is page-cache warming, not a thread-count
effect: it was simply first in the sequence.

## After building it

The aggregate path landed and was re-measured the same way.

| | before | after |
|---|---|---|
| `arrow/read_arrow`, 200k polygons | 189.1 ms | **111.2 ms** |
| against GDAL, 1M rows | 904.1 ms (2.34x) | **544.6 ms (1.39x)** |
| against `row/cursor`, 200k | 1.09x | **1.86x** |

A 38.6% reduction, and the ratio against GDAL falls from 2.34x to 1.39x.
Criterion 3 asks for 1.25x, so it is still not met, but the remaining gap has
moved: fetching is no longer where the time goes.

**Array building is now the gap.** Subtracting `aggregate_fetch` from the total
leaves about 59 ms of 111 ms, against roughly 39 ms when subtracting
`step_and_fetch` from the old total. Those two figures cannot both be right, and
neither is exact: `step_and_fetch` overstates fetching because the benchmark
forces every value into memory, and `aggregate_fetch` may understate it for the
same reason in reverse. What the pair does establish is a bracket, and the
bracket says building is now between a third and a half of the read, where GDAL
fits its whole non-SQLite cost into less than our building alone. That is the
next thing to look at, and it wants a profiler rather than another subtraction.

**A note on criterion 1's share test.** That criterion asks for the
array-building share to be under 30%, measured as the total above the fetch
floor. Against the aggregate floor it now reads about 53%, not because it slowed
because the denominator collapsed. The sub-test compares against whichever floor
the implementation actually uses, and a falling total with a rising share is the
opposite of the failure it was written to catch. It needs rethinking rather than
a pass or a fail; flagged rather than quietly adjusted.

## Parallel reads

Same harness, same file, same cross-checks (1,000,000 rows, 15 columns, 16
batches on both sides). `scripts/compare_gdal_arrow.sh 1000000 5`.

| | time | against our own 1 thread |
|---|---|---|
| ours, 1 thread | 514.9 ms | |
| ours, 2 threads | 386.4 ms | 1.33x |
| ours, 4 threads | **254.2 ms** | **2.02x** |
| ours, 8 threads | 228.9 ms | 2.25x |
| GDAL, 1 thread | 367.3 ms | |
| GDAL, 4 threads | 363.0 ms | its threading does not engage |

Scaling is 2.02x on four threads, short of the 3.1x GDAL's slides report from
one to four. Diminishing past four (2.25x on eight) is what a read bound by
pulling pages rather than by cores looks like, and is why the automatic thread
count stops at four.

**Criterion 3 at equal thread count is still not met**, at 1.40x
single-threaded. Read the other way, four of our threads finish the same work in
254 ms where GDAL takes 367 ms and cannot be made to use more, a ratio of 0.69.
That is worth having but it is not what criterion 3 asks, and the two should not
be confused: one is a comparison of read paths and the other is a comparison of
one path against four.

### Threaded is the default

`read_arrow` now reads on `min(4, available parallelism)` threads unless asked
otherwise, rather than offering a separate threaded entry point. Measured on the
same file: 529.6 ms pinned to one thread, 259.5 ms at the default.

The reasoning is that the single-threaded figure had become the only one anybody
would quote and nobody would run. It also removed an incoherence: the options
struct documented a thread count that the ordinary entry point ignored.

`with_threads(1)` still gives a read that touches no thread but the caller's,
for a caller who needs that. Below two batches of rows the threaded path declines
anyway, so a small read starts no threads and opens no connections.

**Criterion 3 remains unmet and is knowingly accepted at 1.40x**, on the ground
that a gap in a configuration that is no longer the default is worth less than
the 11% suggests. Recorded rather than restated, so the criterion still says
what it said.

### How it works, and what it declines to do

Worker `w` of `n` reads batches `w`, `w + n`, `w + 2n`, and the consumer takes
from the workers in the same rotation, so batches arrive in key order with no
reordering buffer. Each worker's channel holds one batch, bounding the memory in
flight by the thread count. Dropping the reader drops the receivers, which makes
the next send fail and stops the workers; `Drop` then joins them.

Three conditions, each declining rather than failing:

- **A file.** Workers read through their own connections, and a `:memory:`
  database is private to the connection that created it.
- **A dense primary key**, no gaps between smallest and largest. Workers are
  handed key ranges before a row is read, so a range must imply a row count.
  `max - min + 1 == count` is slightly wider than GDAL's `min == 1 && max ==
  count` and costs the same scan.
- **More than one thread requested**, resolved from `min(4, available
  parallelism)` by default.

Workers open **read-only**, which is what makes several connections over one
table safe without agreeing on a snapshot: there is no writer to race. GDAL
restricts its path the same way.

## Consequences

1. **Criterion 3 is not met**: 2.34x by the direct loop, 1.39x with the
   aggregate, against a target of 1.25x. Recorded as the numbers reached and
   why, rather than the criterion being softened, per the note at the end of the
   M3 acceptance criteria.
2. **The aggregate-function technique came out of reserve and is built.** It
   took 38.6% off the read. The projection said about 91 ms at 200k and the
   result was 111 ms, so the projection was optimistic by about a fifth, which
   is the part it attributed to array building staying put.
3. **Array building deserves a second look** after that. Our builders account
   for 43 ms of 189 ms, and GDAL's whole non-SQLite cost is 36 ms, so
   `arrow-rs` builders may not be the cheapest way to fill these arrays.
4. **The threading question is open** and blocks nothing: our own parallel path
   was already scoped on GDAL's published figures rather than on this run.
