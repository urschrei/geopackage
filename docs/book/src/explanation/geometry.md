# Geometry storage: GPB, WKB, and curve types

A GeoPackage stores each geometry as a blob in an ordinary SQLite column, in
an encoding the spec calls GeoPackage Binary, or GPB. A GPB blob is a small
header followed by a body in ISO Well-Known Binary (WKB). Most of how this
library reads and writes geometry follows from the layout of that header, and
from one thing WKB can express that the Rust geometry traits cannot.

## The blob

The header is eight bytes: the magic `GP`, a version byte, a flags byte, and a
32-bit `srs_id`. The flags byte records the byte order of the header and
envelope, an envelope indicator, an empty-geometry flag, and an extended-GPB
flag. After the header comes an optional envelope of four, six or eight
doubles depending on the indicator, and after that the WKB body, which
declares its own byte order and includes its own type code.

The envelope is optional in the format, but this library always writes one:
XY, or XYZ when the geometry has a Z. It does this for write throughput. The
spec's RTree triggers call the `ST_MinX` family of functions four times per
row. These functions can answer from a header envelope in constant time, and
otherwise have to traverse the WKB body. Writing 32 extra bytes per row is
cheap compared with traversing every point of every geometry on every insert.

For points the trade looks unfavourable, because a point's envelope is larger
than the point. This library writes one for points anyway, because uniformity
is worth more than the saving. GDAL omits envelopes for points, and that is
the practical reason a traversal fallback has to exist at all. Blobs without
envelopes are common in files this library did not write, and they still need
bounds computed for them.

## Reading without materialising

A geometry read from a row is a lazy wrapper, which pairs the parsed header
with the WKB body that follows it. The body is read through the georust `wkb`
crate, and the wrapper implements `geo_traits::GeometryTrait` by delegating to
it.

This means you can traverse coordinates without ever creating an owned
geometry object. An algorithm written against `geo_traits` reads coordinates
directly from the stored bytes. The write path accepts any
`impl GeometryTrait<T = f64>`, so you can stream a geometry out of one file,
measure it, and write it into another without converting it to a `geo-types`
value in either direction. Conversion to `geo-types` is available behind a
default feature if you want it. You choose when to convert, and the read path
does not do it for you.

Two things still allocate: each row's blob, which is copied out of SQLite, and
the new blob the writer serialises. An algorithm that produces new geometry
also allocates its output. What you avoid is the intermediate object, not
every allocation.

Parsing arbitrary bytes never panics. A malformed header, a truncated body, or
a type the reader cannot handle all produce a typed error.

## Bytes in, bytes out

Writes normally go through the trait, but you can also pass WKB bytes to the
writer directly. They are copied into the new blob rather than re-serialised,
so a body read out of one GeoPackage passes into another unchanged, with only
the header rebuilt around it. The rest of this chapter explains why this path
exists.

## What the traits cannot represent

The GeoPackage geometry vocabulary in Annex G includes five non-linear types:
`CIRCULARSTRING`, `COMPOUNDCURVE`, `CURVEPOLYGON`, `MULTICURVE` and
`MULTISURFACE`. Their defining feature is the circular arc, which is given by
three points. The first and last are the arc's ends, and the middle one fixes
the circle the arc lies on.

`geo-traits`, the interface this library uses to read and write geometry, has
no representation for an arc, and neither does `geo-types`. This is not an
oversight in either. An arc is not a sequence of coordinates, and a library
built on sequences of coordinates has nowhere to put one. Densifying an arc
into a linestring to approximate it would be a lossy transformation that no
caller requested.

This library therefore handles the non-linear types as bytes. You can write
them, index them and query them by extent, and their WKB is returned as
stored. Only the call that would return a geometry object fails on them,
because there is no object to return. For this reason, anything that moves
geometry between files should use the byte path. Reading into a `geo-types`
value and writing it back loses the curve types entirely, and also parses
every coordinate for no purpose.

## Why an arc's envelope has to be exact

Indexing and querying a curve by extent requires an envelope, and that
envelope has to be computed from the arc rather than from the three points
that define it.

A circular arc bulges away from the chord joining its endpoints, and may bulge
past its middle control point too. The bounding box of the three control
points can therefore be strictly smaller than the arc's own. An envelope that
is too small is a correctness problem, not a tuning or quality question. The
GPB header envelope and the RTree entry are both derived from it, and a reader
that trusts either will silently drop features that do intersect the query
window.

The exact box can be computed in closed form. The arc lies on a circle through
the three points, so its extremes in x and y are either the endpoints or the
points of that circle furthest along each axis. The chord between the
endpoints decides which of those four candidates the arc actually reaches.
This is the approach PostGIS takes in `lw_arc_calculate_gbox_cartesian_2d`.
GDAL computes the same box by a different route, from swept angles, in
`OGRCircularString::ExtendEnvelopeWithCircular`.

A degenerate case explains why the arithmetic uses a tolerance. As the three
points approach collinearity, cancellation error dominates the circumcentre
and the radius diverges. In that limit the arc is treated as its chord. This
is correct, not a fudge. As the points straighten, the bulge past them falls
to zero, so the error the tolerance admits shrinks with it.

## Arcs are planar by definition

An arc is a circle in the coordinate space of the layer's CRS, including when
that CRS is geographic. Three longitude and latitude points define a circle in
degree space, not a small circle on the ellipsoid.

This is not peculiar to this library. PostGIS and GDAL both read arcs the
same way, and the GeoPackage RTree indexes the same degree space. A geodesic
reading would produce envelopes that disagreed with the index they were stored
in. The envelope computation therefore uses no geodesic arithmetic. Nor does it
involve a coordinate transformation, which this library never performs.

## Tight envelopes, and where slack is added

The computed box is the minimum bounding box, with no outward margin. Slack is
added later, in the index, where it can be reasoned about. RTree columns are
32-bit floats, so each bound is rounded outward when it is narrowed. A stored
cell therefore always contains the `f64` box it was derived from.
Annex F.3 of the spec also tells clients to expand query windows to absorb
exactly that rounding.

Keeping the geometry-level envelope tight and the index-level entry loose puts
the widening in one place, where it is a property of the index rather than of
the data.

## Two envelope walkers

As a result, the codebase has two envelope computations rather than one. The
ordinary path traverses a geometry through the `wkb` reader, which cannot read
the non-linear types. The curve path walks the WKB structure itself, byte by
byte. It handles every Annex G type and every XY, XYZ, XYM and XYZM variant,
so it yields an envelope for a curve body without waiting on upstream support.

The curve walker also has some defensive properties. It skips non-finite
coordinates, so an empty point, which by convention has NaN coordinates,
yields no envelope rather than a NaN-valued box. It caps nesting at 32
containers deep, so a hostile body cannot drive the recursion into a stack
overflow. It allocates nothing, so a bad element count in a curve body fails
immediately instead of reserving memory for it. The reader used for the linear
types does not behave this way.

The split creates one error case of its own: a body whose own type is a core
type but which contains a non-linear member, such as a `GEOMETRYCOLLECTION`
containing a `CIRCULARSTRING`. The body's own type code decides which walker
it gets, so such a container reaches the reader that cannot read the member.
This is reported as a distinct error rather than as a malformed body, because
the body is not malformed, and otherwise no caller could tell the two apart.

## Getting the work done

- [How to copy features between files without decoding geometry](../how-to/copy-features.md)
