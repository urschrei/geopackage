//! Property tests over GeoPackage Binary geometries: the envelope the encoder
//! writes, the readers that answer the same questions about a blob, and the
//! round trip back to `geo-types`.
//!
//! The geometries are the seven core types, nested in collections up to three
//! levels deep, with coordinates either finite or from the whole `f64` domain.
//! The oracle walks the drawn `geo-types` value itself.

#![cfg(feature = "geo-types")]
#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geo_types::{
    Coord, Geometry, GeometryCollection, LineString, MultiLineString, MultiPoint, MultiPolygon,
    Point, Polygon,
};
use geopackage_core::geometry::{
    GeometryError, GpbGeometry, blob_envelope_and_empty, blob_is_empty, blob_xy_envelope,
    encode_gpb, encode_gpb_from_wkb, wkb_geometry_type, write_envelope,
};
use geopackage_core::gpb::{Envelope, parse_header};
use geopackage_core::types::GeometryType;
use hegel::generators;

type DrawCoord = fn(&hegel::TestCase) -> f64;

/// A finite coordinate, dense enough that drawn values coincide.
fn draw_finite(tc: &hegel::TestCase) -> f64 {
    tc.draw(generators::floats::<f64>().min_value(-1e6).max_value(1e6))
}

/// Any `f64`: NaN, the infinities, subnormals and the extremes included.
fn draw_any(tc: &hegel::TestCase) -> f64 {
    tc.draw(generators::floats::<f64>())
}

fn draw_coords(tc: &hegel::TestCase, coord: DrawCoord) -> Vec<Coord<f64>> {
    let count = tc.draw(generators::integers::<usize>().min_value(0).max_value(4));
    (0..count)
        .map(|_| Coord {
            x: coord(tc),
            y: coord(tc),
        })
        .collect()
}

fn draw_polygon(tc: &hegel::TestCase, coord: DrawCoord) -> Polygon<f64> {
    let holes = tc.draw(generators::integers::<usize>().min_value(0).max_value(2));
    Polygon::new(
        LineString::new(draw_coords(tc, coord)),
        (0..holes)
            .map(|_| LineString::new(draw_coords(tc, coord)))
            .collect(),
    )
}

/// Draws a geometry of one of the seven core types. Lists have up to four
/// members, and collections nest while `depth` allows.
fn draw_geometry(tc: &hegel::TestCase, coord: DrawCoord, depth: u8) -> Geometry<f64> {
    let max_kind = if depth == 0 { 5 } else { 6 };
    match tc.draw(
        generators::integers::<u8>()
            .min_value(0)
            .max_value(max_kind),
    ) {
        0 => {
            if tc.draw(generators::weighted_booleans(0.1)) {
                Point::new(f64::NAN, f64::NAN).into()
            } else {
                Point::new(coord(tc), coord(tc)).into()
            }
        }
        1 => LineString::new(draw_coords(tc, coord)).into(),
        2 => draw_polygon(tc, coord).into(),
        3 => MultiPoint::new(
            draw_coords(tc, coord)
                .into_iter()
                .map(Point::from)
                .collect(),
        )
        .into(),
        4 => {
            let count = tc.draw(generators::integers::<usize>().min_value(0).max_value(3));
            MultiLineString::new(
                (0..count)
                    .map(|_| LineString::new(draw_coords(tc, coord)))
                    .collect(),
            )
            .into()
        }
        5 => {
            let count = tc.draw(generators::integers::<usize>().min_value(0).max_value(3));
            MultiPolygon::new((0..count).map(|_| draw_polygon(tc, coord)).collect()).into()
        }
        _ => {
            let count = tc.draw(generators::integers::<usize>().min_value(0).max_value(3));
            Geometry::GeometryCollection(GeometryCollection::new_from(
                (0..count)
                    .map(|_| draw_geometry(tc, coord, depth - 1))
                    .collect(),
            ))
        }
    }
}

