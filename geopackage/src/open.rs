//! Lenient open path: [`GeoPackage::open_lenient`] and the [`OpenWarning`]s it
//! collects.
//!
//! Strict [`GeoPackage::open`] identifies a GeoPackage and rejects anything it
//! cannot. `open_lenient` opens the same files but, rather than being stricter,
//! records typed warnings for conditions a fastidious reader would flag,
//! legacy `application_id`s, a missing `gpkg_geometry_columns` table, and
//! catalogue table names that match a real SQLite table only case-insensitively,
//! so callers can inspect and iterate a lightly non-conforming file instead
//! of failing to open it. Strict [`GeoPackage::open`] is unchanged.

use crate::{GeoPackage, Result, resolve_table_name, table_exists};
use geopackage_core::GpkgVersion;
use geopackage_core::extensions::{ExtensionScope, ExtensionSupport};
use geopackage_core::version::{APPLICATION_ID_GP10, APPLICATION_ID_GP11};
use rusqlite::Connection;
use std::path::Path;

/// A non-fatal condition [`GeoPackage::open_lenient`] accepted while opening
/// a file. Retrieve the list with [`GeoPackage::open_warnings`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum OpenWarning {
    /// The file declares a legacy GeoPackage `application_id` (`GP10` for 1.0 or
    /// `GP11` for 1.1) that predates the current `GPKG` identifier.
    LegacyApplicationId {
        /// The spec version the `application_id` maps to.
        version: GpkgVersion,
        /// The raw `application_id` pragma value.
        application_id: u32,
    },
    /// The file has no `gpkg_geometry_columns` table. Valid for an
    /// attribute-only GeoPackage; it means the file contains no feature
    /// layers.
    MissingGeometryColumns,
    /// A `gpkg_contents.table_name` matches a real SQLite table only
    /// case-insensitively. SQLite resolves the table regardless, but the
    /// catalogue string differs from the physical name.
    TableNameCaseMismatch {
        /// The name as written in `gpkg_contents`.
        declared: String,
        /// The physical SQLite table name.
        actual: String,
    },
    /// The file registers an extension this crate cannot identify.
    ///
    /// Reading continues: a `write-only` extension is one a reader may ignore
    /// by Requirement 64, and even a `read-write` one is more useful reported
    /// than rejected, since what it affects may be a table the caller never
    /// touches. Writing to the affected table fails instead, with
    /// [`Error::UnsupportedExtension`](crate::Error::UnsupportedExtension).
    UnsupportedExtension {
        /// The `extension_name` value, as the file spells it.
        extension_name: String,
        /// The table it applies to, or `None` for the whole GeoPackage.
        table_name: Option<String>,
        /// The `scope` value, which says whether readers are affected too.
        scope: ExtensionScope,
    },
}

/// One line saying what was accepted, so a caller reporting warnings does not
/// have to match on them itself.
impl std::fmt::Display for OpenWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LegacyApplicationId {
                version,
                application_id,
            } => {
                // The four characters it spells, which is how the spec writes
                // it and how a hex dump shows it.
                let bytes = application_id.to_be_bytes();
                let tag = String::from_utf8_lossy(&bytes);
                write!(
                    f,
                    "file declares the GeoPackage {version} application_id {tag:?}, which predates the current \"GPKG\""
                )
            }
            Self::MissingGeometryColumns => write!(
                f,
                "no gpkg_geometry_columns table: the file contains no feature layers"
            ),
            Self::TableNameCaseMismatch { declared, actual } => write!(
                f,
                "gpkg_contents says {declared:?} but the table is {actual:?}: they differ only in case"
            ),
            Self::UnsupportedExtension {
                extension_name,
                table_name,
                scope,
            } => {
                let on = match table_name {
                    Some(table) => format!(" on {table:?}"),
                    None => String::new(),
                };
                write!(
                    f,
                    "extension {extension_name:?}{on} is not one this crate recognises (scope {scope}); writes to what it covers will fail"
                )
            }
        }
    }
}

