//! [`GeoPackage::validate`]: every check this crate can make about a file,
//! collected into one pass over it.
//!
//! The checks are not new. Most of them existed already, reachable one at a
//! time: [`crate::Layer::audit_spatial_index`], [`crate::TilePyramid::validate`],
//! the [`crate::OpenWarning`]s `open_lenient` reports, and the extension
//! catalogue's support levels. What this module adds is one call that runs
//! them all and returns typed findings, which is what an embedding caller
//! needs and what `gpkg validate` will print.
//!
//! Nothing here mutates. A [`Finding`] includes repair advice as text when a
//! repair exists, and names the method that performs it; running that is the
//! caller's decision.
//!
//! # Severity
//!
//! [`Severity::Error`] means a reader can get a wrong answer: a query missing
//! rows it should return, or a catalogue entry pointing at nothing.
//! [`Severity::Warning`] means the file is out of step with the current spec
//! but readable. [`Severity::Advisory`] is a remark, not a defect.

use std::fmt;

use geopackage_core::extensions::ExtensionSupport;

use crate::index::SpatialIndexAudit;
use crate::{ExtensionScope, GeoPackage, GpkgVersion, Result, SpatialIndexStatus, table_exists};

/// How much a [`Finding`] matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// A remark rather than a defect.
    Advisory,
    /// The file is out of step with the current spec, but reads correctly.
    Warning,
    /// A reader can get a wrong answer from this file.
    Error,
}

impl Severity {
    /// Returns the severity as a lowercase word: `advisory`, `warning` or
    /// `error`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Advisory => "advisory",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Something [`GeoPackage::validate`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Finding {
    /// The file declares a pre-1.2 `application_id`.
    LegacyApplicationId {
        /// The version the identifier maps to.
        version: GpkgVersion,
        /// The raw pragma value.
        application_id: u32,
    },
    /// A `gpkg_contents` row names a table that is not in the file.
    MissingContentsTable {
        /// The name as `gpkg_contents` gives it.
        table_name: String,
    },
    /// A `gpkg_contents.table_name` matches a real table only when case is
    /// ignored.
    TableNameCaseMismatch {
        /// The name as written in `gpkg_contents`.
        declared: String,
        /// The physical SQLite table name.
        actual: String,
    },
    /// An extension the GeoPackage SWG removed from the standard in 2016.
    RemovedExtension {
        /// The `extension_name` value.
        extension_name: String,
        /// The table it applies to, if any.
        table_name: Option<String>,
    },
    /// An extension this crate cannot identify.
    UnrecognisedExtension {
        /// The `extension_name` value, as the file spells it.
        extension_name: String,
        /// The table it applies to, if any.
        table_name: Option<String>,
        /// What it claims to affect.
        scope: ExtensionScope,
    },
    /// A spatial index that does not describe the rows it should.
    SpatialIndexOutOfStep {
        /// The indexed table.
        table_name: String,
        /// What the audit counted.
        audit: SpatialIndexAudit,
    },
    /// A spatial index maintained by a pre-1.4 or mixed trigger set.
    LegacySpatialIndexTriggers {
        /// The indexed table.
        table_name: String,
    },
    /// A feature table with no spatial index.
    NoSpatialIndex {
        /// The unindexed table.
        table_name: String,
    },
    /// A tile pyramid that breaks the tile matrix consistency rules.
    TilePyramidInconsistent {
        /// The pyramid's table.
        table_name: String,
        /// What the rule check reported.
        detail: String,
    },
    /// A `gpkg_metadata_reference` row pointing at an absent record.
    DanglingMetadataReference {
        /// The `md_file_id` or `md_parent_id` that resolves to nothing.
        md_id: i64,
    },
    /// A `gpkgext_relations` row whose mapping table is absent.
    MissingMappingTable {
        /// The mapping table the relationship names.
        mapping_table_name: String,
    },
    /// A `relation_name` that Requirement 8 does not accept.
    NonConformantRelationName {
        /// The value as the file spells it.
        relation_name: String,
    },
}

