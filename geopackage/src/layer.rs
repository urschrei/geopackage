//! Feature and attribute layers: typed handles over a user table, and the
//! streaming read path ([`Layer::features`], [`Layer::features_in`],
//! [`Layer::select`]).
//!
//! A [`Layer`] borrows the [`GeoPackage`] it came from and caches the table's
//! introspected [`TableSchema`], its resolved geometry column (feature layers),
//! and its single-column primary key. The read methods build a prepared
//! statement from that schema and yield owned [`Feature`]s: a row's values do
//! not outlive the SQLite cursor, so each [`Feature`] owns its geometry blob and
//! its converted column values rather than borrowing the row. It holds them in
//! one buffer with a range recorded per value, and lends them out as
//! [`crate::ValueRef`]s, so the row costs two allocations whatever its width.
//! Neither the geometry nor the primary key is among those values: they are
//! reached through [`Feature::geometry`] and [`Feature::fid`], and the write
//! path takes exactly the same value set.
//!
//! There are two ways to read features, and they return the same rows.
//!
//! [`Layer::features`], [`Layer::features_in`] and [`Layer::select`] materialise
//! the whole result set into owned features before returning the iterator. One
//! call, and the right default for layers small enough that the result set is
//! not a problem.
//!
//! [`Layer::cursor`], [`Layer::cursor_in`] and [`Layer::cursor_select`] stream,
//! holding one row at a time. Two calls, because rusqlite's row cursor borrows
//! its `Statement`: an iterator owning both would be self-referential, which
//! this crate's `#![forbid(unsafe_code)]` rules out without a helper crate.
//! Handing the statement to the caller keeps the borrow one-way, which is the
//! shape rusqlite itself uses. Measured over 100k features, streaming reads in
//! 18.8 ms against 29.5 ms materialised, with peak memory bounded by one row
//! rather than by the result set.
//!
//! Both build the same [`Feature`]s through the same code, so the choice is
//! memory and ergonomics, never results.

use std::sync::Arc;

use geopackage_core::datetime::{Date, DateTime};
use geopackage_core::geometry::{self, GpbGeometry};
use geopackage_core::gpb;
use geopackage_core::ident::quote;
use geopackage_core::triggers::{self, TriggerGeneration};
use rusqlite::OptionalExtension;
use rusqlite::types::ValueRef as SqlValueRef;

use crate::value::{ValueRef, value_ref_from_sql, value_ref_to_sql};
use crate::{
    Column, ConversionOptions, Error, GeoPackage, GeometryColumn, Result, TableSchema,
    resolve_table_name, table_exists,
};

/// An XY bounding box for spatial queries ([`Layer::features_in`]).
///
/// Fields are ordinary map coordinates in the layer's own spatial reference
/// system; this crate never transforms coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoundingBox {
    /// Minimum x (west edge).
    pub min_x: f64,
    /// Minimum y (south edge).
    pub min_y: f64,
    /// Maximum x (east edge).
    pub max_x: f64,
    /// Maximum y (north edge).
    pub max_y: f64,
}

impl BoundingBox {
    /// A bounding box from its four edges.
    pub fn new(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Self {
        Self {
            min_x,
            min_y,
            max_x,
            max_y,
        }
    }

    /// Whether this box intersects an envelope `[min_x, max_x, min_y, max_y]`,
    /// inclusive at the boundary (touching boxes intersect).
    fn intersects_envelope(&self, env: [f64; 4]) -> bool {
        env[0] <= self.max_x && env[1] >= self.min_x && env[2] <= self.max_y && env[3] >= self.min_y
    }
}

/// Which kind of user table a [`Layer`] wraps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LayerKind {
    /// A feature layer (`gpkg_contents.data_type = 'features'`) with a geometry
    /// column.
    Feature,
    /// An attribute layer (`gpkg_contents.data_type = 'attributes'`), no
    /// geometry.
    Attributes,
}

impl LayerKind {
    /// The kind as the word `gpkg_contents` uses for it.
    ///
    /// The same string as the private `data_type`, exposed because reporting a
    /// layer's kind and writing its catalogue row want the same word.
    pub fn as_str(self) -> &'static str {
        self.data_type()
    }

    /// The `gpkg_contents.data_type` string this kind requires.
    fn data_type(self) -> &'static str {
        match self {
            Self::Feature => "features",
            Self::Attributes => "attributes",
        }
    }
}

impl std::fmt::Display for LayerKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A handle to one feature or attribute layer of a [`GeoPackage`].
///
/// Obtained from [`GeoPackage::layer`], [`GeoPackage::attributes`], or
/// [`GeoPackage::layers`]. The schema is introspected once at construction; the
/// handle borrows the [`GeoPackage`] for its lifetime.
pub struct Layer<'a> {
    gpkg: &'a GeoPackage,
    table_name: String,
    schema: TableSchema,
    kind: LayerKind,
    geometry_column: Option<GeometryColumn>,
    pk_column: Option<String>,
    /// The value columns, in schema order: every column except the geometry
    /// and the primary key. The values each [`Feature`] carries, and the ones
    /// the write path binds.
    value_columns: Vec<Column>,
    /// The names of [`Self::value_columns`], shared cheaply into every
    /// [`Feature`] for by-name access.
    value_column_names: Arc<[String]>,
    options: ConversionOptions,
    validate_geometry_type: bool,
    /// Which columns a read of this layer yields. `None` selects every one,
    /// which is the default.
    ///
    /// A read concern only: the writer keeps the layer's whole column list, so
    /// a projected handle cannot silently insert a partial row.
    projection: Option<Projection>,
}

/// The columns a projected [`Layer`] reads: a subset of its value columns, in
/// the table's own order, and whether the geometry comes with them.
#[derive(Debug, Clone)]
struct Projection {
    value_columns: Vec<Column>,
    value_column_names: Arc<[String]>,
    geometry: bool,
}

