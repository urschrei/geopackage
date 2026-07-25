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
//! upstreamed `gpb` feature in georust `wkb` itself; until that lands, this
//! module is ours.
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
use wkb::Endianness;
use wkb::reader::{
    GeometryCollection, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon, Wkb,
};
use wkb::writer::{WriteOptions, write_geometry};

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
    /// The geometry could not be encoded to WKB: the georust `wkb` writer
    /// rejected it (for example a coordinate dimension it cannot serialise).
    #[error("failed to encode geometry to WKB")]
    EncodeWkb(#[source] wkb::error::WkbError),
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
        // `parse_header` guarantees `offset <= blob.len()`; `get` keeps this
        // panic-free, and an (impossible) out-of-range offset yields an empty
        // body that `Wkb::try_new` rejects as a typed error rather than a panic.
        let body = blob.get(offset..).unwrap_or_default();
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
        visit_coords(&self.wkb, &mut |x, y, _| bounds.add(x, y));
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
    // Byte-order marker plus the four-byte type code; a shorter body is
    // truncated. `..` ignores any coordinate bytes that follow.
    let &[order, c0, c1, c2, c3, ..] = wkb_body else {
        return Err(GeometryError::TruncatedWkb);
    };
    let little_endian = match order {
        0 => false,
        1 => true,
        _ => return Err(GeometryError::TruncatedWkb),
    };
    let bytes = [c0, c1, c2, c3];
    let code = if little_endian {
        u32::from_le_bytes(bytes)
    } else {
        u32::from_be_bytes(bytes)
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

/// The GPB envelope to write for a geometry, and whether the geometry is empty.
///
/// This crate's writer always emits an envelope, so readers and the `ST_*`
/// functions get the bounds without traversing the WKB body.
///
/// The envelope is [`Envelope::Xyz`] when the geometry carries a Z dimension
/// (including for a single point), otherwise [`Envelope::Xy`]. An M dimension
/// never widens the envelope: readers and the rtree only use X, Y (and, for
/// 3D, Z) bounds. A geometry with no finite coordinate is *empty*: the returned
/// envelope is [`Envelope::None`] and the boolean is `true`, so the encoder
/// omits the envelope and sets the header empty flag.
///
/// [`Envelope::Xy`]: crate::gpb::Envelope::Xy
/// [`Envelope::Xyz`]: crate::gpb::Envelope::Xyz
/// [`Envelope::None`]: crate::gpb::Envelope::None
pub fn write_envelope<G: GeometryTrait<T = f64>>(geom: &G) -> (gpb::Envelope, bool) {
    let mut bounds = XyzBounds::new();
    visit_coords(geom, &mut |x, y, z| bounds.add(x, y, z));
    let Some([min_x, max_x, min_y, max_y]) = bounds.xy_bounds() else {
        return (gpb::Envelope::None, true);
    };
    match bounds.z_bounds() {
        Some((min_z, max_z)) => (
            gpb::Envelope::Xyz([min_x, max_x, min_y, max_y, min_z, max_z]),
            false,
        ),
        None => (gpb::Envelope::Xy([min_x, max_x, min_y, max_y]), false),
    }
}

/// Encode a GPB blob from a body that is already ISO WKB, without
/// re-serialising it.
///
/// [`encode_gpb`] writes the body out through the `wkb` writer, which is what a
/// caller holding a geometry object needs. A caller holding WKB bytes already,
/// which is what a GeoArrow column is, does not: the GPB body *is* ISO WKB, so
/// the bytes can be copied after the header. This is the write-side counterpart
/// of reading a geometry column as WKB by skipping the header.
///
/// The bytes are still parsed, for two reasons that are not optional. The
/// envelope has to be computed for the header, which always carries one, and
/// for the spatial index, which needs a coordinate traversal either way. And
/// parsing is what rejects a body that is not ISO WKB, such as PostGIS EWKB,
/// which would otherwise be copied verbatim into a file claiming to be
/// conformant.
///
/// Only the geometry's own extent is copied, not any trailing bytes the input
/// slice may carry beyond it.
///
/// Returns the blob, its XY envelope as [`encode_gpb`] does, and the dimensions
/// the body carries. The dimensions come back because the caller has to check
/// them against the column's `z`/`m` constraints, and parsing twice to learn
/// them would undo the point of this function.
///
/// # Errors
///
/// [`GeometryError`] if `wkb_body` is not a geometry the `wkb` reader accepts.
pub fn encode_gpb_from_wkb(wkb_body: &[u8], srs_id: i32) -> Result<EncodedGpb, GeometryError> {
    let geometry = Wkb::try_new(wkb_body)?;
    let (envelope, empty) = write_envelope(&geometry);
    let xy_envelope = envelope
        .xy_bounds()
        .map(|(min_x, max_x, min_y, max_y)| [min_x, max_x, min_y, max_y]);
    let body = geometry.buf();
    let mut blob = gpb::encode_header(srs_id, &envelope, empty, false);
    blob.reserve(body.len());
    blob.extend_from_slice(body);
    Ok(EncodedGpb {
        blob,
        xy_envelope,
        dimensions: geometry.dim(),
    })
}

/// What [`encode_gpb_from_wkb`] produced.
#[derive(Debug, Clone, PartialEq)]
pub struct EncodedGpb {
    /// The complete GPB blob: header followed by the ISO WKB body.
    pub blob: Vec<u8>,
    /// The geometry's XY envelope, or `None` when it is empty.
    pub xy_envelope: Option<[f64; 4]>,
    /// The dimensions the WKB body declares, for checking against the column.
    pub dimensions: Dimensions,
}

/// Encode a geometry as a complete GeoPackage Binary (GPB) blob: an
/// always-little-endian header ([`gpb::encode_header`]) with an envelope per
/// [`write_envelope`], followed by the little-endian ISO WKB body written by
/// the georust `wkb` crate.
///
/// `srs_id` is written into the header (the geometry column's spatial reference
/// system). Returns the blob and its XY envelope `[min_x, max_x, min_y, max_y]`
/// (`None` for an empty geometry) so a caller can fold it into a running
/// bounding box without re-traversing the geometry.
///
/// # Errors
///
/// [`GeometryError::EncodeWkb`] if the `wkb` writer cannot serialise the
/// geometry.
pub fn encode_gpb<G: GeometryTrait<T = f64>>(
    geom: &G,
    srs_id: i32,
) -> Result<(Vec<u8>, Option<[f64; 4]>), GeometryError> {
    let (envelope, empty) = write_envelope(geom);
    let xy = envelope
        .xy_bounds()
        .map(|(min_x, max_x, min_y, max_y)| [min_x, max_x, min_y, max_y]);
    let mut blob = gpb::encode_header(srs_id, &envelope, empty, false);
    let options = WriteOptions {
        endianness: Endianness::LittleEndian,
    };
    write_geometry(&mut blob, geom, &options).map_err(GeometryError::EncodeWkb)?;
    Ok((blob, xy))
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

/// Accumulator for an XY bounding box plus, when present, a Z bounds range.
/// Feeds [`write_envelope`]'s always-envelope policy.
#[derive(Debug, Clone, Copy)]
struct XyzBounds {
    xy: XyBounds,
    min_z: f64,
    max_z: f64,
    seen_z: bool,
}

impl XyzBounds {
    fn new() -> Self {
        Self {
            xy: XyBounds::new(),
            min_z: f64::INFINITY,
            max_z: f64::NEG_INFINITY,
            seen_z: false,
        }
    }

    fn add(&mut self, x: f64, y: f64, z: Option<f64>) {
        self.xy.add(x, y);
        if let Some(z) = z
            && z.is_finite()
        {
            self.min_z = self.min_z.min(z);
            self.max_z = self.max_z.max(z);
            self.seen_z = true;
        }
    }

    /// The XY bounds, or `None` when no finite XY coordinate was seen (an
    /// empty geometry).
    fn xy_bounds(&self) -> Option<[f64; 4]> {
        self.xy.finish()
    }

    /// The Z range, when any coordinate carried a finite Z.
    fn z_bounds(&self) -> Option<(f64, f64)> {
        self.seen_z.then_some((self.min_z, self.max_z))
    }
}

/// Read a coordinate's X and Y and, when it carries a Z dimension, its Z.
///
/// Only [`Dimensions::Xyz`] and [`Dimensions::Xyzm`] place Z at index 2;
/// [`Dimensions::Xym`] puts M there, so Z reads as `None` for it.
fn coord_xyz(coord: &impl CoordTrait<T = f64>) -> (f64, f64, Option<f64>) {
    let z = match coord.dim() {
        Dimensions::Xyz | Dimensions::Xyzm => coord.nth(2),
        _ => None,
    };
    (coord.x(), coord.y(), z)
}

fn visit_point(point: &impl PointTrait<T = f64>, visit: &mut impl FnMut(f64, f64, Option<f64>)) {
    if let Some(coord) = point.coord() {
        let (x, y, z) = coord_xyz(&coord);
        visit(x, y, z);
    }
}

fn visit_line_string(
    line_string: &impl LineStringTrait<T = f64>,
    visit: &mut impl FnMut(f64, f64, Option<f64>),
) {
    for coord in line_string.coords() {
        let (x, y, z) = coord_xyz(&coord);
        visit(x, y, z);
    }
}

fn visit_polygon(
    polygon: &impl PolygonTrait<T = f64>,
    visit: &mut impl FnMut(f64, f64, Option<f64>),
) {
    if let Some(exterior) = polygon.exterior() {
        visit_line_string(&exterior, visit);
    }
    for interior in polygon.interiors() {
        visit_line_string(&interior, visit);
    }
}

/// Walk a geometry through the `geo-traits` interface, passing every
/// coordinate's `(x, y, z?)` to `visit`. Recurses into geometry collections.
/// The XY-only accumulation (read-path envelope) ignores the third argument;
/// the write-path envelope uses it.
fn visit_coords<G: GeometryTrait<T = f64>>(
    geom: &G,
    visit: &mut impl FnMut(f64, f64, Option<f64>),
) {
    match geom.as_type() {
        GtGeometryType::Point(point) => visit_point(point, visit),
        GtGeometryType::LineString(line_string) => visit_line_string(line_string, visit),
        GtGeometryType::Polygon(polygon) => visit_polygon(polygon, visit),
        GtGeometryType::MultiPoint(multi_point) => {
            for point in multi_point.points() {
                visit_point(&point, visit);
            }
        }
        GtGeometryType::MultiLineString(multi_line_string) => {
            for line_string in multi_line_string.line_strings() {
                visit_line_string(&line_string, visit);
            }
        }
        GtGeometryType::MultiPolygon(multi_polygon) => {
            for polygon in multi_polygon.polygons() {
                visit_polygon(&polygon, visit);
            }
        }
        GtGeometryType::GeometryCollection(collection) => {
            for member in collection.geometries() {
                visit_coords(&member, visit);
            }
        }
        // The `wkb` reader never yields these, but the traversal is generic
        // over any `GeometryTrait`, so they are handled rather than ignored.
        GtGeometryType::Rect(rect) => {
            let (min, max) = (rect.min(), rect.max());
            let (min_x, min_y, min_z) = coord_xyz(&min);
            let (max_x, max_y, max_z) = coord_xyz(&max);
            visit(min_x, min_y, min_z);
            visit(max_x, max_y, max_z);
        }
        GtGeometryType::Triangle(triangle) => {
            for coord in triangle.coords() {
                let (x, y, z) = coord_xyz(&coord);
                visit(x, y, z);
            }
        }
        GtGeometryType::Line(line) => {
            for coord in line.coords() {
                let (x, y, z) = coord_xyz(&coord);
                visit(x, y, z);
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
    #[expect(
        clippy::float_cmp,
        reason = "asserting the exact bit-level round-trip of the coordinate through WKB; the values are written and read as literals, so exact equality is the property under test"
    )]
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
        GpbGeometry::parse(b"").unwrap_err();
        GpbGeometry::parse(b"GP").unwrap_err();
        // Valid header, empty WKB body.
        GpbGeometry::parse(&gpb(&[])).unwrap_err();
        // Valid header, WKB byte-order marker only.
        GpbGeometry::parse(&gpb(&[1])).unwrap_err();
        // Valid header, truncated point coordinates.
        GpbGeometry::parse(&gpb(&wkb_point(1.0, 2.0)[..10])).unwrap_err();
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

    /// A little-endian ISO WKB `POINT Z` body (type code 1001).
    fn wkb_point_z(x: f64, y: f64, z: f64) -> Vec<u8> {
        let mut b = vec![1u8];
        b.extend_from_slice(&1001u32.to_le_bytes());
        b.extend_from_slice(&x.to_le_bytes());
        b.extend_from_slice(&y.to_le_bytes());
        b.extend_from_slice(&z.to_le_bytes());
        b
    }

    #[test]
    #[cfg(feature = "geo-types")]
    fn encode_gpb_xy_point_roundtrips() {
        let point = geo_types::Point::new(3.0, 4.0);
        let (blob, xy) = encode_gpb(&point, 4326).unwrap();
        assert_eq!(xy, Some([3.0, 3.0, 4.0, 4.0]));
        let g = GpbGeometry::parse(&blob).unwrap();
        assert_eq!(g.header().srs_id, 4326);
        assert_eq!(g.header().envelope, Envelope::Xy([3.0, 3.0, 4.0, 4.0]));
        assert!(!g.header().empty);
        assert_eq!(g.geometry_type(), GeometryType::Point);
    }

    #[test]
    #[cfg(feature = "geo-types")]
    fn encode_gpb_linestring_envelope() {
        let ls = geo_types::LineString::from(vec![(0.0, 0.0), (10.0, -3.0), (4.0, 8.0)]);
        let (blob, xy) = encode_gpb(&ls, 4326).unwrap();
        assert_eq!(xy, Some([0.0, 10.0, -3.0, 8.0]));
        let g = GpbGeometry::parse(&blob).unwrap();
        assert_eq!(g.header().envelope, Envelope::Xy([0.0, 10.0, -3.0, 8.0]));
        assert_eq!(g.geometry_type(), GeometryType::LineString);
    }

    #[test]
    fn encode_gpb_z_point_writes_xyz_envelope() {
        // A Z geometry (built as a GpbGeometry) re-encodes with an XYZ envelope
        // and an XYZ WKB body: the writer always emits an envelope, widened to
        // Z when the geometry has one.
        let src_blob = gpb(&wkb_point_z(1.0, 2.0, 9.0));
        let src = GpbGeometry::parse(&src_blob).unwrap();
        assert_eq!(src.dim(), Dimensions::Xyz);
        let (blob, xy) = encode_gpb(&src, 4326).unwrap();
        assert_eq!(xy, Some([1.0, 1.0, 2.0, 2.0]));
        let g = GpbGeometry::parse(&blob).unwrap();
        assert_eq!(
            g.header().envelope,
            Envelope::Xyz([1.0, 1.0, 2.0, 2.0, 9.0, 9.0])
        );
        assert_eq!(g.dim(), Dimensions::Xyz);
        assert_eq!(g.geometry_type(), GeometryType::Point);
    }

    #[test]
    fn write_envelope_empty_point_is_empty() {
        let blob = gpb(&wkb_point(f64::NAN, f64::NAN));
        let g = GpbGeometry::parse(&blob).unwrap();
        let (envelope, empty) = write_envelope(&g);
        assert_eq!(envelope, Envelope::None);
        assert!(empty);
        // Re-encoding preserves the empty flag and omits the envelope.
        let (reblob, xy) = encode_gpb(&g, 4326).unwrap();
        assert_eq!(xy, None);
        let re = GpbGeometry::parse(&reblob).unwrap();
        assert_eq!(re.header().envelope, Envelope::None);
        assert!(re.header().empty);
    }
}

#[cfg(test)]
mod encode_from_wkb_tests {
    use super::*;
    use geo_types::{Geometry, LineString, Point};

    /// The pass-through encoder must produce what the re-serialising one does,
    /// for a body the re-serialising one wrote. That equivalence is the whole
    /// claim: the GPB body is ISO WKB, so copying it is not a shortcut with
    /// different semantics.
    #[test]
    fn agrees_with_encode_gpb() {
        let cases: Vec<Geometry<f64>> = vec![
            Geometry::Point(Point::new(1.5, -2.5)),
            Geometry::LineString(LineString::from(vec![(0.0, 0.0), (10.0, 5.0), (-3.0, 7.5)])),
        ];
        for geometry in cases {
            let (expected, expected_xy) = encode_gpb(&geometry, 4326).unwrap();
            // Take the body the round-trip encoder wrote, and feed it back.
            let (_, offset) = gpb::parse_header(&expected).unwrap();
            let actual = encode_gpb_from_wkb(&expected[offset..], 4326).unwrap();
            assert_eq!(actual.blob, expected, "blob differs");
            assert_eq!(actual.xy_envelope, expected_xy, "envelope differs");
        }
    }

    #[test]
    fn trailing_bytes_are_not_copied() {
        let (blob, _) = encode_gpb(&Point::new(3.0, 4.0), 4326).unwrap();
        let (_, offset) = gpb::parse_header(&blob).unwrap();
        let mut body = blob[offset..].to_vec();
        let clean = encode_gpb_from_wkb(&body, 4326).unwrap().blob;
        body.extend_from_slice(b"trailing rubbish");
        let padded = encode_gpb_from_wkb(&body, 4326).unwrap().blob;
        assert_eq!(clean, padded, "trailing bytes reached the blob");
    }

    #[test]
    fn a_body_that_is_not_wkb_is_rejected() {
        encode_gpb_from_wkb(&[], 4326).unwrap_err();
        encode_gpb_from_wkb(b"not wkb at all", 4326).unwrap_err();
        // A valid byte-order marker followed by an unknown geometry type.
        encode_gpb_from_wkb(&[1, 0xff, 0xff, 0, 0], 4326).unwrap_err();
    }
}
