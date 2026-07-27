//! XY envelopes computed directly from ISO WKB bytes, curve types included.
//!
//! [`crate::geometry::write_envelope`] derives an envelope by traversing a
//! geometry through the georust `wkb` reader, which cannot read the non-linear
//! types (`CIRCULARSTRING`, `COMPOUNDCURVE`, `CURVEPOLYGON`, `MULTICURVE`,
//! `MULTISURFACE`). This module walks the WKB structure itself instead, so a
//! curve body yields an envelope without waiting on upstream support.
//!
//! # Why the control points are not enough
//!
//! A circular arc bulges away from the chord joining its endpoints, and may
//! bulge past its middle control point too, so the bounding box of the three
//! points that define an arc can be smaller than the arc's own. An envelope
//! that is too small is not a tuning problem: the GPB header envelope and the
//! rtree entry are both derived from it, and a reader that trusts either will
//! silently drop features that do intersect the query window.
//!
//! [`arc_envelope`] therefore computes the exact box. The arc lies on a circle
//! through the three points, so its extremes in x and y are either the
//! endpoints or the points of that circle furthest along each axis, and the
//! chord between the endpoints decides which of those four candidates the arc
//! actually reaches. This is the approach PostGIS takes in
//! `lw_arc_calculate_gbox_cartesian_2d`; GDAL computes the same box from
//! swept angles in `OGRCircularString::ExtendEnvelopeWithCircular`.
//!
//! # Planar by definition
//!
//! An arc is a circle in the coordinate space of the layer's CRS, including
//! when that CRS is geographic: three lon/lat points define a circle in degree
//! space, not a small circle on the ellipsoid. PostGIS and GDAL both read it
//! that way, and the GeoPackage rtree indexes the same space, so no geodesic
//! arithmetic enters here.
//!
//! # Tightness
//!
//! The returned box is the minimum bounding box, with no outward margin. The
//! rtree path widens it anyway: rtree columns are 32-bit floats, and
//! `packed.rs` rounds each bound outward when narrowing, so a stored cell
//! always contains the `f64` box. Annex F.3 of the spec also tells clients to
//! expand query windows to absorb that rounding.

use crate::geometry::{GeometryError, XyBounds, XyzBounds};
use crate::gpb;

/// Cap on nested geometry containers, so a hostile body cannot drive the
/// recursive walk into a stack overflow. Deeper than any nesting a writer
/// produces in practice.
const MAX_DEPTH: u32 = 32;

/// Relative tolerance below which three points count as collinear, applied to
/// the cross product that is twice the triangle's signed area, scaled by the
/// square of the largest coordinate offset.
///
/// Below this the circumcentre is dominated by cancellation error. Treating
/// the arc as its chord there is the correct limit: as the points straighten,
/// the radius diverges and the bulge past the control points falls to zero, so
/// the error the tolerance admits shrinks with it.
const COLLINEAR_TOLERANCE: f64 = 1e-14;

/// The XY envelope of an ISO WKB body as `[min_x, max_x, min_y, max_y]`, or
/// `None` when the body carries no finite coordinate.
///
/// Handles every GeoPackage geometry type from Annex G, linear and non-linear,
/// and any XY/XYZ/XYM/XYZM variant. Circular arcs contribute their true extent
/// rather than the extent of their control points. Nested geometries carry
/// their own byte order and are read accordingly.
///
/// Non-finite coordinates are skipped, so the NaN empty-point convention
/// yields `None` rather than a NaN-valued box, matching
/// [`crate::geometry::GpbGeometry::xy_envelope`].
///
/// The body is the WKB that follows a GPB header, not the whole blob; use
/// [`crate::gpb::body_offset`] to find it.
///
/// # Errors
///
/// [`GeometryError`] if the body is truncated, declares a type code that is
/// not a GeoPackage geometry type or is one of the abstract supertypes, is
/// EWKB rather than ISO WKB, or nests containers deeper than 32.
pub fn xy_envelope(wkb_body: &[u8]) -> Result<Option<[f64; 4]>, GeometryError> {
    Ok(walk(wkb_body)?.xy_bounds())
}

/// The GPB envelope to write for an ISO WKB body, and whether it is empty.
///
/// The write-path counterpart of [`xy_envelope`], matching
/// [`crate::geometry::write_envelope`]: [`gpb::Envelope::Xyz`] when the body
/// carries a Z dimension, otherwise [`gpb::Envelope::Xy`], and
/// [`gpb::Envelope::None`] with `true` when no coordinate is finite. An M
/// dimension never widens the envelope.
///
/// # Errors
///
/// As [`xy_envelope`].
pub fn write_envelope(wkb_body: &[u8]) -> Result<(gpb::Envelope, bool), GeometryError> {
    let bounds = walk(wkb_body)?;
    let Some([min_x, max_x, min_y, max_y]) = bounds.xy_bounds() else {
        return Ok((gpb::Envelope::None, true));
    };
    let envelope = match bounds.z_bounds() {
        Some((min_z, max_z)) => gpb::Envelope::Xyz([min_x, max_x, min_y, max_y, min_z, max_z]),
        None => gpb::Envelope::Xy([min_x, max_x, min_y, max_y]),
    };
    Ok((envelope, false))
}

