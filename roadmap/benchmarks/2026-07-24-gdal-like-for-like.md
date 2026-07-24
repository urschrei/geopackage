# GDAL index build, measured like for like

This supersedes the GDAL comparison in the two earlier write-ups. Those timed
`ogr2ogr` copying a whole GeoPackage against our write-only path, and concluded
we had reached parity. That conclusion was wrong, and the measurement was the
reason.

## Why the old comparison could not answer the question

`ogr2ogr src.gpkg dst.gpkg -lco SPATIAL_INDEX=YES` reads a source file, parses
its geometries, writes a new file and builds an index. Our figure was a write
plus an index build, with the data already in memory. GDAL was doing strictly
more work, so its number was inflated by an unknown amount, and every conclusion
drawn from the pair had to be hedged. A comparison that needs a caveat that
large is not measuring what it claims to.

## What this measures instead

GDAL exposes `CreateSpatialIndex` and `DisableSpatialIndex` as SQL functions on
an existing GeoPackage. So both implementations can be handed the same file,
with the same rows already written, and asked for exactly one thing: build the
index.

`scripts/compare_gdal_index.sh`, per size and distribution:

1. Write one unindexed fixture.
2. Per repetition, copy it fresh for each arm, so neither sees an existing index
   or a warmed file, and alternate which arm goes first so drift in machine
   state is shared rather than landing on one arm.
3. Time each build as external wall time, then subtract that arm's own measured
   startup floor: `ogrinfo -sql "SELECT 1"` for GDAL, an open and close for
   ours. Raw and adjusted are both reported. The floors are not small: ~146 ms
   for `ogrinfo`, ~20 ms for our binary, which is why the 20k-row numbers below
   should not be leaned on.
4. Take the median across repetitions.
5. Compare what was built, not only how fast: node count, depth, node bytes, and
   query latency over identical boxes read by the same reader, so the read path
   is a constant.

Not controlled: GDAL links its own SQLite and we bundle 3.51.3, so the two are
not running identical B-tree code, and neither arm controls the OS page cache
beyond starting from a fresh copy.

Apple M2 Pro, macOS 15.6.1, GDAL 3.12.3, 5 repetitions, host load under 15
throughout.

## Build time, 1M points

| distribution | ours | GDAL | ratio |
|---|---|---|---|
| uniform | 1841 ms | 1469 ms | **1.25x slower** |
| clustered | 3871 ms | 2237 ms | **1.73x slower** |

Repetitions were tight: uniform 1847-1950 ms for ours against 1592-1619 ms for
GDAL, clustered 3513-4027 ms against 2296-2581 ms. The gap is well outside the
spread, so this is a real difference, not noise.

At 20k rows the same script reports ours 50 ms against GDAL's 32 ms (1.56x), but
at that size the startup floor is most of the raw measurement and the subtraction
dominates the result, so it carries little weight.

## What each build produced, 1M uniform

| | ours | GDAL |
|---|---|---|
| nodes | 20,002 | 29,890 |
| node bytes | 24.6 MB | 36.7 MB |
| depth | 3 | 3 |
| `rtreecheck` | ok | ok |

Our tree is a third smaller because it packs nodes to capacity, where GDAL's
R*-tree insertion leaves the usual slack. Both indexes return identical hit
counts for every query box tested (64, 2,494 and 140,156 on uniform; 6, 248 and
230,685 on clustered), which is a useful cross-check of two independent builders
against each other.

## Query latency, same reader over both indexes

| box | ours (uniform) | GDAL (uniform) | ours (clustered) | GDAL (clustered) |
|---|---|---|---|---|
| tiny | 0.055 ms | 0.054 ms | 0.032 ms | 0.037 ms |
| small | 1.132 ms | 1.114 ms | 0.141 ms | 0.151 ms |
| wide | 72.9 ms | 75.0 ms | 306 ms | 295 ms |

The two are equivalent for queries: every difference is within a few per cent
and the sign is not consistent. So the smaller tree buys no measurable query
advantage here, and costs none either.

## Conclusion

**The GDAL-parity target is not met.** Our index build is 1.25x slower on
uniformly spread points and 1.73x slower on clustered points, against the same
operation on the same file. The earlier claim of parity was an artefact of
comparing against a figure that included GDAL reading a source file.

What the packed build does deliver is a third fewer nodes for the same query
performance, and a build that is a single transaction.

Two things worth investigating, neither of which this run explains:

- Both implementations slow down on clustered data, but ours degrades more (2.1x
  against GDAL's 1.5x), while producing an identically sized tree in both cases.
  The distribution-sensitive part of our path is the Hilbert sort, which is the
  place to look first.
- GDAL's builder is Rouault's `sqlite_rtree_bulk_load`, which reimplements
  SQLite's R*-tree insertion in memory. That it beats a sort-and-pack build is
  not the expected result, and is worth understanding before assuming the gap is
  inherent.

## Reproducing

```sh
scripts/compare_gdal_index.sh 1000000 5 uniform
scripts/compare_gdal_index.sh 1000000 5 clustered
```
