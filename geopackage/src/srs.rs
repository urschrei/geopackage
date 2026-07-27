//! `gpkg_spatial_ref_sys` access: lookup and registration of SRS rows.

use crate::{Error, GeoPackage, Result};
use geopackage_core::ident::quote;
use geopackage_core::srs::epsg_definition;
use rusqlite::{Connection, OptionalExtension};

/// Extension name registered in `gpkg_extensions` for the WKT2 column.
///
/// The 1.1 form, which adds `epoch` alongside `definition_12_063`. Both
/// columns are added together, so both get a row.
const CRS_WKT_EXTENSION: &str = "gpkg_crs_wkt_1_1";
const CRS_WKT_DEFINITION: &str = "http://www.geopackage.org/spec/#extension_crs_wkt";

/// A row of `gpkg_spatial_ref_sys`.
#[derive(Debug, Clone, PartialEq)]
pub struct Srs {
    /// Human-readable SRS name.
    pub name: String,
    /// Unique identifier within this GeoPackage (also the `srs_id` used by
    /// `gpkg_contents`, `gpkg_geometry_columns`, and GPB headers).
    pub srs_id: i32,
    /// Case-insensitive name of the defining organization, e.g. `EPSG`.
    pub organization: String,
    /// Numeric ID of the SRS as assigned by the organization.
    pub organization_coordsys_id: i32,
    /// WKT1 definition, or the literal `undefined`.
    pub definition: String,
    /// Human-readable description.
    pub description: Option<String>,
}