/// Walk a body, accumulating the bounds of every coordinate it carries.
fn walk(wkb_body: &[u8]) -> Result<XyzBounds, GeometryError> {
    let mut cursor = Cursor::new(wkb_body);
    let mut bounds = XyzBounds::new();
    read_geometry(&mut cursor, &mut bounds, 0)?;
    Ok(bounds)
}

/// The XY envelope of the circular arc that runs from `p0` through `p1` to
/// `p2`, or `None` when no control point is finite.
///
/// The three points are `[x, y]` pairs in the layer's own coordinate space.
/// The box covers the whole arc, not just the three points: see the module
/// documentation for why that distinction matters.
///
/// Coincident endpoints (`p0 == p2`) mean the segment closes the circle, and
/// the box is the circle's. Collinear or coincident points have no circle
/// through them, and the box is the box of the points.
pub fn arc_envelope(p0: [f64; 2], p1: [f64; 2], p2: [f64; 2]) -> Option<[f64; 4]> {
    let mut bounds = XyBounds::new();
    for [x, y] in [p0, p1, p2] {
        bounds.add(x, y);
    }
    for [x, y] in arc_extremes(p0, p1, p2).into_iter().flatten() {
        bounds.add(x, y);
    }
    bounds.finish()
}

/// The points of the arc's circle furthest along each axis that the arc itself
/// reaches, in the order left, right, bottom, top.
///
/// Each is `None` when the arc stops short of that axis extreme, and all four
/// are `None` when the control points have no circle through them, in which
/// case the arc is the chord and the control points bound it on their own.
///
/// Kept separate from the control points so the walker can give a coordinate's
/// Z to the bounds while leaving these extremes without one: an extreme is an
/// XY property of the circle, and the Z there would have to be interpolated.
fn arc_extremes(p0: [f64; 2], p1: [f64; 2], p2: [f64; 2]) -> [Option<[f64; 2]>; 4] {
    let [x0, y0] = p0;
    let [x2, y2] = p2;

    let Some(([cx, cy], radius)) = arc_centre(p0, p1, p2) else {
        return [None; 4];
    };
    let candidates = [
        [cx - radius, cy],
        [cx + radius, cy],
        [cx, cy - radius],
        [cx, cy + radius],
    ];

    #[expect(
        clippy::float_cmp,
        reason = "the closed-circle convention is written as an identical point, so exact equality is the encoding being tested, not an approximation of one"
    )]
    let closes_the_circle = x0 == x2 && y0 == y2;
    if closes_the_circle {
        // Matched endpoints close the circle, so the arc reaches all four.
        return candidates.map(Some);
    }

    // The chord p0-p2 cuts the circle into two arcs, and p1 marks which one this
    // is. An extreme belongs to the arc exactly when it falls on p1's side of
    // that chord. An extreme lying on the chord itself is p0 or p2, which the
    // control points already cover, so only a matching sign adds anything.
    let interior = chord_side(p0, p2, p1);
    candidates.map(|candidate| (chord_side(p0, p2, candidate) == interior).then_some(candidate))
}

/// The centre and radius of the circle through the three points, or `None` when
/// they are collinear, coincident, or not all finite.
///
/// Matched endpoints are the closed-circle convention rather than a degenerate
/// case: `p0` and `p1` are then the ends of a diameter. That has to be settled
/// before the collinearity test, because two coincident points are collinear
/// with anything and the circumcentre formula has nothing to work with. The
/// test is exact equality, not a tolerance: a writer closing an arc emits the
/// identical point, and there is no epsilon that is meaningful in both degrees
/// and metres. Endpoints that are merely close take the general path, which
/// handles a near-complete circle correctly anyway.
fn arc_centre(p0: [f64; 2], p1: [f64; 2], p2: [f64; 2]) -> Option<([f64; 2], f64)> {
    let [x0, y0] = p0;
    let [x1, y1] = p1;
    let [x2, y2] = p2;

    #[expect(
        clippy::float_cmp,
        reason = "the closed-circle convention is written as an identical point, so exact equality is the encoding being tested, not an approximation of one"
    )]
    let closes_the_circle = x0 == x2 && y0 == y2;
    if closes_the_circle {
        let centre = [x0 + (x1 - x0) / 2.0, y0 + (y1 - y0) / 2.0];
        let radius = (x1 - x0).hypot(y1 - y0) / 2.0;
        let [centre_x, centre_y] = centre;
        if !centre_x.is_finite() || !centre_y.is_finite() || !radius.is_finite() {
            return None;
        }
        return Some((centre, radius));
    }

    // Translate so p1 is the origin. The products below then combine coordinate
    // differences rather than the coordinates themselves, which keeps the
    // cancellation bounded for the large eastings and northings a projected CRS
    // carries.
    let (ax, ay) = (x0 - x1, y0 - y1);
    let (cx, cy) = (x2 - x1, y2 - y1);

    let cross = ax * cy - ay * cx;
    let scale = ax.abs().max(ay.abs()).max(cx.abs()).max(cy.abs());
    // A non-finite coordinate poisons the cross product, so the finiteness test
    // covers those inputs as well as an overflow in the product itself.
    if !cross.is_finite() || cross.abs() <= COLLINEAR_TOLERANCE * scale * scale {
        return None;
    }

    let a2 = ax * ax + ay * ay;
    let c2 = cx * cx + cy * cy;
    let denominator = 2.0 * cross;
    let ox = (cy * a2 - ay * c2) / denominator;
    let oy = (ax * c2 - cx * a2) / denominator;
    let radius = ox.hypot(oy);
    if !ox.is_finite() || !oy.is_finite() || !radius.is_finite() {
        return None;
    }

    Some(([x1 + ox, y1 + oy], radius))
}

