# The spatial index: structure, contents, and the bulk build

A GeoPackage indexes a feature layer with an SQLite RTree virtual table named
`rtree_<table>_<column>`, kept in step with the rows by a set of triggers that
the spec writes out in Annex F.3. Both halves of that arrangement can go
wrong, they go wrong in different ways, and finding out about each costs a
different amount. That is the shape of everything below.

## Two questions, two prices

The first question is structural: does the virtual table exist, and which
generation of trigger set maintains it? It is answered from the SQLite
catalogue, it is cheap, and it is what a repair acts on. What it cannot tell
you is whether the entries in the index agree with the geometries in the
table.

The second question is about contents: does the index describe exactly the
rows it should, covering each? Answering it means reading every geometry in
the layer and comparing. It reports the indexable rows, the entries, the rows
with no entry, the entries whose row is gone, and the entries that fail to
cover their geometry's true envelope.

An index can pass the first check and fail the second. That happens in a file
where rows were written while the triggers were absent, or that another tool
populated incompletely, and it is not a state this library's own writes can
produce. The two questions have separate remedies for the same reason: repair
puts the structure right and rebuilds the content, and is a no-op when the
structure is already current; rebuild does the content work unconditionally.

The four structural states are worth naming, because they are what a file
arrives in. **Absent** means neither the virtual table nor any trigger is
there. **Current** means the virtual table exists with the GeoPackage 1.4
trigger set. **Legacy** means a pre-1.4 or mixed trigger set. **Stale** means
the two halves disagree: a virtual table with no triggers, or triggers with no
table.

