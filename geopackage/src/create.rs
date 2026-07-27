//! Layer creation: the [`TableSchemaBuilder`] and the
//! [`GeoPackage::create_layer`] / [`GeoPackage::create_attributes_table`]
//! entry points.
//!
//! A builder declares a user table's columns, primary key, and (for feature
//! tables) geometry column; the create methods emit the user-table DDL and the
//! `gpkg_contents` (and, for feature tables, `gpkg_geometry_columns`) catalogue
//! rows in one transaction. `gpkg_geometry_columns` is created lazily on first
//! use from the normative [`ddl::CREATE_GPKG_GEOMETRY_COLUMNS`].
//!
//! Schema is declared through an explicit builder, not a derive macro, so a
//! table's shape can be chosen at run time rather than fixed at compile time.
//! Column defaults are raw SQL text, trusted from the caller; every identifier
//! is quoted via [`ident::quote`].

use geopackage_core::ddl;
use geopackage_core::ident::quote;
use geopackage_core::types::{ColumnType, GeometryType, ZmFlag};

use rusqlite::Connection;

use crate::{Error, GeoPackage, Layer, Result, table_exists};

/// The conventional primary-key column name for a GeoPackage feature or
/// attribute table.
pub const DEFAULT_PRIMARY_KEY: &str = "fid";
/// The conventional geometry column name for a GeoPackage feature table.
pub const DEFAULT_GEOMETRY_COLUMN: &str = "geom";

/// One non-geometry, non-primary-key column of a [`TableSchemaBuilder`].
///
/// The declared type is a spec [`ColumnType`]; `NOT NULL`, `UNIQUE`, and a
/// `DEFAULT` expression are optional constraints. The default expression is
/// emitted verbatim into the DDL (raw SQL, trusted from the caller).
#[derive(Debug, Clone)]
pub struct ColumnSpec {
    name: String,
    column_type: ColumnType,
    not_null: bool,
    unique: bool,
    default: Option<String>,
}

impl ColumnSpec {
    /// A nullable column of the given name and type, with no constraints.
    pub fn new(name: impl Into<String>, column_type: ColumnType) -> Self {
        Self {
            name: name.into(),
            column_type,
            not_null: false,
            unique: false,
            default: None,
        }
    }

    /// Mark the column `NOT NULL`.
    #[must_use]
    pub fn not_null(mut self) -> Self {
        self.not_null = true;
        self
    }

    /// Mark the column `UNIQUE`.
    #[must_use]
    pub fn unique(mut self) -> Self {
        self.unique = true;
        self
    }

    /// Set a `DEFAULT` expression, as raw SQL text (e.g. `"0"`, `"'n/a'"`,
    /// `"CURRENT_TIMESTAMP"`). Emitted verbatim, so it is the caller's
    /// responsibility to supply a valid, safe expression.
    #[must_use]
    pub fn default_value(mut self, sql: impl Into<String>) -> Self {
        self.default = Some(sql.into());
        self
    }

    /// The DDL fragment for this column, e.g. `"name" TEXT(64) NOT NULL`.
    fn to_ddl(&self) -> Result<String> {
        let mut ddl = format!("{} {}", quote(&self.name)?, self.column_type.ddl_name());
        if self.not_null {
            ddl.push_str(" NOT NULL");
        }
        if let Some(default) = &self.default {
            ddl.push_str(" DEFAULT ");
            ddl.push_str(default);
        }
        if self.unique {
            ddl.push_str(" UNIQUE");
        }
        Ok(ddl)
    }
}

/// The geometry column of a feature table: its name, type, spatial reference
/// system, and `z`/`m` dimension constraints.
#[derive(Debug, Clone)]
pub struct GeometrySpec {
    column_name: String,
    geometry_type: GeometryType,
    srs_id: i32,
    z: ZmFlag,
    m: ZmFlag,
}

impl GeometrySpec {
    /// A geometry column named [`DEFAULT_GEOMETRY_COLUMN`], with the given type
    /// and spatial reference system, and `z`/`m` both [`ZmFlag::Prohibited`]
    /// (a 2D column).
    pub fn new(geometry_type: GeometryType, srs_id: i32) -> Self {
        Self {
            column_name: DEFAULT_GEOMETRY_COLUMN.to_owned(),
            geometry_type,
            srs_id,
            z: ZmFlag::Prohibited,
            m: ZmFlag::Prohibited,
        }
    }