/// Which side of the line through `a` and `b` the point `q` falls on: `1`, `-1`,
/// or `0` when it is on the line or a coordinate is not finite.
fn chord_side(a: [f64; 2], b: [f64; 2], q: [f64; 2]) -> i8 {
    let [ax, ay] = a;
    let [bx, by] = b;
    let [qx, qy] = q;
    let cross = (bx - ax) * (qy - ay) - (by - ay) * (qx - ax);
    if cross > 0.0 {
        1
    } else if cross < 0.0 {
        -1
    } else {
        0
    }
}

/// Read one WKB geometry, adding every coordinate it carries to `bounds`.
fn read_geometry(
    cursor: &mut Cursor<'_>,
    bounds: &mut XyzBounds,
    depth: u32,
) -> Result<(), GeometryError> {
    if depth > MAX_DEPTH {
        return Err(GeometryError::NestingTooDeep);
    }

    let little_endian = cursor.read_byte_order()?;
    let code = cursor.read_u32(little_endian)?;
    let (base, coord) = decode_type(code)?;

    match base {
        // Point: one coordinate, with no count in front of it.
        1 => {
            let ([x, y], z) = cursor.read_coord(little_endian, coord)?;
            bounds.add(x, y, z);
        }
        // LineString: a bare coordinate sequence.
        2 => read_coords(cursor, little_endian, coord, bounds)?,
        // Polygon: rings are bare coordinate sequences, unlike CurvePolygon's.
        3 => {
            let rings = cursor.read_u32(little_endian)?;
            for _ in 0..rings {
                read_coords(cursor, little_endian, coord, bounds)?;
            }
        }
        // CircularString: a coordinate sequence read as overlapping arc triples
        // (0,1,2), (2,3,4) and so on.
        8 => read_circular_string(cursor, little_endian, coord, bounds)?,
        // MultiPoint, MultiLineString, MultiPolygon, GeometryCollection,
        // CompoundCurve, CurvePolygon, MultiCurve, MultiSurface: a count
        // followed by that many complete WKB geometries, each with its own
        // byte order and type code.
        4..=7 | 9..=12 => {
            let count = cursor.read_u32(little_endian)?;
            for _ in 0..count {
                read_geometry(cursor, bounds, depth + 1)?;
            }
        }
        // Geometry, Curve and Surface are abstract supertypes: a conformant
        // writer never emits one as a body.
        _ => return Err(GeometryError::AbstractWkbType(code)),
    }

    Ok(())
}

/// Read a counted coordinate sequence into `bounds`.
fn read_coords(
    cursor: &mut Cursor<'_>,
    little_endian: bool,
    coord: CoordLayout,
    bounds: &mut XyzBounds,
) -> Result<(), GeometryError> {
    let count = cursor.read_u32(little_endian)?;
    for _ in 0..count {
        let ([x, y], z) = cursor.read_coord(little_endian, coord)?;
        bounds.add(x, y, z);
    }
    Ok(())
}

/// Read a counted coordinate sequence as a chain of circular arcs, adding both
/// the points and the extent each arc sweeps.
///
/// A well-formed CircularString has an odd count of at least three. A malformed
/// one still contributes its points: the trailing pair of an even count is
/// added without an arc rather than rejected, which keeps the envelope
/// conservative instead of failing the write.
fn read_circular_string(
    cursor: &mut Cursor<'_>,
    little_endian: bool,
    coord: CoordLayout,
    bounds: &mut XyzBounds,
) -> Result<(), GeometryError> {
    let count = cursor.read_u32(little_endian)?;
    let mut start: Option<[f64; 2]> = None;
    let mut middle: Option<[f64; 2]> = None;

    for _ in 0..count {
        let (point, z) = cursor.read_coord(little_endian, coord)?;
        let [x, y] = point;
        bounds.add(x, y, z);
        match (start, middle) {
            (None, _) => start = Some(point),
            (Some(_), None) => middle = Some(point),
            (Some(from), Some(via)) => {
                // The extremes are XY properties of the circle, so they carry
                // no Z of their own.
                for [ex, ey] in arc_extremes(from, via, point).into_iter().flatten() {
                    bounds.add(ex, ey, None);
                }
                // The arc that ends here is the next arc's start.
                start = Some(point);
                middle = None;
            }
        }
    }

    Ok(())
}