impl std::fmt::Debug for Layer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Layer")
            .field("table_name", &self.table_name)
            .field("kind", &self.kind)
            .field("geometry_column", &self.geometry_column)
            .field("primary_key", &self.pk_column)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl GeoPackage {
    /// Enumerate the feature layers: rows of `gpkg_contents` with
    /// `data_type = 'features'` that also have a `gpkg_geometry_columns` row.
    ///
    /// The join is case-insensitive on the table name, so a file whose
    /// catalogue tables disagree on case (see [`GeoPackage::open_lenient`])
    /// still enumerates. Attribute-only files, and files with no
    /// `gpkg_geometry_columns` table at all, yield an empty list.
    pub fn layers(&self) -> Result<Vec<Layer<'_>>> {
        if !table_exists(self.connection(), "gpkg_geometry_columns")? {
            return Ok(Vec::new());
        }
        let names: Vec<String> = {
            let mut stmt = self.connection().prepare(
                "SELECT DISTINCT c.table_name FROM gpkg_contents c \
                 JOIN gpkg_geometry_columns g ON g.table_name = c.table_name COLLATE NOCASE \
                 WHERE c.data_type = 'features' ORDER BY c.table_name",
            )?;
            stmt.query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        names.iter().map(|n| self.layer(n)).collect()
    }

    /// Open a feature layer by name.
    ///
    /// # Errors
    ///
    /// - [`Error::NoSuchLayer`] if `name` is not in `gpkg_contents`.
    /// - [`Error::WrongDataType`] if it is registered but not as `features`
    ///   (use [`GeoPackage::attributes`] for an attribute table).
    /// - [`Error::NoSuchTable`] if the catalogue row has no backing table.
    pub fn layer(&self, name: &str) -> Result<Layer<'_>> {
        self.build_layer(name, LayerKind::Feature)
    }

    /// Open an attribute layer (a non-spatial `data_type = 'attributes'` table)
    /// by name.
    ///
    /// # Errors
    ///
    /// As [`GeoPackage::layer`], but requiring the `attributes` data type.
    pub fn attributes(&self, name: &str) -> Result<Layer<'_>> {
        self.build_layer(name, LayerKind::Attributes)
    }

    fn build_layer(&self, name: &str, kind: LayerKind) -> Result<Layer<'_>> {
        let row = self
            .connection()
            .query_row(
                "SELECT table_name, data_type FROM gpkg_contents \
                 WHERE table_name = ?1 COLLATE NOCASE",
                [name],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        let (declared_name, data_type) = row.ok_or_else(|| Error::NoSuchLayer {
            table_name: name.to_owned(),
        })?;
        if data_type != kind.data_type() {
            return Err(Error::WrongDataType {
                table_name: declared_name,
                expected: kind.data_type(),
                found: data_type,
            });
        }
        let table_name =
            resolve_table_name(self.connection(), &declared_name)?.unwrap_or(declared_name);
        let schema = self.table_schema(&table_name)?;
        let geometry_column = match kind {
            LayerKind::Feature => match &schema.geometry_column {
                Some(g) => Some(g.clone()),
                None => self.geometry_column_ci(&table_name)?,
            },
            LayerKind::Attributes => None,
        };
        let pk_column = schema.primary_key().map(|c| c.name.clone());
        let geom_name = geometry_column.as_ref().map(|g| g.column_name.clone());
        // Neither the geometry nor the primary key is a value column. Both are
        // reached through their own accessors ([`Feature::geometry`] and
        // [`Feature::fid`]), and the write path has always taken values without
        // them, so including them here made the two sides of the API disagree
        // about what a row's values are. Dropping the primary key also stops
        // every query selecting it twice, once as the fid and once as a value.
        let value_columns: Vec<Column> = schema
            .columns
            .iter()
            .filter(|c| geom_name.as_deref() != Some(c.name.as_str()))
            .filter(|c| pk_column.as_deref() != Some(c.name.as_str()))
            .cloned()
            .collect();
        let value_column_names: Arc<[String]> =
            value_columns.iter().map(|c| c.name.clone()).collect();
        Ok(Layer {
            gpkg: self,
            table_name,
            schema,
            kind,
            geometry_column,
            pk_column,
            value_columns,
            value_column_names,
            // `default`, not `strict`: strict `DATETIME` parsing, but a value a
            // declared type does not strictly permit is still read rather than
            // rejected. A layer read should not fail on a file every other
            // implementation opens (see `StorageStrictness`).
            options: ConversionOptions::default(),
            validate_geometry_type: false,
            projection: None,
        })
    }
}

impl<'a> Layer<'a> {
    /// The physical SQLite table name backing this layer.
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    /// Whether this is a feature or attribute layer.
    pub fn kind(&self) -> LayerKind {
        self.kind
    }

    /// The introspected schema of the backing table.
    pub fn schema(&self) -> &TableSchema {
        &self.schema
    }

    /// The geometry column, for a feature layer that has one.
    pub fn geometry_column(&self) -> Option<&GeometryColumn> {
        self.geometry_column.as_ref()
    }

    /// The single-column primary key name (the `fid` column, discovered from
    /// the schema, never assumed), or `None` for a table with no single-column
    /// primary key.
    pub fn primary_key_column(&self) -> Option<&str> {
        self.pk_column.as_deref()
    }