/// Draws a geometry with finite coordinates four times in five, and with
/// coordinates from the whole `f64` domain otherwise.
fn draw_any_geometry(tc: &hegel::TestCase) -> Geometry<f64> {
    let coord: DrawCoord = if tc.draw(generators::weighted_booleans(0.8)) {
        draw_finite
    } else {
        draw_any
    };
    draw_geometry(tc, coord, 3)
}

/// Every coordinate pair of a geometry, in order.
fn pairs(geometry: &Geometry<f64>, out: &mut Vec<(f64, f64)>) {
    let ring = |line: &LineString<f64>, out: &mut Vec<(f64, f64)>| {
        out.extend(line.0.iter().map(|c| (c.x, c.y)));
    };
    let polygon = |p: &Polygon<f64>, out: &mut Vec<(f64, f64)>| {
        ring(p.exterior(), out);
        for hole in p.interiors() {
            ring(hole, out);
        }
    };
    match geometry {
        Geometry::Point(p) => out.push((p.x(), p.y())),
        Geometry::LineString(l) => ring(l, out),
        Geometry::Polygon(p) => polygon(p, out),
        Geometry::MultiPoint(m) => out.extend(m.0.iter().map(|p| (p.x(), p.y()))),
        Geometry::MultiLineString(m) => {
            for line in &m.0 {
                ring(line, out);
            }
        }
        Geometry::MultiPolygon(m) => {
            for p in &m.0 {
                polygon(p, out);
            }
        }
        Geometry::GeometryCollection(c) => {
            for member in &c.0 {
                pairs(member, out);
            }
        }
        Geometry::Line(l) => out.extend([(l.start.x, l.start.y), (l.end.x, l.end.y)]),
        Geometry::Rect(r) => out.extend([(r.min().x, r.min().y), (r.max().x, r.max().y)]),
        Geometry::Triangle(t) => out.extend(t.to_array().iter().map(|c| (c.x, c.y))),
    }
}

/// The oracle envelope `[min_x, max_x, min_y, max_y]`: the box of the pairs
/// whose two values are finite, or `None` if there are none, as
/// `GpbGeometry::xy_envelope` documents.
fn expected_envelope(geometry: &Geometry<f64>) -> Option<[f64; 4]> {
    let mut all = Vec::new();
    pairs(geometry, &mut all);
    let finite: Vec<(f64, f64)> = all
        .into_iter()
        .filter(|(x, y)| x.is_finite() && y.is_finite())
        .collect();
    if finite.is_empty() {
        return None;
    }
    let mut env = [
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ];
    for (x, y) in finite {
        env[0] = env[0].min(x);
        env[1] = env[1].max(x);
        env[2] = env[2].min(y);
        env[3] = env[3].max(y);
    }
    Some(env)
}

/// An envelope by the bits of its values, so that a comparison is exact and
/// tells negative zero from zero.
fn bits(envelope: Option<[f64; 4]>) -> Option<[u64; 4]> {
    envelope.map(|env| env.map(f64::to_bits))
}

/// Returns `true` if a polygon in the geometry, at any depth, has interior
/// rings and an empty exterior ring, which `encode_gpb` rejects.
fn has_interior_rings_without_exterior(geometry: &Geometry<f64>) -> bool {
    let polygon = |p: &Polygon<f64>| p.exterior().0.is_empty() && !p.interiors().is_empty();
    match geometry {
        Geometry::Polygon(p) => polygon(p),
        Geometry::MultiPolygon(m) => m.0.iter().any(polygon),
        Geometry::GeometryCollection(c) => c.0.iter().any(has_interior_rings_without_exterior),
        _ => false,
    }
}

/// Encodes a drawn geometry that `encode_gpb` accepts, discarding the case
/// otherwise; `encode_gpb_accepts_exactly_the_encodable_geometries` covers
/// the rejection.
fn encode(
    tc: &hegel::TestCase,
    geometry: &Geometry<f64>,
    srs_id: i32,
) -> (Vec<u8>, Option<[f64; 4]>) {
    tc.assume(!has_interior_rings_without_exterior(geometry));
    encode_gpb(geometry, srs_id).unwrap()
}

