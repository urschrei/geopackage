# The spatial index: structure, contents, and the bulk build

A GeoPackage indexes a feature layer with an SQLite RTree virtual table named
`rtree_<table>_<column>`. A set of triggers, written out in Annex F.3 of the
spec, keeps it in step with the rows. The table and the triggers can each go
wrong, in different ways, and checking each costs a different amount.

## Structure and contents

The first check is structural: does the virtual table exist, and which
generation of trigger set maintains it? The SQLite catalogue answers this
cheaply, and repair acts on the answer. It cannot tell you whether the entries
in the index agree with the geometries in the table.

The second check is about contents: does the index describe exactly the rows
it should, and cover each one? Answering it means reading every geometry in
the layer and comparing. It reports the indexable rows, the entries, the rows
with no entry, the entries whose row is gone, and the entries that fail to
cover their geometry's true envelope.

An index can pass the first check and fail the second. This happens in a file
where rows were written while the triggers were absent, or that another tool
populated incompletely. This library's own writes cannot produce that state.
Because an index can pass one check and fail the other, the two have separate
remedies. Repair puts the
structure right and rebuilds the content, and does nothing when the structure
is already current. Rebuild does the content work unconditionally.

A file can arrive in one of four structural states:

- **Absent**: neither the virtual table nor any trigger is there.
- **Current**: the virtual table exists with the GeoPackage 1.4 trigger set.
- **Legacy**: a pre-1.4 or mixed trigger set.
- **Stale**: the two halves disagree, with a virtual table but no triggers, or
  triggers but no table.

A legacy trigger set is a real problem, not just untidy. The pre-1.4 `update1`
trigger corrupts the index under `UPSERT`. That is why 1.4 renamed the fixed
triggers, and why the repaired state can be detected by name. QGIS tracked the
same failure in
[QGIS#36935](https://github.com/qgis/QGIS/issues/36935). A mixed set, with
triggers from more than one generation, is the worst case. You get one when
two tools have edited a file in turn.

This library does not trust a Legacy or Stale index. `has_spatial_index`
reports both as unusable, so a bounding-box query falls back to a correct full
scan instead of returning whatever the suspect index says.

## Why repair is never automatic, when extent recording is

This library writes during a read in one place: reading a layer's extent
records the measurement when the recorded bounds are unusable. It does not do
the same for an index, and the difference is deliberate.

The two operations differ in cost, and in what happens without them.
Rebuilding an index reads every geometry in the layer and rewrites the tree.
That is too expensive to run as the side effect of a question, and a caller
who asked about an index's state has not consented to it. Without a usable
index, queries fall back to a scan and still return the correct rows, more
slowly.

An extent is the opposite on both counts. Measuring it is comparatively cheap,
and every reader that opens the file believes a wrong one indefinitely. See
[The layer extent](extent.md) for that side of the comparison.

There is a second reason to keep repair explicit. Repairing a legacy trigger
set rewrites triggers in a file the caller may only have opened to look at.
Doing that unasked would change files on disk just because someone inspected
them.

## Populating an index

Once the virtual table and the triggers exist, the index has to be filled from
the rows already in the table. There are two ways to do it.

Below a threshold, currently 10,000 candidate rows, population is a single
`INSERT INTO rtree SELECT` over the existing rows. It uses the registered
`ST_*` functions and skips empty and NULL geometries exactly as the triggers
do. It is simple, it goes through SQLite's own RTree module, and for a small
table it is cheaper.

At or above that threshold, the library uses the bulk build.

## The bulk build

SQLite's RTree module has no bulk-load entry point. Its only write path is the
per-row virtual-table update, so every insert descends the tree and may split
nodes. Populating an index one row at a time pays that cost for every row.
GDAL reached the same conclusion in
[gdal#7614](https://github.com/OSGeo/gdal/issues/7614).

The obvious alternative is to build the RTree in an `ATTACH`ed scratch
database, through the RTree module, and copy its shadow tables into the
target. Measurements show that this build has the same dominant cost, because
it still inserts every entry through the module one row at a time. The work
moves but does not go away.

So the library builds the tree directly. It lays out the `rtree_%_node`,
`_rowid` and `_parent` contents in memory from the accumulated entry set, and
writes them as ordinary rows. No RTree module logic, no `ST_*` calls, no
triggers, and no scratch database. This works because the GeoPackage RTree is
always `rtree(id, minx, maxx, miny, maxy)`, two-dimensional with no auxiliary
columns. The three shadow tables therefore have a fixed shape whatever the
index is named.

Avoiding the scratch database also matters for a reason more important than
speed. `ATTACH` requires autocommit, which would force the build out of the
caller's transaction and leave a window in which a crash commits the rows but
not the index. Built directly, the whole build is one transaction: dropping the
triggers, every row insert, the `gpkg_contents` flush, the index work and
reinstalling the triggers all commit together. The rows can never be
committed against an index that was not brought up to date with them.

The entry set comes either from a scan of the table through the `ST_*`
functions or, when the caller can account for every indexable row, from the
envelopes computed while encoding the geometries. Because of that second
source, building the index during a write costs between 20% and 47% less than
building it afterwards. It is also why creating a layer leaves an empty index
in place for the first bulk write to fill.

## The verification gate, and its cost

A hand-written RTree uses an on-disk format that SQLite does not document as
an interface. So the library checks the result before anything relies on it.

A gated build must contain exactly the accumulated `(fid, envelope)` set, by
row count and by a per-row containment check. It must also pass a structural
check: `rtreecheck` on the index, or optionally a whole-database
`PRAGMA integrity_check`. Any anomaly discards the built index and falls back
to the triggered population, so a failed gate costs time, not correctness.

The cost is substantial. Profiled over one million points, the gate is about
45% of the build: roughly 745 ms of a 1593 ms build. The time is split about
evenly between the bijection scan, which reads every entry back out of the
index, and `rtreecheck`, which walks the whole tree. GDAL's builder runs no
equivalent check. With the gate on, this library is level with GDAL, not
ahead of it.

Verification is therefore opt-in, at three levels: contents only, contents
plus `rtreecheck`, and both plus a whole-database integrity check. With
verification off, nothing is read back, so a build cannot fail its check and
cannot fall back to the triggered path. The automatic fallback exists only
where a check does. If you are writing files that matter, or bisecting a
suspected index problem, turn verification on to get both.

## Node fill

By default the bulk build packs each RTree node full. This gives the smallest
tree, the shallowest descent and the best queries, and suits a bulk load.

A full node has no room for a later insert, so the first append into it
splits immediately. Appends after a bulk load go through the triggers, which
is the per-row path the bulk build exists to avoid. The default therefore
favours the load and the queries. A lower fill factor suits a freshly built
index that you are about to append to heavily.

## Getting the work done

- [How to add a spatial index to an existing layer](../how-to/add-spatial-index.md)
- [How to repair a file's spatial indexes](../how-to/repair-spatial-indexes.md)