    /// Override the geometry column name (default [`DEFAULT_GEOMETRY_COLUMN`]).
    #[must_use]
    pub fn column_name(mut self, name: impl Into<String>) -> Self {
        self.column_name = name.into();
        self
    }

    /// Set the `z` (elevation) dimension constraint.
    #[must_use]
    pub fn z(mut self, flag: ZmFlag) -> Self {
        self.z = flag;
        self
    }

    /// Set the `m` (measure) dimension constraint.
    #[must_use]
    pub fn m(mut self, flag: ZmFlag) -> Self {
        self.m = flag;
        self
    }

    /// The declared geometry type.
    pub fn geometry_type(&self) -> GeometryType {
        self.geometry_type
    }

    /// The spatial reference system identifier.
    pub fn srs_id(&self) -> i32 {
        self.srs_id
    }
}

/// A declarative builder for a user table's schema.
///
/// Build up columns, an optional geometry column, and metadata, then pass the
/// builder to [`GeoPackage::create_layer`] (feature table) or
/// [`GeoPackage::create_attributes_table`] (non-spatial table).
///
/// The emitted table has an `INTEGER PRIMARY KEY AUTOINCREMENT` column
/// (conventionally `fid`, overridable via [`Self::primary_key`]) first, then
/// the [`ColumnSpec`] columns in the order added, then the geometry column
/// last for a feature table.
#[derive(Debug, Clone)]
pub struct TableSchemaBuilder {
    table_name: String,
    identifier: Option<String>,
    description: Option<String>,
    primary_key: String,
    columns: Vec<ColumnSpec>,
    geometry: Option<GeometrySpec>,
    spatial_index: bool,
}

impl TableSchemaBuilder {
    /// Start a builder for a table of the given name.
    ///
    /// The name is validated (and rejected if it begins `gpkg_`) when the
    /// builder is passed to a create method, not here.
    pub fn new(table_name: impl Into<String>) -> Self {
        Self {
            table_name: table_name.into(),
            identifier: None,
            description: None,
            primary_key: DEFAULT_PRIMARY_KEY.to_owned(),
            columns: Vec::new(),
            geometry: None,
            spatial_index: true,
        }
    }

    /// Override the primary-key column name (default [`DEFAULT_PRIMARY_KEY`]).
    #[must_use]
    pub fn primary_key(mut self, name: impl Into<String>) -> Self {
        self.primary_key = name.into();
        self
    }

    /// The primary-key column name this builder will use.
    pub fn primary_key_name(&self) -> &str {
        &self.primary_key
    }

    /// Whether [`GeoPackage::create_layer`] should build a spatial index for
    /// this layer. Defaults to `true`.
    ///
    /// An indexed feature layer is what every other implementation produces:
    /// GDAL's driver creates one unless told otherwise, so a file from
    /// `ogr2ogr` has one. Without an index [`crate::Layer::features_in`] still
    /// answers correctly, by falling back to a full scan, so the absence is
    /// invisible until someone profiles it. A spatial format whose spatial
    /// queries are quietly linear is a poor default.
    ///
    /// Creating it here also costs less than adding it later: the index is
    /// empty, which is the state that lets a subsequent large
    /// [`crate::Layer::write_all`] or [`crate::Layer::write_arrow`] build the
    /// whole tree in one bulk pass rather than through the per-row triggers.
    ///
    /// Ignored for a builder with no geometry column, which has nothing to
    /// index.
    #[must_use]
    pub fn spatial_index(mut self, spatial_index: bool) -> Self {
        self.spatial_index = spatial_index;
        self
    }

    /// Set `gpkg_contents.identifier` (a human-readable name). Defaults to the
    /// table name when left unset.
    #[must_use]
    pub fn identifier(mut self, identifier: impl Into<String>) -> Self {
        self.identifier = Some(identifier.into());
        self
    }

    /// Set `gpkg_contents.description`.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Add a non-geometry column. Columns appear in the emitted DDL in the
    /// order added.
    #[must_use]
    pub fn column(mut self, column: ColumnSpec) -> Self {
        self.columns.push(column);
        self
    }

