//! `gpkg_spatial_ref_sys` access: lookup and registration of SRS rows.

use crate::transaction::WriteTransaction;
use crate::{Error, GeoPackage, Result};
use geopackage_core::ident::quote;
use geopackage_core::srs::epsg_definition;
use rusqlite::{Connection, OptionalExtension};

/// Extension name registered in `gpkg_extensions` for the WKT2 column.
///
/// The 1.1 form, which adds `epoch` alongside `definition_12_063`. Both
/// columns are added together, so both get a row.
///
/// The CRS WKT 1.1 document lists three rows as required: `gpkg_crs_wkt`
/// against `definition_12_063`, and `gpkg_crs_wkt_1_1` against each of
/// `definition_12_063` and `epoch`. GDAL writes the first of those on its own
/// while a file has no epoch column, and on adding one it *renames* that row
/// to `gpkg_crs_wkt_1_1` rather than keeping both
/// (`OGRGeoPackageDataSource`, `UPDATE gpkg_extensions SET extension_name =
/// 'gpkg_crs_wkt_1_1' WHERE extension_name = 'gpkg_crs_wkt'`), so a 1.1 file
/// from GDAL has the two rows this crate writes and no `gpkg_crs_wkt` row.
/// The ETS agrees, since its `gpkg_crs_wkt` check is "not testable" on an
/// empty result set. This crate follows GDAL: interoperating with the files
/// that exist beats matching a table no widespread implementation produces.
const CRS_WKT_EXTENSION: &str = "gpkg_crs_wkt_1_1";
const CRS_WKT_DEFINITION: &str = "http://www.geopackage.org/spec/#extension_crs_wkt";

/// A row of `gpkg_spatial_ref_sys`.
///
/// The last two fields belong to the `gpkg_crs_wkt` extension (Annex F.10) and
/// its 1.1 revision, and are `None` for a file without it. Between
/// them the fields cover the whole of that extension's table definition, which
/// is why this is a plain struct rather than a builder: the spec fixes the
/// column set, so there is nothing here that is expected to grow.
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
    /// The `definition_12_063` column: a WKT2 definition, per the
    /// `gpkg_crs_wkt` extension.
    ///
    /// `None` when the file does not register the extension. Where both this and
    /// [`Srs::definition`] are populated, this one takes priority, which is
    /// the note under Requirement 117. Some CRSs have no WKT1 form at all, so
    /// for them [`Srs::definition`] is the literal `undefined` and this is
    /// where the definition lives.
    pub definition_wkt2: Option<String>,
    /// The `epoch` column: the coordinate epoch, per the 1.1 revision of the
    /// `gpkg_crs_wkt` extension.
    ///
    /// `None` when the file has neither the extension nor an epoch for
    /// this row. A GeoPackage may contain several rows for one
    /// `organization_coordsys_id` differing only in epoch.
    pub epoch: Option<f64>,
}

/// The six columns every `gpkg_spatial_ref_sys` has, in the order
/// [`row_to_srs`] reads them.
const BASE_COLUMNS: &str =
    "srs_name, srs_id, organization, organization_coordsys_id, definition, description";

/// Which of the `gpkg_crs_wkt` extension's columns a file has.
///
/// The two are added together by this crate and by GDAL, but they arrived in
/// different versions of the extension, so a file can have `definition_12_063`
/// without `epoch` and the two are checked separately rather than inferred
/// from each other.
#[derive(Debug, Clone, Copy)]
struct CrsWktColumns {
    definition_12_063: bool,
    epoch: bool,
}

impl CrsWktColumns {
    /// One `PRAGMA table_info` rather than one per column.
    fn read(conn: &Connection) -> Result<Self> {
        let mut stmt = conn.prepare("PRAGMA table_info(gpkg_spatial_ref_sys)")?;
        let mut rows = stmt.query([])?;
        let mut columns = Self {
            definition_12_063: false,
            epoch: false,
        };
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name.eq_ignore_ascii_case("definition_12_063") {
                columns.definition_12_063 = true;
            } else if name.eq_ignore_ascii_case("epoch") {
                columns.epoch = true;
            }
        }
        Ok(columns)
    }

    /// The select list: the base columns, then the extension columns the file
    /// has, so the reader can index by position either way.
    fn select_list(self) -> String {
        let mut list = BASE_COLUMNS.to_owned();
        if self.definition_12_063 {
            list.push_str(", definition_12_063");
        }
        if self.epoch {
            list.push_str(", epoch");
        }
        list
    }
}

/// Reads one row of [`CrsWktColumns::select_list`].
fn row_to_srs(row: &rusqlite::Row<'_>, columns: CrsWktColumns) -> rusqlite::Result<Srs> {
    // The extension columns follow the six base ones, in the order
    // `select_list` appends them, so an absent column shifts the next one down
    // rather than leaving a hole.
    let mut next = 6;
    let mut take = |present: bool| {
        present.then(|| {
            let index = next;
            next += 1;
            index
        })
    };
    let wkt2_index = take(columns.definition_12_063);
    let epoch_index = take(columns.epoch);
    Ok(Srs {
        name: row.get(0)?,
        srs_id: row.get(1)?,
        organization: row.get(2)?,
        organization_coordsys_id: row.get(3)?,
        definition: row.get(4)?,
        description: row.get(5)?,
        // `undefined` is the spec's own "no definition here" value
        // (Requirement 117), and the default this crate and GDAL give the
        // column, so it reads back as absent rather than as a definition.
        definition_wkt2: match wkt2_index {
            Some(index) => row.get::<_, Option<String>>(index)?.filter(is_defined),
            None => None,
        },
        epoch: match epoch_index {
            Some(index) => row.get(index)?,
            None => None,
        },
    })
}

