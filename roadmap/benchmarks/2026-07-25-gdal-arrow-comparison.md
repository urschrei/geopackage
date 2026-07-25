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

## Consequences

1. **Criterion 3 is not met** by the direct loop, at 2.34x against a target of
   1.25x. Recorded as the number reached and why, rather than the criterion
   being softened, per the note at the end of the M3 acceptance criteria.
2. **The aggregate-function technique comes out of reserve.** The roadmap said
   to reach for it if the direct loop fell short of a target; it has, and the
   projection above says the technique closes the gap.
3. **Array building deserves a second look** after that. Our builders account
   for 43 ms of 189 ms, and GDAL's whole non-SQLite cost is 36 ms, so
   `arrow-rs` builders may not be the cheapest way to fill these arrays.
4. **The threading question is open** and blocks nothing: our own parallel path
   was already scoped on GDAL's published figures rather than on this run.
