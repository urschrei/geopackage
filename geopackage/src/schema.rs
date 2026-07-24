//! User-table schema introspection: `gpkg_geometry_columns` rows and
//! per-table column metadata read from `PRAGMA table_info`.

use crate::{Error, GeoPackage, Result, table_exists};
use geopackage_core::types::{GeometryType, ZmFlag};
use rusqlite::OptionalExtension;

/// A row of `gpkg_geometry_columns` (spec Table 21): the geometry column of a
/// feature table, its declared geometry type, spatial reference system, and
/// `z`/`m` dimension constraints.
#[derive(Debug, Clone, PartialEq)]
pub struct GeometryColumn {
    /// Feature table this geometry column belongs to.
    pub table_name: String,
    /// Name of the geometry column within that table.
    pub column_name: String,
    /// Declared geometry type. Extension (non-linear) type names parse and are
    /// accepted on read; see [`GeometryType`].
    pub geometry_type: GeometryType,
    /// Spatial reference system identifier of the geometries.
    pub srs_id: i32,
    /// Constraint on the `z` (elevation) dimension.
    pub z: ZmFlag,
    /// Constraint on the `m` (measure) dimension.
    pub m: ZmFlag,
}

impl GeometryColumn {
    /// Build a row from the raw column values, parsing the geometry type name
    /// and `z`/`m` codes into their spec vocabulary.
    fn from_raw(
        table_name: String,
        column_name: String,
        geometry_type_name: String,
        srs_id: i32,
        z: i64,
        m: i64,
    ) -> Result<Self> {
        let geometry_type =
            GeometryType::parse(&geometry_type_name).ok_or_else(|| Error::UnknownGeometryType {
                table_name: table_name.clone(),
                name: geometry_type_name,
            })?;
        let z = zm_flag(&table_name, "z", z)?;
        let m = zm_flag(&table_name, "m", m)?;
        Ok(Self {
            table_name,
            column_name,
            geometry_type,
            srs_id,
            z,
            m,
        })
    }
}

/// Convert a raw `z`/`m` code into a [`ZmFlag`], mapping an out-of-range value
/// to a typed error.
fn zm_flag(table_name: &str, column: &'static str, value: i64) -> Result<ZmFlag> {
    u8::try_from(value)
        .ok()
        .and_then(ZmFlag::from_code)
        .ok_or(Error::InvalidZmFlag {
            table_name: table_name.to_owned(),
            column,
            value,
        })
}

const GEOMETRY_COLUMNS_SELECT: &str =
    "SELECT table_name, column_name, geometry_type_name, srs_id, z, m FROM gpkg_geometry_columns";

impl GeoPackage {
    /// Look up the `gpkg_geometry_columns` row for `table_name`, if any.
    ///
    /// `gpkg_geometry_columns` is created lazily with the first feature table
    /// and is absent from attribute-only files; a missing table is reported as
    /// `Ok(None)`, not an error.
    pub fn geometry_column(&self, table_name: &str) -> Result<Option<GeometryColumn>> {
        if !table_exists(self.connection(), "gpkg_geometry_columns")? {
            return Ok(None);
        }
        let raw = self
            .connection()
            .query_row(
                &format!("{GEOMETRY_COLUMNS_SELECT} WHERE table_name = ?1"),
                [table_name],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i32>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()?;
        raw.map(|(t, c, g, s, z, m)| GeometryColumn::from_raw(t, c, g, s, z, m))
            .transpose()
    }

    /// All `gpkg_geometry_columns` rows, ordered by table name.
    ///
    /// Returns an empty vector when the table is absent (see
    /// [`GeoPackage::geometry_column`]).
    pub fn geometry_columns(&self) -> Result<Vec<GeometryColumn>> {
        if !table_exists(self.connection(), "gpkg_geometry_columns")? {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .connection()
            .prepare(&format!("{GEOMETRY_COLUMNS_SELECT} ORDER BY table_name"))?;
        let raw = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i32>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        raw.into_iter()
            .map(|(t, c, g, s, z, m)| GeometryColumn::from_raw(t, c, g, s, z, m))
            .collect()
    }
}
