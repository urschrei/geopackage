//! User-table schema introspection: `gpkg_geometry_columns` rows and
//! per-table column metadata read from `PRAGMA table_info`.

use crate::{Error, GeoPackage, Result, table_exists};
use geopackage_core::ident;
use geopackage_core::schema::DataColumn;
use geopackage_core::types::{ColumnType, GeometryType, ZmFlag};
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

    /// Like [`GeoPackage::geometry_column`] but matching the table name
    /// case-insensitively.
    ///
    /// The read path uses this so a feature layer keeps its geometry column
    /// even when the `gpkg_geometry_columns` row spells the table name in a
    /// different case than `gpkg_contents` or the physical table (one of the
    /// conditions [`GeoPackage::open_lenient`] tolerates and warns about).
    pub(crate) fn geometry_column_ci(&self, table_name: &str) -> Result<Option<GeometryColumn>> {
        if !table_exists(self.connection(), "gpkg_geometry_columns")? {
            return Ok(None);
        }
        let raw = self
            .connection()
            .query_row(
                &format!("{GEOMETRY_COLUMNS_SELECT} WHERE table_name = ?1 COLLATE NOCASE"),
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

    /// Introspect a user table via `PRAGMA table_info`, attaching its
    /// `gpkg_geometry_columns` row if one exists.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchTable`] if the pragma returns no columns (the table is
    /// absent).
    pub fn table_schema(&self, table_name: &str) -> Result<TableSchema> {
        let sql = format!("PRAGMA table_info({})", ident::quote(table_name)?);
        let mut stmt = self.connection().prepare(&sql)?;
        // PRAGMA table_info columns: cid, name, type, notnull, dflt_value, pk.
        let mut columns = stmt
            .query_map([], |r| {
                let declared_type: String = r.get(2)?;
                Ok(Column {
                    name: r.get(1)?,
                    column_type: ColumnType::parse(&declared_type),
                    declared_type,
                    not_null: r.get::<_, i64>(3)? != 0,
                    default_value: r.get(4)?,
                    primary_key: r.get::<_, u32>(5)?,
                    data_column: None,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if columns.is_empty() {
            return Err(Error::NoSuchTable {
                table_name: table_name.to_owned(),
            });
        }
        // One query for the table's `gpkg_data_columns` rows, attached by
        // name, rather than a lookup per column. Empty, and so free beyond the
        // `sqlite_master` check, for a file without the `gpkg_schema`
        // extension.
        let mut descriptions = self.data_columns(table_name)?;
        for column in &mut columns {
            if let Some(index) = descriptions
                .iter()
                .position(|described| described.column_name == column.name)
            {
                column.data_column = Some(descriptions.swap_remove(index));
            }
        }
        Ok(TableSchema {
            table_name: table_name.to_owned(),
            columns,
            geometry_column: self.geometry_column(table_name)?,
        })
    }
}

/// Metadata for one column of a user table, from `PRAGMA table_info`.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    /// Column name.
    pub name: String,
    /// The declared type exactly as stored in the schema (e.g. `"TEXT(64)"`,
    /// `"VARCHAR(20)"`, or the empty string for a column declared without a
    /// type). Preserved verbatim even when it is outside the spec vocabulary.
    pub declared_type: String,
    /// The declared type parsed into the spec vocabulary, or `None` when
    /// [`Column::declared_type`] is outside it (lenient readers keep the raw
    /// string).
    pub column_type: Option<ColumnType>,
    /// Whether the column has a `NOT NULL` constraint.
    pub not_null: bool,
    /// The column default as SQL source text, or `None` for no default.
    pub default_value: Option<String>,
    /// Position of the column within the primary key: `0` if it is not part of
    /// the key, otherwise its 1-based position. SQLite reports a composite key
    /// as `1, 2, …`; a conformant GeoPackage feature or attribute table has a
    /// single `INTEGER` primary key column reported as `1`.
    pub primary_key: u32,
    /// The column's `gpkg_data_columns` row, for a file carrying the
    /// `gpkg_schema` extension: a human-readable name and title, a
    /// description, a MIME type, and the name of any constraint its values are
    /// subject to.
    ///
    /// `None` for a file without the extension, or a column it does not
    /// describe. Resolve [`DataColumn::constraint_name`] with
    /// [`GeoPackage::column_constraint`].
    pub data_column: Option<DataColumn>,
}

impl Column {
    /// Whether this column participates in the table's primary key.
    pub fn is_primary_key(&self) -> bool {
        self.primary_key != 0
    }
}

/// The introspected schema of a user table: its columns and, for feature
/// tables, the attached [`GeometryColumn`].
#[derive(Debug, Clone, PartialEq)]
pub struct TableSchema {
    /// Table name.
    pub table_name: String,
    /// Columns in declaration order (as `PRAGMA table_info` reports them).
    pub columns: Vec<Column>,
    /// The `gpkg_geometry_columns` row for this table, if any.
    pub geometry_column: Option<GeometryColumn>,
}

impl TableSchema {
    /// The column with the given name, if present.
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// The primary-key columns, ordered by their position within the key.
    ///
    /// Empty when the table has no primary key; more than one element for a
    /// composite key (which is not valid for a GeoPackage feature or attribute
    /// table, but is surfaced here rather than rejected).
    pub fn primary_key_columns(&self) -> Vec<&Column> {
        let mut pk: Vec<&Column> = self.columns.iter().filter(|c| c.is_primary_key()).collect();
        pk.sort_by_key(|c| c.primary_key);
        pk
    }

    /// The single primary-key column, or `None` when the table has no primary
    /// key or a composite one. This is the conventional `fid` column of a
    /// GeoPackage feature or attribute table, but the name is read from the
    /// schema and never assumed.
    pub fn primary_key(&self) -> Option<&Column> {
        let pk = self.primary_key_columns();
        match pk.as_slice() {
            [single] => Some(single),
            _ => None,
        }
    }
}
