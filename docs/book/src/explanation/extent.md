# The layer extent

Every row of `gpkg_contents` has four bounding-box columns: `min_x`, `min_y`,
`max_x` and `max_y`. Much software reads them as the layer's extent, but the
standard does not require them to be accurate.

## What the spec says

The relevant text is prose, not a numbered requirement, and it is identical
in every version from 1.2.0 to 1.4.0. The bounding box "provides an
informative bounding box of the content". Applications "may use this bounding
box as the extents of a default view". And "there are no requirements that
this bounding box be exact or represent the minimum bounding box of the
content". The columns are nullable, and no requirement governs their values.

The GeoPackage SWG considered adding a test that features fall inside the
recorded extent, and
[declined in 2018](https://github.com/opengeospatial/geopackage/issues/443),
because maintaining that invariant on every insert and delete is too
expensive to require of a writer.

A conformant file can therefore record an extent that is stale, larger than
the data, or that does not cover all of it.

## No query uses the recorded extent

No read path in this library consults the recorded extent. `features_in` and
`cursor_in` go through the RTree index, or through a full scan when there is
none, and re-filter every candidate against its exact `f64` envelope.

In particular, a query whose box misses the recorded extent still runs. Skipping
it would be correct only when the recorded extent covers the layer. That holds
for files this library wrote, but not in general, and on other files the query
would return no rows when matching rows exist.

## NULL and incorrect extents

GDAL checks only that the four values are present and that `min <= max` on
each axis. It returns a well-ordered extent as stored and does not recompute
it, even when the caller passes `bForce`. A NULL extent makes GDAL compute the
actual one. QGIS behaves the same way, and since 3.34 its "Update Extents"
action does nothing for a local GeoPackage, so a user cannot correct a stored
extent there without a full edit session.

A NULL extent therefore costs a reader one scan, after which GDAL can store
the computed value. An incorrect extent is used as stored, and is hard for a
user to correct.

This library records an extent only when it is guaranteed to cover the layer,
and otherwise leaves it NULL, which the spec permits. Four rules follow.

The writer grows the recorded box to cover what it writes, and never shrinks
it. The result is exact or larger than the data, which the spec permits. A
delete leaves the box unchanged and does not trigger a rescan.

When the stored extent is absent, NULL or inverted but the table already
contains rows, the writer leaves it unchanged. A box covering only the rows
this writer wrote would exclude every existing row.

A layer with no geometry to measure gets NULL. GDAL does the same, through
`UpdateContentsToNullExtent`.

A measurement is recorded only when the whole layer could be measured inside
one transaction. If another writer changes the layer first, the measurement
is returned to the caller but not recorded, and this is not an error.

## Reading an extent can write one

`Layer::extent` records the extent it measures. If the stored extent is
unusable, calling it changes the file's contents and modification time.

This is the only read operation in the library that writes. Later readers, in
this library or elsewhere, then find a usable extent without scanning. GDAL
does the same, storing a computed extent through `SaveExtent` on any dataset
open for update.

A pipeline that checksums its inputs will see them change. To avoid the
write, open the file read-only, which still returns the measurement, or read
`GeoPackage::contents`, which reports the stored values without measuring
anything.

## Compared with index repair

The library never repairs a spatial index unless asked, but it does record an
extent when reading finds the stored one unusable. Two differences account
for this.

Rebuilding an index reads every geometry in the layer and rewrites the tree,
which is too expensive to do as a side effect of a query. And a stale or
older-generation index is not used: `has_spatial_index` reports it as
unusable, and queries fall back to a full scan, so results stay correct. An
extent is cheaper to measure, and GDAL and QGIS use an incorrect one without
checking it. See [The spatial index](spatial-index.md) for the index side.

## One difference from GDAL

When GDAL computes an extent, it uses the RTree if there is one. That is
faster, but the result is rounded outward to `f32`, and it is only as accurate
as the index.

This library always reads the geometries, so the stored extent is exact and
does not depend on the index being up to date.
