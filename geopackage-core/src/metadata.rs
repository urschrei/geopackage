//! The `gpkg_metadata` extension's model (Annex F.8): metadata records and
//! what they are attached to.
//!
//! Two tables. `gpkg_metadata` stores a document: its scope, the URI of the
//! standard it follows, its MIME type, and the document itself.
//! `gpkg_metadata_reference` attaches a record to something in the file, at one
//! of five granularities, and optionally names a parent record so the
//! attachments form a hierarchy.
//!
//! This module is the model only: what a file says. The SQL that reads and
//! writes the rows lives in the `geopackage` crate, as it does for
//! [`crate::schema`].
//!
//! Payloads stay strings. The spec allows any authoritative metadata encoding,
//! naming ISO 19115, ISO 19139, Dublin Core, CSDGM, DDMS and others, and this
//! crate interprets none of them: `mime_type` and `md_standard_uri` say what a
//! reader would need to know, and the document is returned as written. Tile
//! payloads are treated the same way.

use std::fmt;

/// Registered extension name for metadata (Annex F.8).
pub const EXTENSION_NAME: &str = "gpkg_metadata";
/// `gpkg_extensions.definition` value for [`EXTENSION_NAME`].
pub const EXTENSION_DEFINITION: &str = "http://www.geopackage.org/spec140/#extension_metadata";
/// `gpkg_extensions.scope` value for [`EXTENSION_NAME`].
pub const EXTENSION_SCOPE: &str = "read-write";

/// A row of `gpkg_metadata`: one metadata document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataRecord {
    /// The row's `id`, which [`MetadataReference::md_file_id`] points at.
    pub id: i64,
    /// What the document describes.
    pub scope: MetadataScope,
    /// URI of the metadata standard the document follows.
    pub standard_uri: String,
    /// MIME type of `metadata`. The table's default is `text/xml`.
    pub mime_type: String,
    /// The document, exactly as stored and never parsed.
    pub metadata: String,
}

/// A row of `gpkg_metadata_reference`: what a record is attached to.
///
/// Which of `table_name`, `column_name` and `row_id_value` may be set is fixed
/// by `scope` (Requirements 97 to 99); [`ReferenceScope::targets`] states the
/// rule, and the three fields are `Option` here because the spec makes them so
/// rather than because any combination is meaningful.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataReference {
    /// The granularity of the attachment.
    pub scope: ReferenceScope,
    /// The table attached to, `None` only for [`ReferenceScope::GeoPackage`].
    pub table_name: Option<String>,
    /// The column attached to, set only for the column-level scopes.
    pub column_name: Option<String>,
    /// The `ROWID` attached to, set only for the row-level scopes.
    pub row_id_value: Option<i64>,
    /// When the reference was made. Requirement 100 puts this in the same
    /// DATETIME format as everything else, so it goes through
    /// [`crate::datetime`] rather than a second format path.
    pub timestamp: String,
    /// The `gpkg_metadata.id` of the record being attached (Requirement 101).
    pub md_file_id: i64,
    /// A parent record's `gpkg_metadata.id`, which Requirement 102 says must
    /// differ from `md_file_id` when it is set.
    pub md_parent_id: Option<i64>,
}

