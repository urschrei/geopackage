//! Column and geometry type vocabulary.
//!
//! GeoPackage constrains user data table columns to the types of spec
//! Table 1 ("GeoPackage Data Types", Requirement 5) and geometry columns to
//! the type names of Annex G ("Geometry Types (Normative)"): the core set
//! plus the non-linear types of the `gpkg_geom_<TYPE>` extensions.

use std::fmt;

/// A geometry type name as used in `gpkg_geometry_columns.geometry_type_name`
/// and the `gpkg_geom_<TYPE>` extension names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GeometryType {
    /// Any geometry ("GEOMETRY"), the heterogeneous supertype.
    Geometry,
    /// "POINT".
    Point,
    /// "LINESTRING".
    LineString,
    /// "POLYGON".
    Polygon,
    /// "MULTIPOINT".
    MultiPoint,
    /// "MULTILINESTRING".
    MultiLineString,
    /// "MULTIPOLYGON".
    MultiPolygon,
    /// "GEOMETRYCOLLECTION".
    GeometryCollection,
    /// "CIRCULARSTRING" (extension, non-linear).
    CircularString,
    /// "COMPOUNDCURVE" (extension, non-linear).
    CompoundCurve,
    /// "CURVEPOLYGON" (extension, non-linear).
    CurvePolygon,
    /// "MULTICURVE" (extension, non-linear).
    MultiCurve,
    /// "MULTISURFACE" (extension, non-linear).
    MultiSurface,
    /// "CURVE" (extension, abstract non-instantiable supertype).
    Curve,
    /// "SURFACE" (extension, abstract non-instantiable supertype).
    Surface,
}

impl GeometryType {
    /// Parse a geometry type name, case-insensitively.
    ///
    /// The spec writes the names in upper case; other cases appear in the
    /// wild and are accepted on read.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name.to_ascii_uppercase().as_str() {
            "GEOMETRY" => Self::Geometry,
            "POINT" => Self::Point,
            "LINESTRING" => Self::LineString,
            "POLYGON" => Self::Polygon,
            "MULTIPOINT" => Self::MultiPoint,
            "MULTILINESTRING" => Self::MultiLineString,
            "MULTIPOLYGON" => Self::MultiPolygon,
            "GEOMETRYCOLLECTION" => Self::GeometryCollection,
            "CIRCULARSTRING" => Self::CircularString,
            "COMPOUNDCURVE" => Self::CompoundCurve,
            "CURVEPOLYGON" => Self::CurvePolygon,
            "MULTICURVE" => Self::MultiCurve,
            "MULTISURFACE" => Self::MultiSurface,
            "CURVE" => Self::Curve,
            "SURFACE" => Self::Surface,
            _ => return None,
        })
    }

    /// The canonical (upper-case) spec name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Geometry => "GEOMETRY",
            Self::Point => "POINT",
            Self::LineString => "LINESTRING",
            Self::Polygon => "POLYGON",
            Self::MultiPoint => "MULTIPOINT",
            Self::MultiLineString => "MULTILINESTRING",
            Self::MultiPolygon => "MULTIPOLYGON",
            Self::GeometryCollection => "GEOMETRYCOLLECTION",
            Self::CircularString => "CIRCULARSTRING",
            Self::CompoundCurve => "COMPOUNDCURVE",
            Self::CurvePolygon => "CURVEPOLYGON",
            Self::MultiCurve => "MULTICURVE",
            Self::MultiSurface => "MULTISURFACE",
            Self::Curve => "CURVE",
            Self::Surface => "SURFACE",
        }
    }

    /// Whether this type requires a `gpkg_geom_<TYPE>` extension row
    /// (everything beyond the core eight).
    pub fn is_extension(&self) -> bool {
        !matches!(
            self,
            Self::Geometry
                | Self::Point
                | Self::LineString
                | Self::Polygon
                | Self::MultiPoint
                | Self::MultiLineString
                | Self::MultiPolygon
                | Self::GeometryCollection
        )
    }
}

impl fmt::Display for GeometryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A declared column type from spec Table 1.
///
/// Integer widths (`TINYINT` … `INTEGER`) and the `BOOLEAN`, `DATE`, and
/// `DATETIME` types are storage-class INTEGER/TEXT under SQLite's type
/// affinity; the declared type constrains interpretation, not storage.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ColumnType {
    /// `BOOLEAN`: 0 or 1, stored as INTEGER.
    Boolean,
    /// `TINYINT`: 8-bit signed integer.
    TinyInt,
    /// `SMALLINT`: 16-bit signed integer.
    SmallInt,
    /// `MEDIUMINT`: 32-bit signed integer.
    MediumInt,
    /// `INT` / `INTEGER`: 64-bit signed integer.
    Integer,
    /// `FLOAT`: 32-bit IEEE 754 float.
    Float,
    /// `DOUBLE` / `REAL`: 64-bit IEEE 754 float.
    Double,
    /// `TEXT` / `TEXT(N)`: UTF-8 string, optionally with a declared maximum
    /// character length (informative; SQLite does not enforce it).
    Text(Option<u32>),
    /// `BLOB` / `BLOB(N)`: byte array, optionally with a declared maximum
    /// length.
    Blob(Option<u32>),
    /// `DATE`: ISO 8601 date string.
    Date,
    /// `DATETIME`: ISO 8601 UTC datetime string.
    DateTime,
    /// A geometry type name used as a declared column type.
    Geometry(GeometryType),
}