impl Finding {
    /// Returns how much this matters.
    pub fn severity(&self) -> Severity {
        match self {
            // A query against these returns the wrong rows, or a catalogue
            // entry leads nowhere.
            Self::MissingContentsTable { .. }
            | Self::SpatialIndexOutOfStep { .. }
            | Self::DanglingMetadataReference { .. }
            | Self::MissingMappingTable { .. } => Severity::Error,
            // Readable, but not what the current spec says.
            Self::LegacyApplicationId { .. }
            | Self::TableNameCaseMismatch { .. }
            | Self::RemovedExtension { .. }
            | Self::UnrecognisedExtension { .. }
            | Self::LegacySpatialIndexTriggers { .. }
            | Self::TilePyramidInconsistent { .. }
            | Self::NonConformantRelationName { .. } => Severity::Warning,
            // A choice, not a defect: an unindexed layer still reads.
            Self::NoSpatialIndex { .. } => Severity::Advisory,
        }
    }

    /// Returns the table this concerns, when it concerns one.
    pub fn table_name(&self) -> Option<&str> {
        match self {
            Self::MissingContentsTable { table_name }
            | Self::SpatialIndexOutOfStep { table_name, .. }
            | Self::LegacySpatialIndexTriggers { table_name }
            | Self::NoSpatialIndex { table_name }
            | Self::TilePyramidInconsistent { table_name, .. } => Some(table_name),
            Self::TableNameCaseMismatch { declared, .. } => Some(declared),
            Self::RemovedExtension { table_name, .. }
            | Self::UnrecognisedExtension { table_name, .. } => table_name.as_deref(),
            Self::MissingMappingTable {
                mapping_table_name, ..
            } => Some(mapping_table_name),
            Self::LegacyApplicationId { .. }
            | Self::DanglingMetadataReference { .. }
            | Self::NonConformantRelationName { .. } => None,
        }
    }

    /// Returns what would put this right, when anything here can.
    ///
    /// `None` means the fix is outside this crate: it needs the writer that
    /// produced the file, or a decision about data this crate should not take
    /// on the caller's behalf.
    pub fn repair(&self) -> Option<&'static str> {
        match self {
            Self::SpatialIndexOutOfStep { .. } => {
                Some("rebuild the index with Layer::rebuild_spatial_index")
            }
            Self::LegacySpatialIndexTriggers { .. } => {
                Some("upgrade the trigger set with Layer::repair_spatial_index")
            }
            Self::NoSpatialIndex { .. } => Some("build one with Layer::create_spatial_index"),
            Self::LegacyApplicationId { .. } => {
                Some("rewriting the file through this crate stamps the current application_id")
            }
            Self::MissingContentsTable { .. } => {
                Some("delete the gpkg_contents row, or restore the table it names")
            }
            // The rest need the producing writer, or a decision about the data.
            Self::TableNameCaseMismatch { .. }
            | Self::RemovedExtension { .. }
            | Self::UnrecognisedExtension { .. }
            | Self::TilePyramidInconsistent { .. }
            | Self::DanglingMetadataReference { .. }
            | Self::MissingMappingTable { .. }
            | Self::NonConformantRelationName { .. } => None,
        }
    }
}

