//! Feature and attribute layers: typed handles over a user table, and the
//! streaming read path ([`Layer::features`], [`Layer::features_in`],
//! [`Layer::select`]).
//!
//! A [`Layer`] borrows the [`GeoPackage`] it came from and caches the table's
//! introspected [`TableSchema`], its resolved geometry column (feature layers),
//! and its single-column primary key. The read methods build a prepared
//! statement from that schema and yield owned [`Feature`]s: a row's values do
//! not outlive the SQLite cursor, so each [`Feature`] owns its geometry blob
//! and its converted column values rather than borrowing the row.
//!
//! The read methods currently materialise the result set into owned features
//! before returning the iterator. rusqlite's row cursor (`Rows`) borrows its
//! `Statement` and resets it on drop, so a truly lazy iterator that owns both
//! would be a self-referential struct — not expressible under this crate's
//! `#![forbid(unsafe_code)]` without a helper such as `self_cell`. The
//! [`Feature`] ownership shape is unaffected; the trade-off is peak memory for
//! very large result sets, revisited when the GeoArrow bulk plane lands.

use std::sync::Arc;

use geopackage_core::geometry::GpbGeometry;
use geopackage_core::gpb;
use geopackage_core::ident::quote;
use geopackage_core::triggers::{self, TriggerGeneration};
use rusqlite::OptionalExtension;
use rusqlite::types::ValueRef;

use crate::value::{value_from_ref, value_to_sql};
use crate::{
    Column, ConversionOptions, Error, GeoPackage, GeometryColumn, Result, TableSchema, Value,
    resolve_table_name, table_exists,
};

