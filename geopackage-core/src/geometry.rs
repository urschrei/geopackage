//! Lazy geometry wrapper over a GeoPackage Binary (GPB) blob.
//!
//! A [`GpbGeometry`] pairs a parsed GPB header (see [`crate::gpb`]) with the
//! ISO WKB body that follows it. The body is read through the georust
//! [`wkb`] crate: [`GpbGeometry`] implements [`geo_traits::GeometryTrait`] by
//! delegating to [`wkb::reader::Wkb`], so callers can traverse coordinates
//! without materialising an owned geometry, and can convert to `geo-types`
//! via [`GpbGeometry::to_geo`] when the `geo-types` feature is enabled.
//!
//! Placement: this wrapper and the envelope traversal below live in
//! `geopackage-core` rather than the container crate, which keeps the fuzz
//! workspace free of the SQLite dependency. The intended long-term home is an
//! upstreamed `gpb` feature in georust `wkb` itself (tracked in the ecosystem
//! roadmap); until that lands, this module is ours.
//!
//! Parsing arbitrary bytes never panics: a malformed header, a truncated body,
//! or a geometry type the `wkb` crate cannot read all yield a
//! [`GeometryError`].

use crate::gpb::{self, GpbHeader};
use crate::types::GeometryType;

use geo_traits::{
    CoordTrait, Dimensions, GeometryTrait, LineStringTrait, LineTrait, MultiLineStringTrait,
    MultiPointTrait, MultiPolygonTrait, PointTrait, PolygonTrait, RectTrait, TriangleTrait,
};
use geo_traits::{GeometryCollectionTrait, GeometryType as GtGeometryType};
use wkb::reader::{
    GeometryCollection, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon, Wkb,
};

/// Errors from constructing or reading a [`GpbGeometry`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GeometryError {
    /// The GPB header could not be parsed.
    #[error(transparent)]
    Header(#[from] gpb::GpbError),
    /// The ISO WKB body could not be read. This includes non-linear curve
    /// types (`CIRCULARSTRING`, `CURVEPOLYGON`, …) that the `wkb` crate does
    /// not yet support, as well as structurally malformed bodies.
    #[error("WKB body is unreadable (unsupported curve type or malformed geometry)")]
    Body(#[from] wkb::error::WkbError),
    /// The WKB body was too short to contain the one-byte order marker and
    /// four-byte geometry type code.
    #[error("WKB body truncated: need at least 5 bytes for the geometry type code")]
    TruncatedWkb,
    /// The WKB geometry type code is not one of the GeoPackage geometry types
    /// (Annex G).
    #[error("unknown WKB geometry type code {0}")]
    UnknownWkbType(u32),
}

/// A GeoPackage geometry: a parsed GPB header plus its ISO WKB body.
///
/// Construct with [`GpbGeometry::parse`]. The wrapper borrows the source blob;
/// coordinate access is constant-time (the underlying `wkb` reader performs a
/// single validating pass on construction) but not zero-copy.
#[derive(Debug, Clone)]
pub struct GpbGeometry<'a> {
    header: GpbHeader,
    body: &'a [u8],
    wkb: Wkb<'a>,
}

impl<'a> GpbGeometry<'a> {
    /// Parse a complete GPB blob: the header, then the ISO WKB body.
    ///
    /// Returns a [`GeometryError`] for a malformed header, a truncated or
    /// malformed body, or a geometry type the `wkb` crate cannot read. Never
    /// panics on arbitrary input.
    pub fn parse(blob: &'a [u8]) -> Result<Self, GeometryError> {
        let (header, offset) = gpb::parse_header(blob)?;
        let body = &blob[offset..];
        let wkb = Wkb::try_new(body)?;
        Ok(Self { header, body, wkb })
    }

    /// The parsed GPB header.
    pub fn header(&self) -> &GpbHeader {
        &self.header
    }

