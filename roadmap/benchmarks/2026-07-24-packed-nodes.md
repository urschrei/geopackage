# Packed RTree node construction

Follow-up to [2026-07-24-bulk-build.md](2026-07-24-bulk-build.md), which left
the scratch RTree build as the dominant remaining cost. Same machine and
software as the earlier runs (Apple M2 Pro, macOS 15.6.1, SQLite 3.51.3
bundled, criterion 0.8.2, `sample_size = 10`, 1,000,000 rows).

## The change

The D8 bulk build used GDAL's technique: insert every entry into a scratch
in-memory RTree, then copy its `%_node` / `%_rowid` / `%_parent` shadow tables
into the target. The copy is cheap, but the scratch build still pushed every
entry through the RTree module one row at a time, paying a tree descent and
possible node split per row. That was 3.03 s of the 4.87 s total.

`geopackage/src/packed.rs` now builds the tree outright and writes the three
shadow tables directly. The node format is taken from the RTree module's own
documentation and its `rtreecheck` implementation in `sqlite3.c`:

```text
bytes 0..2    big-endian u16: tree depth, root node only (0 = root is a leaf)
bytes 2..4    big-endian u16: number of cells
bytes 4..     cells, 24 bytes each for a 2-D index:
                big-endian i64  rowid (leaf) or child node number (internal)
                big-endian f32  min_x, max_x, min_y, max_y
remainder     zero padding, to exactly node_size bytes
```

Node size is read from `length(data)` of the freshly created root rather than
re-derived from `PRAGMA page_size`, because that is what the module itself does
when reopening an index. Minimum coordinates are rounded down and maxima up to
`f32` using the module's own `rtreeValueDown` / `rtreeValueUp` constants, so a
stored box never excludes a geometry it must contain.

Entries are ordered by the Hilbert index of their centre, packed into leaves at
full capacity, and internal levels are built bottom-up. `rtreecheck` imposes no
minimum node fill, so nodes are packed full.

Removing the scratch database also removed the `ATTACH`, which required
autocommit and forced the build outside the surrounding transaction. The build
is now a single transaction.

### Why Hilbert and not OMT or STR

Lee and Lee's OMT partitioning was implemented and measured against Hilbert
packing, then dropped:

| data | metric | Hilbert | OMT |
|---|---|---|---|
| uniform (bench generator) | build, 1M points | 1.97 s | 2.28 s |
| uniform | `features_in` full-cover box | 506 ms | 535 ms |
| uniform | `features_in` small box | 1.119 ms | 1.135 ms |
| clustered (12 dense blobs + 10% background) | build, 1M points | 2.99 s | 2.94 s |
| clustered | dense-box query | 41.0 ms | 41.8 ms |
| clustered | wide-box query | 323 ms | 329 ms |

OMT sorts at every level of the recursion rather than once, which costs about
15% more build time on uniformly spread data without producing better queries.
On clustered data, where OMT's data-driven partitioning would be expected to
pay off, the two are within noise. Hilbert packing is also the smaller
implementation, so it is what ships.

## Write throughput (1M rows)

| geometry | v0.1.0 | after gate/transaction work | packed | vs v0.1.0 |
|---|---|---|---|---|
| point, bulk | 7.31 s | 4.95 s | ~2.08 s | 3.5x |
| linestring, bulk | 7.48 s | 4.99 s | ~2.08 s | 3.6x |
| polygon, bulk | 7.81 s | 5.03 s | ~2.14 s | 3.7x |

Run-to-run spread on the bulk cases is roughly 5%, so the packed figures are
quoted approximately.

## Ours vs GDAL

| geometry | GDAL indexed `ogr2ogr` | ours, packed |
|---|---|---|
| point | 1.89 s | ~2.08 s |
| linestring | 2.03 s | ~2.08 s |
| polygon | 2.13 s | ~2.14 s |

The M2 acceptance criterion asked for bulk indexed writes at GDAL parity. On
these figures lines and polygons are at parity and points are within about 10%.

The comparison remains asymmetric in the direction recorded in the original
write-up, and this now matters more than it did: `ogr2ogr`'s time includes
reading its GeoPackage source, while ours measures the write only, with the
data already in memory. GDAL is therefore doing strictly more work for the same
number. Parity on this measurement is not the same as parity on equal work, and
the criterion should be read with that caveat rather than as a clean win.

## Read throughput (1M point rows, reopened fixture)

| query | path | before packing | packed |
|---|---|---|---|
| `features()` full scan | plain | 225 ms | 221 ms |
| `features_in` full-cover box | RTree | 567 ms | 512 ms |
| `features_in` small box (~0.25%) | RTree | 1.13 ms | 1.12 ms |
| `features_in` small box (~0.25%) | full scan | 99.1 ms | 98.7 ms |

The packed tree is not a query regression: the full-cover box improves by
around 5-10% across runs, the small box is unchanged, and the non-index paths
move only with measurement noise.

## Correctness

The tree is written by hand, so it is checked by machinery that does not share
its assumptions:

- `rtreecheck()` runs in the build gate on every bulk build and validates the
  structure SQLite itself expects: cell counts against node size, `min <= max`
  per dimension, every cell contained in its parent cell, and `%_rowid` /
  `%_parent` rows and counts matching the leaf and internal cell counts.
- The existing content gate still compares the written index against the
  accumulated envelope set as a bijection with a containment check, and falls
  back to the triggered population on any anomaly.
- The Hegel property tests compare packed builds against triggered builds for
  arbitrary feature sets and arbitrary insert/update/delete/upsert sequences,
  and `features_in` against a full-scan filter.
- Unit tests in `packed.rs` cover node size, the root being node 1, outward
  coordinate rounding, parent containment, and mapping coverage.

## Commands

```sh
cargo bench -p geopackage --bench write -- bulk
cargo bench -p geopackage --bench read
```
