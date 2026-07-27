//! Registered extension names (Annex F) and the `scope` values a
//! `gpkg_extensions` row may carry.
//!
//! This is the naming half of the extension mechanism (spec clause 2.3.2,
//! `spec/core/2e_extensions-mechanism.adoc`). Reading a file's catalogue, and
//! deciding what this workspace does about a row it finds there, belong to the
//! `geopackage` crate's `extensions` module.
//!
//! An extension name is `<author>_<extension name>`, case sensitive, with
//! `gpkg` reserved for extensions OGC maintains (Requirement 62). Two names
//! here have other authors: `gdal_aspatial`, which predates the attributes
//! data type, and `related_tables`, which OGC 18-000 registered without an
//! author prefix at all.
//!
//! Annex F numbers the extensions in the order the annex includes them, so the
//! two extensions removed in 2016 still occupy F.2, F.4 and F.5 and everything
//! after them is numbered around the gaps.

use crate::types::GeometryType;

/// `gpkg_extensions.scope`: what an extension affects (Requirement 64).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionScope {
    /// `read-write`: the extension affects both readers and writers.
    ReadWrite,
    /// `write-only`: the extension affects only writers.
    ///
    /// The spec's example is an extension defining a trigger that calls a
    /// non-standard SQL function: triggers fire only on write, so read-only
    /// access can ignore it safely.
    WriteOnly,
    /// A value Requirement 64 does not allow, kept as written.
    Other(String),
}

impl ExtensionScope {
    /// Classify a `scope` column value.
    ///
    /// Requirement 64 fixes the two valid values as lowercase, and anything
    /// else becomes [`ExtensionScope::Other`] rather than an error: this is a
    /// value read from a file someone else wrote.
    pub fn parse(value: &str) -> Self {
        match value {
            "read-write" => Self::ReadWrite,
            "write-only" => Self::WriteOnly,
            other => Self::Other(other.to_owned()),
        }
    }

    /// The `scope` column value.
    pub fn as_str(&self) -> &str {
        match self {
            Self::ReadWrite => "read-write",
            Self::WriteOnly => "write-only",
            Self::Other(value) => value,
        }
    }

    /// Whether a reader has to understand the extension to read the affected
    /// data correctly.
    ///
    /// True for an unrecognised scope value: a value the spec does not define
    /// says nothing about what can be ignored.
    pub fn affects_readers(&self) -> bool {
        !matches!(self, Self::WriteOnly)
    }

    /// Whether a writer has to understand the extension to write the affected
    /// data correctly. True for every scope, including an unrecognised one.
    pub fn affects_writers(&self) -> bool {
        true
    }
}

/// An extension name this workspace can identify.
///
/// Names come from Annex F, from the two extensions the GeoPackage SWG voted
/// to remove on 2016-08-15 (still numbered in the annex, and still present in
/// files written before then), and from `gdal_aspatial`, which is not an OGC
/// extension but is common enough in older GDAL output to be worth naming.
///
/// This is the interpretation of a name, not the name itself: several
/// extensions have been registered under more than one spelling over the
/// years, and all of an extension's spellings map to one variant here.
/// [`Extension::name`] returns the current spelling, so a name read from a
/// file and passed back through this type is normalised rather than preserved.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Extension {
    /// `gpkg_geom_<TYPE>` (Annex F.1): a geometry type beyond the core seven,
    /// registered against the geometry column that holds it.
    GeometryType(GeometryType),
    /// `gpkg_rtree_index` (Annex F.3): the RTree spatial index.
    RtreeIndex,
    /// `gpkg_geometry_type_trigger` (Annex F.4).
    ///
    /// Removed from the standard on 2016-08-15 by SWG vote, over
    /// interoperability concerns. Files written before then still carry it.
    GeometryTypeTrigger,
    /// `gpkg_srs_id_trigger` (Annex F.5). Removed on 2016-08-15, as
    /// [`Extension::GeometryTypeTrigger`] was.
    SrsIdTrigger,
    /// `gpkg_zoom_other` (Annex F.6): zoom levels that do not step by factors
    /// of two.
    ZoomOther,
    /// `gpkg_webp` (Annex F.7): WebP tile payloads.
    Webp,
    /// `gpkg_metadata` (Annex F.8): the `gpkg_metadata` and
    /// `gpkg_metadata_reference` tables.
    Metadata,
    /// `gpkg_schema` (Annex F.9): the `gpkg_data_columns` and
    /// `gpkg_data_column_constraints` tables.
    Schema,
    /// `gpkg_crs_wkt` (Annex F.10): the `definition_12_063` column on
    /// `gpkg_spatial_ref_sys`, holding a WKT2 CRS definition.
    CrsWkt,
    /// `gpkg_crs_wkt_1_1`: version 1.1 of Annex F.10, which adds the `epoch`
    /// column beside `definition_12_063`.
    ///
    /// A file conforming to 1.1 registers both this and [`Extension::CrsWkt`].
    CrsWkt11,
    /// `gpkg_2d_gridded_coverage` (Annex F.11, published separately as OGC
    /// 17-066r1): tile payloads holding gridded values rather than pictures.
    ///
    /// Also matches the two earlier spellings, `gpkg_elevation_tiles` from
    /// before GeoPackage 1.2 and `2d_gridded_coverage` from before 17-066r1
    /// was final.
    GriddedCoverage,
    /// `related_tables` (Annex F.12, published separately as OGC 18-000):
    /// `gpkgext_relations` and the user-defined mapping tables it describes.
    ///
    /// Also matches the `gpkg_related_tables` spelling, which OGC 18-000 uses
    /// in places and which GDAL both reads and writes.
    RelatedTables,
    /// `gdal_aspatial`: GDAL's pre-1.2 convention for a table with no
    /// geometry, superseded by the `attributes` data type.
    ///
    /// Not an OGC extension: the author prefix is `gdal`.
    GdalAspatial,
    /// A name this workspace does not recognise, kept as written.
    Other(String),
}

