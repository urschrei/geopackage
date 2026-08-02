# Geometry storage: GPB, WKB, and curve types

A geometry in a GeoPackage is a blob in an ordinary SQLite column, in an
encoding the spec calls GeoPackage Binary, or GPB. It is a small header
followed by a body in ISO Well-Known Binary. Almost everything interesting
about reading and writing geometry here follows from the shape of that header
and from one thing WKB can express that the Rust geometry traits cannot.

## The blob

The header is eight bytes: the magic `GP`, a version byte, a flags byte, and a
32-bit `srs_id`. The flags byte records the byte order of the header and
envelope, an envelope indicator, an empty-geometry flag, and an extended-GPB
flag. After the header comes an optional envelope of four, six or eight
doubles depending on the indicator, and after that the WKB body, which
declares its own byte order and includes its own type code.

The envelope is optional in the format, and this library always writes one:
XY, or XYZ when the geometry has a Z. That is a write-throughput decision
rather than a nicety. The `ST_MinX` family of functions, which the spec's RTree
triggers call four times per row, can answer from a header envelope in
constant time and otherwise have to traverse the WKB body. Writing 32 extra
bytes per row is cheap against traversing every point of every geometry on
every insert.

Points are the case where that trade looks unfavourable, since a point's
envelope is larger than the point. They get an envelope here anyway, on the
grounds that uniformity is worth more than the saving. GDAL omits envelopes
for points, which is the concrete reason a traversal fallback has to exist at
all: envelope-less blobs are common in files this library did not write, and
they still need bounds computed for them.

## Reading without materialising

A geometry read from a row is a lazy wrapper: the parsed header paired with
the WKB body that follows it. The body is read through the georust `wkb`
crate, and the wrapper implements `geo_traits::GeometryTrait` by delegating to
it.

The point of that arrangement is that a caller can traverse coordinates
without an owned geometry object ever existing. An algorithm written against
`geo_traits` reads coordinates directly out of the stored bytes, and the write
path accepts any `impl GeometryTrait<T = f64>`, so a geometry can be streamed
out of one file, measured, and written into another without being converted to
a `geo-types` value in either direction. Conversion to `geo-types` is
available behind a default feature for callers who want it, and it is a
conversion they choose rather than one the read path performs on their behalf.

What still allocates, and it is worth being clear about this, is each row's
blob, copied out of SQLite, and the new blob the writer serialises. An
algorithm that produces new geometry allocates its output too. The saving is
the intermediate object, not all of them.

Parsing arbitrary bytes never panics: a malformed header, a truncated body, or
a type the reader cannot handle all produce a typed error. There is a
known limitation next door to that guarantee, which is that the `wkb` reader
pre-allocates from element counts read out of the blob without bounding them
against the buffer. A malformed geometry declaring a `0xFFFFFFFF`-member
collection therefore drives a very large allocation. The fix belongs upstream,
and until it lands, parsing GeoPackage files from untrusted sources deserves
caution.

## Bytes in, bytes out

Writes normally go through the trait, but WKB bytes can also be handed to the
writer directly, and are copied into the new blob rather than re-serialised. A
body read out of one GeoPackage passes into another unchanged, with only the
header rebuilt around it.

That path exists for a specific reason, and it is the subject of the rest of
this chapter.

## What the traits cannot represent

The GeoPackage geometry vocabulary in Annex G includes five non-linear types:
`CIRCULARSTRING`, `COMPOUNDCURVE`, `CURVEPOLYGON`, `MULTICURVE` and
`MULTISURFACE`. Their defining feature is the circular arc: three points, of
which the first and last are the ends and the middle one fixes the circle the
arc lies on.

`geo-traits`, the interface this library reads and writes geometry through,
has no representation for an arc. Nor does `geo-types`. This is not an
oversight in either: an arc is not a sequence of coordinates, and a library
built on sequences of coordinates has nowhere to put it. Approximating one by
densifying it into a linestring would be a lossy transformation that no caller
requested.