    /// The raw ISO WKB body slice (everything after the GPB header).
    ///
    /// This may include trailing bytes beyond the geometry; use
    /// [`GpbGeometry::wkb`] and [`wkb::reader::Wkb::buf`] for the exact
    /// geometry extent.
    pub fn wkb_body(&self) -> &'a [u8] {
        self.body
    }

    /// The parsed `wkb` reader for the body.
    pub fn wkb(&self) -> &Wkb<'a> {
        &self.wkb
    }

    /// Convert to an owned [`geo_types::Geometry`].
    ///
    /// Returns `None` for a geometry `geo-types` cannot represent (an empty
    /// point). Only the X and Y dimensions are kept; any Z or M values in the
    /// body are dropped.
    #[cfg(feature = "geo-types")]
    pub fn to_geo(&self) -> Option<geo_types::Geometry<f64>> {
        use geo_traits::to_geo::ToGeoGeometry;
        self.wkb.try_to_geometry()
    }

    /// The XY bounding box `[min_x, max_x, min_y, max_y]` computed by walking
    /// the WKB body, or `None` when the geometry has no finite coordinate (an
    /// empty geometry).
    ///
    /// This is the fallback the `ST_*` SQL functions use when the GPB header
    /// carries no envelope. Coordinates are visited through the `geo-traits`
    /// interface, so every geometry type the `wkb` crate can read is handled,
    /// in either byte order and with any Z/M dimensions (Z and M are ignored;
    /// only X and Y bound the box). Non-finite coordinates (the NaN empty-point
    /// convention, or malformed values) do not contribute to the box.
    pub fn xy_envelope(&self) -> Option<[f64; 4]> {
        let mut bounds = XyBounds::new();
        accumulate_xy_bounds(&self.wkb, &mut bounds);
        bounds.finish()
    }

    /// Whether this geometry is empty.
    ///
    /// True when the header's empty flag is set, or when the body carries no
    /// finite coordinate: an empty point (the all-NaN convention), an empty
    /// linestring/polygon/multi-geometry, or an empty geometry collection.
    pub fn is_empty(&self) -> bool {
        self.header.empty || self.xy_envelope().is_none()
    }

    /// The geometry type of the WKB body, as a [`GeometryType`].
    ///
    /// This reflects what the `wkb` crate parsed, so it is always one of the
    /// seven linear types. To classify a body whose type the `wkb` crate
    /// cannot read (a curve type), read the raw discriminator with
    /// [`wkb_geometry_type`] instead.
    pub fn geometry_type(&self) -> GeometryType {
        use wkb::reader::GeometryType as WkbType;
        match self.wkb.geometry_type() {
            WkbType::Point => GeometryType::Point,
            WkbType::LineString => GeometryType::LineString,
            WkbType::Polygon => GeometryType::Polygon,
            WkbType::MultiPoint => GeometryType::MultiPoint,
            WkbType::MultiLineString => GeometryType::MultiLineString,
            WkbType::MultiPolygon => GeometryType::MultiPolygon,
            WkbType::GeometryCollection => GeometryType::GeometryCollection,
            // `wkb::reader::GeometryType` is `#[non_exhaustive]`; a future
            // variant would be a type `wkb` newly learned to read. Report it
            // as the `GEOMETRY` supertype until this mapping is extended.
            _ => GeometryType::Geometry,
        }
    }

    /// Whether this geometry's type satisfies a column `declared` as a given
    /// [`GeometryType`], per [`geometry_type_matches`].
    pub fn matches_declared(&self, declared: GeometryType) -> bool {
        geometry_type_matches(self.geometry_type(), declared)
    }
}