/// The prefix of an Annex F.1 geometry type extension name.
const GEOM_PREFIX: &str = "gpkg_geom_";

impl Extension {
    /// Identify an `extension_name` column value.
    ///
    /// Names are case sensitive per Requirement 62, and are matched that way,
    /// with one exception: the geometry type in a `gpkg_geom_<TYPE>` name is
    /// parsed the way [`GeometryType::parse`] parses one from
    /// `gpkg_geometry_columns`, which tolerates the spellings that turn up in
    /// the wild. A `gpkg_geom_` name whose type is not one this crate knows,
    /// or is not an extension type, is [`Extension::Other`]: naming a type we
    /// cannot parse would claim an understanding we do not have.
    pub fn from_name(name: &str) -> Self {
        if let Some(geometry_type) = name.strip_prefix(GEOM_PREFIX) {
            return match GeometryType::parse(geometry_type) {
                Some(parsed) if parsed.is_extension() => Self::GeometryType(parsed),
                _ => Self::Other(name.to_owned()),
            };
        }
        match name {
            crate::triggers::EXTENSION_NAME => Self::RtreeIndex,
            "gpkg_geometry_type_trigger" => Self::GeometryTypeTrigger,
            "gpkg_srs_id_trigger" => Self::SrsIdTrigger,
            crate::tiles::ZOOM_OTHER_EXTENSION_NAME => Self::ZoomOther,
            crate::tiles::WEBP_EXTENSION_NAME => Self::Webp,
            "gpkg_metadata" => Self::Metadata,
            "gpkg_schema" => Self::Schema,
            "gpkg_crs_wkt" => Self::CrsWkt,
            "gpkg_crs_wkt_1_1" => Self::CrsWkt11,
            "gpkg_2d_gridded_coverage" | "2d_gridded_coverage" | "gpkg_elevation_tiles" => {
                Self::GriddedCoverage
            }
            "related_tables" | "gpkg_related_tables" => Self::RelatedTables,
            "gdal_aspatial" => Self::GdalAspatial,
            other => Self::Other(other.to_owned()),
        }
    }

    /// The current `extension_name` spelling.
    ///
    /// For an extension registered under more than one name over the years
    /// this is the newest spelling, which is not necessarily the one the file
    /// being read carries.
    pub fn name(&self) -> String {
        match self {
            Self::GeometryType(geometry_type) => {
                format!("{GEOM_PREFIX}{}", geometry_type.as_str())
            }
            Self::RtreeIndex => crate::triggers::EXTENSION_NAME.to_owned(),
            Self::GeometryTypeTrigger => "gpkg_geometry_type_trigger".to_owned(),
            Self::SrsIdTrigger => "gpkg_srs_id_trigger".to_owned(),
            Self::ZoomOther => crate::tiles::ZOOM_OTHER_EXTENSION_NAME.to_owned(),
            Self::Webp => crate::tiles::WEBP_EXTENSION_NAME.to_owned(),
            Self::Metadata => "gpkg_metadata".to_owned(),
            Self::Schema => "gpkg_schema".to_owned(),
            Self::CrsWkt => "gpkg_crs_wkt".to_owned(),
            Self::CrsWkt11 => "gpkg_crs_wkt_1_1".to_owned(),
            Self::GriddedCoverage => "gpkg_2d_gridded_coverage".to_owned(),
            Self::RelatedTables => "related_tables".to_owned(),
            Self::GdalAspatial => "gdal_aspatial".to_owned(),
            Self::Other(name) => name.clone(),
        }
    }