/// What a metadata record describes (`md_scope`, spec Table 15).
///
/// **Open, not closed.** Requirement 94 first says each value SHALL be one of
/// the table's names and then that it SHOULD be, adding "however, this list is
/// not exhaustive; new scopes are permitted". The weaker reading is the one
/// that keeps a conformant file readable, so an unlisted scope is
/// [`MetadataScope::Other`] rather than an error.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MetadataScope {
    /// Scope is undefined.
    Undefined,
    /// Applies to the field session.
    FieldSession,
    /// Applies to the collection session.
    CollectionSession,
    /// Applies to the (dataset) series.
    Series,
    /// Applies to the (geographic feature) dataset. The table's default.
    Dataset,
    /// Applies to a feature type (class).
    FeatureType,
    /// Applies to a feature (instance).
    Feature,
    /// Applies to the attribute class.
    AttributeType,
    /// Applies to the characteristic of a feature (instance).
    Attribute,
    /// Applies to a tile, a spatial subset of geographic data.
    Tile,
    /// Applies to a copy or imitation of an existing or hypothetical object.
    Model,
    /// Applies to a feature catalog.
    Catalog,
    /// Applies to an application schema.
    Schema,
    /// Applies to a taxonomy or knowledge system.
    Taxonomy,
    /// Applies to a computer program or routine.
    Software,
    /// Applies to a service.
    Service,
    /// Applies to the collection hardware class.
    CollectionHardware,
    /// Applies to non-geographic data.
    NonGeographicDataset,
    /// Applies to a dimension group.
    DimensionGroup,
    /// Applies to a specific style.
    Style,
    /// A scope not in Table 15, kept as written.
    Other(String),
}

impl MetadataScope {
    /// Identifies an `md_scope` value. Never fails: an unlisted name becomes
    /// [`MetadataScope::Other`].
    pub fn parse(value: &str) -> Self {
        match value {
            "undefined" => Self::Undefined,
            "fieldSession" => Self::FieldSession,
            "collectionSession" => Self::CollectionSession,
            "series" => Self::Series,
            "dataset" => Self::Dataset,
            "featureType" => Self::FeatureType,
            "feature" => Self::Feature,
            "attributeType" => Self::AttributeType,
            "attribute" => Self::Attribute,
            "tile" => Self::Tile,
            "model" => Self::Model,
            "catalog" => Self::Catalog,
            "schema" => Self::Schema,
            "taxonomy" => Self::Taxonomy,
            "software" => Self::Software,
            "service" => Self::Service,
            "collectionHardware" => Self::CollectionHardware,
            "nonGeographicDataset" => Self::NonGeographicDataset,
            "dimensionGroup" => Self::DimensionGroup,
            "style" => Self::Style,
            other => Self::Other(other.to_owned()),
        }
    }

    /// Returns the `md_scope` column value.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Undefined => "undefined",
            Self::FieldSession => "fieldSession",
            Self::CollectionSession => "collectionSession",
            Self::Series => "series",
            Self::Dataset => "dataset",
            Self::FeatureType => "featureType",
            Self::Feature => "feature",
            Self::AttributeType => "attributeType",
            Self::Attribute => "attribute",
            Self::Tile => "tile",
            Self::Model => "model",
            Self::Catalog => "catalog",
            Self::Schema => "schema",
            Self::Taxonomy => "taxonomy",
            Self::Software => "software",
            Self::Service => "service",
            Self::CollectionHardware => "collectionHardware",
            Self::NonGeographicDataset => "nonGeographicDataset",
            Self::DimensionGroup => "dimensionGroup",
            Self::Style => "style",
            Self::Other(name) => name,
        }
    }

    /// Returns `true` if this is one of the names Table 15 lists.
    pub fn is_listed(&self) -> bool {
        !matches!(self, Self::Other(_))
    }
}

impl fmt::Display for MetadataScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The granularity of a metadata attachment (`reference_scope`).
///
/// **Closed**, unlike [`MetadataScope`]: Requirement 96 says every value SHALL
/// be one of these five in lowercase, with no escape hatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceScope {
    /// The whole GeoPackage.
    GeoPackage,
    /// One table.
    Table,
    /// One column of one table.
    Column,
    /// One row of one table.
    Row,
    /// One cell: a row and a column.
    RowCol,
}

/// Which of a reference's target columns Requirements 97 to 99 require to be
/// set, for a given [`ReferenceScope`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferenceTargets {
    /// Whether `table_name` is set. False only for the whole-GeoPackage scope.
    pub table_name: bool,
    /// Whether `column_name` is set.
    pub column_name: bool,
    /// Whether `row_id_value` is set.
    pub row_id_value: bool,
}