/// `encode_gpb` rejects a geometry with a polygon that has interior rings and
/// an empty exterior ring, and encodes every other drawn geometry.
#[hegel::test]
fn encode_gpb_accepts_exactly_the_encodable_geometries(tc: hegel::TestCase) {
    let geometry = draw_any_geometry(&tc);
    let result = encode_gpb(&geometry, 4326);
    if has_interior_rings_without_exterior(&geometry) {
        assert!(
            matches!(result, Err(GeometryError::InteriorRingsWithoutExterior)),
            "{geometry:?}: {result:?}"
        );
    } else {
        assert!(result.is_ok(), "{geometry:?}: {result:?}");
    }
}

/// `encode_gpb` returns, and writes in the header, the box of the geometry's
/// finite coordinate pairs; a geometry with none is empty, with no envelope
/// and the empty flag set. `write_envelope` gives the same answer.
#[hegel::test]
fn the_encoded_envelope_is_the_box_of_the_finite_pairs(tc: hegel::TestCase) {
    let geometry = draw_any_geometry(&tc);
    let expected = expected_envelope(&geometry);
    let srs_id = tc.draw(generators::integers::<i32>());
    let (blob, returned) = encode(&tc, &geometry, srs_id);
    assert_eq!(bits(returned), bits(expected));

    let (header, _) = parse_header(&blob).unwrap();
    assert_eq!(header.srs_id, srs_id);
    assert_eq!(header.empty, expected.is_none());
    let in_header = header
        .envelope
        .xy_bounds()
        .map(|(min_x, max_x, min_y, max_y)| [min_x, max_x, min_y, max_y]);
    assert_eq!(bits(in_header), bits(expected));

    let (envelope, empty) = write_envelope(&geometry);
    assert_eq!(empty, expected.is_none());
    assert_eq!(matches!(envelope, Envelope::None), expected.is_none());
}

/// The readers of a blob agree on its envelope and on whether it is empty:
/// `blob_xy_envelope`, `blob_is_empty`, `blob_envelope_and_empty`, and
/// `GpbGeometry`'s `xy_envelope` and `is_empty`, with the header envelope
/// kept or removed.
#[hegel::test]
fn the_blob_readers_agree(tc: hegel::TestCase) {
    let geometry = draw_any_geometry(&tc);
    let expected = expected_envelope(&geometry);
    let (mut blob, _) = encode(&tc, &geometry, 4326);
    if tc.draw(generators::booleans()) {
        // The same geometry without a header envelope, so that every reader
        // walks the body. The empty flag stays as the encoder set it.
        let (header, offset) = parse_header(&blob).unwrap();
        let mut bare = geopackage_core::gpb::encode_header(
            header.srs_id,
            &Envelope::None,
            header.empty,
            header.extended,
        );
        bare.extend_from_slice(&blob[offset..]);
        blob = bare;
    }

    assert_eq!(bits(blob_xy_envelope(&blob).unwrap()), bits(expected));
    assert_eq!(blob_is_empty(&blob).unwrap(), expected.is_none());
    let (envelope, empty) = blob_envelope_and_empty(&blob).unwrap();
    assert_eq!(
        (bits(envelope), empty),
        (bits(expected), expected.is_none())
    );
    let parsed = GpbGeometry::parse(&blob).unwrap();
    assert_eq!(bits(parsed.xy_envelope()), bits(expected));
    assert_eq!(parsed.is_empty(), expected.is_none());
}

/// Returns `true` if the geometry contains a point whose two values are NaN,
/// the WKB encoding of an empty point.
fn has_empty_point(geometry: &Geometry<f64>) -> bool {
    match geometry {
        Geometry::Point(p) => p.x().is_nan() && p.y().is_nan(),
        Geometry::MultiPoint(m) => m.0.iter().any(|p| p.x().is_nan() && p.y().is_nan()),
        Geometry::GeometryCollection(c) => c.0.iter().any(has_empty_point),
        _ => false,
    }
}