impl ColumnType {
    /// Parse a declared SQL column type as it appears in
    /// `PRAGMA table_info`, case-insensitively, including `TEXT(N)`/`BLOB(N)`
    /// length suffixes.
    ///
    /// Returns `None` for declared types outside the spec vocabulary (which
    /// SQLite happily stores; lenient readers keep the raw string instead).
    pub fn parse(declared: &str) -> Option<Self> {
        let s = declared.trim().to_ascii_uppercase();
        if let Some(rest) = s.strip_prefix("TEXT") {
            return parse_len_suffix(rest).map(Self::Text);
        }
        if let Some(rest) = s.strip_prefix("BLOB") {
            return parse_len_suffix(rest).map(Self::Blob);
        }
        Some(match s.as_str() {
            "BOOLEAN" => Self::Boolean,
            "TINYINT" => Self::TinyInt,
            "SMALLINT" => Self::SmallInt,
            "MEDIUMINT" => Self::MediumInt,
            "INT" | "INTEGER" => Self::Integer,
            "FLOAT" => Self::Float,
            "DOUBLE" | "REAL" => Self::Double,
            "DATE" => Self::Date,
            "DATETIME" => Self::DateTime,
            other => Self::Geometry(GeometryType::parse(other)?),
        })
    }

    /// The canonical DDL spelling of this type.
    pub fn ddl_name(&self) -> String {
        match self {
            Self::Boolean => "BOOLEAN".into(),
            Self::TinyInt => "TINYINT".into(),
            Self::SmallInt => "SMALLINT".into(),
            Self::MediumInt => "MEDIUMINT".into(),
            Self::Integer => "INTEGER".into(),
            Self::Float => "FLOAT".into(),
            Self::Double => "DOUBLE".into(),
            Self::Text(None) => "TEXT".into(),
            Self::Text(Some(n)) => format!("TEXT({n})"),
            Self::Blob(None) => "BLOB".into(),
            Self::Blob(Some(n)) => format!("BLOB({n})"),
            Self::Date => "DATE".into(),
            Self::DateTime => "DATETIME".into(),
            Self::Geometry(g) => g.as_str().into(),
        }
    }
}

/// Parse the `(N)` suffix of `TEXT(N)`/`BLOB(N)`; empty means no length.
fn parse_len_suffix(rest: &str) -> Option<Option<u32>> {
    let rest = rest.trim();
    if rest.is_empty() {
        return Some(None);
    }
    let inner = rest.strip_prefix('(')?.strip_suffix(')')?.trim();
    inner.parse::<u32>().ok().map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_type_roundtrip() {
        for name in [
            "GEOMETRY",
            "POINT",
            "LINESTRING",
            "POLYGON",
            "MULTIPOINT",
            "MULTILINESTRING",
            "MULTIPOLYGON",
            "GEOMETRYCOLLECTION",
            "CIRCULARSTRING",
            "COMPOUNDCURVE",
            "CURVEPOLYGON",
            "MULTICURVE",
            "MULTISURFACE",
            "CURVE",
            "SURFACE",
        ] {
            let t = GeometryType::parse(name).unwrap();
            assert_eq!(t.as_str(), name);
        }
        assert_eq!(
            GeometryType::parse("point"),
            Some(GeometryType::Point),
            "case-insensitive"
        );
        assert_eq!(GeometryType::parse("TRIANGLE"), None);
    }

    #[test]
    fn extension_types_flagged() {
        assert!(!GeometryType::Point.is_extension());
        assert!(!GeometryType::GeometryCollection.is_extension());
        assert!(GeometryType::CircularString.is_extension());
        assert!(GeometryType::Surface.is_extension());
    }

    #[test]
    fn column_type_parse() {
        assert_eq!(ColumnType::parse("BOOLEAN"), Some(ColumnType::Boolean));
        assert_eq!(ColumnType::parse("INT"), Some(ColumnType::Integer));
        assert_eq!(ColumnType::parse("integer"), Some(ColumnType::Integer));
        assert_eq!(ColumnType::parse("REAL"), Some(ColumnType::Double));
        assert_eq!(ColumnType::parse("TEXT"), Some(ColumnType::Text(None)));
        assert_eq!(
            ColumnType::parse("TEXT(50)"),
            Some(ColumnType::Text(Some(50)))
        );
        assert_eq!(
            ColumnType::parse("BLOB(16)"),
            Some(ColumnType::Blob(Some(16)))
        );
        assert_eq!(
            ColumnType::parse("POINT"),
            Some(ColumnType::Geometry(GeometryType::Point))
        );
        assert_eq!(ColumnType::parse("VARCHAR(20)"), None);
        assert_eq!(ColumnType::parse("TEXT(x)"), None);
        assert_eq!(ColumnType::parse("NUMERIC"), None);
    }

    #[test]
    fn ddl_names_roundtrip() {
        for t in [
            ColumnType::Boolean,
            ColumnType::TinyInt,
            ColumnType::SmallInt,
            ColumnType::MediumInt,
            ColumnType::Integer,
            ColumnType::Float,
            ColumnType::Double,
            ColumnType::Text(None),
            ColumnType::Text(Some(10)),
            ColumnType::Blob(None),
            ColumnType::Blob(Some(4)),
            ColumnType::Date,
            ColumnType::DateTime,
            ColumnType::Geometry(GeometryType::MultiPolygon),
        ] {
            assert_eq!(ColumnType::parse(&t.ddl_name()), Some(t.clone()));
        }
    }
}
