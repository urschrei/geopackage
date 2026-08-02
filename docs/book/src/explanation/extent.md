# The layer extent, and why it cannot be trusted

Every row of `gpkg_contents` has four bounding-box columns: `min_x`, `min_y`,
`max_x` and `max_y`. They look like the layer's extent, and a great deal of
software treats them as though they were. They are the least reliable values
in a GeoPackage, and that is by the standard's design rather than by anyone's
neglect.

## What the spec actually says

The relevant text is prose rather than a numbered requirement, and it is
identical in every version from 1.2.0 to 1.4.0. The bounding box "provides an
informative bounding box of the content"; applications "may use this bounding
box as the extents of a default view"; and "there are no requirements that
this bounding box be exact or represent the minimum bounding box of the
content". The columns are nullable, and no requirement governs their values.

That is not an oversight. The GeoPackage SWG considered adding a test that
features fall inside the recorded extent, and
[declined in 2018](https://github.com/opengeospatial/geopackage/issues/443),
on the grounds that maintaining such an invariant on every insert and delete
is too expensive to demand of a writer.

The consequence for anyone reading files they did not produce is direct: a
file may record an extent that is stale, inflated, or simply wrong, and remain
perfectly conformant.

## Nothing here answers a query from it

No read path in this library consults the recorded extent. `features_in` and
`cursor_in` go through the RTree index, or through a full scan when there is
none, and re-filter every candidate against its exact `f64` envelope. The
recorded bounds are never used to short-circuit anything.

There is an obvious optimisation available: if the query box misses the
recorded extent, skip the scan and return nothing. It is worth being explicit
about why that optimisation stays unavailable. It would be correct against
every file this library wrote, and wrong against files it did not, and the
wrongness would take the form of a query silently returning no rows on a file
that contains matching ones. A read path whose correctness depends on the
provenance of its input is not a read path worth having.

## Why a wrong extent is worse than an absent one

The columns being nullable matters more than it first appears, because readers
treat the two cases very differently.

GDAL validates only that the four values are present and that `min <= max` per
axis. A well-ordered extent that happens to be wrong is returned verbatim and
never recomputed, even when the caller passes `bForce`. A NULL extent, by
contrast, makes GDAL compute the true one. QGIS behaves the same way, and
since 3.34 its "Update Extents" action is a no-op for a local GeoPackage, so a
user who receives a file with a wrong extent has no way to correct it short of
a full edit session.

So the two failure modes are not symmetric. An absent extent costs a reader
one scan and then repairs itself. A wrong extent is believed indefinitely, by
every reader, and the people best placed to notice have no button to press.

The rule this library follows is therefore: never record an extent that cannot
be guaranteed to cover the layer. NULL is spec-legal, accurate, and
self-repairing at the reader; a wrong box is none of those things. Four things
follow from it.

The writer grows the recorded box to cover what it writes, and never shrinks
it. The result is exact or an over-estimate, which the spec expressly permits,
and a delete leaves the box alone rather than triggering a rescan.

A writer that cannot rely on its starting point leaves the extent alone rather
than replacing it with a box covering only the rows it wrote. That is the case
where the stored extent is absent, NULL or inverted while the table already
contains rows: a fold over what this writer wrote is a lower bound, not the
truth, and recording it would replace an accurate "unknown" with a box that
excludes every pre-existing row.

A layer with no geometry to measure gets NULL rather than anything invented.
GDAL does the same, through `UpdateContentsToNullExtent`.

And a measurement is recorded only when the whole layer could be measured
inside one transaction. Losing that race to another writer means the
measurement describes a layer that changed underneath it, which is exactly the
value the rule forbids recording. Lock contention is therefore not treated as
an error here: the file keeps what it had, and the measurement is returned to
the caller anyway.

## Reading an extent can write one

`Layer::extent` records what it had to measure, which means that reading the
extent of a file whose recorded extent is unusable modifies that file. Its
content and its modification time both change.

This is deliberate, and it is the one place where this library writes during
what reads like a read. The point is that the file improves by being read: the
next reader, here or anywhere else, finds a usable box and no longer has to
scan for it. GDAL behaves the same way, persisting a computed extent through
`SaveExtent` on any dataset open for update.

The cost falls on a particular kind of caller: a pipeline that checksums its
inputs will see them change. There are two ways to avoid it, and they are
cheap. Open the file read-only, which suppresses the write entirely and still
returns the measurement. Or read `GeoPackage::contents`, which reports the
recorded values without measuring or writing anything.

## The asymmetry with index repair

Set beside this, spatial-index repair never happens unless it is requested,
and the difference is worth stating because the two look like the same
question.

The two operations differ in what they cost and in what their absence does.
Rebuilding an index reads every geometry in the layer and rewrites the tree,
which is too expensive to run as the side effect of a question. And an index
that is stale or of an older generation is not silently believed:
`has_spatial_index` reports it as unusable, and queries fall back to a correct
full scan. An extent is neither of those things. Measuring one is comparatively
cheap, and a wrong one is believed indefinitely by every reader that opens the
file. See
[The spatial index](spatial-index.md) for the other half of that comparison.

## One deviation from GDAL

GDAL prefers the RTree when it has to compute an extent. That is faster, and
it yields a box rounded outward to `f32` while taking the index's word for
what the layer contains.

This library always measures the geometries. A value that is about to be
written into a file and then believed indefinitely is worth measuring exactly,
and measuring removes any dependence on the index being current, which is a
dependence that would be awkward given how many files arrive with indexes that
are not.