/// An XY bounding box for spatial queries ([`Layer::features_in`]).
///
/// Fields are ordinary map coordinates in the layer's own spatial reference
/// system; this crate never transforms coordinates (design decision D3).
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
    /// The `gpkg_contents.data_type` string this kind requires.
    fn data_type(self) -> &'static str {
        match self {
            Self::Feature => "features",
            Self::Attributes => "attributes",
        }
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
    /// Non-geometry columns, in schema order — the values each [`Feature`]
    /// carries.
    value_columns: Vec<Column>,
    /// The names of [`Self::value_columns`], shared cheaply into every
    /// [`Feature`] for by-name access.
    value_column_names: Arc<[String]>,
    options: ConversionOptions,
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
        let value_columns: Vec<Column> = schema
            .columns
            .iter()
            .filter(|c| geom_name.as_deref() != Some(c.name.as_str()))
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
            options: ConversionOptions::strict(),
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

    /// The [`ConversionOptions`] used when converting column values.
    pub fn conversion_options(&self) -> ConversionOptions {
        self.options
    }

    /// Set the [`ConversionOptions`] for value conversion (e.g. lenient
    /// `DATETIME` parsing). Applies to [`Self::features`], [`Self::features_in`]
    /// and [`Self::select`].
    #[must_use]
    pub fn with_conversion_options(mut self, options: ConversionOptions) -> Self {
        self.options = options;
        self
    }

    /// Iterate every row of the layer as an owned [`Feature`].
    ///
    /// The iterator is fallible per row: a value that does not fit its declared
    /// column type surfaces as an `Err` for that row without stopping the scan.
    /// Rows are read in the table's natural order.
    pub fn features(&self) -> Result<Features> {
        let (sql, geom_idx) = self.base_select()?;
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
    /// fallback — the same rule the `ST_*` functions use).
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
            vec![
                Sql::Real(bbox.max_x),
                Sql::Real(bbox.min_x),
                Sql::Real(bbox.max_y),
                Sql::Real(bbox.min_y),
            ]
        } else {
            Vec::new()
        };
        self.execute(&sql, params, geom_idx, Some(bbox))
    }

    /// The SQL [`Self::features_in`] runs: an RTree join when a usable spatial
    /// index is present, otherwise the full-scan `features` query.
    ///
    /// Exposed for diagnostics — running `EXPLAIN QUERY PLAN` on it confirms
    /// whether the RTree virtual table is used. The RTree form carries `?1`–`?4`
    /// placeholders for the query box.
    pub fn features_in_sql(&self) -> Result<String> {
        Ok(self.features_in_plan()?.0)
    }

    /// Iterate the features matching a caller-supplied `WHERE` clause.
    ///
    /// `where_clause` is appended (parenthesised) to the layer's base query and
    /// is **raw SQL, trusted from the caller** — this crate does not parse or
    /// sanitise it (design decision D9: SQL is the query engine;
    /// [`GeoPackage::connection`] is the full escape hatch). `params` bind its
    /// placeholders; they are this crate's [`Value`] enum, converted internally
    /// so rusqlite types stay out of the public API. [`Value::Date`] and
    /// [`Value::DateTime`] bind as their canonical text form.
    ///
    /// ```no_run
    /// # fn main() -> Result<(), geopackage::Error> {
    /// # let gpkg = geopackage::GeoPackage::open("x.gpkg")?;
    /// # let layer = gpkg.layer("roads")?;
    /// use geopackage::Value;
    /// for feature in layer.select("name = ?1", &[Value::Text("A1".into())])? {
    ///     let feature = feature?;
    ///     println!("{}", feature.fid());
    /// }
    /// # Ok(()) }
    /// ```
    pub fn select(&self, where_clause: &str, params: &[Value]) -> Result<Features> {
        let (base, geom_idx) = self.base_select()?;
        let sql = format!("{base} WHERE ({where_clause})");
        let sql_params: Vec<rusqlite::types::Value> = params.iter().map(value_to_sql).collect();
        self.execute(&sql, sql_params, geom_idx, None)
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
        let names: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = ?1",
            )?;
            stmt.query_map([self.table_name.as_str()], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        let generation = triggers::classify_triggers(
            names.iter().map(String::as_str),
            &self.table_name,
            &geom.column_name,
        );
        Ok(generation != TriggerGeneration::None)
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
            let (sql, geom_idx) = self.base_select()?;
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
    fn column_exprs(&self, prefix: Option<&str>) -> Result<(String, Option<usize>)> {
        let mut exprs = Vec::with_capacity(self.value_columns.len() + 2);
        exprs.push(self.fid_expr(prefix)?);
        for column in &self.value_columns {
            exprs.push(qualified(&column.name, prefix)?);
        }
        let geom_idx = match &self.geometry_column {
            Some(geom) => {
                exprs.push(qualified(&geom.column_name, prefix)?);
                Some(exprs.len() - 1)
            }
            None => None,
        };
        Ok((exprs.join(", "), geom_idx))
    }

    fn base_select(&self) -> Result<(String, Option<usize>)> {
        let (list, geom_idx) = self.column_exprs(None)?;
        Ok((
            format!("SELECT {list} FROM {}", quote(&self.table_name)?),
            geom_idx,
        ))
    }

    fn rtree_select(&self, geom: &GeometryColumn) -> Result<(String, Option<usize>)> {
        let table = quote(&self.table_name)?;
        let (list, geom_idx) = self.column_exprs(Some(&table))?;
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
        while let Some(row) = rows.next()? {
            // Decide bbox membership from the geometry blob before converting
            // any values: a row outside the box is skipped entirely, so value
            // conversion errors surface only for rows the query returns —
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
            out.push(self.feature_from_row(row, geom_idx));
        }
        Ok(Features {
            inner: out.into_iter(),
        })
    }

    fn feature_from_row(
        &self,
        row: &rusqlite::Row<'_>,
        geom_idx: Option<usize>,
    ) -> Result<Feature> {
        let fid: i64 = row.get(0)?;
        let mut values = Vec::with_capacity(self.value_columns.len());
        for (i, column) in self.value_columns.iter().enumerate() {
            values.push(value_from_ref(
                row.get_ref(i + 1)?,
                column.column_type.as_ref(),
                &column.name,
                self.options,
            )?);
        }
        let geometry = match geom_idx {
            Some(gi) => match row.get_ref(gi)? {
                ValueRef::Blob(bytes) => Some(bytes.to_vec()),
                // NULL, or a non-blob value in the geometry column, reads as no
                // geometry rather than an error.
                _ => None,
            },
            None => None,
        };
        Ok(Feature {
            fid,
            geometry,
            values,
            columns: Arc::clone(&self.value_column_names),
        })
    }
}