    /// The [`GeoPackage`] this layer borrows (for the write path's transaction).
    pub(crate) fn gpkg(&self) -> &'a GeoPackage {
        self.gpkg
    }

    /// The value columns in schema order (the values a [`Feature`]
    /// carries, and the columns the write path binds).
    pub(crate) fn value_columns(&self) -> &[Column] {
        &self.value_columns
    }

    /// The [`ConversionOptions`] used when converting column values.
    pub fn conversion_options(&self) -> ConversionOptions {
        self.options
    }

    /// Set the [`ConversionOptions`] for value conversion (e.g. lenient
    /// `DATETIME` parsing, or rejecting values their declared type does not
    /// permit). Applies to [`Self::features`], [`Self::features_in`] and
    /// [`Self::select`]. Defaults to [`ConversionOptions::default`].
    #[must_use]
    pub fn with_conversion_options(mut self, options: ConversionOptions) -> Self {
        self.options = options;
        self
    }

    /// Check each geometry's WKB type against the column's declared
    /// `gpkg_geometry_columns` type while reading (off by default).
    ///
    /// A row whose body does not satisfy the declared type (per the rules of
    /// [`geopackage_core::geometry::geometry_type_matches`]: exact match,
    /// `GEOMETRY` accepts anything, `GEOMETRYCOLLECTION` accepts collection
    /// types) surfaces as [`Error::GeometryTypeMismatch`] for that row,
    /// without stopping iteration. The check reads only the WKB type
    /// discriminator, so it also classifies (and rejects) non-linear curve
    /// bodies in a linear-typed column.
    #[must_use]
    pub fn with_geometry_type_validation(mut self) -> Self {
        self.validate_geometry_type = true;
        self
    }

    /// Read only the named columns.
    ///
    /// A read of the returned handle selects the feature id, the named value
    /// columns, and the geometry only if it is named. On a layer whose
    /// geometries are large this is the difference between fetching and copying
    /// every blob and touching none of them: a scan of 5,000 rows carrying
    /// 1,000-vertex linestrings, reading one integer attribute, spends most of
    /// its time on geometry nobody asked for.
    ///
    /// Columns come back in the table's order whatever order they are named in,
    /// so this selects rather than reorders and [`Feature::get`] indices stay
    /// predictable. Naming a column twice selects it once. The feature id is
    /// always present and need not be named.
    ///
    /// The projection is a read concern: [`Self::writer`] on a projected handle
    /// still writes the layer's whole column list, so a partial row cannot be
    /// inserted by accident.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchColumn`] if a name is neither a value column nor the
    /// geometry column. Names are checked here rather than at the query, so a
    /// typo fails at the point it was written instead of quietly selecting
    /// nothing.
    pub fn with_columns(mut self, columns: &[&str]) -> Result<Self> {
        let geometry_name = self.geometry_column.as_ref().map(|g| &g.column_name);
        for name in columns {
            let is_value = self.value_columns.iter().any(|c| c.name == *name);
            let is_geometry = geometry_name.is_some_and(|g| g == *name);
            if !is_value && !is_geometry {
                return Err(Error::NoSuchColumn {
                    table_name: self.table_name.clone(),
                    column_name: (*name).to_owned(),
                });
            }
        }
        let value_columns: Vec<Column> = self
            .value_columns
            .iter()
            .filter(|c| columns.contains(&c.name.as_str()))
            .cloned()
            .collect();
        let geometry = geometry_name.is_some_and(|g| columns.contains(&g.as_str()));
        self.projection = Some(Projection {
            value_column_names: value_columns.iter().map(|c| c.name.clone()).collect(),
            value_columns,
            geometry,
        });
        Ok(self)
    }

    /// Read every value column but not the geometry.
    ///
    /// The common half of [`Self::with_columns`], since naming every attribute
    /// of a wide layer to be rid of one geometry column is tedious. On a layer
    /// with no geometry column this changes nothing.
    ///
    /// A bounding-box query still works on the result: it reads each candidate
    /// geometry to filter exactly, which is this crate's business rather than
    /// the caller's, and simply does not carry it into the feature.
    #[must_use]
    pub fn without_geometry(mut self) -> Self {
        let value_columns = self
            .projection
            .as_ref()
            .map_or_else(|| self.value_columns.clone(), |p| p.value_columns.clone());
        self.projection = Some(Projection {
            value_column_names: value_columns.iter().map(|c| c.name.clone()).collect(),
            value_columns,
            geometry: false,
        });
        self
    }

    /// The value columns a read yields: the projection's, or all of them.
    pub(crate) fn read_value_columns(&self) -> &[Column] {
        self.projection
            .as_ref()
            .map_or(&self.value_columns, |p| &p.value_columns)
    }

    /// Whether a projection is set, whatever it selects. The parallel Arrow
    /// path declines on a projected layer, since its workers rebuild the layer
    /// from the table name and would read every column.
    pub(crate) fn is_projected(&self) -> bool {
        self.projection.is_some()
    }

    /// The names of [`Self::read_value_columns`], for a [`Feature`] to share.
    fn read_value_column_names(&self) -> &Arc<[String]> {
        self.projection
            .as_ref()
            .map_or(&self.value_column_names, |p| &p.value_column_names)
    }

    /// Whether a read carries the geometry into the features it yields. A
    /// filtered query may still select it, to filter with.
    pub(crate) fn reads_geometry(&self) -> bool {
        match &self.projection {
            Some(projection) => projection.geometry,
            None => self.geometry_column.is_some(),
        }
    }

    /// How many rows the layer holds.
    ///
    /// A `SELECT COUNT(*)`, so it reads no geometry and builds no [`Feature`].
    /// Counting by iterating instead costs a full materialising scan, which on
    /// a layer of any size is the whole file.
    ///
    /// # Errors
    ///
    /// [`Error`] if the query fails.
    pub fn count(&self) -> Result<u64> {
        let sql = format!("SELECT COUNT(*) FROM {}", quote(&self.table_name)?);
        let count: i64 = self
            .gpkg
            .connection()
            .query_row(&sql, [], |row| row.get(0))?;
        // COUNT(*) is never negative, so the cast cannot wrap.
        Ok(count.unsigned_abs())
    }

    /// Iterate every row of the layer as an owned [`Feature`].
    ///
    /// The iterator is fallible per row: a value that does not fit its declared
    /// column type surfaces as an `Err` for that row without stopping the scan.
    /// Rows are read in the table's natural order.
    pub fn features(&self) -> Result<Features> {
        let (sql, geom_idx) = self.base_select(self.reads_geometry())?;
        self.execute(&sql, Vec::new(), geom_idx, None)
    }

    /// Iterate the features whose geometry envelope intersects `bbox`
    /// (inclusive at the boundary).
    ///
    /// When a usable RTree spatial index is present (see
    /// [`Self::has_spatial_index`]) the query is served by the
    /// `rtree_<table>_<column>` virtual table; otherwise it is a full scan with
    /// an envelope filter. Both paths return exactly the same rows: SQLite's
    /// RTree stores 32-bit-float bounds (minima rounded down, maxima rounded
    /// up), so it returns a conservative superset of candidates near a
    /// boundary, and each candidate is re-tested against the true `f64`
    /// envelope read from the blob (header envelope preferred, WKB traversal
    /// fallback, the same rule the `ST_*` functions use).
    ///
    /// Rows outside the box are skipped before their values are converted, so
    /// a value that does not fit its declared column type surfaces as an `Err`
    /// only when its row intersects the box. An unreadable geometry blob
    /// surfaces as an `Err` on whichever path visits it: always on a full
    /// scan, only as an index candidate on the RTree path.
    ///
    /// # Errors
    ///
    /// [`Error::NoGeometryColumn`] if the layer has no geometry column.
    pub fn features_in(&self, bbox: BoundingBox) -> Result<Features> {
        let (sql, geom_idx, uses_rtree) = self.features_in_plan()?;
        let params = if uses_rtree {
            use rusqlite::types::Value as Sql;
            // SQLite's RTree coerces bound f64 constraints to f32, and at
            // sub-normal magnitudes that coercion is not conservative: a
            // truly-intersecting candidate can be excluded before the f64
            // re-test runs. Widen each bound one f32 ULP outward before
            // binding; the exact f64 re-filter below restores precision.
            vec![
                Sql::Real(widen_up(bbox.max_x)),
                Sql::Real(widen_down(bbox.min_x)),
                Sql::Real(widen_up(bbox.max_y)),
                Sql::Real(widen_down(bbox.min_y)),
            ]
        } else {
            Vec::new()
        };
        self.execute(&sql, params, geom_idx, Some(bbox))
    }

    /// The SQL [`Self::features_in`] runs: an RTree join when a usable spatial
    /// index is present, otherwise the full-scan `features` query.
    ///
    /// Exposed for diagnostics: running `EXPLAIN QUERY PLAN` on it confirms
    /// whether the RTree virtual table is used. The RTree form carries `?1`–`?4`
    /// placeholders for the query box.
    pub fn features_in_sql(&self) -> Result<String> {
        Ok(self.features_in_plan()?.0)
    }

    /// Iterate the features matching a caller-supplied `WHERE` clause.
    ///
    /// `where_clause` is appended (parenthesised) to the layer's base query and
    /// is **raw SQL, trusted from the caller**: this crate does not parse or
    /// sanitise it, and provides no query DSL of its own
    /// ([`GeoPackage::connection`] is the full escape hatch). `params` bind its
    /// placeholders; they are this crate's [`ValueRef`] values, converted
    /// internally so rusqlite types stay out of the public API.
    /// [`ValueRef::Date`] and [`ValueRef::DateTime`] bind as their canonical
    /// text form.
    ///
    /// Parameters are borrowed, so a literal needs no allocation to bind.
    ///
    /// ```no_run
    /// # fn main() -> Result<(), geopackage::Error> {
    /// # let gpkg = geopackage::GeoPackage::open("x.gpkg")?;
    /// # let layer = gpkg.layer("roads")?;
    /// use geopackage::ValueRef;
    /// for feature in layer.select("name = ?1", &[ValueRef::Text("A1")])? {
    ///     let feature = feature?;
    ///     println!("{}", feature.fid());
    /// }
    /// # Ok(()) }
    /// ```
    pub fn select(&self, where_clause: &str, params: &[ValueRef<'_>]) -> Result<Features> {
        let (base, geom_idx) = self.base_select(self.reads_geometry())?;
        let sql = format!("{base} WHERE ({where_clause})");
        let sql_params: Vec<rusqlite::types::Value> = params.iter().map(value_ref_to_sql).collect();
        self.execute(&sql, sql_params, geom_idx, None)
    }

    /// A streaming full scan: the same rows as [`Self::features`], one at a
    /// time instead of materialised.
    ///
    /// Two steps, because the returned cursor owns the prepared statement that
    /// the iterator borrows. See [`FeatureCursor`] for why.
    ///
    /// ```no_run
    /// # fn main() -> Result<(), geopackage::Error> {
    /// # let gpkg = geopackage::GeoPackage::open("x.gpkg")?;
    /// # let layer = gpkg.layer("roads")?;
    /// let mut cursor = layer.cursor()?;
    /// for feature in cursor.features()? {
    ///     println!("{}", feature?.fid());
    /// }
    /// # Ok(()) }
    /// ```
    pub fn cursor(&self) -> Result<FeatureCursor<'_>> {
        let (sql, geom_idx) = self.base_select(self.reads_geometry())?;
        self.prepare_cursor(&sql, Vec::new(), geom_idx, None)
    }

    /// A streaming bounding-box query: the same rows as [`Self::features_in`],
    /// using the RTree index on the same terms.
    ///
    /// # Errors
    ///
    /// [`Error::NoGeometryColumn`] if the layer has no geometry column.
    pub fn cursor_in(&self, bbox: BoundingBox) -> Result<FeatureCursor<'_>> {
        let (sql, geom_idx, uses_rtree) = self.features_in_plan()?;
        let params = if uses_rtree {
            use rusqlite::types::Value as Sql;
            vec![
                Sql::Real(widen_up(bbox.max_x)),
                Sql::Real(widen_down(bbox.min_x)),
                Sql::Real(widen_up(bbox.max_y)),
                Sql::Real(widen_down(bbox.min_y)),
            ]
        } else {
            Vec::new()
        };
        self.prepare_cursor(&sql, params, geom_idx, Some(bbox))
    }

    /// A streaming `WHERE` query: the same rows as [`Self::select`], with the
    /// same raw-SQL contract.
    pub fn cursor_select(
        &self,
        where_clause: &str,
        params: &[ValueRef<'_>],
    ) -> Result<FeatureCursor<'_>> {
        let (base, geom_idx) = self.base_select(self.reads_geometry())?;
        let sql = format!("{base} WHERE ({where_clause})");
        let sql_params: Vec<rusqlite::types::Value> = params.iter().map(value_ref_to_sql).collect();
        self.prepare_cursor(&sql, sql_params, geom_idx, None)
    }

    /// Prepare the statement a cursor will own, with the row metadata it needs
    /// detached from this handle.
    fn prepare_cursor(
        &self,
        sql: &str,
        params: Vec<rusqlite::types::Value>,
        geom_idx: Option<usize>,
        filter: Option<BoundingBox>,
    ) -> Result<FeatureCursor<'_>> {
        Ok(FeatureCursor {
            stmt: self.gpkg.connection().prepare(sql)?,
            params,
            geom_idx,
            filter,
            ctx: self.row_context(),
        })
    }

    /// Whether [`Self::features_in`] will use the RTree spatial index.
    ///
    /// True when the layer has a geometry column and a single-column primary
    /// key, the `rtree_<table>_<column>` virtual table exists, and its trigger
    /// set is a recognised generation ([`TriggerGeneration`] other than
    /// `None`). Otherwise `features_in` falls back to a full scan.
    pub fn has_spatial_index(&self) -> Result<bool> {
        let Some(geom) = &self.geometry_column else {
            return Ok(false);
        };
        if self.pk_column.is_none() {
            return Ok(false);
        }
        let conn = self.gpkg.connection();
        let rtree = triggers::rtree_table_name(&self.table_name, &geom.column_name);
        if !table_exists(conn, &rtree)? {
            return Ok(false);
        }
        Ok(self.classify_rtree_triggers(&geom.column_name)? != TriggerGeneration::None)
    }

    /// Classify the RTree trigger generation present on this layer's table for
    /// `column`, reading the trigger names from `sqlite_master`.
    ///
    /// This is the classification the read path
    /// ([`Self::has_spatial_index`]) and the index-repair path
    /// ([`Self::repair_spatial_index`]) share; it inspects only trigger names,
    /// not the virtual table or primary key.
    pub(crate) fn classify_rtree_triggers(&self, column: &str) -> Result<TriggerGeneration> {
        let conn = self.gpkg.connection();
        let names: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = ?1",
            )?;
            stmt.query_map([self.table_name.as_str()], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        Ok(triggers::classify_triggers(
            names.iter().map(String::as_str),
            &self.table_name,
            column,
        ))
    }

    fn features_in_plan(&self) -> Result<(String, Option<usize>, bool)> {
        let Some(geom) = &self.geometry_column else {
            return Err(Error::NoGeometryColumn {
                table_name: self.table_name.clone(),
            });
        };
        if self.has_spatial_index()? {
            let (sql, geom_idx) = self.rtree_select(geom)?;
            Ok((sql, geom_idx, true))
        } else {
            // As `rtree_select`: needed for the exact filter regardless.
            let (sql, geom_idx) = self.base_select(true)?;
            Ok((sql, geom_idx, false))
        }
    }

    /// The `fid` select/join expression: the primary-key column when the table
    /// has one, else SQLite's `rowid`.
    fn fid_expr(&self, prefix: Option<&str>) -> Result<String> {
        match &self.pk_column {
            Some(pk) => qualified(pk, prefix),
            None => Ok(match prefix {
                Some(p) => format!("{p}.rowid"),
                None => "rowid".to_owned(),
            }),
        }
    }

    /// Build the select list: `fid` at index 0, the value columns at
    /// `1..=value_columns.len()`, and (for a feature layer) the geometry blob
    /// last. Returns the joined SQL and the geometry column's index.
    fn column_exprs(
        &self,
        prefix: Option<&str>,
        select_geometry: bool,
    ) -> Result<(String, Option<usize>)> {
        let mut exprs = Vec::with_capacity(self.read_value_columns().len() + 2);
        exprs.push(self.fid_expr(prefix)?);
        for column in self.read_value_columns() {
            exprs.push(qualified(&column.name, prefix)?);
        }
        let geom_idx = match &self.geometry_column {
            Some(geom) if select_geometry => {
                exprs.push(qualified(&geom.column_name, prefix)?);
                Some(exprs.len() - 1)
            }
            _ => None,
        };
        Ok((exprs.join(", "), geom_idx))
    }

    fn base_select(&self, select_geometry: bool) -> Result<(String, Option<usize>)> {
        let (list, geom_idx) = self.column_exprs(None, select_geometry)?;
        Ok((
            format!("SELECT {list} FROM {}", quote(&self.table_name)?),
            geom_idx,
        ))
    }

    fn rtree_select(&self, geom: &GeometryColumn) -> Result<(String, Option<usize>)> {
        let table = quote(&self.table_name)?;
        // Always selected: the exact re-filter reads each candidate's blob,
        // whether or not the caller asked to be given it.
        let (list, geom_idx) = self.column_exprs(Some(&table), true)?;
        let rtree = quote(&triggers::rtree_table_name(
            &self.table_name,
            &geom.column_name,
        ))?;
        let id = self.fid_expr(Some(&table))?;
        let sql = format!(
            "SELECT {list} FROM {table} \
             JOIN {rtree} AS \"__gpkg_rtree\" ON {id} = \"__gpkg_rtree\".id \
             WHERE \"__gpkg_rtree\".minx <= ?1 AND \"__gpkg_rtree\".maxx >= ?2 \
             AND \"__gpkg_rtree\".miny <= ?3 AND \"__gpkg_rtree\".maxy >= ?4"
        );
        Ok((sql, geom_idx))
    }

    fn execute(
        &self,
        sql: &str,
        params: Vec<rusqlite::types::Value>,
        geom_idx: Option<usize>,
        filter: Option<BoundingBox>,
    ) -> Result<Features> {
        let conn = self.gpkg.connection();
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
        let mut out: Vec<Result<Feature>> = Vec::new();
        // Built once for the whole query, not per row: the context owns cloned
        // column metadata, so rebuilding it inside the loop would copy every
        // column name and declared type again for every feature returned.
        let ctx = self.row_context();
        while let Some(row) = rows.next()? {
            // Decide bbox membership from the geometry blob before converting
            // any values: a row outside the box is skipped entirely, so value
            // conversion errors surface only for rows the query returns,
            // identically on the RTree and full-scan paths.
            if let Some(bbox) = &filter {
                match row_in_box(row, geom_idx, bbox) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(e) => {
                        out.push(Err(e));
                        continue;
                    }
                }
            }
            out.push(ctx.feature_from_row(row, geom_idx));
        }
        Ok(Features {
            inner: out.into_iter(),
        })
    }

    /// The per-row metadata needed to build owned [`Feature`]s, detached from
    /// this handle so a [`FeatureCursor`] can outlive the borrow that made it.
    ///
    /// Cloning this is not free: it copies the column list, including each
    /// column's name and declared type. Every caller must build it once per
    /// query and reuse it across rows.
    fn row_context(&self) -> RowContext {
        RowContext {
            table_name: self.table_name.clone(),
            value_columns: self.read_value_columns().to_vec(),
            value_column_names: Arc::clone(self.read_value_column_names()),
            options: self.options,
            validate_geometry_type: self.validate_geometry_type,
            geometry_column: self.geometry_column.clone(),
            store_geometry: self.reads_geometry(),
            value_bytes_hint: std::cell::Cell::new(0),
        }
    }
}