/// Whether the WKB geometry type `actual` satisfies a column declared as
/// `declared`, per the GeoPackage instantiable-type rules.
///
/// The rules are deliberately narrow: there is no general subtype lattice
/// walk. A value satisfies its column when one of the following holds:
///
/// - it is an exact type match; or
/// - the column is declared `GEOMETRY`, the root supertype, which accepts any
///   geometry; or
/// - the column is declared `GEOMETRYCOLLECTION`, which accepts only the
///   collection types (`GEOMETRYCOLLECTION`, `MULTIPOINT`, `MULTILINESTRING`,
///   `MULTIPOLYGON`, `MULTICURVE`, `MULTISURFACE`).
///
/// In particular a `LINESTRING` does **not** satisfy a `MULTILINESTRING`
/// column: a multi-geometry is a collection of its parts, not their supertype.
/// Wiring this into an open/read validation option belongs to the read-API
/// work; the primitive itself lives here.
pub fn geometry_type_matches(actual: GeometryType, declared: GeometryType) -> bool {
    use GeometryType::*;
    if declared == Geometry || declared == actual {
        return true;
    }
    if declared == GeometryCollection {
        return matches!(
            actual,
            GeometryCollection
                | MultiPoint
                | MultiLineString
                | MultiPolygon
                | MultiCurve
                | MultiSurface
        );
    }
    false
}

/// Read the geometry type discriminator from the start of an ISO WKB body,
/// without materialising coordinates.
///
/// Unlike [`GpbGeometry::geometry_type`], this works on curve types the `wkb`
/// crate cannot fully read, which is what a declared-type validator needs: a
/// `CURVEPOLYGON` body in a column declared `POLYGON` must be detectable.
/// GeoPackage bodies are ISO WKB; an extended-WKB (EWKB) type flag is handled
/// defensively but is not expected in a GPB blob.
pub fn wkb_geometry_type(wkb_body: &[u8]) -> Result<GeometryType, GeometryError> {
    if wkb_body.len() < 5 {
        return Err(GeometryError::TruncatedWkb);
    }
    let little_endian = match wkb_body[0] {
        0 => false,
        1 => true,
        _ => return Err(GeometryError::TruncatedWkb),
    };
    let code = {
        let bytes: [u8; 4] = wkb_body[1..5].try_into().expect("length checked above");
        if little_endian {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        }
    };
    // ISO WKB encodes the dimension as a +1000/+2000/+3000 offset on the base
    // type; EWKB instead sets high bit flags and keeps the base type low.
    let base = if code & 0xE000_0000 != 0 {
        code & 0x0000_00FF
    } else {
        code % 1000
    };
    let ty = match base {
        0 => GeometryType::Geometry,
        1 => GeometryType::Point,
        2 => GeometryType::LineString,
        3 => GeometryType::Polygon,
        4 => GeometryType::MultiPoint,
        5 => GeometryType::MultiLineString,
        6 => GeometryType::MultiPolygon,
        7 => GeometryType::GeometryCollection,
        8 => GeometryType::CircularString,
        9 => GeometryType::CompoundCurve,
        10 => GeometryType::CurvePolygon,
        11 => GeometryType::MultiCurve,
        12 => GeometryType::MultiSurface,
        13 => GeometryType::Curve,
        14 => GeometryType::Surface,
        _ => return Err(GeometryError::UnknownWkbType(code)),
    };
    Ok(ty)
}

/// Accumulator for an XY bounding box over visited coordinates.
#[derive(Debug, Clone, Copy)]
struct XyBounds {
    min_x: f64,
    max_x: f64,
    min_y: f64,
    max_y: f64,
    seen: bool,
}

impl XyBounds {
    fn new() -> Self {
        Self {
            min_x: f64::INFINITY,
            max_x: f64::NEG_INFINITY,
            min_y: f64::INFINITY,
            max_y: f64::NEG_INFINITY,
            seen: false,
        }
    }

    fn add(&mut self, x: f64, y: f64) {
        if !x.is_finite() || !y.is_finite() {
            return;
        }
        self.min_x = self.min_x.min(x);
        self.max_x = self.max_x.max(x);
        self.min_y = self.min_y.min(y);
        self.max_y = self.max_y.max(y);
        self.seen = true;
    }