/// One line describing the finding, without its severity or repair advice.
///
/// Those are [`Finding::severity`] and [`Finding::repair`], kept separate so a
/// caller decides how to arrange them. This exists so that every caller that
/// prints a finding does not write the same match; `gpkg validate` composes all
/// three.
impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LegacyApplicationId {
                version,
                application_id,
            } => {
                // Rendered as the four characters it spells, which is how the
                // spec writes it and how a hex dump shows it.
                let bytes = application_id.to_be_bytes();
                let tag = String::from_utf8_lossy(&bytes);
                write!(
                    f,
                    "file declares the GeoPackage {version} application_id {tag:?}, which predates 1.2"
                )
            }
            Self::MissingContentsTable { table_name } => {
                write!(
                    f,
                    "gpkg_contents names table {table_name:?}, which is not in the file"
                )
            }
            Self::TableNameCaseMismatch { declared, actual } => {
                write!(
                    f,
                    "gpkg_contents says {declared:?} but the table is {actual:?}: they differ only in case"
                )
            }
            Self::RemovedExtension {
                extension_name,
                table_name,
            } => {
                write!(
                    f,
                    "extension {extension_name:?}{} was removed from the standard in 2016",
                    On(table_name)
                )
            }
            Self::UnrecognisedExtension {
                extension_name,
                table_name,
                scope,
            } => {
                write!(
                    f,
                    "extension {extension_name:?}{} is not one this crate recognises (scope {})",
                    On(table_name),
                    scope.as_str()
                )
            }
            Self::SpatialIndexOutOfStep { table_name, audit } => {
                write!(
                    f,
                    "spatial index on {table_name:?} is out of step: {} indexable rows, {} entries, {} missing, {} stale, {} not covering their geometry",
                    audit.indexable, audit.entries, audit.missing, audit.extra, audit.not_covering
                )
            }
            Self::LegacySpatialIndexTriggers { table_name } => {
                write!(
                    f,
                    "spatial index on {table_name:?} is maintained by a pre-1.4 or mixed trigger set"
                )
            }
            Self::NoSpatialIndex { table_name } => {
                write!(f, "feature table {table_name:?} has no spatial index")
            }
            Self::TilePyramidInconsistent { table_name, detail } => {
                write!(
                    f,
                    "tile pyramid {table_name:?} breaks the tile matrix rules: {detail}"
                )
            }
            Self::DanglingMetadataReference { md_id } => {
                write!(
                    f,
                    "gpkg_metadata_reference points at metadata id {md_id}, which is not there"
                )
            }
            Self::MissingMappingTable { mapping_table_name } => {
                write!(
                    f,
                    "relationship names mapping table {mapping_table_name:?}, which is not in the file"
                )
            }
            Self::NonConformantRelationName { relation_name } => {
                write!(
                    f,
                    "relation_name {relation_name:?} is not one Requirement 8 accepts"
                )
            }
        }
    }
}

/// Renders `Some(table)` as ` on "table"` and `None` as nothing, so the two
/// findings with an optional table read as sentences either way.
struct On<'a>(&'a Option<String>);

impl fmt::Display for On<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(table) => write!(f, " on {table:?}"),
            None => Ok(()),
        }
    }
}

impl GeoPackage {
    /// Checks the file and reports what is wrong with it.
    ///
    /// One pass over everything this crate knows how to check: the container's
    /// version stamp and catalogue, the extension registrations, every feature
    /// table's spatial index, every tile pyramid's matrix rules, and the two
    /// extension catalogues that point at other rows. Findings come back
    /// most severe first.
    ///
    /// An empty vector means every check passed, not that the file is
    /// conformant in every respect the spec defines: this reports what it can
    /// see, and the OGC ETS remains the authority.
    ///
    /// Nothing is modified. [`Finding::repair`] says what would put a finding
    /// right where this crate can.
    ///
    /// # Errors
    ///
    /// [`crate::Error`] if the file cannot be read far enough to check it.
    pub fn validate(&self) -> Result<Vec<Finding>> {
        let mut findings = Vec::new();
        self.validate_container(&mut findings)?;
        self.validate_extensions(&mut findings)?;
        self.validate_spatial_indexes(&mut findings)?;
        self.validate_tile_pyramids(&mut findings)?;
        self.validate_metadata(&mut findings)?;
        self.validate_relations(&mut findings)?;
        // Most severe first, and stable within a severity so a diff of the
        // output is a diff of the findings rather than of their order.
        findings.sort_by_key(|finding| std::cmp::Reverse(finding.severity()));
        Ok(findings)
    }