    /// What this workspace can do with the extension.
    ///
    /// This lives beside the names rather than in the `geopackage` crate so
    /// that the match is exhaustive: adding a variant above stops compiling
    /// until its support level is stated, where a wildcard in another crate
    /// would silently classify it as [`ExtensionSupport::Unrecognised`].
    pub fn support(&self) -> ExtensionSupport {
        match self {
            Self::RtreeIndex | Self::ZoomOther | Self::Webp => ExtensionSupport::Implemented,
            // The CRS WKT columns are written by `GeoPackage::add_epsg_srs`
            // but are not surfaced on read, so this is not yet an extension
            // the workspace implements in the round.
            Self::CrsWkt
            | Self::CrsWkt11
            | Self::Metadata
            | Self::Schema
            | Self::RelatedTables
            | Self::GriddedCoverage
            | Self::GdalAspatial
            | Self::GeometryType(_) => ExtensionSupport::Known,
            Self::GeometryTypeTrigger | Self::SrsIdTrigger => ExtensionSupport::Removed,
            Self::Other(_) => ExtensionSupport::Unrecognised,
        }
    }

    /// Whether OGC removed this extension from the standard.
    ///
    /// Both removals happened on 2016-08-15, in the same SWG vote, over
    /// interoperability concerns. A file may still carry them, and this
    /// workspace reads such a file, but never writes either.
    pub fn is_removed(&self) -> bool {
        self.support() == ExtensionSupport::Removed
    }
}

/// What this workspace can do with a registered extension.
///
/// This describes the implementation, not the extension: an extension is
/// [`ExtensionSupport::Known`] when we can say what it is and which tables it
/// owns, which is enough to leave it alone safely, and
/// [`ExtensionSupport::Implemented`] only when we read and write it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExtensionSupport {
    /// Read and written by this workspace.
    Implemented,
    /// Identified, but not read or written.
    ///
    /// The extension's own tables are left untouched, and writing to an
    /// ordinary feature or tile table in the same file is unaffected, because
    /// what these extensions add sits beside the feature data rather than
    /// inside it.
    Known,
    /// Removed from the standard by the SWG vote of 2016-08-15.
    ///
    /// Tolerated on read, never written.
    Removed,
    /// Not recognised.
    ///
    /// Nothing can be assumed about what such an extension requires of a
    /// writer, which is what separates it from [`ExtensionSupport::Known`].
    Unrecognised,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for extension in [
            Extension::GeometryType(GeometryType::CircularString),
            Extension::RtreeIndex,
            Extension::GeometryTypeTrigger,
            Extension::SrsIdTrigger,
            Extension::ZoomOther,
            Extension::Webp,
            Extension::Metadata,
            Extension::Schema,
            Extension::CrsWkt,
            Extension::CrsWkt11,
            Extension::GriddedCoverage,
            Extension::RelatedTables,
            Extension::GdalAspatial,
            Extension::Other("acme_something".to_owned()),
        ] {
            assert_eq!(Extension::from_name(&extension.name()), extension);
        }
    }

    #[test]
    fn historical_spellings_map_to_the_current_extension() {
        for name in [
            "gpkg_elevation_tiles",
            "2d_gridded_coverage",
            "gpkg_2d_gridded_coverage",
        ] {
            assert_eq!(
                Extension::from_name(name),
                Extension::GriddedCoverage,
                "{name}"
            );
        }
        for name in ["related_tables", "gpkg_related_tables"] {
            assert_eq!(
                Extension::from_name(name),
                Extension::RelatedTables,
                "{name}"
            );
        }
    }

    #[test]
    fn geometry_type_names_need_an_extension_type() {
        assert_eq!(
            Extension::from_name("gpkg_geom_CIRCULARSTRING"),
            Extension::GeometryType(GeometryType::CircularString)
        );
        // A core type needs no extension, so this name is meaningless rather
        // than an Annex F.1 registration.
        assert_eq!(
            Extension::from_name("gpkg_geom_POINT"),
            Extension::Other("gpkg_geom_POINT".to_owned())
        );
        // TIN and POLYHEDRALSURFACE are Annex G types this crate cannot name,
        // so their registrations stay unrecognised rather than half-understood.
        assert_eq!(
            Extension::from_name("gpkg_geom_TIN"),
            Extension::Other("gpkg_geom_TIN".to_owned())
        );
    }

    #[test]
    fn scope_values_outside_requirement_64_are_kept_as_written() {
        assert_eq!(
            ExtensionScope::parse("read-write"),
            ExtensionScope::ReadWrite
        );
        assert_eq!(
            ExtensionScope::parse("write-only"),
            ExtensionScope::WriteOnly
        );
        // Requirement 64 asks for lowercase, so this is not "read-write".
        let shouty = ExtensionScope::parse("Read-Write");
        assert_eq!(shouty, ExtensionScope::Other("Read-Write".to_owned()));
        assert_eq!(shouty.as_str(), "Read-Write");
        assert!(
            shouty.affects_readers(),
            "an undefined scope excuses nothing"
        );
        assert!(shouty.affects_writers());
    }

    #[test]
    fn write_only_is_the_only_scope_a_reader_can_ignore() {
        assert!(!ExtensionScope::WriteOnly.affects_readers());
        assert!(ExtensionScope::WriteOnly.affects_writers());
        assert!(ExtensionScope::ReadWrite.affects_readers());
        assert!(ExtensionScope::ReadWrite.affects_writers());
    }
}