    fn finish(self) -> Option<[f64; 4]> {
        self.seen
            .then_some([self.min_x, self.max_x, self.min_y, self.max_y])
    }
}

fn add_point(point: &impl PointTrait<T = f64>, bounds: &mut XyBounds) {
    if let Some(coord) = point.coord() {
        bounds.add(coord.x(), coord.y());
    }
}

fn add_line_string(line_string: &impl LineStringTrait<T = f64>, bounds: &mut XyBounds) {
    for coord in line_string.coords() {
        bounds.add(coord.x(), coord.y());
    }
}

fn add_polygon(polygon: &impl PolygonTrait<T = f64>, bounds: &mut XyBounds) {
    if let Some(exterior) = polygon.exterior() {
        add_line_string(&exterior, bounds);
    }
    for interior in polygon.interiors() {
        add_line_string(&interior, bounds);
    }
}

/// Walk a geometry through the `geo-traits` interface, folding every finite
/// coordinate into `bounds`. Recurses into geometry collections.
fn accumulate_xy_bounds<G: GeometryTrait<T = f64>>(geom: &G, bounds: &mut XyBounds) {
    match geom.as_type() {
        GtGeometryType::Point(point) => add_point(point, bounds),
        GtGeometryType::LineString(line_string) => add_line_string(line_string, bounds),
        GtGeometryType::Polygon(polygon) => add_polygon(polygon, bounds),
        GtGeometryType::MultiPoint(multi_point) => {
            for point in multi_point.points() {
                add_point(&point, bounds);
            }
        }
        GtGeometryType::MultiLineString(multi_line_string) => {
            for line_string in multi_line_string.line_strings() {
                add_line_string(&line_string, bounds);
            }
        }
        GtGeometryType::MultiPolygon(multi_polygon) => {
            for polygon in multi_polygon.polygons() {
                add_polygon(&polygon, bounds);
            }
        }
        GtGeometryType::GeometryCollection(collection) => {
            for member in collection.geometries() {
                accumulate_xy_bounds(&member, bounds);
            }
        }
        // The `wkb` reader never yields these, but the traversal is generic
        // over any `GeometryTrait`, so they are handled rather than ignored.
        GtGeometryType::Rect(rect) => {
            let (min, max) = (rect.min(), rect.max());
            bounds.add(min.x(), min.y());
            bounds.add(max.x(), max.y());
        }
        GtGeometryType::Triangle(triangle) => {
            for coord in triangle.coords() {
                bounds.add(coord.x(), coord.y());
            }
        }
        GtGeometryType::Line(line) => {
            for coord in line.coords() {
                bounds.add(coord.x(), coord.y());
            }
        }
    }
}

impl<'a> GeometryTrait for GpbGeometry<'a> {
    type T = f64;
    type PointType<'b>
        = Point<'a>
    where
        Self: 'b;
    type LineStringType<'b>
        = LineString<'a>
    where
        Self: 'b;
    type PolygonType<'b>
        = Polygon<'a>
    where
        Self: 'b;
    type MultiPointType<'b>
        = MultiPoint<'a>
    where
        Self: 'b;
    type MultiLineStringType<'b>
        = MultiLineString<'a>
    where
        Self: 'b;
    type MultiPolygonType<'b>
        = MultiPolygon<'a>
    where
        Self: 'b;
    type GeometryCollectionType<'b>
        = GeometryCollection<'a>
    where
        Self: 'b;
    type RectType<'b>
        = geo_traits::UnimplementedRect<f64>
    where
        Self: 'b;
    type TriangleType<'b>
        = geo_traits::UnimplementedTriangle<f64>
    where
        Self: 'b;
    type LineType<'b>
        = geo_traits::UnimplementedLine<f64>
    where
        Self: 'b;

    fn dim(&self) -> Dimensions {
        self.wkb.dim()
    }