/// The geometry as `to_geo` rebuilds it: each polygon through
/// `Polygon::new`, which closes a ring by appending its first coordinate if
/// that differs from the last. A coordinate with a NaN never equals itself, so
/// such a ring gains a coordinate each time it is built.
fn rebuilt(geometry: &Geometry<f64>) -> Geometry<f64> {
    let polygon = |p: &Polygon<f64>| Polygon::new(p.exterior().clone(), p.interiors().to_vec());
    match geometry {
        Geometry::Polygon(p) => polygon(p).into(),
        Geometry::MultiPolygon(m) => MultiPolygon::new(m.0.iter().map(polygon).collect()).into(),
        Geometry::GeometryCollection(c) => Geometry::GeometryCollection(
            GeometryCollection::new_from(c.0.iter().map(rebuilt).collect()),
        ),
        other => other.clone(),
    }
}

/// A blob converts back to the geometry it was encoded from, every value to
/// the bit, as `Polygon::new` rebuilds it, or to `None` when the geometry
/// contains an empty point, which `geo-types` cannot represent.
#[hegel::test]
fn a_blob_converts_back_to_its_geometry(tc: hegel::TestCase) {
    let geometry = draw_any_geometry(&tc);
    let (blob, _) = encode(&tc, &geometry, 4326);
    let back = GpbGeometry::parse(&blob).unwrap().to_geo();
    if has_empty_point(&geometry) {
        assert!(back.is_none(), "{geometry:?} came back as {back:?}");
    } else {
        // `Debug` prints each `f64` exactly, NaN and negative zero included.
        assert_eq!(
            format!("{:?}", back.unwrap()),
            format!("{:?}", rebuilt(&geometry))
        );
    }
}

/// `encode_gpb_from_wkb` over the body that `encode_gpb` wrote produces the
/// same blob and envelope, and ignores bytes after the geometry.
#[hegel::test]
fn encode_gpb_from_wkb_agrees_with_encode_gpb(tc: hegel::TestCase) {
    let geometry = draw_any_geometry(&tc);
    let srs_id = tc.draw(generators::integers::<i32>());
    let (blob, envelope) = encode(&tc, &geometry, srs_id);
    let (_, offset) = parse_header(&blob).unwrap();
    let mut body = blob[offset..].to_vec();
    body.extend(tc.draw(generators::vecs(generators::integers::<u8>()).max_size(16)));

    let encoded = encode_gpb_from_wkb(&body, srs_id).unwrap();
    assert_eq!(encoded.blob, blob);
    assert_eq!(bits(encoded.xy_envelope), bits(envelope));
}

/// `wkb_geometry_type` reads the type of the body `encode_gpb` wrote.
#[hegel::test]
fn wkb_geometry_type_reads_the_written_type(tc: hegel::TestCase) {
    let geometry = draw_any_geometry(&tc);
    let expected = match &geometry {
        Geometry::Point(_) => GeometryType::Point,
        Geometry::LineString(_) => GeometryType::LineString,
        Geometry::Polygon(_) => GeometryType::Polygon,
        Geometry::MultiPoint(_) => GeometryType::MultiPoint,
        Geometry::MultiLineString(_) => GeometryType::MultiLineString,
        Geometry::MultiPolygon(_) => GeometryType::MultiPolygon,
        _ => GeometryType::GeometryCollection,
    };
    let (blob, _) = encode(&tc, &geometry, 4326);
    let (_, offset) = parse_header(&blob).unwrap();
    assert_eq!(wkb_geometry_type(&blob[offset..]).unwrap(), expected);
}