impl GeoPackage {
    /// Look up the SRS row with the given `srs_id`, if present.
    pub fn srs(&self, srs_id: i32) -> Result<Option<Srs>> {
        self.connection()
            .query_row(
                "SELECT srs_name, srs_id, organization, organization_coordsys_id, \
                 definition, description FROM gpkg_spatial_ref_sys WHERE srs_id = ?1",
                [srs_id],
                |r| {
                    Ok(Srs {
                        name: r.get(0)?,
                        srs_id: r.get(1)?,
                        organization: r.get(2)?,
                        organization_coordsys_id: r.get(3)?,
                        definition: r.get(4)?,
                        description: r.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(Error::from)
    }

    /// All SRS rows, ascending by `srs_id`.
    pub fn srs_list(&self) -> Result<Vec<Srs>> {
        let mut stmt = self.connection().prepare(
            "SELECT srs_name, srs_id, organization, organization_coordsys_id, \
             definition, description FROM gpkg_spatial_ref_sys ORDER BY srs_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Srs {
                name: r.get(0)?,
                srs_id: r.get(1)?,
                organization: r.get(2)?,
                organization_coordsys_id: r.get(3)?,
                definition: r.get(4)?,
                description: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Insert an SRS row.
    ///
    /// Returns `true` if the row was inserted, `false` if a row with that
    /// `srs_id` already exists (the existing row is left untouched: within a
    /// file, the existing definition is authoritative).
    pub fn add_srs(&self, srs: &Srs) -> Result<bool> {
        if self.srs(srs.srs_id)?.is_some() {
            return Ok(false);
        }
        self.connection().execute(
            "INSERT INTO gpkg_spatial_ref_sys \
             (srs_name, srs_id, organization, organization_coordsys_id, definition, description) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (
                &srs.name,
                srs.srs_id,
                &srs.organization,
                srs.organization_coordsys_id,
                &srs.definition,
                &srs.description,
            ),
        )?;
        Ok(true)
    }

    /// Insert the vendored definition for an EPSG code (see
    /// [`geopackage_core::srs`] for the vendored subset).
    ///
    /// The row's `srs_id` is the EPSG code itself, per convention. Returns
    /// `true` if inserted, `false` if the file already has a row for that id.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownEpsgCode`] if the code is in neither the vendored WKT1
    /// subset nor the EPSG registry; supply the definition yourself via
    /// [`GeoPackage::add_srs`].
    pub fn add_epsg_srs(&self, code: i32) -> Result<bool> {
        if let Some(def) = epsg_definition(code) {
            return self.add_srs(&Srs {
                name: def.name.into_owned(),
                srs_id: code,
                organization: def.organization.into_owned(),
                organization_coordsys_id: def.organization_coordsys_id,
                definition: def.definition.into_owned(),
                description: None,
            });
        }
        self.add_epsg_srs_via_wkt2(code)
    }

    /// Register an EPSG code that has no WKT1 form in the vendored subset,
    /// using WKT2 in the `gpkg_crs_wkt_1_1` extension column.
    ///
    /// Some CRSs cannot be expressed in WKT1 at all: geographic 3D codes such
    /// as EPSG:4979 are the common case, since WKT1's `GEOGCS` has no way to
    /// carry a third axis. For those the spec's answer is the extension
    /// column, and `definition` holds the literal `undefined`. This is what
    /// GDAL writes for the same codes, so the files interoperate.
    fn add_epsg_srs_via_wkt2(&self, code: i32) -> Result<bool> {
        let wkt2 = epsg_utils::epsg_to_wkt2(code)
            .ok()
            .ok_or(Error::UnknownEpsgCode { code })?;
        if self.srs(code)?.is_some() {
            return Ok(false);
        }
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        // Enabling the extension rewrites the SRS table's shape and backfills
        // other rows, so it shares the transaction with the insert: a caller
        // that hits an error here finds the file exactly as it was, rather
        // than carrying a half-applied extension.
        enable_crs_wkt_extension(&tx)?;
        tx.execute(
            "INSERT INTO gpkg_spatial_ref_sys \
             (srs_name, srs_id, organization, organization_coordsys_id, definition, \
              description, definition_12_063) \
             VALUES (?1, ?2, 'EPSG', ?2, 'undefined', NULL, ?3)",
            rusqlite::params![srs_name_from_wkt2(wkt2, code), code, wkt2],
        )?;
        tx.commit()?;
        Ok(true)
    }
}

/// Add the `gpkg_crs_wkt_1_1` columns and register the extension, if the file
/// does not already have them. Idempotent.
fn enable_crs_wkt_extension(conn: &Connection) -> Result<()> {
    if column_exists(conn, "gpkg_spatial_ref_sys", "definition_12_063")? {
        return Ok(());
    }
    // SQLite will not add a NOT NULL column to a populated table without a
    // default, and the spec's own value for "no definition here" is the
    // literal `undefined`, which is what the two reserved rows (-1 and 0)
    // carry in the WKT1 column too.
    conn.execute_batch(
        "ALTER TABLE gpkg_spatial_ref_sys \
           ADD COLUMN definition_12_063 TEXT NOT NULL DEFAULT 'undefined'; \
         ALTER TABLE gpkg_spatial_ref_sys ADD COLUMN epoch DOUBLE;",
    )?;
    backfill_wkt2(conn)?;
    for column in ["definition_12_063", "epoch"] {
        crate::extensions::register(
            conn,
            Some("gpkg_spatial_ref_sys"),
            Some(column),
            CRS_WKT_EXTENSION,
            CRS_WKT_DEFINITION,
            "read-write",
        )?;
    }
    Ok(())
}

/// Populate `definition_12_063` for rows already in the table whose EPSG code
/// the registry knows.
///
/// Without this, a file that gains the extension would carry WKT2 for the code
/// that triggered it and `undefined` for every code registered earlier, which
/// reads as "these have no WKT2 form" when they do. GDAL backfills the same
/// way. Rows outside the registry keep `undefined`, which is accurate.
fn backfill_wkt2(conn: &Connection) -> Result<()> {
    let codes: Vec<i32> = conn
        .prepare(
            "SELECT srs_id FROM gpkg_spatial_ref_sys \
             WHERE organization_coordsys_id > 0 AND organization LIKE 'EPSG'",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for code in codes {
        if let Ok(wkt2) = epsg_utils::epsg_to_wkt2(code) {
            conn.execute(
                "UPDATE gpkg_spatial_ref_sys SET definition_12_063 = ?1 WHERE srs_id = ?2",
                rusqlite::params![wkt2, code],
            )?;
        }
    }
    Ok(())
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", quote(table)?))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)?.eq_ignore_ascii_case(column) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Pull the CRS name out of a WKT2 string: the quoted token right after the
/// opening keyword, as in `GEODCRS["WGS 84",...`.
///
/// A full parse would buy nothing here. If the shape is not what we expect we
/// fall back to the code, which is always a usable name.
fn srs_name_from_wkt2(wkt2: &str, code: i32) -> String {
    wkt2.split_once('"')
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(name, _)| name)
        .filter(|name| !name.is_empty())
        .map_or_else(|| format!("EPSG:{code}"), ToOwned::to_owned)
}

#[cfg(test)]
mod crs_wkt_tests {
    use super::*;
    use crate::GeoPackage;

    /// EPSG:4979 is geographic 3D, so it has no WKT1 form at all. Before the
    /// extension it was simply unregistrable.
    #[test]
    fn geographic_3d_code_lands_in_the_extension_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.gpkg");
        let gpkg = GeoPackage::create(&path).unwrap();
        assert!(gpkg.add_epsg_srs(4979).unwrap());

        let (definition, wkt2): (String, String) = gpkg
            .connection()
            .query_row(
                "SELECT definition, definition_12_063 FROM gpkg_spatial_ref_sys \
                 WHERE srs_id = 4979",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(definition, "undefined");
        // The third axis is the whole point: this is what WKT1 could not say.
        assert!(wkt2.contains("CS[ellipsoidal,3"), "{wkt2}");
        assert!(wkt2.contains(r#"ID["EPSG",4979]"#), "{wkt2}");
        assert_eq!(gpkg.srs(4979).unwrap().unwrap().name, "WGS 84");

        // The extension has to be declared, or a reader is entitled to reject
        // the column outright.
        let rows: i64 = gpkg
            .connection()
            .query_row(
                "SELECT count(*) FROM gpkg_extensions \
                 WHERE extension_name = 'gpkg_crs_wkt_1_1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 2, "one row each for definition_12_063 and epoch");
    }

    #[test]
    fn enabling_the_extension_backfills_rows_already_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.gpkg");
        let gpkg = GeoPackage::create(&path).unwrap();
        // 4326 is in the file from creation, carrying WKT1 and no extension
        // column at all. Adding 4979 is what introduces the column.
        assert_eq!(gpkg.srs(4326).unwrap().unwrap().srs_id, 4326);
        gpkg.add_epsg_srs(4979).unwrap();

        let wkt2: String = gpkg
            .connection()
            .query_row(
                "SELECT definition_12_063 FROM gpkg_spatial_ref_sys WHERE srs_id = 4326",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            wkt2.contains(r#"ID["EPSG",4326]"#),
            "4326 should have gained WKT2, got {wkt2}"
        );
    }

    #[test]
    fn adding_a_second_extension_code_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.gpkg");
        let gpkg = GeoPackage::create(&path).unwrap();
        gpkg.add_epsg_srs(4979).unwrap();
        gpkg.add_epsg_srs(4937).unwrap();
        assert!(!gpkg.add_epsg_srs(4979).unwrap(), "already present");

        let rows: i64 = gpkg
            .connection()
            .query_row(
                "SELECT count(*) FROM gpkg_extensions \
                 WHERE extension_name = 'gpkg_crs_wkt_1_1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 2, "the extension is registered once, not per code");
    }

    #[test]
    fn a_code_in_neither_the_subset_nor_the_registry_is_still_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.gpkg");
        let gpkg = GeoPackage::create(&path).unwrap();
        assert!(matches!(
            gpkg.add_epsg_srs(999_999),
            Err(Error::UnknownEpsgCode { code: 999_999 })
        ));
    }
}