/// A prepared streaming read over a layer, owning its statement.
///
/// Obtained from [`Layer::cursor`], [`Layer::cursor_in`] or
/// [`Layer::cursor_select`], and turned into an iterator by
/// [`FeatureCursor::features`].
///
/// # Why this is two steps
///
/// rusqlite's row cursor borrows its `Statement`, so an iterator owning both
/// would be self-referential, which this crate's `#![forbid(unsafe_code)]`
/// rules out without a helper crate. Handing the statement to the caller keeps
/// the borrow one-way, which is the same shape rusqlite itself uses:
///
/// ```no_run
/// # fn main() -> Result<(), geopackage::Error> {
/// # let gpkg = geopackage::GeoPackage::open("x.gpkg")?;
/// # let layer = gpkg.layer("roads")?;
/// let mut cursor = layer.cursor()?;
/// for feature in cursor.features()? {
///     let feature = feature?;
///     println!("{}", feature.fid());
/// }
/// # Ok(()) }
/// ```
///
/// The difference from [`Layer::features`] is peak memory, not results: this
/// holds one row at a time, where the materialising methods build the whole
/// result set before returning. Prefer this for layers large enough that the
/// result set is a problem, and the one-step methods otherwise.
#[derive(Debug)]
pub struct FeatureCursor<'a> {
    stmt: rusqlite::Statement<'a>,
    params: Vec<rusqlite::types::Value>,
    geom_idx: Option<usize>,
    filter: Option<BoundingBox>,
    ctx: RowContext,
}

