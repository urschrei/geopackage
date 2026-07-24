//! `gpkg_spatial_ref_sys` access: lookup and registration of SRS rows.

use crate::{Error, GeoPackage, Result};
use geopackage_core::srs::epsg_definition;
use rusqlite::OptionalExtension;

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
    /// [`Error::UnknownEpsgCode`] if the code is outside the vendored subset;
    /// supply the definition yourself via [`GeoPackage::add_srs`].
    pub fn add_epsg_srs(&self, code: i32) -> Result<bool> {
        let def = epsg_definition(code).ok_or(Error::UnknownEpsgCode { code })?;
        self.add_srs(&Srs {
            name: def.name.into_owned(),
            srs_id: code,
            organization: def.organization.into_owned(),
            organization_coordsys_id: def.organization_coordsys_id,
            definition: def.definition.into_owned(),
            description: None,
        })
    }
}