Legacy is not merely untidy. The pre-1.4 `update1` trigger corrupts the index
under `UPSERT`, which is why 1.4 renamed the fixed triggers, and why the
repaired state is detectable by name at all. QGIS tracked the same failure in
[QGIS#36935](https://github.com/qgis/QGIS/issues/36935). A mixed set, where
some triggers are one generation and some another, is the worst version of it,
and is what you get from a file two tools have edited in turn.

Neither Legacy nor Stale is silently believed here. `has_spatial_index`
reports both as unusable, so a bounding-box query falls back to a correct full
scan rather than returning whatever the suspect index says.

## Why repair is never automatic, when extent recording is

This library does write during a read in one place: reading a layer's extent
records the measurement when the recorded bounds are unusable. It does not do
the equivalent for an index, and the inconsistency is deliberate rather than
accidental.

The difference is what the two operations cost, and what their absence does.
Rebuilding an index reads every geometry in the layer and rewrites the tree.
That is too expensive to run as the side effect of a question, and a caller
who asked what state an index was in has not consented to it. Meanwhile the
absence of a usable index is not silently wrong: queries fall back to a scan
and return the correct rows, more slowly.

An extent is the other way round on both counts. Measuring it is comparatively
cheap, and a wrong one is believed indefinitely by every reader that opens the
file. See [The layer extent](extent.md) for that side of the comparison.

There is a second argument for keeping repair explicit, which is that
repairing a legacy trigger set rewrites triggers in a file the caller may only
have opened to look at. Doing that unasked would change files on disk as a
consequence of inspecting them.

## Populating an index

Once the virtual table and the triggers exist, the index has to be filled from
the rows already in the table, and there are two ways to do it.

Below a threshold, currently 10,000 candidate rows, population is a single
`INSERT INTO rtree SELECT` over the existing rows, driven by the registered
`ST_*` functions and skipping empty and NULL geometries exactly as the
triggers do. It is simple, it goes through SQLite's own RTree module, and for
a small table it is the cheaper option.

Above that threshold it is the bulk build, which is a longer story.

## The bulk build, and why it exists at all

SQLite's RTree module has no bulk-load entry point. Its only write path is the
per-row virtual-table update, so every insert descends the tree and may split
nodes, and populating an index one row at a time pays that cost for every row.
This is not a peculiar observation: GDAL reached the same conclusion in
[gdal#7614](https://github.com/OSGeo/gdal/issues/7614).

The first attempt here was to build the RTree in an `ATTACH`ed scratch
database and copy its shadow tables into the target. Measurement showed the
scratch build had simply become the dominant cost, because it still inserted
every entry through the module one row at a time. The work had moved rather
than gone away.

So the tree is now built outright. The `rtree_%_node`, `_rowid` and `_parent`
contents are laid out in memory from the accumulated entry set, and written as
ordinary rows. No RTree module logic, no `ST_*` calls, no triggers, and no
scratch database. This is possible because the GeoPackage RTree is always
`rtree(id, minx, maxx, miny, maxy)`, two-dimensional with no auxiliary
columns, so the three shadow tables have a fixed shape whatever the index is
named.

Dropping the scratch database had a second effect that matters more than the
speed. `ATTACH` requires autocommit, which had forced the build out of the
caller's transaction and left a window in which a crash committed the rows but
not the index. The whole build is now one transaction: dropping the triggers,
every row insert, the `gpkg_contents` flush, the index work and reinstalling
the triggers all commit together, so the rows can never be committed against
an index that was not brought up to date with them.

The entry set itself comes either from a scan of the table through the `ST_*`
functions or, when the caller can account for every indexable row, from the
envelopes computed while encoding the geometries. That second source is why
building the index during a write costs between 20% and 47% less than building
it afterwards, and why creating a layer leaves an empty index in place for the
first bulk write to fill.

## The gate, and what it costs

Writing an RTree by hand means writing into an on-disk format that SQLite does
not document as an interface. So the result is checked before anything relies
on it.

A gated build must contain exactly the accumulated `(fid, envelope)` set, by
row count and by a per-row containment check, and must pass a structural check:
`rtreecheck` on the index, or optionally a whole-database
`PRAGMA integrity_check`. Any anomaly discards the built index and falls back
to the triggered population, so a failed gate costs time rather than
correctness.

The cost is substantial and worth quoting precisely. Profiled over one million
points, the gate is about 45% of the build: roughly 745 ms of a 1593 ms build,
split about evenly between the bijection scan, which reads every entry back
out of the index, and `rtreecheck`, which walks the whole tree. GDAL's builder
runs no equivalent, and with the gate on by default this library is level with
it rather than comfortably ahead.

## Why verification became opt-in

Through version 0.5 the gate ran on every bulk build. From 0.6 the default is
to run none of it, and the reasoning is about the age of the code rather than
about the speed.

Paying 45% for insurance is the right price while the packer is new, because
the failure it guards against is a structurally malformed tree that no
ordinary test would notice until a query returned the wrong rows. It stops
being the right price once the packer has enough history behind it for that
failure to be unlikely, at which point the cost is better paid by the
callers who want it than by everyone.

There is a consequence worth stating plainly: with verification off, nothing
is read back, so a build cannot fail its check and cannot fall back to the
triggered path. The automatic fallback exists only where a check does. A
caller writing files of consequence, or bisecting a suspected index problem,
turns verification back on and gets both.

Removing the gate entirely is a separate question, deferred to 1.0. Until
then it remains available at three levels: contents only, contents plus
`rtreecheck`, and both plus a whole-database integrity check.

## Node fill

One more knob is worth understanding rather than looking up. The bulk build
packs each RTree node full by default, which gives the smallest tree, the
shallowest descent and the best queries, and suits a bulk load.

The cost is that a full node has no room for a later insert, so the first
append into it splits immediately. Appends after a bulk load go through the
triggers, which is the per-row path the bulk build exists to avoid. The
default therefore favours the load and the queries; a lower fill factor
favours a freshly built index that is about to be appended to heavily.

## Getting the work done

- [How to add a spatial index to an existing layer](../how-to/add-spatial-index.md)
- [How to repair a file's spatial indexes](../how-to/repair-spatial-indexes.md)