impl FeatureCursor<'_> {
    /// Run the query and stream its rows.
    ///
    /// Each call re-runs the query from the start, so a cursor can be iterated
    /// more than once. Rows are yielded as `Result<Feature>`: a value that does
    /// not fit its declared column type surfaces as an `Err` for that row
    /// without ending the scan, exactly as the materialising methods do.
    pub fn features(&mut self) -> Result<FeatureStream<'_>> {
        let rows = self
            .stmt
            .query(rusqlite::params_from_iter(self.params.iter()))?;
        Ok(FeatureStream {
            rows,
            ctx: &self.ctx,
            geom_idx: self.geom_idx,
            filter: self.filter,
        })
    }
}

/// A streaming iterator over a [`FeatureCursor`]'s rows.
///
/// Yields `Result<Feature>` per row and holds one row at a time. Borrows the
/// cursor that produced it.
pub struct FeatureStream<'c> {
    rows: rusqlite::Rows<'c>,
    ctx: &'c RowContext,
    geom_idx: Option<usize>,
    filter: Option<BoundingBox>,
}

impl std::fmt::Debug for FeatureStream<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `rusqlite::Rows` is not `Debug`, and a cursor mid-scan has no useful
        // state to print beyond what the query already said.
        f.debug_struct("FeatureStream")
            .field("table_name", &self.ctx.table_name)
            .field("filtered", &self.filter.is_some())
            .finish_non_exhaustive()
    }
}