/// A blob cut short anywhere in its body is an error for `GpbGeometry::parse`
/// and for `encode_gpb_from_wkb`, never a panic or a shorter geometry.
#[hegel::test]
fn a_truncated_body_is_an_error(tc: hegel::TestCase) {
    let geometry = draw_any_geometry(&tc);
    let (blob, _) = encode(&tc, &geometry, 4326);
    let (_, offset) = parse_header(&blob).unwrap();
    let cut = tc.draw(
        generators::integers::<usize>()
            .min_value(offset)
            .max_value(blob.len() - 1),
    );
    let truncated = &blob[..cut];
    assert!(GpbGeometry::parse(truncated).is_err(), "cut at {cut}");
    assert!(
        encode_gpb_from_wkb(&truncated[offset..], 4326).is_err(),
        "cut at {cut}"
    );
}

/// The coordinate dimensions of a hand-built WKB body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dims {
    Xy,
    Xyz,
    Xym,
    Xyzm,
}

/// Draws a little-endian ISO WKB body in drawn dimensions: a point, line
/// string, one-ring polygon or multi-point, with up to four points where the
/// type takes a list. Returns the body, its dimensions, and every coordinate
/// as `(x, y, z)`, with `z` `None` where the body has no Z.
/// A coordinate as `(x, y, z)`, with `z` `None` where the body has no Z.
type Xyz = (f64, f64, Option<f64>);

fn draw_wkb(tc: &hegel::TestCase) -> (Vec<u8>, Dims, Vec<Xyz>) {
    let coord: DrawCoord = if tc.draw(generators::weighted_booleans(0.8)) {
        draw_finite
    } else {
        draw_any
    };
    let dims = tc.draw(generators::sampled_from(vec![
        Dims::Xy,
        Dims::Xyz,
        Dims::Xym,
        Dims::Xyzm,
    ]));
    let (offset, has_z, extra) = match dims {
        Dims::Xy => (0, false, 0),
        Dims::Xyz => (1000, true, 1),
        Dims::Xym => (2000, false, 1),
        Dims::Xyzm => (3000, true, 2),
    };
    let mut coords = Vec::new();
    let mut point = |tc: &hegel::TestCase, body: &mut Vec<u8>| {
        let (x, y) = (coord(tc), coord(tc));
        body.extend_from_slice(&x.to_le_bytes());
        body.extend_from_slice(&y.to_le_bytes());
        let mut z = None;
        for i in 0..extra {
            let value = coord(tc);
            body.extend_from_slice(&value.to_le_bytes());
            if i == 0 && has_z {
                z = Some(value);
            }
        }
        coords.push((x, y, z));
    };
    let kind = tc.draw(generators::integers::<u32>().min_value(1).max_value(4));
    let mut body = vec![1];
    body.extend_from_slice(&(kind + offset).to_le_bytes());
    let count = tc.draw(generators::integers::<u32>().min_value(0).max_value(4));
    match kind {
        1 => point(tc, &mut body),
        2 | 3 => {
            if kind == 3 {
                body.extend_from_slice(&1u32.to_le_bytes());
            }
            body.extend_from_slice(&count.to_le_bytes());
            for _ in 0..count {
                point(tc, &mut body);
            }
        }
        _ => {
            body.extend_from_slice(&count.to_le_bytes());
            for _ in 0..count {
                body.push(1);
                body.extend_from_slice(&(1 + offset).to_le_bytes());
                point(tc, &mut body);
            }
        }
    }
    (body, dims, coords)
}

