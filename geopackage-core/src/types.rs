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

impl GeometryType {
    /// Every type, in the order this enum declares them, which is the order of
    /// their ISO WKB base codes.
    const ALL: [Self; 15] = [
        Self::Geometry,
        Self::Point,
        Self::LineString,
        Self::Polygon,
        Self::MultiPoint,
        Self::MultiLineString,
        Self::MultiPolygon,
        Self::GeometryCollection,
        Self::CircularString,
        Self::CompoundCurve,
        Self::CurvePolygon,
        Self::MultiCurve,
        Self::MultiSurface,
        Self::Curve,
        Self::Surface,
    ];

    /// The type an ISO WKB base code names, or `None` for a code that is not a
    /// GeoPackage geometry type.
    ///
    /// The base code is the type code with its dimension offset removed: ISO
    /// WKB adds 1000 for Z, 2000 for M and 3000 for ZM.
    pub(crate) fn from_wkb_base(base: u32) -> Option<Self> {
        Self::ALL.get(base as usize).copied()
    }

    /// Which bit of a [`GeometryTypeSet`] stands for this type.
    const fn bit(self) -> u16 {
        1 << match self {
            Self::Geometry => 0,
            Self::Point => 1,
            Self::LineString => 2,
            Self::Polygon => 3,
            Self::MultiPoint => 4,
            Self::MultiLineString => 5,
            Self::MultiPolygon => 6,
            Self::GeometryCollection => 7,
            Self::CircularString => 8,
            Self::CompoundCurve => 9,
            Self::CurvePolygon => 10,
            Self::MultiCurve => 11,
            Self::MultiSurface => 12,
            Self::Curve => 13,
            Self::Surface => 14,
        }
    }
}

impl fmt::Display for GeometryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A set of geometry types, held as a bitset so that walking a geometry can
/// accumulate one without allocating.
///
/// This exists to report what a geometry body actually contains, which is not
/// the same as the type its column declares: a `MULTICURVE` may hold
/// `CIRCULARSTRING`s and a `MULTISURFACE` may hold `CURVEPOLYGON`s, and under
/// Annex F.1 Requirement 67 each non-linear type present needs its own
/// `gpkg_geom_<TYPE>` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GeometryTypeSet(u16);

impl GeometryTypeSet {
    /// The empty set.
    pub const fn new() -> Self {
        Self(0)
    }

    /// Add one type.
    pub fn insert(&mut self, ty: GeometryType) {
        self.0 |= ty.bit();
    }

    /// Add everything `other` holds.
    pub fn extend(&mut self, other: Self) {
        self.0 |= other.0;
    }

    /// Whether `ty` is in the set.
    pub fn contains(self, ty: GeometryType) -> bool {
        self.0 & ty.bit() != 0
    }

    /// Whether the set holds nothing.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The types in the set, in the order [`GeometryType`] declares them.
    pub fn iter(self) -> impl Iterator<Item = GeometryType> {
        GeometryType::ALL
            .into_iter()
            .filter(move |ty| self.contains(*ty))
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

/// The presence constraint on a geometry's `z` or `m` dimension, as recorded
/// in the `z` and `m` columns of `gpkg_geometry_columns` (spec Table 21,
/// Requirement 27). Those columns hold the code `0`, `1`, or `2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ZmFlag {
    /// `0`: the dimension is prohibited (geometries must not carry it).
    Prohibited,
    /// `1`: the dimension is mandatory (every geometry carries it).
    Mandatory,
    /// `2`: the dimension is optional (geometries may or may not carry it).
    Optional,
}

impl ZmFlag {
    /// Interpret a raw `z`/`m` column code. Returns `None` for any value
    /// other than `0`, `1`, or `2`.
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Self::Prohibited,
            1 => Self::Mandatory,
            2 => Self::Optional,
            _ => return None,
        })
    }

    /// The raw `z`/`m` column code (`0`, `1`, or `2`) for this constraint.
    pub fn code(&self) -> u8 {
        match self {
            Self::Prohibited => 0,
            Self::Mandatory => 1,
            Self::Optional => 2,
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
    fn zm_flag_codes() {
        for f in [ZmFlag::Prohibited, ZmFlag::Mandatory, ZmFlag::Optional] {
            assert_eq!(ZmFlag::from_code(f.code()), Some(f));
        }
        assert_eq!(ZmFlag::from_code(0), Some(ZmFlag::Prohibited));
        assert_eq!(ZmFlag::from_code(1), Some(ZmFlag::Mandatory));
        assert_eq!(ZmFlag::from_code(2), Some(ZmFlag::Optional));
        assert_eq!(ZmFlag::from_code(3), None);
        assert_eq!(ZmFlag::from_code(255), None);
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