So the non-linear types are handled as bytes here. They can be written,
indexed and queried by extent, and their WKB is returned as stored. The one
call that fails on them is the one that would return a geometry object,
because there is no object to return. Anything that moves geometry between
files should use the byte path for this reason: reading into a `geo-types`
value and writing it back loses the curve types entirely, as well as parsing
every coordinate for no purpose.

## Why an arc's envelope has to be exact

Indexing and querying a curve by extent requires an envelope, and that
envelope has to be computed from the arc rather than from the three points
that define it.

A circular arc bulges away from the chord joining its endpoints, and may bulge
past its middle control point too. The bounding box of the three control
points can therefore be strictly smaller than the arc's own. An envelope that
is too small is not a tuning problem or a quality-of-results question: the GPB
header envelope and the RTree entry are both derived from it, and a reader
that trusts either will silently drop features that do intersect the query
window.

The exact box is computable in closed form. The arc lies on a circle through
the three points, so its extremes in x and y are either the endpoints or the
points of that circle furthest along each axis, and the chord between the
endpoints decides which of those four candidates the arc actually reaches.
This is the approach PostGIS takes in `lw_arc_calculate_gbox_cartesian_2d`.
GDAL computes the same box by a different route, from swept angles, in
`OGRCircularString::ExtendEnvelopeWithCircular`.

There is a degenerate case at the end of that reasoning worth noting, because
it explains why a tolerance appears in the arithmetic. As three points
approach collinearity, the circumcentre is dominated by cancellation error and
the radius diverges. Treating the arc as its chord in that limit is the
correct answer rather than a fudge: as the points straighten, the bulge past
them falls to zero, so the error admitted by the tolerance shrinks along with
it.

## Arcs are planar by definition

An arc is a circle in the coordinate space of the layer's CRS, including when
that CRS is geographic. Three longitude and latitude points define a circle in
degree space, not a small circle on the ellipsoid.

That is not this library's interpretation. PostGIS and GDAL both read arcs the
same way, and the GeoPackage RTree indexes the same degree space, so a
geodesic reading would produce envelopes that disagreed with the index they
were stored in. No geodesic arithmetic enters the envelope computation as a
result, and none of it is a coordinate transformation, which this library
never performs.

## Tightness, and where slack is added instead

The computed box is the minimum bounding box, with no outward margin. Slack is
added later and elsewhere, where it can be reasoned about: RTree columns are
32-bit floats, so each bound is rounded outward when it is narrowed, and a
stored cell therefore always contains the `f64` box it was derived from.
Annex F.3 of the spec also tells clients to expand query windows to absorb
exactly that rounding.

Keeping the geometry-level envelope tight and the index-level entry loose puts
the widening in one place, where it is a property of the index rather than of
the data.

## A second walker

One structural consequence of all this is that there are two envelope
computations in the codebase rather than one. The ordinary path traverses a
geometry through the `wkb` reader, which cannot read the non-linear types. The
curve path walks the WKB structure itself, byte by byte, handling every Annex
G type and every XY, XYZ, XYM and XYZM variant, and yielding an envelope for a
curve body without waiting on upstream support.

The walker is also where a few defensive properties live. Non-finite
coordinates are skipped, so the NaN convention for an empty point yields no
envelope rather than a NaN-valued box. Nesting is capped at 32 containers deep
so that a hostile body cannot drive the recursion into a stack overflow. And
because it allocates nothing, a bad element count in a curve body fails
immediately rather than reserving for it, which is not true of the reader used
for the linear types.

One error case exists only because of the split between the two: a body whose
own type is a core one but which contains a non-linear member, such as a
`GEOMETRYCOLLECTION` containing a `CIRCULARSTRING`. Which walker a body gets
is decided from its own type code, so such a container reaches the reader that
cannot read the member. It is reported as its own case rather than as a
malformed body, because the body is not malformed and no caller could draw the
distinction otherwise.

## Getting the work done

- [How to copy features between files without decoding geometry](../how-to/copy-features.md)
