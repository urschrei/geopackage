# Bulk index build optimisation, and a read-benchmark methodology fix

Follow-up to [2026-07-24-m2.md](2026-07-24-m2.md), which recorded the v0.1.0
figures and left the GDAL-parity target open. Same machine and software as that
run (Apple M2 Pro, macOS 15.6.1, SQLite 3.51.3 bundled, criterion 0.8.2,
`sample_size = 10`, 1,000,000 rows).

## What changed in the build

Three changes to the D8 bulk path, largest effect first. The phase figures below
come from a temporary instrumented run over 1M points. The headline figures come
from criterion.

1. **The scratch RTree inserts ran in autocommit.** `ScratchDb::build` executed
   one prepared `INSERT` per row with no surrounding transaction, so each row
   committed separately. Wrapping the loop in one transaction cut that phase
   from 4.61 s to 3.03 s.
2. **The gate ran a whole-database `PRAGMA integrity_check`.** It now runs
   `rtreecheck()` against the index just built, which walks that index rather
   than the whole file. The phase drops from 0.97 s to 0.50 s. The full check
   remains available as `StructuralCheck::FullDatabase` (issue #16).
3. **`write_all` re-derived envelopes it had already computed.** The bulk write
   encoded every geometry's GPB envelope, then discarded it, and the build
   re-derived the entry set with a five-function-per-row `ST_*` scan. Passing
   the encode-time envelopes through removes that phase: 0.26 s to 0. This
   applies only when the table was empty before the write, because that proves
   the set covers every indexable row. Otherwise the scan still runs.

### Phase breakdown, bulk `write_all` of 1M points

| phase | before | after |
|---|---|---|
| `ST_*` envelope scan | 0.26 s | 0 (reused) |
| scratch RTree build | 4.61 s | 3.03 s |
| shadow-table copy | 0.18 s | 0.19 s |
| structural check | 0.97 s (`integrity_check`) | 0.50 s (`rtreecheck`) |
| row inserts + gate + triggers | ~1.24 s | ~1.15 s |
| **total** | **7.26 s** | **4.87 s** |

### Write throughput (criterion, 1M rows)

Measured against the saved v0.1.0 baseline, so the deltas compare directly with
the published figures.

| geometry | v0.1.0 | now | change |
|---|---|---|---|
| point, bulk | 7.31 s | 4.95 s | -32.3% |
| linestring, bulk | 7.48 s | 4.99 s | -33.3% |
| polygon, bulk | 7.81 s | 5.03 s | -35.6% |

All three changes have p < 0.05. The unindexed and triggered paths are
untouched.

## Ours vs GDAL, revisited

GDAL's indexed `ogr2ogr` copy of 1M points takes 1.89 s, including its source
read. Our bulk indexed point write goes from roughly 3.9x that to roughly 2.6x.
The parity target is closer but still **not met**.

The scratch RTree build, at 3.03 s, dominates the remaining cost. It is
SQLite's own per-row RTree insertion, and tuning around it does not help.
Inserting in Hilbert-curve order was tried and is marginally slower (3.19 s,
plus 0.07 s to sort). Raising the scratch page size to 64 KB changed nothing.
Beating it requires constructing the RTree node blobs directly in Rust, as a
packed bulk load rather than repeated insertion. That is substantial work and
is tracked separately.

## Read benchmark: a methodology fix

The read benchmark's fixture built each file and then queried it through the
same connection. Its numbers therefore depended on a side effect of
construction, not on the query. With the old gate, the build's whole-database
`integrity_check` read every page, which left index queries roughly 4x faster
than on a freshly opened file. Removing `integrity_check` from the default gate
made this show up as an apparent 3-4x read regression.

It was not a regression. Direct measurements:

- The index is byte-identical before and after the change, in every
  configuration tested: 29,905 nodes, 36,723,340 bytes of node data, 858 parent
  entries.
- After the change, with `StructuralCheck::FullDatabase` (so `integrity_check`
  runs again), the small-box query takes 867 us on the same index. This
  restores the old figure.
- After the change, with the default gate and a reopened file: 1.07 ms.
- An explicit full-index warm-up query does not close the gap, so this is not a
  transient cold-cache effect.

The fixture now closes and reopens the file before measuring. This matches a
caller querying an existing `.gpkg`, and makes the figures independent of how
the fixture was built. Every read figure is slower than in the v0.1.0 set,
because that set was measured on a warmed build connection.

### Read throughput (1M point rows, reopened fixture)

| query | path | time |
|---|---|---|
| `features()` full scan | plain | 225 ms |
| `features_in` full-cover box | RTree | 567 ms |
| `features_in` small box (~0.25%) | RTree | 1.13 ms |
| `features_in` small box (~0.25%) | full scan | 99.1 ms |

The small-box RTree query is still ~88x faster than the equivalent full-scan
filter. These figures are not comparable to the v0.1.0 read table, which used
the old fixture. They replace it.

## Commands

```sh
cargo bench -p geopackage --bench write -- bulk
cargo bench -p geopackage --bench read
```