impl GeoPackage {
    /// Opens an existing GeoPackage read-write, accepting a set of legacy and
    /// lightly non-conforming conditions and recording them as
    /// [`OpenWarning`]s rather than errors.
    ///
    /// Retrieve the warnings with [`GeoPackage::open_warnings`]. A file that
    /// cannot be identified as a GeoPackage at all, or is missing a required
    /// core table (`gpkg_spatial_ref_sys`, `gpkg_contents`), is still an error:
    /// leniency covers presentation, not identity.
    pub fn open_lenient<P: AsRef<Path>>(path: P) -> Result<Self> {
        crate::OpenOptions::new().lenient(true).open(path)
    }

    /// Opens an existing GeoPackage read-only, with the same leniency as
    /// [`GeoPackage::open_lenient`].
    ///
    /// The combination an inspection tool needs: the files most worth
    /// inspecting are the ones something is wrong with, and inspecting them
    /// should not require write access to the file, its directory, or the
    /// medium it sits on. [`GeoPackage::open_read_only`] is read-only but
    /// strict, and [`GeoPackage::open_lenient`] is lenient but requires write
    /// access; this is the combination `gpkg info` and `gpkg validate` use.
    ///
    /// Warnings are retrieved with [`GeoPackage::open_warnings`], as for
    /// `open_lenient`.
    ///
    /// # Errors
    ///
    /// As [`GeoPackage::open_lenient`]: a file that cannot be identified as a
    /// GeoPackage, or is missing a required core table, is still an error.
    pub fn open_read_only_lenient<P: AsRef<Path>>(path: P) -> Result<Self> {
        crate::OpenOptions::new().lenient(true).open_read_only(path)
    }

    /// Returns the warnings collected by [`GeoPackage::open_lenient`] (always
    /// empty for a handle opened with strict [`GeoPackage::open`] or created
    /// fresh).
    pub fn open_warnings(&self) -> &[OpenWarning] {
        &self.warnings
    }
}

/// Collects everything a lenient open accepts, as warnings.
///
/// Called from the one open path in `lib.rs` when `OpenOptions::lenient` is
/// set, so leniency composes with every other setting rather than living in a
/// parallel constructor that ignored them.
pub(crate) fn collect_warnings(
    conn: &Connection,
    application_id: u32,
    version: GpkgVersion,
) -> Result<Vec<OpenWarning>> {
    let mut warnings = Vec::new();
    if application_id == APPLICATION_ID_GP10 || application_id == APPLICATION_ID_GP11 {
        warnings.push(OpenWarning::LegacyApplicationId {
            version,
            application_id,
        });
    }
    if !table_exists(conn, "gpkg_geometry_columns")? {
        warnings.push(OpenWarning::MissingGeometryColumns);
    }
    collect_case_mismatches(conn, &mut warnings)?;
    collect_unsupported_extensions(conn, &mut warnings)?;
    Ok(warnings)
}

/// Pushes an [`OpenWarning::UnsupportedExtension`] for every `gpkg_extensions`
/// row naming an extension this crate cannot identify.
///
/// The rows this crate can name are not reported, whether or not it implements
/// them: knowing what an extension is and which tables it owns is enough to
/// leave it alone.
fn collect_unsupported_extensions(
    conn: &Connection,
    warnings: &mut Vec<OpenWarning>,
) -> Result<()> {
    for row in crate::extensions::read_all(conn)? {
        if row.support() == ExtensionSupport::Unrecognised {
            warnings.push(OpenWarning::UnsupportedExtension {
                extension_name: row.name,
                table_name: row.table_name,
                scope: row.scope,
            });
        }
    }
    Ok(())
}

/// Pushes a [`OpenWarning::TableNameCaseMismatch`] for every `gpkg_contents`
/// row whose `table_name` differs in case from the physical SQLite table it
/// names.
fn collect_case_mismatches(conn: &Connection, warnings: &mut Vec<OpenWarning>) -> Result<()> {
    let declared_names: Vec<String> = {
        let mut stmt = conn.prepare("SELECT table_name FROM gpkg_contents")?;
        stmt.query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    for declared in declared_names {
        if let Some(actual) = resolve_table_name(conn, &declared)?
            && actual != declared
        {
            warnings.push(OpenWarning::TableNameCaseMismatch { declared, actual });
        }
    }
    Ok(())
}