/// `encode_gpb_from_wkb` writes a body in any dimensions unchanged after a
/// header whose envelope is the box of the finite XY pairs, widened to XYZ
/// by the range of the finite Z values of those same coordinates when the
/// body has a Z and one exists, and reports the body's dimensions. A body with
/// no finite XY pair is empty, with no envelope.
#[hegel::test]
fn encode_gpb_from_wkb_writes_the_body_after_its_envelope(tc: hegel::TestCase) {
    let (body, dims, coords) = draw_wkb(&tc);
    let srs_id = tc.draw(generators::integers::<i32>());
    let encoded = encode_gpb_from_wkb(&body, srs_id).unwrap();

    let finite: Vec<&Xyz> = coords
        .iter()
        .filter(|(x, y, _)| x.is_finite() && y.is_finite())
        .collect();
    let xy = (!finite.is_empty()).then(|| {
        let mut env = [
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ];
        for (x, y, _) in &finite {
            env = [
                env[0].min(*x),
                env[1].max(*x),
                env[2].min(*y),
                env[3].max(*y),
            ];
        }
        env
    });
    let z: Vec<f64> = finite
        .iter()
        .filter_map(|(_, _, z)| *z)
        .filter(|z| z.is_finite())
        .collect();
    let envelope = match xy {
        None => Envelope::None,
        Some([min_x, max_x, min_y, max_y]) if !z.is_empty() => Envelope::Xyz([
            min_x,
            max_x,
            min_y,
            max_y,
            z.iter().copied().fold(f64::INFINITY, f64::min),
            z.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        ]),
        Some(env) => Envelope::Xy(env),
    };
    let mut expected = geopackage_core::gpb::encode_header(srs_id, &envelope, xy.is_none(), false);
    expected.extend_from_slice(&body);

    assert_eq!(encoded.blob, expected, "{dims:?}");
    assert_eq!(bits(encoded.xy_envelope), bits(xy));
    let declared = match dims {
        Dims::Xy => geo_traits::Dimensions::Xy,
        Dims::Xyz => geo_traits::Dimensions::Xyz,
        Dims::Xym => geo_traits::Dimensions::Xym,
        Dims::Xyzm => geo_traits::Dimensions::Xyzm,
    };
    assert_eq!(encoded.dimensions, declared);
}

/// The deepest nesting level of any member, counting each member of a
/// collection or multi-geometry as one level below its container.
fn deepest(geometry: &Geometry<f64>, depth: u32) -> u32 {
    let members = |count: usize| if count > 0 { depth + 1 } else { depth };
    match geometry {
        Geometry::MultiPoint(m) => members(m.0.len()),
        Geometry::MultiLineString(m) => members(m.0.len()),
        Geometry::MultiPolygon(m) => members(m.0.len()),
        Geometry::GeometryCollection(c) => {
            c.0.iter()
                .map(|member| deepest(member, depth + 1))
                .max()
                .unwrap_or(depth)
        }
        _ => depth,
    }
}

/// `encode_gpb` writes only what every reader of the crate can read back:
/// it rejects a geometry nested more than 32 levels deep, and the blob of any
/// other geometry is readable by `GpbGeometry::parse`, the `ST_*` readers and
/// `encode_gpb_from_wkb`.
#[hegel::test]
fn every_encoded_blob_is_readable_by_every_reader(tc: hegel::TestCase) {
    let mut geometry = draw_any_geometry(&tc);
    tc.assume(!has_interior_rings_without_exterior(&geometry));
    let wrappers = tc.draw(generators::integers::<u32>().min_value(0).max_value(40));
    for _ in 0..wrappers {
        geometry = Geometry::GeometryCollection(GeometryCollection::new_from(vec![geometry]));
    }
    let too_deep = deepest(&geometry, 0) > 32;
    tc.event(if too_deep {
        "too deep"
    } else {
        "readable depth"
    });
    match encode_gpb(&geometry, 4326) {
        Err(GeometryError::NestingTooDeep) => {
            assert!(too_deep, "rejected at depth {}", deepest(&geometry, 0))
        }
        Err(error) => panic!("{error:?}"),
        Ok((blob, _)) => {
            assert!(!too_deep, "accepted at depth {}", deepest(&geometry, 0));
            let (_, offset) = parse_header(&blob).unwrap();
            drop(GpbGeometry::parse(&blob).unwrap());
            blob_is_empty(&blob).unwrap();
            blob_xy_envelope(&blob).unwrap();
            blob_envelope_and_empty(&blob).unwrap();
            drop(encode_gpb_from_wkb(&blob[offset..], 4326).unwrap());
        }
    }
}
