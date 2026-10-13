# The layer extent, and why it cannot be trusted

Every row of `gpkg_contents` has four bounding-box columns: `min_x`, `min_y`,
`max_x` and `max_y`. They look like the layer's extent, and a lot of software
treats them as the extent. They are the least reliable values in a
GeoPackage, by the standard's design rather than through neglect.

## What the spec says

The relevant text is prose, not a numbered requirement, and it is identical
in every version from 1.2.0 to 1.4.0. The bounding box "provides an
informative bounding box of the content". Applications "may use this bounding
box as the extents of a default view". And "there are no requirements that
this bounding box be exact or represent the minimum bounding box of the
content". The columns are nullable, and no requirement governs their values.

This is intentional. The GeoPackage SWG considered adding a test that
features fall inside the recorded extent, and
[declined in 2018](https://github.com/opengeospatial/geopackage/issues/443).
Maintaining that invariant on every insert and delete is too expensive to
demand of a writer.

So if you read files you did not produce, a file can record an extent that is
stale, inflated or simply wrong, and still be conformant.

## No query uses the recorded extent

No read path in this library consults the recorded extent. `features_in` and
`cursor_in` go through the RTree index, or through a full scan when there is
none, and re-filter every candidate against its exact `f64` envelope. The
recorded bounds never short-circuit anything.

An obvious optimisation would be to skip the scan and return nothing when the
query box misses the recorded extent. The library does not do this. It would
be correct for every file this library wrote and wrong for files it did not.
On those files, a query would silently return no rows even though matching
rows exist. A read path's correctness should not depend on which tool wrote the
file.

## Why a wrong extent is worse than an absent one

Readers treat a NULL extent and a wrong extent very differently, so the
nullable columns matter.

GDAL validates only that the four values are present and that `min <= max` per
axis. It returns a well-ordered but wrong extent verbatim and never
recomputes it, even when the caller passes `bForce`. A NULL extent, by
contrast, makes GDAL compute the true one. QGIS behaves the same way. Since
3.34 its "Update Extents" action is a no-op for a local GeoPackage, so a user
who receives a file with a wrong extent cannot correct it without a full edit
session.

An absent extent costs a reader one scan, and then the reader fixes it. Every
reader believes a wrong extent indefinitely, and the users most likely to
notice have no simple way to fix it.

So this library follows one rule: never record an extent that is not
guaranteed to cover the layer. NULL is spec-legal and accurate, and readers
repair it. A wrong box is none of those things. Four things follow from the
rule.

The writer grows the recorded box to cover what it writes, and never shrinks
it. The result is exact or an over-estimate, which the spec expressly permits.
A delete leaves the box alone and does not trigger a rescan.

A writer that cannot rely on its starting point leaves the extent alone. It
does not replace it with a box covering only the rows it wrote. This is the
case where the stored extent is absent, NULL or inverted while the table
already contains rows. A fold over what this writer wrote is a lower bound,
not the true extent. Recording it would replace an accurate "unknown" with a
box that excludes every pre-existing row.

A layer with no geometry to measure gets NULL, not an invented value. GDAL
does the same, through `UpdateContentsToNullExtent`.

A measurement is recorded only when the whole layer could be measured inside
one transaction. If it loses that race to another writer, the measurement
describes a layer that changed underneath it, and the rule forbids recording
that.
So lock contention is not an error here. The file keeps what it had, and the
caller still gets the measurement.

## Reading an extent can write one

`Layer::extent` records what it had to measure. If a file's recorded extent is
unusable, reading the extent modifies the file. Both its content and its
modification time change.

This is deliberate, and it is the only place where this library writes during
what looks like a read. Later readers, in this library or elsewhere, find a
usable box and do not have to scan for it. GDAL does the same, persisting a
computed extent through `SaveExtent` on any dataset open for update.

The cost falls on some callers. For example, a pipeline that checksums its
inputs will see them change. There are two cheap ways to avoid this. Open the
file read-only, which suppresses the write and still returns the measurement.
Or read `GeoPackage::contents`, which reports the recorded values without
measuring or writing anything.

## Compared with index repair

Spatial-index repair, by contrast, never happens unless you request it. The
two can look like the same question, but they differ in cost and in what
happens when they are missing.

Rebuilding an index reads every geometry in the layer and rewrites the tree.
That is too expensive to run as the side effect of a question. And a stale or
older-generation index is not silently believed: `has_spatial_index` reports
it as unusable, and queries fall back to a correct full scan. An extent is the
opposite on both counts. Measuring one is comparatively cheap, and every
reader that opens the file believes a wrong one indefinitely. See
[The spatial index](spatial-index.md) for the other half of that comparison.

## One deviation from GDAL

GDAL prefers the RTree when it has to compute an extent. That is faster, but
it yields a box rounded outward to `f32`, and it trusts the index about what
the layer contains.

This library always measures the geometries. The value will be written into
the file and believed indefinitely, so it is worth measuring exactly.
Measuring also removes any dependence on the index being current, and many
files arrive with indexes that are not.