    fn validate_container(&self, findings: &mut Vec<Finding>) -> Result<()> {
        let conn = self.connection();
        let application_id =
            u32::try_from(conn.query_row("PRAGMA application_id", [], |row| row.get::<_, i64>(0))?)
                .unwrap_or_default();
        let user_version =
            u32::try_from(conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?)
                .unwrap_or_default();
        // GP10 and GP11 predate the GPKG identifier that 1.2 introduced.
        if let Some(version @ (GpkgVersion::V1_0 | GpkgVersion::V1_1)) =
            GpkgVersion::from_pragmas(application_id, user_version)
        {
            findings.push(Finding::LegacyApplicationId {
                version,
                application_id,
            });
        }

        for entry in self.contents()? {
            if table_exists(conn, &entry.table_name)? {
                continue;
            }
            // Not there under that spelling: SQLite would still resolve a
            // case-mismatched name, so separate the two.
            let actual: Option<String> = conn
                .query_row(
                    "SELECT name FROM sqlite_master WHERE type IN ('table', 'view') \
                     AND name = ?1 COLLATE NOCASE",
                    [&entry.table_name],
                    |row| row.get(0),
                )
                .ok();
            match actual {
                Some(actual) => findings.push(Finding::TableNameCaseMismatch {
                    declared: entry.table_name,
                    actual,
                }),
                None => findings.push(Finding::MissingContentsTable {
                    table_name: entry.table_name,
                }),
            }
        }
        Ok(())
    }

    fn validate_extensions(&self, findings: &mut Vec<Finding>) -> Result<()> {
        for row in self.extensions()? {
            match row.support() {
                ExtensionSupport::Removed => findings.push(Finding::RemovedExtension {
                    extension_name: row.name,
                    table_name: row.table_name,
                }),
                ExtensionSupport::Unrecognised => findings.push(Finding::UnrecognisedExtension {
                    extension_name: row.name,
                    table_name: row.table_name,
                    scope: row.scope,
                }),
                // Implemented and Known both mean the file is fine as it is;
                // the wildcard covers a level added later, which should not
                // become a finding without a decision.
                _ => {}
            }
        }
        Ok(())
    }

    fn validate_spatial_indexes(&self, findings: &mut Vec<Finding>) -> Result<()> {
        for layer in self.layers()? {
            let table_name = layer.table_name().to_owned();
            match layer.spatial_index_status()? {
                SpatialIndexStatus::Absent => {
                    findings.push(Finding::NoSpatialIndex { table_name });
                }
                SpatialIndexStatus::Legacy => {
                    findings.push(Finding::LegacySpatialIndexTriggers { table_name });
                }
                SpatialIndexStatus::Current | SpatialIndexStatus::Stale => {
                    let audit = layer.audit_spatial_index()?;
                    if !audit.is_consistent() {
                        findings.push(Finding::SpatialIndexOutOfStep { table_name, audit });
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_tile_pyramids(&self, findings: &mut Vec<Finding>) -> Result<()> {
        for pyramid in self.tile_pyramids()? {
            if let Err(error) = pyramid.validate() {
                findings.push(Finding::TilePyramidInconsistent {
                    table_name: pyramid.table_name().to_owned(),
                    detail: error.to_string(),
                });
            }
        }
        Ok(())
    }

    fn validate_metadata(&self, findings: &mut Vec<Finding>) -> Result<()> {
        let records = self.metadata()?;
        if records.is_empty() {
            return Ok(());
        }
        let known: Vec<i64> = records.iter().map(|record| record.id).collect();
        for reference in self.metadata_references()? {
            for id in std::iter::once(reference.md_file_id).chain(reference.md_parent_id) {
                if !known.contains(&id) {
                    findings.push(Finding::DanglingMetadataReference { md_id: id });
                }
            }
        }
        Ok(())
    }

    fn validate_relations(&self, findings: &mut Vec<Finding>) -> Result<()> {
        let conn = self.connection();
        for relation in self.relations()? {
            if !table_exists(conn, &relation.mapping_table_name)? {
                findings.push(Finding::MissingMappingTable {
                    mapping_table_name: relation.mapping_table_name.clone(),
                });
            }
            if !relation.relation_name.is_conformant() {
                findings.push(Finding::NonConformantRelationName {
                    relation_name: relation.relation_name.as_string(),
                });
            }
        }
        Ok(())
    }
}
