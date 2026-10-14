# Arrow read: what is left against GDAL

Working notes for the next round of work on `Layer::read_arrow`. Figures are
from the benchmark page (`roadmap/benchmarks/README.md`): `main` at 24707b4
against GDAL 3.13.3, on the dataset subsets, Fly.io `performance-8x`,
medians of 3, in milliseconds.

| | `buildings` | GDAL | `rivers` | GDAL | `admin` | GDAL |
|---|---|---|---|---|---|---|
| Arrow read (up to 4 threads) | 158 | 152 | 269 | 191 | 351 | 607 |
| Arrow read, one thread | 331 | 331 | 623 | 530 | 340 | 605 |

The gaps are on `rivers`: GDAL is 18 % faster on one thread and 41 % faster
threaded. `rivers` has 14 numeric attributes and a line string per row.
`admin` is read on one thread on both sides, because its subset of 15,000
rows is shorter than two batches.

## Already done (merged)

- `SQLITE_OPEN_NO_MUTEX` on `open` and `open_read_only`.
- Threaded read without `count(*)`: key windows with an upper bound, a skip
  over empty windows, and a merge by window index (`arrow/parallel.rs`).
- Batches filled by `SELECT agg(...) FROM t WHERE key BETWEEN a AND b`, with
  no subquery; the aggregate stops the scan with an error when the batch is
  full (`arrow/aggregate.rs`, `aggregate::stop`).

## Tried and dropped

A raw aggregate through the SQLite C API (`unsafe`), reading each argument
with the accessor its column expects instead of through rusqlite's
`ValueRef::from_value`. Measured with `quick` against its parent:

- one thread: instructions −1.7 to −5.1 %, cycles −4.1 to −9.5 % (rivers
  −9.5 %);
- threaded: wall time within ±3 %.

The profile had shown `from_value` at 17 % of cycles, half of it on a
mispredicted indirect jump on the storage class. The raw path still makes two
calls into C per value (`sqlite3_value_type` and the accessor) and still
dispatches on the builder variant per value. Dropped as not worth an `unsafe`
module of about 280 lines. The approach is avenue 3 below if that changes.

## Avenues, in order of likely gain

### 1. Thread scaling

Four threads make our `rivers` read 2.3 times faster than one thread; GDAL's
2.8 times. Hypotheses, each with the measurement that would test it:

- **The last round of windows is uneven.** The `rivers` subset has 847,788
  rows: 13 windows of 65,536 keys over 4 workers, so the last round has one
  worker busy and three idle. Test: compare the scaling on the subset with
  the scaling on the full `rivers` (130 windows), and with a batch size that
  divides the subset into a multiple of 4 windows. If this is the cause,
  options are smaller work units than a batch (a worker fills part of a
  window, the consumer joins the parts), or windows handed out by a shared
  counter instead of a fixed rotation.
- **Workers wait for the consumer.** Each worker's channel is a rendezvous
  channel; the consumer takes what is offered with `try_recv` on each call of
  `next`, but a worker whose batch is not taken stops. Test: per-thread busy
  and idle time from `samply record` with `--samples` (counts time off the
  CPU), or user plus system time against wall time.
- **Worker start-up.** Each worker opens a read-only connection, reads the
  layer's schema and registers an aggregate. Test: time from `read_arrow` to
  the first batch, against the total.

First experiment: 1, 2, 4 and 8 threads on each side, on the `rivers` subset
and on the full `rivers`, reporting wall time, user and system time. This
needs wall time on an idle machine, so use `bench_datasets.sh` (or a small
script on the Fly machine), not `quick`, which counts instructions.

`dataset_bench arrow <file> <layer> 1 <threads>` and
`OGR_GPKG_NUM_THREADS=<n> gdal_dataset_bench arrow <file> <layer>` take a
thread count.

### 2. Pre-size the builders from the previous batch

`ColumnBuilder::new` (`arrow/builder.rs`) starts text and blob buffers at 16
bytes per value and geometry at 64. A `rivers` line string is several hundred
bytes, so the geometry buffer of a 65,536-row batch grows, and copies itself,
several times. The scalar path already sizes each row from the previous one
(`RowContext::value_bytes_hint`). Keep the byte length of each column from the
last batch in the aggregate, and size the next batch's builders from it.
Measure with `quick`: instruction and cycle counts on `arrow1` and `arrow`.
Expected: a few percent.

### 3. Per-value dispatch

See "Tried and dropped". About 9.5 % of cycles on a single-threaded `rivers`
read. Needs `unsafe`.

### 4. UTF-8 validation per batch

`builder::text` validates each string. A `BinaryBuilder` validated once per
batch at `finish` would make fewer calls. `rivers` has no text attributes, and
`run_utf8_validation` was about 1 % on `buildings`. Probably not worth it.

## Not reachable from here

About 30 % of a single-threaded read is `sqlite3VdbeExec` decoding the record
into the aggregate's arguments. GDAL pays the same cost.

## Tools

- `scripts/bench_fly.sh start`, then `stop` when finished. The volume keeps
  the datasets, the subsets in `/data/quick`, the GDAL cache in
  `/data/quick/gdal-cache`, and cached `quick` builds in `/data/quick-ab/bin`.
- `scripts/bench_fly.sh quick <revA> <revB> [reps] [filter]`: two local jj
  revisions, instructions, cycles and wall time on the subsets, about a
  minute. Instructions vary by under 0.1 % between runs; cycles and wall time
  by 3 to 10 %, and up to 25 % on reads under 0.5 s. Filter example:
  `arrow`, or `arrow1 rivers`.
- `scripts/bench_fly.sh bench <rev>`: the whole page at a local revision,
  about 90 s with GDAL cached.
- Profiling on the machine: `samply` 0.13.1 is installed on the volume, and
  `/data/samply_summary.py` summarises a profile there. `perf` is
  `/usr/lib/linux-tools/*/perf` (`/usr/bin/perf` refuses the Fly kernel).
  Build with `CARGO_PROFILE_RELEASE_DEBUG=true
  RUSTFLAGS="-C force-frame-pointers=yes"` into a separate target directory.

Do not run benchmarks on the workstation.