impl Iterator for FeatureStream<'_> {
    type Item = Result<Feature>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let row = match self.rows.next() {
                Ok(Some(row)) => row,
                Ok(None) => return None,
                Err(e) => return Some(Err(e.into())),
            };
            // Decide bbox membership from the geometry blob before converting
            // any values, so a row outside the box is skipped entirely and
            // conversion errors surface only for rows the query returns. Same
            // rule as the materialising path.
            if let Some(bbox) = &self.filter {
                match row_in_box(row, self.geom_idx, bbox) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(e) => return Some(Err(e)),
                }
            }
            return Some(self.ctx.feature_from_row(row, self.geom_idx));
        }
    }
}

/// Everything needed to turn a result row into an owned [`Feature`], owned
/// rather than borrowed so it can be held by a [`FeatureCursor`].
#[derive(Debug, Clone)]
struct RowContext {
    table_name: String,
    value_columns: Vec<Column>,
    value_column_names: Arc<[String]>,
    options: ConversionOptions,
    validate_geometry_type: bool,
    geometry_column: Option<GeometryColumn>,
    /// Whether a row keeps its geometry. False under a projection that did not
    /// name it, in which case a filtered query still selects the blob to filter
    /// with but no row carries it.
    store_geometry: bool,
    /// The non-geometry byte count of the last row built, used to size the next
    /// row's buffer. A `Cell` because rows are built through a shared
    /// reference, and the value is a hint: wrong only costs a growth.
    value_bytes_hint: std::cell::Cell<usize>,
}

