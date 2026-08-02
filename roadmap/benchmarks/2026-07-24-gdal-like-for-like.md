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

The first run of this harness showed us well behind, which prompted a phase
profile. That found the cause, and fixing it changed the answer:

| distribution | ours, first run | ours, after the fix | GDAL | ratio now |
|---|---|---|---|---|
| uniform | 1841 ms | 1593 ms | 1480 ms | 1.08x slower |
| clustered | 3871 ms | 1956 ms | 2140 ms | 0.91x, i.e. faster |

At 20k rows the same script reports ours 50 ms against GDAL's 32 ms, but at that
size the startup floor is most of the raw measurement and the subtraction
dominates, so it means little.

## Where the time actually goes

Phase profile of our build at 1M points, before the fix. This is the evidence
that decided what to do about the gap:

| phase | uniform | clustered |
|---|---|---|
| `ST_*` envelope scan | 343 ms | 351 ms |
| tree construction (Hilbert keys, sort, node encoding) | **92 ms** | **89 ms** |
| SQLite writes of the shadow tables | 594 ms | 1855 ms |
| gate (bijection scan + `rtreecheck`) | 745 ms | 1223 ms |
| total | 1810 ms | 3550 ms |

Two things follow immediately.

**Tree construction is 5% of the time.** Replacing the packing algorithm with
something better, including porting the R*-tree builder GDAL uses, could at best
save a fraction of 90 ms. It cannot account for a 372 ms gap, let alone close
it. That question is settled by this table.

**The shadow-table writes were nearly all `%_rowid`.** Splitting them by table
gave 36 ms for `%_node`, 6 ms for `%_parent`, and 556 ms (uniform) or 1758 ms
(clustered) for `%_rowid`. That table is keyed by feature id, but the packer
emits its rows in leaf order, which is Hilbert order and close to random with
respect to feature id, so a million inserts were paying page splits the whole
way. Buffering them and inserting in key order costs 16 bytes per entry and a
sort, and brings both distributions to ~265 ms. It also removes the
distribution sensitivity entirely, which is why the clustered case improved so
much more than the uniform one.

## The gate, which GDAL does not have

After the fix, the largest single component of our build is the gate: 745 ms of
1593 ms on uniform data, being a full bijection scan of the written index
against the accumulated envelopes plus `rtreecheck` over the whole tree. GDAL
runs no equivalent.

That is a deliberate trade. The tree is written by hand from our own
understanding of an undocumented on-disk format, so it is checked by SQLite's
own checker and against the input set before it is trusted, with a fallback to
the triggered build on any anomaly. Roughly half our build time is that
insurance. It is worth stating plainly rather than hiding in an aggregate: on
these figures, without the gate we would be comfortably faster than GDAL on both
distributions, and the reason not to do that is confidence, not speed.

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

On the same operation over the same file, our index build is now **8% slower
than GDAL on uniformly spread points and 9% faster on clustered points**, while
carrying a verification pass GDAL does not run and producing a tree a third
smaller for equivalent query latency. Calling that parity is fair; claiming a
win is not.

The earlier claim of parity, from the `ogr2ogr` comparison, was still an
artefact and is still withdrawn: it was right by accident, for a measurement
that could not have shown it.

**Do not port GDAL's R*-tree bulk loader.** Tree construction is 5% of our
runtime. The gap it could address does not exist; the costs that do exist are
the `ST_*` scan, the shadow-table writes, and our own gate. This was worth
measuring rather than arguing about, and the phase table is the reason the
answer is clear.

Remaining candidates, in the order the profile suggests:

- The gate, at ~45% of the build. Not a defect, and not something to remove
  lightly, but the one place where real time is available if confidence in the
  packer ever justifies making it optional.
- The `ST_*` envelope scan at ~343 ms, which `write_all` already avoids by
  reusing encode-time envelopes but `create_spatial_index` cannot.

## Reproducing

```sh
scripts/compare_gdal_index.sh 1000000 5 uniform
scripts/compare_gdal_index.sh 1000000 5 clustered
```