impl ReferenceScope {
    /// Identifies a `reference_scope` value, or `None` when it is not one of
    /// the five (Requirement 96).
    ///
    /// Matched case sensitively, because the requirement spells the values and
    /// says "in lowercase".
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "geopackage" => Some(Self::GeoPackage),
            "table" => Some(Self::Table),
            "column" => Some(Self::Column),
            "row" => Some(Self::Row),
            "row/col" => Some(Self::RowCol),
            _ => None,
        }
    }

    /// Returns the `reference_scope` column value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GeoPackage => "geopackage",
            Self::Table => "table",
            Self::Column => "column",
            Self::Row => "row",
            Self::RowCol => "row/col",
        }
    }

    /// Returns which target columns are set for a reference at this scope.
    ///
    /// Requirement 97: `table_name` is NULL for `geopackage` and set
    /// otherwise. Requirement 98: `column_name` is NULL for `geopackage`,
    /// `table` and `row`, and set otherwise. Requirement 99: `row_id_value` is
    /// NULL for `geopackage`, `table` and `column`, and set otherwise.
    pub fn targets(self) -> ReferenceTargets {
        ReferenceTargets {
            table_name: !matches!(self, Self::GeoPackage),
            column_name: matches!(self, Self::Column | Self::RowCol),
            row_id_value: matches!(self, Self::Row | Self::RowCol),
        }
    }
}

impl fmt::Display for ReferenceScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_scopes_round_trip() {
        for scope in [
            MetadataScope::Undefined,
            MetadataScope::FieldSession,
            MetadataScope::CollectionSession,
            MetadataScope::Series,
            MetadataScope::Dataset,
            MetadataScope::FeatureType,
            MetadataScope::Feature,
            MetadataScope::AttributeType,
            MetadataScope::Attribute,
            MetadataScope::Tile,
            MetadataScope::Model,
            MetadataScope::Catalog,
            MetadataScope::Schema,
            MetadataScope::Taxonomy,
            MetadataScope::Software,
            MetadataScope::Service,
            MetadataScope::CollectionHardware,
            MetadataScope::NonGeographicDataset,
            MetadataScope::DimensionGroup,
            MetadataScope::Style,
        ] {
            assert_eq!(MetadataScope::parse(scope.as_str()), scope);
            assert!(scope.is_listed());
        }
    }

    #[test]
    fn an_unlisted_scope_is_kept_rather_than_rejected() {
        // Requirement 94 says the list is not exhaustive, so this is a
        // conformant file rather than a broken one.
        let scope = MetadataScope::parse("x-vendor_thing");
        assert_eq!(scope, MetadataScope::Other("x-vendor_thing".to_owned()));
        assert_eq!(scope.as_str(), "x-vendor_thing");
        assert!(!scope.is_listed());
    }

    #[test]
    fn reference_scopes_round_trip_and_reject_the_rest() {
        for scope in [
            ReferenceScope::GeoPackage,
            ReferenceScope::Table,
            ReferenceScope::Column,
            ReferenceScope::Row,
            ReferenceScope::RowCol,
        ] {
            assert_eq!(ReferenceScope::parse(scope.as_str()), Some(scope));
        }
        // Closed set, and Requirement 96 spells the values in lowercase.
        assert_eq!(ReferenceScope::parse("Table"), None);
        assert_eq!(ReferenceScope::parse("rowcol"), None);
        assert_eq!(ReferenceScope::parse("cell"), None);
    }

    #[test]
    fn targets_follow_requirements_97_to_99() {
        let cases = [
            (ReferenceScope::GeoPackage, [false, false, false]),
            (ReferenceScope::Table, [true, false, false]),
            (ReferenceScope::Column, [true, true, false]),
            (ReferenceScope::Row, [true, false, true]),
            (ReferenceScope::RowCol, [true, true, true]),
        ];
        for (scope, [table, column, row_id]) in cases {
            let targets = scope.targets();
            assert_eq!(targets.table_name, table, "{scope} table_name");
            assert_eq!(targets.column_name, column, "{scope} column_name");
            assert_eq!(targets.row_id_value, row_id, "{scope} row_id_value");
        }
    }
}