impl RowContext {
    /// Build one owned [`Feature`]. The single implementation shared by the
    /// materialising read methods and the streaming cursor.
    fn feature_from_row(
        &self,
        row: &rusqlite::Row<'_>,
        geom_idx: Option<usize>,
    ) -> Result<Feature> {
        let fid: i64 = row.get(0)?;

        // The geometry blob goes in first, so its range starts at zero and only
        // its end has to be recorded.
        let geometry = match geom_idx {
            Some(gi) => match row.get_ref(gi)? {
                SqlValueRef::Blob(bytes) => Some(bytes),
                // NULL, or a non-blob value in the geometry column, reads as no
                // geometry rather than an error.
                _ => None,
            },
            None => None,
        };
        if self.validate_geometry_type
            && let (Some(blob), Some(declared)) = (geometry, &self.geometry_column)
        {
            self.check_declared_type(blob, declared)?;
        }

        // Sized from the previous row rather than by measuring this one. A
        // sizing pass would have to fetch every cell a second time, and fetching
        // values is around half a scalar read's total cost, so paying it twice
        // to save a reallocation is a bad trade: measured over three real
        // datasets it cost 3% of the read on a four-column layer and over 30% on
        // sixteen- and fifty-four-column ones. Rows in a layer are usually
        // close in size, so the estimate is normally right, and being wrong
        // costs a growth rather than a wrong answer.
        let mut buf: Vec<u8> =
            Vec::with_capacity(geometry.map_or(0, <[u8]>::len) + self.value_bytes_hint.get());
        let mut slots = Vec::with_capacity(self.value_columns.len());
        let geometry_end = geometry.filter(|_| self.store_geometry).map(|blob| {
            buf.extend_from_slice(blob);
            // SQLite's own value length is an i32, so a row's bytes cannot
            // reach the u32 ceiling.
            u32::try_from(buf.len()).unwrap_or(u32::MAX)
        });

        for (i, column) in self.value_columns.iter().enumerate() {
            let value = value_ref_from_sql(
                row.get_ref(i + 1)?,
                column.column_type.as_ref(),
                &column.name,
                self.options,
            )?;
            slots.push(match value {
                ValueRef::Null => Slot::Null,
                ValueRef::Boolean(b) => Slot::Boolean(b),
                ValueRef::Integer(i) => Slot::Integer(i),
                ValueRef::Float(f) => Slot::Float(f),
                ValueRef::Text(s) => {
                    let (start, end) = push_bytes(&mut buf, s.as_bytes());
                    Slot::Text { start, end }
                }
                ValueRef::Blob(b) => {
                    let (start, end) = push_bytes(&mut buf, b);
                    Slot::Blob { start, end }
                }
                ValueRef::Date(d) => Slot::Date(d),
                ValueRef::DateTime(dt) => Slot::DateTime(dt),
            });
        }

        // Carry this row's value bytes forward as the next row's estimate.
        self.value_bytes_hint
            .set(buf.len().saturating_sub(geometry_end.unwrap_or(0) as usize));
        Ok(Feature {
            fid,
            buf,
            geometry_end,
            // A layer with no geometry column has had nothing projected away,
            // so its rows answer `Ok(None)` as they always did; only a layer
            // that has one and did not select it makes `geometry` an error.
            geometry_projected: self.geometry_column.is_none() || self.store_geometry,
            slots: slots.into_boxed_slice(),
            columns: Arc::clone(&self.value_column_names),
        })
    }

    /// Enforce the opt-in declared-type check for one geometry blob: read the
    /// WKB type discriminator (no coordinate materialisation) and test it
    /// against the declared `gpkg_geometry_columns` type.
    fn check_declared_type(&self, blob: &[u8], declared: &GeometryColumn) -> Result<()> {
        let (_, offset) = gpb::parse_header(blob).map_err(|e| Error::Core(e.into()))?;
        // `parse_header` guarantees `offset <= blob.len()`; `get` keeps the
        // slice panic-free.
        let body = blob.get(offset..).unwrap_or_default();
        let found = geometry::wkb_geometry_type(body).map_err(|e| Error::Core(e.into()))?;
        if !geometry::geometry_type_matches(found, declared.geometry_type) {
            return Err(Error::GeometryTypeMismatch {
                table_name: self.table_name.clone(),
                column_name: declared.column_name.clone(),
                declared: declared.geometry_type,
                found,
            });
        }
        Ok(())
    }
}

/// Whether the row's true `f64` geometry envelope intersects `bbox`, read
/// straight from the raw blob. A NULL geometry cell, or an empty geometry (no
/// finite coordinate), never matches.
pub(crate) fn row_in_box(
    row: &rusqlite::Row<'_>,
    geom_idx: Option<usize>,
    bbox: &BoundingBox,
) -> Result<bool> {
    // A filtered query always selects the geometry column (features_in errors
    // on a layer without one), so geom_idx is present here.
    let Some(gi) = geom_idx else {
        return Ok(false);
    };
    let SqlValueRef::Blob(blob) = row.get_ref(gi)? else {
        return Ok(false);
    };
    match blob_xy_envelope(blob)? {
        Some(env) => Ok(bbox.intersects_envelope(env)),
        None => Ok(false),
    }
}

/// The true `f64` XY envelope `[min_x, max_x, min_y, max_y]` of a GPB blob:
/// the header envelope when present, else a full WKB traversal. `None` for an
/// empty geometry. This is the same rule the registered `ST_*` functions use.
fn blob_xy_envelope(blob: &[u8]) -> Result<Option<[f64; 4]>> {
    geometry::blob_xy_envelope(blob).map_err(|e| Error::Core(e.into()))
}

/// Round a query upper bound outward: to `f32` (nearest), then one ULP up.
/// Conservative for any input; the `f64` re-filter restores exactness.
pub(crate) fn widen_up(v: f64) -> f64 {
    f64::from((v as f32).next_up())
}

/// Round a query lower bound outward: to `f32` (nearest), then one ULP down.
pub(crate) fn widen_down(v: f64) -> f64 {
    f64::from((v as f32).next_down())
}

/// Qualify an identifier with an optional table prefix, quoting it.
fn qualified(name: &str, prefix: Option<&str>) -> Result<String> {
    let quoted = quote(name)?;
    Ok(match prefix {
        Some(p) => format!("{p}.{quoted}"),
        None => quoted,
    })
}

/// Append `bytes` to `buf`, returning the range they occupy.
fn push_bytes(buf: &mut Vec<u8>, bytes: &[u8]) -> (u32, u32) {
    let start = u32::try_from(buf.len()).unwrap_or(u32::MAX);
    buf.extend_from_slice(bytes);
    (start, u32::try_from(buf.len()).unwrap_or(u32::MAX))
}

/// One value of a [`Feature`], with the variable-length cases held as a range
/// into the feature's byte buffer rather than as their own allocation.
#[derive(Debug, Clone, Copy)]
enum Slot {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    /// UTF-8, checked when the row was read.
    Text {
        start: u32,
        end: u32,
    },
    Blob {
        start: u32,
        end: u32,
    },
    Date(Date),
    DateTime(DateTime),
}