    fn as_type(
        &self,
    ) -> geo_traits::GeometryType<
        '_,
        Self::PointType<'_>,
        Self::LineStringType<'_>,
        Self::PolygonType<'_>,
        Self::MultiPointType<'_>,
        Self::MultiLineStringType<'_>,
        Self::MultiPolygonType<'_>,
        Self::GeometryCollectionType<'_>,
        Self::RectType<'_>,
        Self::TriangleType<'_>,
        Self::LineType<'_>,
    > {
        self.wkb.as_type()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpb::{Envelope, encode_header};
    use geo_traits::GeometryType as GtType;

    /// Build a GPB blob from a little-endian WKB body and no envelope.
    fn gpb(body: &[u8]) -> Vec<u8> {
        let mut blob = encode_header(4326, &Envelope::None, false, false);
        blob.extend_from_slice(body);
        blob
    }

    /// A little-endian WKB point body.
    fn wkb_point(x: f64, y: f64) -> Vec<u8> {
        let mut b = vec![1u8];
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&x.to_le_bytes());
        b.extend_from_slice(&y.to_le_bytes());
        b
    }

    #[test]
    fn parses_point_and_exposes_header_and_body() {
        let body = wkb_point(3.0, 4.0);
        let blob = gpb(&body);
        let g = GpbGeometry::parse(&blob).unwrap();
        assert_eq!(g.header().srs_id, 4326);
        assert_eq!(g.wkb_body(), body.as_slice());
    }

    #[test]
    fn delegates_geometry_trait_to_wkb() {
        let blob = gpb(&wkb_point(3.0, 4.0));
        let g = GpbGeometry::parse(&blob).unwrap();
        match g.as_type() {
            GtType::Point(p) => {
                use geo_traits::{CoordTrait, PointTrait};
                let c = p.coord().unwrap();
                assert_eq!(c.x(), 3.0);
                assert_eq!(c.y(), 4.0);
            }
            _ => panic!("expected a point"),
        }
    }

    #[test]
    fn arbitrary_bytes_error_never_panic() {
        assert!(GpbGeometry::parse(b"").is_err());
        assert!(GpbGeometry::parse(b"GP").is_err());
        // Valid header, empty WKB body.
        assert!(GpbGeometry::parse(&gpb(&[])).is_err());
        // Valid header, WKB byte-order marker only.
        assert!(GpbGeometry::parse(&gpb(&[1])).is_err());
        // Valid header, truncated point coordinates.
        assert!(GpbGeometry::parse(&gpb(&wkb_point(1.0, 2.0)[..10])).is_err());
    }

    #[test]
    #[cfg(feature = "geo-types")]
    fn to_geo_yields_geo_types() {
        let blob = gpb(&wkb_point(3.0, 4.0));
        let g = GpbGeometry::parse(&blob).unwrap();
        let geo = g.to_geo().unwrap();
        assert_eq!(
            geo,
            geo_types::Geometry::Point(geo_types::Point::new(3.0, 4.0))
        );
    }

    #[test]
    fn point_envelope_from_traversal() {
        let blob = gpb(&wkb_point(3.0, -4.0));
        let g = GpbGeometry::parse(&blob).unwrap();
        assert_eq!(g.xy_envelope(), Some([3.0, 3.0, -4.0, -4.0]));
        assert!(!g.is_empty());
    }

    #[test]
    fn empty_point_is_empty_no_envelope() {
        let blob = gpb(&wkb_point(f64::NAN, f64::NAN));
        let g = GpbGeometry::parse(&blob).unwrap();
        assert_eq!(g.xy_envelope(), None);
        assert!(g.is_empty());
    }

    #[test]
    fn header_empty_flag_reports_empty() {
        // Header empty flag set, but the body still carries a finite point.
        let mut blob = encode_header(4326, &Envelope::None, true, false);
        blob.extend_from_slice(&wkb_point(1.0, 2.0));
        let g = GpbGeometry::parse(&blob).unwrap();
        assert!(g.is_empty());
    }

