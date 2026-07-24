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

use geo_traits::{Dimensions, GeometryTrait};
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
}