fn is_defined(definition: &String) -> bool {
    definition != "undefined"
}

impl GeoPackage {
    /// Looks up the SRS row with the given `srs_id`, if present.
    pub fn srs(&self, srs_id: i32) -> Result<Option<Srs>> {
        let conn = self.connection();
        let columns = CrsWktColumns::read(conn)?;
        conn.query_row(
            &format!(
                "SELECT {} FROM gpkg_spatial_ref_sys WHERE srs_id = ?1",
                columns.select_list()
            ),
            [srs_id],
            |r| row_to_srs(r, columns),
        )
        .optional()
        .map_err(Error::from)
    }

    /// Returns all SRS rows, ascending by `srs_id`.
    pub fn srs_list(&self) -> Result<Vec<Srs>> {
        let conn = self.connection();
        let columns = CrsWktColumns::read(conn)?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM gpkg_spatial_ref_sys ORDER BY srs_id",
            columns.select_list()
        ))?;
        let rows = stmt.query_map([], |r| row_to_srs(r, columns))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Inserts an SRS row.
    ///
    /// Returns `true` if the row was inserted, `false` if a row with that
    /// `srs_id` already exists (the existing row is left untouched: within a
    /// file, the existing definition is authoritative).
    ///
    /// Supplying [`Srs::definition_wkt2`] or [`Srs::epoch`] adds the
    /// `gpkg_crs_wkt` extension's columns to the file and registers the
    /// extension if it is not there already, which is how a caller supplies a
    /// definition for a CRS that has no WKT1 form. The added columns and the
    /// insert share one transaction, so a failure leaves the file as it was
    /// rather than half-extended.
    pub fn add_srs(&self, srs: &Srs) -> Result<bool> {
        if self.srs(srs.srs_id)?.is_some() {
            return Ok(false);
        }
        let conn = self.connection();
        let tx = WriteTransaction::begin(conn)?;
        if srs.definition_wkt2.is_some() || srs.epoch.is_some() {
            enable_crs_wkt_extension(conn)?;
        }
        // Whether the extension columns can be bound at all depends on the
        // file, not on this row: a file that already has them takes the
        // values, one that does not cannot be given them without the ALTER
        // above, which only a caller supplying one of the two asks for.
        let columns = CrsWktColumns::read(conn)?;
        let mut names = BASE_COLUMNS.to_owned();
        let mut placeholders = "?1, ?2, ?3, ?4, ?5, ?6".to_owned();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![
            Box::new(srs.name.clone()),
            Box::new(srs.srs_id),
            Box::new(srs.organization.clone()),
            Box::new(srs.organization_coordsys_id),
            Box::new(srs.definition.clone()),
            Box::new(srs.description.clone()),
        ];
        if columns.definition_12_063 {
            names.push_str(", definition_12_063");
            placeholders.push_str(", ?7");
            // The column is NOT NULL, and `undefined` is the spec's value for
            // a definition that cannot be produced (Requirement 117).
            params.push(Box::new(
                srs.definition_wkt2
                    .clone()
                    .unwrap_or_else(|| "undefined".to_owned()),
            ));
        }
        if columns.epoch {
            names.push_str(", epoch");
            placeholders.push_str(if columns.definition_12_063 {
                ", ?8"
            } else {
                ", ?7"
            });
            params.push(Box::new(srs.epoch));
        }
        conn.execute(
            &format!("INSERT INTO gpkg_spatial_ref_sys ({names}) VALUES ({placeholders})"),
            rusqlite::params_from_iter(params.iter()),
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Inserts the vendored definition for an EPSG code (see
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
                definition_wkt2: None,
                epoch: None,
            });
        }
        self.add_epsg_srs_via_wkt2(code)
    }

    /// Registers an EPSG code that has no WKT1 form in the vendored subset,
    /// using WKT2 in the `gpkg_crs_wkt_1_1` extension column.
    ///
    /// Some CRSs cannot be expressed in WKT1 at all: geographic 3D codes such
    /// as EPSG:4979 are the common case, since WKT1's `GEOGCS` has no way to
    /// express a third axis. For those the spec's answer is the extension
    /// column, and `definition` contains the literal `undefined`. This is what
    /// GDAL writes for the same codes, so the files interoperate.
    fn add_epsg_srs_via_wkt2(&self, code: i32) -> Result<bool> {
        let wkt2 = epsg_utils::epsg_to_wkt2(code)
            .ok()
            .ok_or(Error::UnknownEpsgCode { code })?;
        self.add_srs(&Srs {
            name: srs_name_from_wkt2(wkt2, code),
            srs_id: code,
            organization: "EPSG".to_owned(),
            organization_coordsys_id: code,
            definition: "undefined".to_owned(),
            description: None,
            definition_wkt2: Some(wkt2.to_owned()),
            epoch: None,
        })
    }
}

/// Adds the `gpkg_crs_wkt_1_1` columns and registers the extension, if the
/// file does not already have them. Idempotent.
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

/// Populates `definition_12_063` for rows already in the table whose EPSG
/// code the registry knows.
///
/// Without this, a file that gains the extension would have WKT2 for the code
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

/// Extracts the CRS name from a WKT2 string: the quoted token right after the
/// opening keyword, as in `GEODCRS["WGS 84",...`.
///
/// A full parse would buy nothing here. If the shape is unexpected the code is
/// used instead, which is always a usable name.
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