/// How many `f64`s a coordinate occupies, and whether the third is a Z.
///
/// XYM shares its width with XYZ but carries no Z, so the two cannot be told
/// apart by width alone.
#[derive(Debug, Clone, Copy)]
struct CoordLayout {
    stride: usize,
    has_z: bool,
}

/// Split an ISO WKB type code into its base type and its coordinate layout.
///
/// ISO WKB adds 1000 for Z, 2000 for M and 3000 for ZM. EWKB instead sets high
/// bit flags and may prefix an SRID, which a GPB body must not do, so it is
/// rejected rather than guessed at.
fn decode_type(code: u32) -> Result<(u32, CoordLayout), GeometryError> {
    if code & 0xE000_0000 != 0 {
        return Err(GeometryError::UnknownWkbType(code));
    }
    let coord = match code / 1000 {
        0 => CoordLayout {
            stride: 2,
            has_z: false,
        },
        1 => CoordLayout {
            stride: 3,
            has_z: true,
        },
        2 => CoordLayout {
            stride: 3,
            has_z: false,
        },
        3 => CoordLayout {
            stride: 4,
            has_z: true,
        },
        _ => return Err(GeometryError::UnknownWkbType(code)),
    };
    let base = code % 1000;
    if base == 0 || base > 14 {
        return Err(GeometryError::UnknownWkbType(code));
    }
    Ok((base, coord))
}

