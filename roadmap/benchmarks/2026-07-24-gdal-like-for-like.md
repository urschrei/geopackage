# GDAL index build, measured like for like

This supersedes the GDAL comparison in the two earlier write-ups. Those compared
`ogr2ogr` copying a whole GeoPackage with our write-only path, and concluded we
had reached parity. That conclusion was wrong, and the flawed measurement was
the cause.

## Problem with the old comparison

`ogr2ogr src.gpkg dst.gpkg -lco SPATIAL_INDEX=YES` reads a source file, parses
its geometries, writes a new file and builds an index. Our figure was a write
plus an index build, with the data already in memory. GDAL was doing strictly
more work, so its number was inflated by an unknown amount, and every
conclusion drawn from the pair needed a large caveat. The comparison did not
measure what it claimed to.

## What this measures instead

GDAL exposes `CreateSpatialIndex` and `DisableSpatialIndex` as SQL functions on
an existing GeoPackage. Both implementations can therefore be given the same
file, with the same rows already written, and asked only to build the index.

`scripts/compare_gdal_index.sh`, per size and distribution:

1. Write one unindexed fixture.
2. Per repetition, copy it fresh for each arm, so neither sees an existing index
   or a warmed file. Alternate which arm goes first, so drift in machine state
   affects both arms rather than one.
3. Time each build as external wall time, then subtract that arm's own measured
   startup floor: `ogrinfo -sql "SELECT 1"` for GDAL, an open and close for
   ours. Both raw and adjusted times are reported. The floors are not small
   (~146 ms for `ogrinfo`, ~20 ms for our binary), so the 20k-row numbers below
   should not be relied on.
4. Take the median across repetitions.
5. Compare what was built as well as how fast: node count, depth, node bytes,
   and query latency over identical boxes. The same reader queries both
   indexes, so the read path is constant.

Not controlled: GDAL links its own SQLite and we bundle 3.51.3, so the two do
not run identical B-tree code. Neither arm controls the OS page cache beyond
starting from a fresh copy.

Apple M2 Pro, macOS 15.6.1, GDAL 3.12.3, 5 repetitions, host load under 15
throughout.

## Build time, 1M points

The first run of this harness showed us well behind. A phase profile found the
cause, and fixing it changed the result:

| distribution | ours, first run | ours, after the fix | GDAL | ratio now |
|---|---|---|---|---|
| uniform | 1841 ms | 1593 ms | 1480 ms | 1.08x slower |
| clustered | 3871 ms | 1956 ms | 2140 ms | 0.91x, i.e. faster |

At 20k rows the same script reports 50 ms for ours and 32 ms for GDAL. At that
size the startup floor is most of the raw measurement and the subtraction
dominates, so the comparison means little.

## Where the time goes

Phase profile of our build at 1M points, before the fix:

| phase | uniform | clustered |
|---|---|---|
| `ST_*` envelope scan | 343 ms | 351 ms |
| tree construction (Hilbert keys, sort, node encoding) | **92 ms** | **89 ms** |
| SQLite writes of the shadow tables | 594 ms | 1855 ms |
| gate (bijection scan + `rtreecheck`) | 745 ms | 1223 ms |
| total | 1810 ms | 3550 ms |

**Tree construction is 5% of the time.** A better packing algorithm, including
a port of the R*-tree builder GDAL uses, could save at most a fraction of
90 ms. That cannot account for a 372 ms gap, let alone close it.

**The shadow-table writes were nearly all `%_rowid`.** By table: 36 ms for
`%_node`, 6 ms for `%_parent`, and 556 ms (uniform) or 1758 ms (clustered) for
`%_rowid`. That table is keyed by feature id, but the packer emitted its rows
in leaf order. Leaf order is Hilbert order, close to random with respect to
feature id, so a million inserts caused page splits throughout. Buffering the
rows and inserting them in key order costs 16 bytes per entry and a sort, and
brings both distributions to ~265 ms. It also removes the sensitivity to
distribution entirely, which is why the clustered case improved much more than
the uniform one.

## The gate, which GDAL does not have

After the fix, the gate is the largest single component of our build: 745 ms of
1593 ms on uniform data. It is a full bijection scan of the written index
against the accumulated envelopes, plus `rtreecheck` over the whole tree. GDAL
runs no equivalent.

The trade-off is deliberate. We write the tree by hand from our own
understanding of an undocumented on-disk format. Before it is trusted, it is
checked by SQLite's own checker and against the input set, with a fallback to
the triggered build on any anomaly. The gate is roughly half our build time.
On these figures, without it we would be comfortably faster than GDAL on
both distributions. We keep it for confidence, not speed.

## What each build produced, 1M uniform

| | ours | GDAL |
|---|---|---|
| nodes | 20,002 | 29,890 |
| node bytes | 24.6 MB | 36.7 MB |
| depth | 3 | 3 |
| `rtreecheck` | ok | ok |

Our tree is a third smaller because it packs nodes to capacity, while GDAL's
R*-tree insertion leaves the usual slack. Both indexes return identical hit
counts for every query box tested (64, 2,494 and 140,156 on uniform; 6, 248 and
230,685 on clustered). This cross-checks the two independent builders against
each other.

## Query latency, same reader over both indexes

| box | ours (uniform) | GDAL (uniform) | ours (clustered) | GDAL (clustered) |
|---|---|---|---|---|
| tiny | 0.055 ms | 0.054 ms | 0.032 ms | 0.037 ms |
| small | 1.132 ms | 1.114 ms | 0.141 ms | 0.151 ms |
| wide | 72.9 ms | 75.0 ms | 306 ms | 295 ms |

Query performance is equivalent. Every difference is within a few per cent, and
the sign is not consistent. The smaller tree gives no measurable query
advantage here, and no disadvantage either.

## Conclusion

On the same operation over the same file, our index build is now **8% slower
than GDAL on uniformly spread points and 9% faster on clustered points**. It
also runs a verification pass GDAL does not, and produces a tree a third
smaller with equivalent query latency. That is fair to call parity, but not a
win.

The earlier parity claim, from the `ogr2ogr` comparison, was still an artefact
and remains withdrawn. It was right by accident, from a measurement that could
not have shown it.

**Do not port GDAL's R*-tree bulk loader.** Tree construction is 5% of our
runtime, so there is no gap for it to close. The real costs are the `ST_*`
scan, the shadow-table writes, and our own gate.

Remaining candidates, in the order the profile suggests:

- The gate, at ~45% of the build. It is not a defect and should not be removed
  lightly, but it is the one place with real time to save if confidence in the
  packer ever justifies making it optional.
- The `ST_*` envelope scan at ~343 ms, which `write_all` already avoids by
  reusing encode-time envelopes but `create_spatial_index` cannot.

## Reproducing

```sh
scripts/compare_gdal_index.sh 1000000 5 uniform
scripts/compare_gdal_index.sh 1000000 5 clustered
```
