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

use geopackage_core::TileError;
use geopackage_core::coverage::{CoverageDatatype, TILE_ANCILLARY_TABLE, coverage_payload};
use geopackage_core::ident::quote;

use crate::index::SpatialIndexAudit;
use crate::{
    ContentsDataType, Coverage, ExtensionScope, GeoPackage, GpkgVersion, Result,
    SpatialIndexStatus, table_exists,
};

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
    /// A `gpkg_contents` row for a tiled gridded coverage that cannot be
    /// interpreted: no `gpkg_2d_gridded_coverage_ancillary` row, or no tile
    /// matrix set.
    CoverageUninterpretable {
        /// The coverage's table.
        table_name: String,
        /// Why it could not be opened.
        detail: String,
    },
    /// A coverage with a `datatype` that is neither `integer` nor `float`
    /// (Requirement 9), so the meaning of its samples is not defined.
    CoverageDatatypeUnknown {
        /// The coverage's table.
        table_name: String,
        /// The value as the file spells it.
        datatype: String,
    },
    /// A `float` coverage whose scale or offset is not the default
    /// (Requirement 11).
    CoverageScaleNotDefault {
        /// The coverage's table.
        table_name: String,
        /// Which pair, and its values.
        detail: String,
    },
    /// Coverage tiles with payloads that break the encoding profile of the
    /// extension (Requirements 13 and 15 to 20).
    CoveragePayloadNonConformant {
        /// The coverage's table.
        table_name: String,
        /// The requirement that the first nonconformant payload breaks.
        requirement: u8,
        /// What that payload declares.
        detail: String,
        /// How many tiles are affected.
        tiles: i64,
    },
    /// Coverage tiles with payloads that contradict the `datatype` of their
    /// coverage (Requirements 13 and 14).
    CoveragePayloadDatatypeMismatch {
        /// The coverage's table.
        table_name: String,
        /// The `datatype` that the ancillary row declares.
        datatype: String,
        /// What the first mismatched payload declares instead.
        detail: String,
        /// How many tiles are affected.
        tiles: i64,
    },
    /// Coverage tiles with no `gpkg_2d_gridded_tile_ancillary` row
    /// (Requirement 10).
    MissingTileAncillary {
        /// The coverage's table.
        table_name: String,
        /// The coverage's `datatype`, which sets the severity of the finding.
        datatype: String,
        /// How many tiles have no row.
        tiles: i64,
    },
    /// `gpkg_2d_gridded_tile_ancillary` rows whose `tpudt_id` matches no tile
    /// (Requirement 12).
    DanglingTileAncillary {
        /// The coverage's table, as the rows name it.
        table_name: String,
        /// How many rows point to no tile.
        rows: i64,
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
            | Self::CoverageUninterpretable { .. }
            | Self::CoverageDatatypeUnknown { .. }
            | Self::CoveragePayloadDatatypeMismatch { .. }
            | Self::MissingMappingTable { .. } => Severity::Error,
            // The severity depends on the coverage. Without the row, a reader
            // uses a scale of 1 and an offset of 0. Requirement 11 sets these
            // values for a float coverage. For an integer coverage, the scale
            // and offset convert samples to values, so the defaults give
            // incorrect values.
            Self::MissingTileAncillary { datatype, .. } => {
                if datatype == "float" {
                    Severity::Warning
                } else {
                    Severity::Error
                }
            }
            // Readable, but not what the current spec says.
            Self::LegacyApplicationId { .. }
            | Self::TableNameCaseMismatch { .. }
            | Self::RemovedExtension { .. }
            | Self::UnrecognisedExtension { .. }
            | Self::LegacySpatialIndexTriggers { .. }
            | Self::TilePyramidInconsistent { .. }
            | Self::CoverageScaleNotDefault { .. }
            | Self::DanglingTileAncillary { .. }
            // The header of the payload is correct, so a reader that uses the
            // header reads the payload correctly. The file does not conform to
            // the extension, but it is readable.
            | Self::CoveragePayloadNonConformant { .. }
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
            | Self::TilePyramidInconsistent { table_name, .. }
            | Self::CoverageUninterpretable { table_name, .. }
            | Self::CoverageDatatypeUnknown { table_name, .. }
            | Self::CoverageScaleNotDefault { table_name, .. }
            | Self::CoveragePayloadNonConformant { table_name, .. }
            | Self::CoveragePayloadDatatypeMismatch { table_name, .. }
            | Self::MissingTileAncillary { table_name, .. }
            | Self::DanglingTileAncillary { table_name, .. } => Some(table_name),
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
            // A repair of a coverage finding needs the writer that produced the
            // file: the values that a repair must supply are the data itself.
            | Self::CoverageUninterpretable { .. }
            | Self::CoverageDatatypeUnknown { .. }
            | Self::CoverageScaleNotDefault { .. }
            | Self::CoveragePayloadNonConformant { .. }
            | Self::CoveragePayloadDatatypeMismatch { .. }
            | Self::MissingTileAncillary { .. }
            | Self::DanglingTileAncillary { .. }
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
            Self::CoverageUninterpretable { table_name, detail } => {
                write!(f, "coverage {table_name:?} cannot be interpreted: {detail}")
            }
            Self::CoverageDatatypeUnknown {
                table_name,
                datatype,
            } => {
                write!(
                    f,
                    "coverage {table_name:?} declares datatype {datatype:?}, which is neither \"integer\" nor \"float\""
                )
            }
            Self::CoverageScaleNotDefault { table_name, detail } => {
                write!(
                    f,
                    "float coverage {table_name:?} does not keep the default scale and offset: {detail}"
                )
            }
            Self::CoveragePayloadNonConformant {
                table_name,
                requirement,
                detail,
                tiles,
            } => {
                write!(
                    f,
                    "{tiles} tile(s) of coverage {table_name:?} break tiled gridded coverage Requirement {requirement}: {detail}"
                )
            }
            Self::CoveragePayloadDatatypeMismatch {
                table_name,
                datatype,
                detail,
                tiles,
            } => {
                write!(
                    f,
                    "{tiles} tile(s) of coverage {table_name:?} do not match its datatype {datatype:?}: {detail}"
                )
            }
            Self::MissingTileAncillary {
                table_name,
                datatype,
                tiles,
            } => {
                write!(
                    f,
                    "{tiles} tile(s) of {datatype} coverage {table_name:?} have no gpkg_2d_gridded_tile_ancillary row"
                )
            }
            Self::DanglingTileAncillary { table_name, rows } => {
                write!(
                    f,
                    "{rows} gpkg_2d_gridded_tile_ancillary row(s) for {table_name:?} match no tile"
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
    /// table's spatial index, every tile pyramid's matrix rules, the ancillary
    /// rows and payloads of every tiled gridded coverage, and the two extension
    /// catalogues that point at other rows. Findings come back most severe
    /// first.
    ///
    /// # Cost
    ///
    /// Each check except one reads catalogue rows, pragmas and index shadow
    /// tables, so the work is proportional to the number of tables, not to the
    /// size of the file. The exception is coverages: the encoding requirements
    /// of the extension are statements about payloads, so the check reads each
    /// coverage tile. For a file without a coverage, the cost does not change.
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
    #[hotpath::measure(label = "GeoPackage::validate")]
    pub fn validate(&self) -> Result<Vec<Finding>> {
        let mut findings = Vec::new();
        self.validate_container(&mut findings)?;
        self.validate_extensions(&mut findings)?;
        self.validate_spatial_indexes(&mut findings)?;
        self.validate_tile_pyramids(&mut findings)?;
        self.validate_coverages(&mut findings)?;
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

    /// Checks every tiled gridded coverage: its ancillary rows, and its
    /// payloads.
    ///
    /// This is the one pass with work that scales with the size of the file.
    /// It reads every coverage tile, because the encoding requirements apply to
    /// payloads, and the profile needs the header, not a prefix of the blob (the
    /// IFD of a TIFF can follow its image data). The pass does not check a
    /// sample of the tiles, because the result would then report on tiles that
    /// it did not check.
    fn validate_coverages(&self, findings: &mut Vec<Finding>) -> Result<()> {
        for entry in self.contents()? {
            if entry.data_type != ContentsDataType::Coverage {
                continue;
            }
            let coverage = match self.coverage(&entry.table_name) {
                Ok(coverage) => coverage,
                Err(error) => {
                    findings.push(Finding::CoverageUninterpretable {
                        table_name: entry.table_name,
                        detail: error.to_string(),
                    });
                    continue;
                }
            };
            let datatype = coverage.datatype();
            if datatype.is_none() {
                findings.push(Finding::CoverageDatatypeUnknown {
                    table_name: coverage.table_name().to_owned(),
                    datatype: coverage.ancillary().datatype.clone(),
                });
            }
            self.validate_coverage_ancillary(&coverage, datatype, findings)?;
            self.validate_coverage_payloads(&coverage, datatype, findings)?;
        }
        Ok(())
    }

    /// Checks the ancillary rows of one coverage: Requirements 10, 11 and 12.
    #[expect(
        clippy::float_cmp,
        reason = "Requirement 11 says the scale and offset of a float coverage are \"set to the defaults\", which is exactly 1.0 and 0.0; a tolerance would accept values the requirement does not"
    )]
    fn validate_coverage_ancillary(
        &self,
        coverage: &Coverage<'_>,
        datatype: Option<CoverageDatatype>,
        findings: &mut Vec<Finding>,
    ) -> Result<()> {
        let table_name = coverage.table_name().to_owned();
        let quoted = quote(&table_name)?;
        let ancillary = coverage.ancillary();

        if datatype == Some(CoverageDatatype::Float)
            && (ancillary.scale != 1.0 || ancillary.offset != 0.0)
        {
            findings.push(Finding::CoverageScaleNotDefault {
                table_name: table_name.clone(),
                detail: format!(
                    "the coverage row has scale {} and offset {}",
                    ancillary.scale, ancillary.offset
                ),
            });
        }

        let tile_ancillary_exists = table_exists(self.connection(), TILE_ANCILLARY_TABLE)?;
        if !tile_ancillary_exists {
            // Requirement 2 specifies the table. Without the table, no tile
            // has a row, which is the same finding.
            let tiles = coverage.tile_count()?;
            if tiles > 0 {
                findings.push(Finding::MissingTileAncillary {
                    table_name,
                    datatype: ancillary.datatype.clone(),
                    tiles,
                });
            }
            return Ok(());
        }

        let missing: i64 = self.connection().query_row(
            &format!(
                "SELECT count(*) FROM {quoted} t \
                 LEFT JOIN {TILE_ANCILLARY_TABLE} a \
                 ON a.tpudt_id = t.id AND a.tpudt_name = ?1 COLLATE NOCASE \
                 WHERE a.id IS NULL"
            ),
            [&table_name],
            |row| row.get(0),
        )?;
        if missing > 0 {
            findings.push(Finding::MissingTileAncillary {
                table_name: table_name.clone(),
                datatype: ancillary.datatype.clone(),
                tiles: missing,
            });
        }

        let dangling: i64 = self.connection().query_row(
            &format!(
                "SELECT count(*) FROM {TILE_ANCILLARY_TABLE} a \
                 LEFT JOIN {quoted} t ON t.id = a.tpudt_id \
                 WHERE a.tpudt_name = ?1 COLLATE NOCASE AND t.id IS NULL"
            ),
            [&table_name],
            |row| row.get(0),
        )?;
        if dangling > 0 {
            findings.push(Finding::DanglingTileAncillary {
                table_name: table_name.clone(),
                rows: dangling,
            });
        }

        if datatype == Some(CoverageDatatype::Float) {
            let scaled: i64 = self.connection().query_row(
                &format!(
                    "SELECT count(*) FROM {TILE_ANCILLARY_TABLE} \
                     WHERE tpudt_name = ?1 COLLATE NOCASE AND (scale != 1.0 OR offset != 0.0)"
                ),
                [&table_name],
                |row| row.get(0),
            )?;
            if scaled > 0 {
                findings.push(Finding::CoverageScaleNotDefault {
                    table_name,
                    detail: format!(
                        "{scaled} tile row(s) have a scale or offset other than the defaults"
                    ),
                });
            }
        }
        Ok(())
    }

    /// Checks every payload of one coverage against the encoding profile and
    /// against the `datatype` of the coverage.
    ///
    /// Counts the payloads for each coverage, and does not report each tile. One
    /// defective encoder that writes ten thousand tiles is one fault, and ten
    /// thousand identical findings would hide all other findings. The finding
    /// keeps the detail of the first payload, which identifies the fault.
    fn validate_coverage_payloads(
        &self,
        coverage: &Coverage<'_>,
        datatype: Option<CoverageDatatype>,
        findings: &mut Vec<Finding>,
    ) -> Result<()> {
        let mut profile: Option<(u8, String)> = None;
        let mut profile_tiles = 0;
        let mut mismatch: Option<String> = None;
        let mut mismatch_tiles = 0;

        let mut cursor = coverage.cursor()?;
        let mut stream = cursor.tiles()?;
        while let Some(tile) = stream.next()? {
            let payload = match coverage_payload(tile.data()) {
                Ok(payload) => payload,
                Err(error) => {
                    profile_tiles += 1;
                    if profile.is_none() {
                        let requirement = match &error {
                            TileError::CoverageProfileViolation { requirement, .. } => *requirement,
                            // Not a TIFF and not a PNG. Requirements 13 and 14
                            // together permit these two encodings only, and 13
                            // names both.
                            _ => 13,
                        };
                        profile = Some((requirement, error.to_string()));
                    }
                    continue;
                }
            };
            if let Some(datatype) = datatype
                && let Err(error) = datatype.check_payload(&payload)
            {
                mismatch_tiles += 1;
                if mismatch.is_none() {
                    mismatch = Some(error.to_string());
                }
            }
        }

        if let Some((requirement, detail)) = profile {
            findings.push(Finding::CoveragePayloadNonConformant {
                table_name: coverage.table_name().to_owned(),
                requirement,
                detail,
                tiles: profile_tiles,
            });
        }
        if let Some(detail) = mismatch {
            findings.push(Finding::CoveragePayloadDatatypeMismatch {
                table_name: coverage.table_name().to_owned(),
                datatype: coverage.ancillary().datatype.clone(),
                detail,
                tiles: mismatch_tiles,
            });
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