/// A forward-only reader over a WKB body.
struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    /// Consume the one-byte order marker that opens every WKB geometry.
    fn read_byte_order(&mut self) -> Result<bool, GeometryError> {
        let offset = self.offset;
        match self.take(1)? {
            [0] => Ok(false),
            [1] => Ok(true),
            _ => Err(GeometryError::InvalidByteOrder { offset }),
        }
    }

    fn read_u32(&mut self, little_endian: bool) -> Result<u32, GeometryError> {
        let &[b0, b1, b2, b3] = self.take(4)? else {
            return Err(GeometryError::TruncatedAt {
                offset: self.offset,
            });
        };
        let bytes = [b0, b1, b2, b3];
        Ok(if little_endian {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    }

    fn read_f64(&mut self, little_endian: bool) -> Result<f64, GeometryError> {
        let &[b0, b1, b2, b3, b4, b5, b6, b7] = self.take(8)? else {
            return Err(GeometryError::TruncatedAt {
                offset: self.offset,
            });
        };
        let bytes = [b0, b1, b2, b3, b4, b5, b6, b7];
        Ok(if little_endian {
            f64::from_le_bytes(bytes)
        } else {
            f64::from_be_bytes(bytes)
        })
    }

    /// Read one coordinate, keeping X, Y and any Z, and skipping an M.
    fn read_coord(
        &mut self,
        little_endian: bool,
        coord: CoordLayout,
    ) -> Result<([f64; 2], Option<f64>), GeometryError> {
        let x = self.read_f64(little_endian)?;
        let y = self.read_f64(little_endian)?;
        let z = if coord.stride >= 3 {
            let third = self.read_f64(little_endian)?;
            coord.has_z.then_some(third)
        } else {
            None
        };
        // Anything left is an M ordinate, which never bounds the envelope.
        let skipped = coord.stride.saturating_sub(3).saturating_mul(8);
        self.take(skipped)?;
        Ok(([x, y], z))
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], GeometryError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(GeometryError::TruncatedAt {
                offset: self.offset,
            })?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or(GeometryError::TruncatedAt {
                offset: self.offset,
            })?;
        self.offset = end;
        Ok(slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::f64::consts::PI;

    /// Assert two envelopes agree to `tolerance` on every bound.
    #[track_caller]
    fn assert_envelope(actual: Option<[f64; 4]>, expected: [f64; 4], tolerance: f64) {
        let Some(actual) = actual else {
            panic!("expected an envelope, got None");
        };
        let [amin_x, amax_x, amin_y, amax_y] = actual;
        let [emin_x, emax_x, emin_y, emax_y] = expected;
        for (got, want, name) in [
            (amin_x, emin_x, "min_x"),
            (amax_x, emax_x, "max_x"),
            (amin_y, emin_y, "min_y"),
            (amax_y, emax_y, "max_y"),
        ] {
            assert!(
                (got - want).abs() <= tolerance,
                "{name}: got {got}, want {want} (tolerance {tolerance})"
            );
        }
    }

    /// The point of the circle at angle `t`.
    fn on_circle(centre: [f64; 2], radius: f64, t: f64) -> [f64; 2] {
        let [cx, cy] = centre;
        [cx + radius * t.cos(), cy + radius * t.sin()]
    }

    /// The three control points of the arc that starts at `start` and sweeps
    /// through `sweep` radians, with the middle point at the halfway angle.
    fn arc_controls(
        centre: [f64; 2],
        radius: f64,
        start: f64,
        sweep: f64,
    ) -> ([f64; 2], [f64; 2], [f64; 2]) {
        (
            on_circle(centre, radius, start),
            on_circle(centre, radius, start + sweep / 2.0),
            on_circle(centre, radius, start + sweep),
        )
    }

    /// A little-endian WKB geometry: byte order, type code, then the payload.
    fn wkb(code: u32, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![1u8];
        bytes.extend_from_slice(&code.to_le_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    /// A counted little-endian XY coordinate sequence.
    fn coords(points: &[[f64; 2]]) -> Vec<u8> {
        let count = u32::try_from(points.len()).expect("test point count fits in u32");
        let mut bytes = count.to_le_bytes().to_vec();
        for [x, y] in points {
            bytes.extend_from_slice(&x.to_le_bytes());
            bytes.extend_from_slice(&y.to_le_bytes());
        }
        bytes
    }

    /// A counted sequence of already-encoded child geometries.
    fn children(parts: &[Vec<u8>]) -> Vec<u8> {
        let count = u32::try_from(parts.len()).expect("test child count fits in u32");
        let mut bytes = count.to_le_bytes().to_vec();
        for part in parts {
            bytes.extend_from_slice(part);
        }
        bytes
    }

    // --- the arc primitive ---

    #[test]
    fn upper_semicircle_reaches_its_top() {
        // The control points alone would give a box of height 1 only because the
        // middle point happens to sit at the apex; the width comes from the ends.
        let envelope = arc_envelope([-1.0, 0.0], [0.0, 1.0], [1.0, 0.0]);
        assert_envelope(envelope, [-1.0, 1.0, 0.0, 1.0], 1e-12);
    }

    #[test]
    fn arc_bulges_past_its_control_points() {
        // A 350 degree sweep starting at 20 degrees, so the arc passes every
        // axis extreme while no control point sits on one. This is the case the
        // control-point box gets wrong on all four sides at once.
        let (p0, p1, p2) = arc_controls([0.0, 0.0], 1.0, PI / 9.0, 35.0 * PI / 18.0);
        let control_box = {
            let mut bounds = XyBounds::new();
            for [x, y] in [p0, p1, p2] {
                bounds.add(x, y);
            }
            bounds.finish().expect("control points are finite")
        };
        let envelope = arc_envelope(p0, p1, p2).expect("arc has an envelope");

        let [cmin_x, cmax_x, cmin_y, cmax_y] = control_box;
        let [amin_x, amax_x, amin_y, amax_y] = envelope;
        assert!(
            amin_x < cmin_x && amax_x > cmax_x && amin_y < cmin_y && amax_y > cmax_y,
            "arc box {envelope:?} should be strictly wider than the control box {control_box:?}"
        );
    }

    #[test]
    fn shallow_arc_reaching_no_extreme_stays_tight() {
        // 10 to 50 degrees: the arc crosses no axis, so its box is its endpoints.
        let (p0, p1, p2) = arc_controls([0.0, 0.0], 1.0, PI / 18.0, 2.0 * PI / 9.0);
        let expected = [
            (50.0_f64).to_radians().cos(),
            (10.0_f64).to_radians().cos(),
            (10.0_f64).to_radians().sin(),
            (50.0_f64).to_radians().sin(),
        ];
        assert_envelope(arc_envelope(p0, p1, p2), expected, 1e-12);
    }

    #[test]
    fn matched_endpoints_give_the_whole_circle() {
        let envelope = arc_envelope([1.0, 0.0], [-1.0, 0.0], [1.0, 0.0]);
        assert_envelope(envelope, [-1.0, 1.0, -1.0, 1.0], 1e-12);
    }

    #[test]
    fn collinear_points_bound_the_chord() {
        let envelope = arc_envelope([0.0, 0.0], [1.0, 1.0], [2.0, 2.0]);
        assert_envelope(envelope, [0.0, 2.0, 0.0, 2.0], 0.0);
    }

    #[test]
    fn coincident_points_bound_themselves() {
        let envelope = arc_envelope([5.0, 5.0], [5.0, 5.0], [5.0, 5.0]);
        assert_envelope(envelope, [5.0, 5.0, 5.0, 5.0], 0.0);
    }

    #[test]
    fn a_flat_arc_gives_up_less_than_the_rtree_rounds_away() {
        // A flat enough arc loses its apex: `cy + radius` cancels once the
        // radius dwarfs the sagitta, and further out `arc_centre` calls the
        // triple collinear. Either way the box can stop short of the true top.
        //
        // Sweeping the radius and the middle point's position together finds
        // the worst of it. Coordinates stay at chord scale while the centre
        // goes far away, so the shortfall is measured against the numbers
        // actually stored. The bound below is measured, not derived.
        let chord = 1000.0_f64;
        let half_chord = chord / 2.0;
        let mut worst_relative = 0.0_f64;

        for exponent in 4..30 {
            let radius = 10.0_f64.powi(exponent) * chord;
            // Stable forms: `radius - (radius^2 - h^2).sqrt()` cancels away
            // entirely once the radius dominates the half chord.
            let sagitta = half_chord * half_chord
                / (radius + (radius * radius - half_chord * half_chord).sqrt());

            for step in 1..100 {
                // The middle control point anywhere along the arc, including
                // close to an endpoint where it bounds almost nothing.
                let x1 = half_chord * (f64::from(step) / 50.0 - 1.0);
                let y1 = sagitta - x1 * x1 / (radius + (radius * radius - x1 * x1).sqrt());

                let p0 = [-half_chord, 0.0];
                let p1 = [x1, y1];
                let p2 = [half_chord, 0.0];
                let [_, _, _, max_y] = arc_envelope(p0, p1, p2).expect("control points are finite");

                // The apex is the top of the circle, and lies between p0 and p2.
                worst_relative = worst_relative.max((sagitta - max_y).max(0.0) / chord);
            }
        }

        // An f32 rtree bound at this coordinate scale is looser than this, so
        // the index absorbs the shortfall before it can hide a feature.
        let f32_step = f64::from(f32::EPSILON) * half_chord / chord;
        assert!(
            worst_relative < f32_step,
            "worst shortfall {worst_relative} of the coordinate scale, \
             against an f32 step of {f32_step}"
        );
    }

    #[test]
    fn nearly_collinear_arc_does_not_blow_up() {
        // A sagitta of one millimetre over a two-metre chord: the circle has a
        // radius of about 500 m, so a mishandled extreme would inflate the box
        // by hundreds of metres.
        let envelope = arc_envelope([0.0, 0.0], [1.0, 0.001], [2.0, 0.0]);
        assert_envelope(envelope, [0.0, 2.0, 0.0, 0.001], 1e-9);
    }

    #[test]
    fn non_finite_control_points_give_no_envelope() {
        let envelope = arc_envelope(
            [f64::NAN, f64::NAN],
            [f64::NAN, f64::NAN],
            [f64::NAN, f64::NAN],
        );
        assert!(envelope.is_none());
    }

    #[test]
    fn envelope_contains_a_dense_sample_of_every_arc() {
        // Sweeps up to just short of a full turn, from a range of start angles,
        // on a circle offset far from the origin so the translation in
        // `arc_centre` is exercised rather than incidentally cancelling.
        let centre = [412_345.0, 5_678_901.0];
        let radius = 1_234.5;
        for start_step in 0..12 {
            for sweep_step in 1..24 {
                let start = f64::from(start_step) * PI / 6.0;
                let sweep = f64::from(sweep_step) * (2.0 * PI - 0.05) / 24.0;
                let (p0, p1, p2) = arc_controls(centre, radius, start, sweep);
                let [min_x, max_x, min_y, max_y] =
                    arc_envelope(p0, p1, p2).expect("arc has an envelope");

                for sample in 0..=4096 {
                    let t = start + sweep * f64::from(sample) / 4096.0;
                    let [x, y] = on_circle(centre, radius, t);
                    assert!(
                        x >= min_x - 1e-6
                            && x <= max_x + 1e-6
                            && y >= min_y - 1e-6
                            && y <= max_y + 1e-6,
                        "sample ({x}, {y}) outside {:?} for start {start} sweep {sweep}",
                        [min_x, max_x, min_y, max_y]
                    );
                }
            }
        }
    }

    #[test]
    fn envelope_is_no_larger_than_a_dense_sample_needs() {
        let centre = [-3.5, 47.25];
        let radius = 2.75;
        for start_step in 0..12 {
            for sweep_step in 1..24 {
                let start = f64::from(start_step) * PI / 6.0;
                let sweep = f64::from(sweep_step) * (2.0 * PI - 0.05) / 24.0;
                let (p0, p1, p2) = arc_controls(centre, radius, start, sweep);
                let envelope = arc_envelope(p0, p1, p2);

                let mut sampled = XyBounds::new();
                for sample in 0..=4096 {
                    let t = start + sweep * f64::from(sample) / 4096.0;
                    let [x, y] = on_circle(centre, radius, t);
                    sampled.add(x, y);
                }
                let expected = sampled.finish().expect("samples are finite");
                // The sample misses each extreme by at most radius * step^2 / 8.
                assert_envelope(envelope, expected, 1e-5);
            }
        }
    }

    // --- the WKB walk ---

    #[test]
    fn reads_a_circular_string() {
        let body = wkb(8, &coords(&[[-1.0, 0.0], [0.0, 1.0], [1.0, 0.0]]));
        assert_envelope(
            xy_envelope(&body).expect("valid body"),
            [-1.0, 1.0, 0.0, 1.0],
            1e-12,
        );
    }

    #[test]
    fn reads_a_chain_of_arcs() {
        // Upper half of the unit circle, then upper half of the circle centred
        // on (3, 0): five points, two arcs sharing the point (1, 0).
        let body = wkb(
            8,
            &coords(&[[-1.0, 0.0], [0.0, 1.0], [1.0, 0.0], [3.0, 2.0], [5.0, 0.0]]),
        );
        assert_envelope(
            xy_envelope(&body).expect("valid body"),
            [-1.0, 5.0, 0.0, 2.0],
            1e-12,
        );
    }

    #[test]
    fn reads_a_compound_curve_of_line_and_arc() {
        let line = wkb(2, &coords(&[[-3.0, -1.0], [-1.0, 0.0]]));
        let arc = wkb(8, &coords(&[[-1.0, 0.0], [0.0, 1.0], [1.0, 0.0]]));
        let body = wkb(9, &children(&[line, arc]));
        assert_envelope(
            xy_envelope(&body).expect("valid body"),
            [-3.0, 1.0, -1.0, 1.0],
            1e-12,
        );
    }

    #[test]
    fn reads_a_curve_polygon_whose_rings_are_full_geometries() {
        // A CurvePolygon ring carries its own byte order and type code, unlike
        // a Polygon's bare linear rings.
        let ring = wkb(8, &coords(&[[1.0, 0.0], [-1.0, 0.0], [1.0, 0.0]]));
        let body = wkb(10, &children(&[ring]));
        assert_envelope(
            xy_envelope(&body).expect("valid body"),
            [-1.0, 1.0, -1.0, 1.0],
            1e-12,
        );
    }

    #[test]
    fn reads_polygon_rings_as_bare_sequences() {
        let mut payload = 1u32.to_le_bytes().to_vec();
        payload.extend_from_slice(&coords(&[[0.0, 0.0], [4.0, 0.0], [4.0, 3.0], [0.0, 0.0]]));
        let body = wkb(3, &payload);
        assert_envelope(
            xy_envelope(&body).expect("valid body"),
            [0.0, 4.0, 0.0, 3.0],
            0.0,
        );
    }

    #[test]
    fn reads_a_multisurface_of_curve_polygons() {
        let ring = wkb(8, &coords(&[[1.0, 0.0], [-1.0, 0.0], [1.0, 0.0]]));
        let curve_polygon = wkb(10, &children(&[ring]));
        let body = wkb(12, &children(&[curve_polygon]));
        assert_envelope(
            xy_envelope(&body).expect("valid body"),
            [-1.0, 1.0, -1.0, 1.0],
            1e-12,
        );
    }

    #[test]
    fn reads_a_geometry_collection_holding_a_curve() {
        let point = wkb(1, &{
            let mut b = 9.0_f64.to_le_bytes().to_vec();
            b.extend_from_slice(&9.0_f64.to_le_bytes());
            b
        });
        let arc = wkb(8, &coords(&[[-1.0, 0.0], [0.0, 1.0], [1.0, 0.0]]));
        let body = wkb(7, &children(&[point, arc]));
        assert_envelope(
            xy_envelope(&body).expect("valid body"),
            [-1.0, 9.0, 0.0, 9.0],
            1e-12,
        );
    }

    #[test]
    fn reads_a_big_endian_child_inside_a_little_endian_parent() {
        let mut arc = vec![0u8];
        arc.extend_from_slice(&8u32.to_be_bytes());
        arc.extend_from_slice(&3u32.to_be_bytes());
        for [x, y] in [[-1.0_f64, 0.0_f64], [0.0, 1.0], [1.0, 0.0]] {
            arc.extend_from_slice(&x.to_be_bytes());
            arc.extend_from_slice(&y.to_be_bytes());
        }
        let body = wkb(11, &children(&[arc]));
        assert_envelope(
            xy_envelope(&body).expect("valid body"),
            [-1.0, 1.0, 0.0, 1.0],
            1e-12,
        );
    }

    #[test]
    fn skips_z_and_m_without_letting_them_widen_the_box() {
        // CIRCULARSTRING ZM: four f64s per coordinate, only two of which count.
        let mut payload = 3u32.to_le_bytes().to_vec();
        for ([x, y], z, m) in [
            ([-1.0_f64, 0.0_f64], 100.0_f64, -100.0_f64),
            ([0.0, 1.0], 200.0, -200.0),
            ([1.0, 0.0], 300.0, -300.0),
        ] {
            payload.extend_from_slice(&x.to_le_bytes());
            payload.extend_from_slice(&y.to_le_bytes());
            payload.extend_from_slice(&z.to_le_bytes());
            payload.extend_from_slice(&m.to_le_bytes());
        }
        let body = wkb(3008, &payload);
        assert_envelope(
            xy_envelope(&body).expect("valid body"),
            [-1.0, 1.0, 0.0, 1.0],
            1e-12,
        );
    }

    #[test]
    fn a_z_body_gets_an_xyz_envelope_and_an_m_body_does_not() {
        // CIRCULARSTRING Z and CIRCULARSTRING M have the same coordinate width,
        // so only the type code says whether the third ordinate bounds anything.
        let points = [
            ([-1.0_f64, 0.0_f64], 5.0_f64),
            ([0.0, 1.0], 9.0),
            ([1.0, 0.0], 7.0),
        ];
        let payload = |third_written: bool| {
            let mut bytes = 3u32.to_le_bytes().to_vec();
            for ([x, y], third) in points {
                bytes.extend_from_slice(&x.to_le_bytes());
                bytes.extend_from_slice(&y.to_le_bytes());
                if third_written {
                    bytes.extend_from_slice(&third.to_le_bytes());
                }
            }
            bytes
        };

        let (z_envelope, empty) = write_envelope(&wkb(1008, &payload(true))).expect("valid body");
        assert!(!empty);
        assert_eq!(
            z_envelope,
            gpb::Envelope::Xyz([-1.0, 1.0, 0.0, 1.0, 5.0, 9.0])
        );

        let (m_envelope, empty) = write_envelope(&wkb(2008, &payload(true))).expect("valid body");
        assert!(!empty);
        assert_eq!(m_envelope, gpb::Envelope::Xy([-1.0, 1.0, 0.0, 1.0]));
    }

    #[test]
    fn an_empty_curve_body_is_reported_empty_by_the_write_path() {
        let (envelope, empty) = write_envelope(&wkb(8, &coords(&[]))).expect("valid body");
        assert_eq!(envelope, gpb::Envelope::None);
        assert!(empty);
    }

    #[test]
    fn an_empty_point_has_no_envelope() {
        let mut payload = f64::NAN.to_le_bytes().to_vec();
        payload.extend_from_slice(&f64::NAN.to_le_bytes());
        let body = wkb(1, &payload);
        assert!(xy_envelope(&body).expect("valid body").is_none());
    }

    #[test]
    fn an_even_point_count_still_bounds_its_points() {
        // Malformed: a CircularString needs an odd count. The trailing point is
        // kept rather than the whole geometry rejected.
        let body = wkb(
            8,
            &coords(&[[-1.0, 0.0], [0.0, 1.0], [1.0, 0.0], [7.0, 7.0]]),
        );
        assert_envelope(
            xy_envelope(&body).expect("valid body"),
            [-1.0, 7.0, 0.0, 7.0],
            1e-12,
        );
    }

    #[test]
    fn a_truncated_body_is_an_error() {
        let full = wkb(8, &coords(&[[-1.0, 0.0], [0.0, 1.0], [1.0, 0.0]]));
        let truncated = full
            .get(..full.len() - 8)
            .expect("body is longer than 8 bytes");
        assert!(matches!(
            xy_envelope(truncated),
            Err(GeometryError::TruncatedAt { .. })
        ));
    }

    #[test]
    fn a_count_larger_than_the_body_is_an_error() {
        let mut payload = u32::MAX.to_le_bytes().to_vec();
        payload.extend_from_slice(&0.0_f64.to_le_bytes());
        payload.extend_from_slice(&0.0_f64.to_le_bytes());
        let body = wkb(8, &payload);
        assert!(matches!(
            xy_envelope(&body),
            Err(GeometryError::TruncatedAt { .. })
        ));
    }

    #[test]
    fn an_abstract_supertype_is_rejected() {
        let body = wkb(13, &coords(&[[0.0, 0.0]]));
        assert!(matches!(
            xy_envelope(&body),
            Err(GeometryError::AbstractWkbType(13))
        ));
    }

    #[test]
    fn ewkb_is_rejected_rather_than_guessed_at() {
        let body = wkb(0x8000_0002, &coords(&[[0.0, 0.0]]));
        assert!(matches!(
            xy_envelope(&body),
            Err(GeometryError::UnknownWkbType(_))
        ));
    }

    #[test]
    fn an_invalid_byte_order_marker_is_an_error() {
        let mut body = vec![7u8];
        body.extend_from_slice(&8u32.to_le_bytes());
        assert!(matches!(
            xy_envelope(&body),
            Err(GeometryError::InvalidByteOrder { .. })
        ));
    }

    #[test]
    fn nesting_past_the_limit_is_an_error_not_a_stack_overflow() {
        let mut body = wkb(1, &{
            let mut b = 0.0_f64.to_le_bytes().to_vec();
            b.extend_from_slice(&0.0_f64.to_le_bytes());
            b
        });
        for _ in 0..(MAX_DEPTH + 2) {
            body = wkb(7, &children(&[body]));
        }
        assert!(matches!(
            xy_envelope(&body),
            Err(GeometryError::NestingTooDeep)
        ));
    }
}
