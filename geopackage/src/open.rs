//! Lenient open path: [`GeoPackage::open_lenient`] and the [`OpenWarning`]s it
//! collects.
//!
//! Strict [`GeoPackage::open`] identifies a GeoPackage and rejects anything it
//! cannot. `open_lenient` opens the same files but, rather than being stricter,
//! records typed warnings for conditions a fastidious reader would flag —
//! legacy `application_id`s, a missing `gpkg_geometry_columns` table, and
//! catalogue table names that match a real SQLite table only case-insensitively
//! — so callers can inspect and iterate a lightly non-conforming file instead
//! of being turned away. Strict [`GeoPackage::open`] is unchanged.

use crate::{
    Error, GeoPackage, Result, functions, read_header_u32, resolve_table_name, table_exists,
};
use geopackage_core::GpkgVersion;
use geopackage_core::version::{APPLICATION_ID_GP10, APPLICATION_ID_GP11};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

/// A non-fatal condition [`GeoPackage::open_lenient`] tolerated while opening a
/// file. Retrieve the list with [`GeoPackage::open_warnings`].
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
    /// attribute-only GeoPackage; it means the file carries no feature layers.
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
}

impl GeoPackage {
    /// Open an existing GeoPackage read-write, tolerating a set of legacy and
    /// lightly non-conforming conditions that are recorded as [`OpenWarning`]s
    /// rather than errors.
    ///
    /// Retrieve the warnings with [`GeoPackage::open_warnings`]. A file that
    /// cannot be identified as a GeoPackage at all, or is missing a required
    /// core table (`gpkg_spatial_ref_sys`, `gpkg_contents`), is still an error —
    /// leniency covers presentation, not identity.
    pub fn open_lenient<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        Self::from_connection_lenient(conn)
    }

    /// The warnings collected by [`GeoPackage::open_lenient`] (always empty for
    /// a handle opened with strict [`GeoPackage::open`] or created fresh).
    pub fn open_warnings(&self) -> &[OpenWarning] {
        &self.warnings
    }

    fn from_connection_lenient(conn: Connection) -> Result<Self> {
        let application_id = read_header_u32(&conn, "application_id")?;
        let user_version = read_header_u32(&conn, "user_version")?;
        let version = GpkgVersion::from_pragmas(application_id, user_version).ok_or(
            Error::NotAGeoPackage {
                reason: "unrecognized application_id/user_version",
                application_id,
                user_version,
            },
        )?;
        for required in ["gpkg_spatial_ref_sys", "gpkg_contents"] {
            if !table_exists(&conn, required)? {
                return Err(Error::NotAGeoPackage {
                    reason: "missing required core table",
                    application_id,
                    user_version,
                });
            }
        }

        let mut warnings = Vec::new();
        if application_id == APPLICATION_ID_GP10 || application_id == APPLICATION_ID_GP11 {
            warnings.push(OpenWarning::LegacyApplicationId {
                version,
                application_id,
            });
        }
        if !table_exists(&conn, "gpkg_geometry_columns")? {
            warnings.push(OpenWarning::MissingGeometryColumns);
        }
        collect_case_mismatches(&conn, &mut warnings)?;

        functions::register(&conn)?;
        Ok(Self {
            conn,
            version,
            warnings,
        })
    }
}

/// Push a [`OpenWarning::TableNameCaseMismatch`] for every `gpkg_contents` row
/// whose `table_name` differs in case from the physical SQLite table it names.
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