    #[cfg(feature = "geo-types")]
    fn wkb_body(geom: &geo_types::Geometry<f64>, little_endian: bool) -> Vec<u8> {
        use wkb::Endianness;
        use wkb::writer::{WriteOptions, write_geometry};
        let options = WriteOptions {
            endianness: if little_endian {
                Endianness::LittleEndian
            } else {
                Endianness::BigEndian
            },
        };
        let mut buf = Vec::new();
        write_geometry(&mut buf, geom, &options).unwrap();
        buf
    }

    /// Build several geometries with known XY bounds, write them (little- and
    /// big-endian), wrap them with that envelope in the header, and assert the
    /// traversal reproduces the header envelope.
    #[test]
    #[cfg(feature = "geo-types")]
    fn traversal_bounds_equal_header_envelope() {
        use geo_types::{
            Geometry, LineString, MultiLineString, MultiPolygon, Point, Polygon, coord,
        };

        let line: Geometry<f64> =
            LineString::from(vec![(0.0, 0.0), (10.0, -3.0), (4.0, 8.0)]).into();
        let polygon: Geometry<f64> = Polygon::new(
            LineString::from(vec![
                (0.0, 0.0),
                (6.0, 0.0),
                (6.0, 5.0),
                (0.0, 5.0),
                (0.0, 0.0),
            ]),
            vec![LineString::from(vec![
                (1.0, 1.0),
                (2.0, 1.0),
                (2.0, 2.0),
                (1.0, 1.0),
            ])],
        )
        .into();
        let multipolygon: Geometry<f64> = MultiPolygon::new(vec![
            Polygon::new(
                LineString::from(vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)]),
                vec![],
            ),
            Polygon::new(
                LineString::from(vec![(-5.0, -5.0), (-4.0, -5.0), (-4.0, -4.0), (-5.0, -5.0)]),
                vec![],
            ),
        ])
        .into();
        let multiline: Geometry<f64> = MultiLineString::new(vec![
            LineString::from(vec![(0.0, 0.0), (3.0, 3.0)]),
            LineString::from(vec![(-2.0, 7.0), (9.0, -1.0)]),
        ])
        .into();
        let point: Geometry<f64> = Point::from(coord! { x: 2.5, y: -6.0 }).into();

        let cases: [(Geometry<f64>, [f64; 4]); 5] = [
            (point, [2.5, 2.5, -6.0, -6.0]),
            (line, [0.0, 10.0, -3.0, 8.0]),
            (polygon, [0.0, 6.0, 0.0, 5.0]),
            (multipolygon, [-5.0, 1.0, -5.0, 1.0]),
            (multiline, [-2.0, 9.0, -1.0, 7.0]),
        ];

        for (geom, bounds) in cases {
            for little_endian in [true, false] {
                let mut blob = encode_header(4326, &Envelope::Xy(bounds), false, false);
                blob.extend_from_slice(&wkb_body(&geom, little_endian));
                let g = GpbGeometry::parse(&blob).unwrap();
                let (bx0, bx1, by0, by1) = g.header().envelope.xy_bounds().unwrap();
                assert_eq!(
                    g.xy_envelope(),
                    Some([bx0, bx1, by0, by1]),
                    "traversal must match header envelope (little_endian={little_endian})"
                );
                assert!(!g.is_empty());
            }
        }
    }

    #[test]
    #[cfg(feature = "geo-types")]
    fn geometry_collection_traversal() {
        use geo_types::{Geometry, GeometryCollection, LineString, Point};
        let members: Vec<Geometry<f64>> = vec![
            Geometry::Point(Point::new(1.0, 1.0)),
            Geometry::LineString(LineString::from(vec![(-3.0, 0.0), (4.0, 9.0)])),
        ];
        let gc = Geometry::GeometryCollection(GeometryCollection::new_from(members));
        let mut blob = encode_header(4326, &Envelope::None, false, false);
        blob.extend_from_slice(&wkb_body(&gc, true));
        let g = GpbGeometry::parse(&blob).unwrap();
        assert_eq!(g.xy_envelope(), Some([-3.0, 4.0, 0.0, 9.0]));
    }

    /// A hand-built big-endian WKB LineString body, no `geo-types` needed.
    #[test]
    fn big_endian_linestring_traversal() {
        let mut body = vec![0u8]; // big-endian
        body.extend_from_slice(&2u32.to_be_bytes()); // LineString
        body.extend_from_slice(&2u32.to_be_bytes()); // 2 points
        for (x, y) in [(1.0f64, 2.0f64), (5.0, -1.0)] {
            body.extend_from_slice(&x.to_be_bytes());
            body.extend_from_slice(&y.to_be_bytes());
        }
        let blob = gpb(&body);
        let g = GpbGeometry::parse(&blob).unwrap();
        assert_eq!(g.xy_envelope(), Some([1.0, 5.0, -1.0, 2.0]));
    }

    #[test]
    fn declared_type_matching_rules() {
        use GeometryType::*;
        // Exact match.
        assert!(geometry_type_matches(Point, Point));
        assert!(geometry_type_matches(LineString, LineString));
        // GEOMETRY accepts anything.
        assert!(geometry_type_matches(Point, Geometry));
        assert!(geometry_type_matches(CircularString, Geometry));
        // GEOMETRYCOLLECTION accepts collections only.
        assert!(geometry_type_matches(MultiPoint, GeometryCollection));
        assert!(geometry_type_matches(MultiSurface, GeometryCollection));
        assert!(geometry_type_matches(
            GeometryCollection,
            GeometryCollection
        ));
        assert!(!geometry_type_matches(Point, GeometryCollection));
        // A LINESTRING does not satisfy a MULTILINESTRING column.
        assert!(!geometry_type_matches(LineString, MultiLineString));
        assert!(!geometry_type_matches(Point, LineString));
    }

    #[test]
    fn reads_wkb_type_code_including_curves() {
        // POINT (ISO type 1), little-endian.
        assert_eq!(
            wkb_geometry_type(&wkb_point(0.0, 0.0)).unwrap(),
            GeometryType::Point
        );
        // POINT ZM (ISO type 3001) still classifies as POINT.
        let mut point_zm = vec![1u8];
        point_zm.extend_from_slice(&3001u32.to_le_bytes());
        assert_eq!(wkb_geometry_type(&point_zm).unwrap(), GeometryType::Point);
        // CIRCULARSTRING (ISO type 8): the wkb reader cannot parse this, but
        // the raw type-code reader classifies it.
        let mut circular = vec![1u8];
        circular.extend_from_slice(&8u32.to_le_bytes());
        assert_eq!(
            wkb_geometry_type(&circular).unwrap(),
            GeometryType::CircularString
        );
        // Truncated and unknown-code inputs are typed errors, not panics.
        assert!(matches!(
            wkb_geometry_type(b"\x01\x00"),
            Err(GeometryError::TruncatedWkb)
        ));
        let mut unknown = vec![1u8];
        unknown.extend_from_slice(&99u32.to_le_bytes());
        assert!(matches!(
            wkb_geometry_type(&unknown),
            Err(GeometryError::UnknownWkbType(99))
        ));
    }

    #[test]
    fn matches_declared_on_parsed_geometry() {
        let blob = gpb(&wkb_point(1.0, 2.0));
        let g = GpbGeometry::parse(&blob).unwrap();
        assert_eq!(g.geometry_type(), GeometryType::Point);
        assert!(g.matches_declared(GeometryType::Point));
        assert!(g.matches_declared(GeometryType::Geometry));
        assert!(!g.matches_declared(GeometryType::LineString));
    }
}