/// A single row of a layer, owned so it outlives the SQLite cursor.
///
/// The geometry is kept as the raw GPB blob and parsed lazily by
/// [`Feature::geometry`]. Non-geometry column values are converted eagerly;
/// access them by name ([`Feature::value`]) or by index ([`Feature::get`]).
///
/// # Storage
///
/// The geometry blob and every text and binary cell live end to end in one
/// buffer, with each value recorded as a range into it. A row is therefore two
/// allocations whatever its width, where a `Vec<Value>` holding a `String` or
/// `Vec<u8>` per cell was one plus one per variable-length cell: on a thirteen
/// column layer with four text columns and a blob, seven.
///
/// This is why the accessors hand out [`ValueRef`] rather than `&Value`. There
/// is no `Value` in a feature to lend out; one is built on demand pointing into
/// the buffer. [`ValueRef::to_value`] gives a `Value` where one is needed.
#[derive(Clone)]
pub struct Feature {
    fid: i64,
    /// Geometry bytes first when present, then each text or blob cell in column
    /// order. Ranges in `slots` index this.
    buf: Vec<u8>,
    /// Where the geometry ends, and so `None` when the row has no geometry.
    geometry_end: Option<u32>,
    /// Whether the read that built this row carried the geometry, so that a
    /// row from a projection that dropped it is distinguishable from one whose
    /// geometry is NULL.
    geometry_projected: bool,
    slots: Box<[Slot]>,
    columns: Arc<[String]>,
}

impl std::fmt::Debug for Feature {
    /// Prints the values, not the storage: the byte buffer and its ranges are
    /// an implementation detail, and dumping them instead of the row's
    /// `(column, value)` pairs would make a feature unreadable in a test
    /// failure or a `dbg!`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Feature")
            .field("fid", &self.fid)
            .field("geometry_bytes", &self.geometry_bytes().map(<[u8]>::len))
            .field("values", &self.iter().collect::<Vec<_>>())
            .finish()
    }
}

impl Feature {
    /// The feature id: the value of the layer's primary-key column (or SQLite's
    /// `rowid` for a table without a single-column primary key).
    pub fn fid(&self) -> i64 {
        self.fid
    }

    /// The raw GeoPackage Binary (GPB) geometry blob, if the geometry cell is
    /// non-NULL.
    ///
    /// This is the way to read a non-linear geometry: a circular string, a
    /// compound curve, a curve polygon, a multicurve or a multisurface. The
    /// bytes are exactly what the file holds, and this crate computes their
    /// envelopes and indexes them, but [`Self::geometry`] cannot return one,
    /// because `geo-traits` has no representation for an arc to return it as.
    pub fn geometry_bytes(&self) -> Option<&[u8]> {
        let end = self.geometry_end?;
        self.buf.get(..end as usize)
    }

    /// Parse the geometry lazily as a [`GpbGeometry`].
    ///
    /// `Ok(None)` when the geometry cell is NULL (or the layer has none);
    /// `Err` when the blob is not a readable GPB geometry. A body declaring a
    /// non-linear type is one such case: use [`Self::geometry_bytes`] for
    /// those.
    pub fn geometry(&self) -> Result<Option<GpbGeometry<'_>>> {
        if !self.geometry_projected {
            return Err(Error::GeometryNotProjected);
        }
        match self.geometry_bytes() {
            None => Ok(None),
            Some(blob) => Ok(Some(
                GpbGeometry::parse(blob).map_err(|e| Error::Core(e.into()))?,
            )),
        }
    }

    /// Rebuild one slot as a borrowed value.
    fn slot_value(&self, slot: Slot) -> ValueRef<'_> {
        // Every range was recorded from this buffer's own length as it was
        // filled, so a miss is impossible; `unwrap_or` keeps the indexing
        // panic-free rather than guarding against a real case.
        let bytes = |start: u32, end: u32| self.buf.get(start as usize..end as usize);
        match slot {
            Slot::Null => ValueRef::Null,
            Slot::Boolean(b) => ValueRef::Boolean(b),
            Slot::Integer(i) => ValueRef::Integer(i),
            Slot::Float(f) => ValueRef::Float(f),
            Slot::Text { start, end } => ValueRef::Text(
                bytes(start, end)
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .unwrap_or_default(),
            ),
            Slot::Blob { start, end } => ValueRef::Blob(bytes(start, end).unwrap_or_default()),
            Slot::Date(d) => ValueRef::Date(d),
            Slot::DateTime(dt) => ValueRef::DateTime(dt),
        }
    }

    /// A value by column name, or `None` if the layer has no such value column.
    pub fn value(&self, name: &str) -> Option<ValueRef<'_>> {
        let index = self.columns.iter().position(|c| c == name)?;
        self.get(index)
    }

    /// A value by position within the value columns (in schema order), or
    /// `None` if the index is out of range.
    pub fn get(&self, index: usize) -> Option<ValueRef<'_>> {
        self.slots.get(index).map(|slot| self.slot_value(*slot))
    }

    /// All value-column values, in schema order.
    pub fn values(&self) -> impl ExactSizeIterator<Item = ValueRef<'_>> {
        self.slots.iter().map(|slot| self.slot_value(*slot))
    }

    /// The value-column names, parallel to [`Feature::values`].
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// Whether this row carries `name`.
    ///
    /// False both for a column the table does not have and for one a
    /// projection did not select, which [`Self::value`] cannot tell apart: it
    /// answers `None` for either, where a column that is present but NULL
    /// answers `Some(ValueRef::Null)`.
    #[must_use]
    pub fn has_column(&self, name: &str) -> bool {
        self.columns.iter().any(|c| c == name)
    }

    /// Whether this row carries its layer's geometry.
    ///
    /// True for a layer that has no geometry column at all, since nothing was
    /// projected away there and [`Self::geometry`] answers `Ok(None)` as it
    /// always has. False only under a projection that did not select it; see
    /// [`Layer::with_columns`]. [`Self::geometry`] errors rather than
    /// answering `Ok(None)` in that case, so that an absent geometry column is
    /// never mistaken for a NULL geometry.
    #[must_use]
    pub fn has_geometry_column(&self) -> bool {
        self.geometry_projected
    }

    /// The number of value columns.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether the feature has no value columns.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Iterate `(column name, value)` pairs in schema order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, ValueRef<'_>)> {
        self.columns.iter().map(String::as_str).zip(self.values())
    }
}

/// A fallible iterator of [`Feature`]s from a layer read.
///
/// Yields `Result<Feature>` per row: geometry or value errors surface as `Err`
/// for the offending row without ending iteration. See the module note on why
/// features are materialised rather than streamed lazily.
#[derive(Debug)]
pub struct Features {
    inner: std::vec::IntoIter<Result<Feature>>,
}

impl Iterator for Features {
    type Item = Result<Feature>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl ExactSizeIterator for Features {}