    /// Set the geometry column (required for [`GeoPackage::create_layer`],
    /// rejected by [`GeoPackage::create_attributes_table`]).
    #[must_use]
    pub fn geometry(mut self, geometry: GeometrySpec) -> Self {
        self.geometry = Some(geometry);
        self
    }

    /// The table name.
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    /// The `CREATE TABLE` statement for the user table.
    fn create_table_sql(&self) -> Result<String> {
        let mut defs = Vec::with_capacity(self.columns.len() + 2);
        defs.push(format!(
            "{} INTEGER PRIMARY KEY AUTOINCREMENT",
            quote(&self.primary_key)?
        ));
        for column in &self.columns {
            defs.push(column.to_ddl()?);
        }
        if let Some(geometry) = &self.geometry {
            defs.push(format!(
                "{} {}",
                quote(&geometry.column_name)?,
                geometry.geometry_type.as_str()
            ));
        }
        Ok(format!(
            "CREATE TABLE {} ({})",
            quote(&self.table_name)?,
            defs.join(", ")
        ))
    }
}

impl GeoPackage {
    /// Create a feature layer from a [`TableSchemaBuilder`].
    ///
    /// Emits the user-table DDL, a `gpkg_contents` row (`data_type = 'features'`),
    /// and a `gpkg_geometry_columns` row (creating that table lazily on first
    /// use), then returns a read/write [`Layer`] handle for the new table.
    ///
    /// # Errors
    ///
    /// - [`Error::MissingGeometrySpec`] if the builder has no geometry column.
    /// - [`Error::ReservedTablePrefix`] if the name begins `gpkg_`.
    /// - [`Error::TableAlreadyExists`] if a table or view of that name exists.
    /// - [`Error::UnknownSrs`] if the geometry `srs_id` is not registered in
    ///   `gpkg_spatial_ref_sys`.
    /// - [`Error::ExtensionGeometryUnsupported`] for a non-linear or abstract
    ///   geometry type.
    pub fn create_layer(&self, builder: &TableSchemaBuilder) -> Result<Layer<'_>> {
        // The new table has no rows of its own to be covered yet, so what this
        // catches is a whole-GeoPackage registration: an extension we cannot
        // identify that applies to the file may govern what a table in it has
        // to look like.
        self.check_writable(&builder.table_name)?;
        let geometry = builder
            .geometry
            .as_ref()
            .ok_or_else(|| Error::MissingGeometrySpec {
                table_name: builder.table_name.clone(),
            })?;
        // The table and its index are one transaction: a failure building the
        // index must not leave a table behind without one, which is the state a
        // caller would have to notice and clean up.
        let tx = self.connection().unchecked_transaction()?;
        self.create_table_in(&tx, builder, Some(geometry))?;
        if builder.spatial_index {
            crate::index::create_index_in_transaction(
                &tx,
                &builder.table_name,
                &geometry.column_name,
                &builder.primary_key,
            )?;
        }
        tx.commit()?;
        self.layer(&builder.table_name)
    }

    /// Create a non-spatial attributes table from a [`TableSchemaBuilder`].
    ///
    /// Emits the user-table DDL and a `gpkg_contents` row
    /// (`data_type = 'attributes'`), then returns a read/write [`Layer`] handle.
    ///
    /// # Errors
    ///
    /// - [`Error::UnexpectedGeometrySpec`] if the builder has a geometry column.
    /// - [`Error::ReservedTablePrefix`] / [`Error::TableAlreadyExists`] as
    ///   [`Self::create_layer`].
    pub fn create_attributes_table(&self, builder: &TableSchemaBuilder) -> Result<Layer<'_>> {
        if builder.geometry.is_some() {
            return Err(Error::UnexpectedGeometrySpec {
                table_name: builder.table_name.clone(),
            });
        }
        self.create_table(builder, None)?;
        self.attributes(&builder.table_name)
    }

    /// Shared create path for feature and attribute tables.
    fn create_table(
        &self,
        builder: &TableSchemaBuilder,
        geometry: Option<&GeometrySpec>,
    ) -> Result<()> {
        let tx = self.connection().unchecked_transaction()?;
        self.create_table_in(&tx, builder, geometry)?;
        tx.commit()?;
        Ok(())
    }

    /// [`Self::create_table`] without the transaction management: every
    /// statement runs on `tx`, which the caller owns and commits.
    fn create_table_in(
        &self,
        tx: &Connection,
        builder: &TableSchemaBuilder,
        geometry: Option<&GeometrySpec>,
    ) -> Result<()> {
        let name = &builder.table_name;
        if name
            .get(..5)
            .is_some_and(|p| p.eq_ignore_ascii_case("gpkg_"))
        {
            return Err(Error::ReservedTablePrefix {
                table_name: name.clone(),
            });
        }
        if table_exists(tx, name)? {
            return Err(Error::TableAlreadyExists {
                table_name: name.clone(),
            });
        }
        if let Some(geometry) = geometry {
            if geometry.geometry_type.is_extension() {
                return Err(Error::ExtensionGeometryUnsupported {
                    geometry_type: geometry.geometry_type,
                });
            }
            if self.srs(geometry.srs_id)?.is_none() {
                return Err(Error::UnknownSrs {
                    srs_id: geometry.srs_id,
                });
            }
        }

        let create_sql = builder.create_table_sql()?;
        let data_type = if geometry.is_some() {
            "features"
        } else {
            "attributes"
        };
        let identifier = builder.identifier.clone().unwrap_or_else(|| name.clone());
        let description = builder.description.clone().unwrap_or_default();
        let contents_srs: Option<i32> = geometry.map(|g| g.srs_id);

        if geometry.is_some() && !table_exists(tx, "gpkg_geometry_columns")? {
            tx.execute_batch(ddl::CREATE_GPKG_GEOMETRY_COLUMNS)?;
        }
        tx.execute_batch(&create_sql)?;
        tx.execute(
            "INSERT INTO gpkg_contents \
             (table_name, data_type, identifier, description, srs_id) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![name, data_type, identifier, description, contents_srs],
        )?;
        if let Some(geometry) = geometry {
            tx.execute(
                "INSERT INTO gpkg_geometry_columns \
                 (table_name, column_name, geometry_type_name, srs_id, z, m) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    name,
                    geometry.column_name,
                    geometry.geometry_type.as_str(),
                    geometry.srs_id,
                    geometry.z.code(),
                    geometry.m.code(),
                ],
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_table_ddl() {
        let builder = TableSchemaBuilder::new("roads")
            .column(ColumnSpec::new("name", ColumnType::Text(Some(64))))
            .column(
                ColumnSpec::new("lanes", ColumnType::MediumInt)
                    .not_null()
                    .default_value("2"),
            )
            .geometry(GeometrySpec::new(GeometryType::LineString, 4326));
        assert_eq!(
            builder.create_table_sql().unwrap(),
            "CREATE TABLE \"roads\" (\
             \"fid\" INTEGER PRIMARY KEY AUTOINCREMENT, \
             \"name\" TEXT(64), \
             \"lanes\" MEDIUMINT NOT NULL DEFAULT 2, \
             \"geom\" LINESTRING)"
        );
    }

    #[test]
    fn attributes_table_ddl_and_custom_pk() {
        let builder = TableSchemaBuilder::new("notes")
            .primary_key("id")
            .column(ColumnSpec::new("body", ColumnType::Text(None)).unique());
        assert_eq!(
            builder.create_table_sql().unwrap(),
            "CREATE TABLE \"notes\" (\
             \"id\" INTEGER PRIMARY KEY AUTOINCREMENT, \
             \"body\" TEXT UNIQUE)"
        );
    }

    #[test]
    fn quotes_awkward_identifiers() {
        let builder = TableSchemaBuilder::new("we\"ird")
            .column(ColumnSpec::new("select", ColumnType::Integer))
            .geometry(GeometrySpec::new(GeometryType::Point, 4326).column_name("the geom"));
        assert_eq!(
            builder.create_table_sql().unwrap(),
            "CREATE TABLE \"we\"\"ird\" (\
             \"fid\" INTEGER PRIMARY KEY AUTOINCREMENT, \
             \"select\" INTEGER, \
             \"the geom\" POINT)"
        );
    }
}