/// Whether the row's true `f64` geometry envelope intersects `bbox`, read
/// straight from the raw blob. A NULL geometry cell, or an empty geometry (no
/// finite coordinate), never matches.
fn row_in_box(
    row: &rusqlite::Row<'_>,
    geom_idx: Option<usize>,
    bbox: &BoundingBox,
) -> Result<bool> {
    // A filtered query always selects the geometry column (features_in errors
    // on a layer without one), so geom_idx is present here.
    let Some(gi) = geom_idx else {
        return Ok(false);
    };
    let ValueRef::Blob(blob) = row.get_ref(gi)? else {
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
    let (header, _) = gpb::parse_header(blob).map_err(|e| Error::Core(e.into()))?;
    if let Some((min_x, max_x, min_y, max_y)) = header.envelope.xy_bounds() {
        return Ok(Some([min_x, max_x, min_y, max_y]));
    }
    Ok(GpbGeometry::parse(blob)
        .map_err(|e| Error::Core(e.into()))?
        .xy_envelope())
}

/// Qualify an identifier with an optional table prefix, quoting it.
fn qualified(name: &str, prefix: Option<&str>) -> Result<String> {
    let quoted = quote(name)?;
    Ok(match prefix {
        Some(p) => format!("{p}.{quoted}"),
        None => quoted,
    })
}

/// A single row of a layer, owned so it outlives the SQLite cursor.
///
/// The geometry is kept as the raw GPB blob and parsed lazily by
/// [`Feature::geometry`]. Non-geometry column values are converted eagerly to
/// [`Value`]s; access them by name ([`Feature::value`]) or by index
/// ([`Feature::get`]).
#[derive(Debug, Clone)]
pub struct Feature {
    fid: i64,
    geometry: Option<Vec<u8>>,
    values: Vec<Value>,
    columns: Arc<[String]>,
}

impl Feature {
    /// The feature id: the value of the layer's primary-key column (or SQLite's
    /// `rowid` for a table without a single-column primary key).
    pub fn fid(&self) -> i64 {
        self.fid
    }

    /// The raw GeoPackage Binary (GPB) geometry blob, if the geometry cell is
    /// non-NULL.
    pub fn geometry_bytes(&self) -> Option<&[u8]> {
        self.geometry.as_deref()
    }

    /// Parse the geometry lazily as a [`GpbGeometry`].
    ///
    /// `Ok(None)` when the geometry cell is NULL (or the layer has none);
    /// `Err` when the blob is not a readable GPB geometry.
    pub fn geometry(&self) -> Result<Option<GpbGeometry<'_>>> {
        match &self.geometry {
            None => Ok(None),
            Some(blob) => Ok(Some(
                GpbGeometry::parse(blob).map_err(|e| Error::Core(e.into()))?,
            )),
        }
    }

    /// A value by column name, or `None` if the layer has no such value column.
    pub fn value(&self, name: &str) -> Option<&Value> {
        self.columns
            .iter()
            .position(|c| c == name)
            .map(|i| &self.values[i])
    }

    /// A value by position within the value columns (in schema order), or
    /// `None` if the index is out of range.
    pub fn get(&self, index: usize) -> Option<&Value> {
        self.values.get(index)
    }

    /// All value-column values, in schema order.
    pub fn values(&self) -> &[Value] {
        &self.values
    }

    /// The value-column names, parallel to [`Feature::values`].
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// The number of value columns.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether the feature has no value columns.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Iterate `(column name, value)` pairs in schema order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.columns
            .iter()
            .map(String::as_str)
            .zip(self.values.iter())
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
