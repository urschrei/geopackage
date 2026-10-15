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

### 1. Thread scaling: measured 2026-10-15

Full `rivers` (8,477,883 rows, 130 windows), build of 24707b4, medians of 3,
wall time in ms, user and system time in seconds:

| threads | ours | user | sys | GDAL | user | sys |
|---|---|---|---|---|---|---|
| 1 | 5476 | 4.59 | 0.92 | 4809 | 4.16 | 0.59 |
| 2 | 2870 | 4.85 | 0.87 | 2449 | 4.21 | 0.71 |
| 4 | 1756 | 5.08 | 1.22 | 1301 | 4.16 | 0.74 |
| 8 | 1282 | 5.67 | 2.27 | 857 | 4.32 | 1.03 |

The gap is about the same on the full file as on the subset, so the uneven
last round of windows is not the cause. Both sides keep their threads busy
(CPU time over wall time is 3.6 for us at 4 threads, 3.8 for GDAL), so
workers waiting for the consumer is not the cause either. The cause is extra
CPU time per row that grows with the thread count, from two sources.

**SQLite's page cache is shared between connections.** libsqlite3-sys
compiles the bundled SQLite with `SQLITE_ENABLE_MEMORY_MANAGEMENT`, which
makes all connections in a process share one page-cache group behind one
global mutex (`pcache1.separateCache = 0`). Each worker has its own
connection, but every page fetch and unpin takes that mutex. At 4 threads we
made 49,919 `futex` calls (14,967 contended) to GDAL's 5,665, and 85 % of our
context switches were in `pcache1Fetch` and `pcache1Unpin`; 10 % more were
in `sqlite3Malloc`, which takes the memory-statistics mutex
(`SQLITE_DEFAULT_MEMSTATUS` is 1). Ubuntu's system SQLite 3.45.1 and GDAL's
conda-forge SQLite 3.54.0 do not have `SQLITE_ENABLE_MEMORY_MANAGEMENT`;
macOS's system SQLite also has `DEFAULT_MEMSTATUS=0`. So this affects the
`bundled` feature only, which the benchmark and the CLI use.

**glibc returns batch memory to the system.** On one thread we take 129,101
page faults to GDAL's 13,876, and spend 2.85 G kernel cycles to GDAL's
2.03 G. User cycles are equal (13.4 G each), although we execute 14 % more
instructions. With `GLIBC_TUNABLES` that stop malloc from using `mmap` or
trimming the heap, faults fall to 5,934 and system time to that of GDAL.

The same read with each change, built with `LIBSQLITE3_FLAGS`:

| variant | 1 thread | 4 threads | 8 threads |
|---|---|---|---|
| as released | 5530 | 1763 | 1234 |
| `-USQLITE_ENABLE_MEMORY_MANAGEMENT` | 5772 | 1567 | 916 |
| and `-DSQLITE_DEFAULT_MEMSTATUS=0` | 5267 | 1497 | 820 |
| and the malloc tunables | 4966 | 1302 | 742 |
| `mmap_size` of 4 GiB, either build | 6153 | 1668 | 958 |
| GDAL 3.13.3 | 4809 | 1301 | 857 |

The tunables are `glibc.malloc.mmap_threshold=4294967295` and
`glibc.malloc.trim_threshold=4294967295`. A memory map adds about 1 s of user
time and is not a way round the page-cache mutex.

Next steps:

- Pre-size the builders (avenue 2), now to reduce the page faults as well as
  the copies. Measure page faults and system time, not only instructions.
- Decide what to do about the bundled SQLite flags: set `LIBSQLITE3_FLAGS` in
  the workspace's `.cargo/config.toml` (affects the benchmark, the tests and
  the CLI built from this repository, not dependants), document the flags
  for dependants that use `bundled`, and raise the shared page cache with
  rusqlite.

`dataset_bench arrow <file> <layer> 1 <threads>` and
`OGR_GPKG_NUM_THREADS=<n> gdal_dataset_bench arrow <file> <layer>` take a
thread count. `DATASET_BENCH_MMAP=<bytes>` sets the memory map.

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
