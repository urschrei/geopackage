//! The `Layer` class: a feature layer or attribute table, by name.

use geopackage::core::gpb;
use geopackage::{
    BoundingBox, GeoPackage, GeometryColumn, Layer, SpatialIndexStatus, Value, ValueRef,
};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple};

use crate::PyGpkgResult;
use crate::convert::{py_to_value, py_to_values};
use crate::features::{FeatureQuery, PyFeatureIterator};
use crate::gpkg::{PyExtension, PyGeoPackage, lookup_error, zm_name};

/// A value column: every column except the primary key and the geometry.
#[pyclass(name = "Field", module = "geopackage_rs", frozen, get_all)]
#[derive(Clone)]
pub struct PyField {
    pub(crate) name: String,
    /// The declared type, as the table's DDL spells it, such as ``"TEXT"``
    /// or ``"DOUBLE"``.
    pub(crate) declared_type: String,
    /// The GeoPackage type it parses to, upper case, or ``None`` for a type
    /// outside the spec's vocabulary.
    pub(crate) column_type: Option<String>,
    pub(crate) not_null: bool,
    pub(crate) default_value: Option<String>,
}

#[pymethods]
impl PyField {
    fn __repr__(&self) -> String {
        format!("Field({:?}, {:?})", self.name, self.declared_type)
    }
}

/// A row of ``gpkg_geometry_columns``.
#[pyclass(name = "GeometryColumn", module = "geopackage_rs", frozen, get_all)]
#[derive(Clone)]
pub struct PyGeometryColumn {
    pub(crate) column_name: String,
    /// Upper case, such as ``"MULTIPOLYGON"``.
    pub(crate) geometry_type: String,
    pub(crate) srs_id: i32,
    /// ``"prohibited"``, ``"mandatory"`` or ``"optional"``.
    pub(crate) z: String,
    pub(crate) m: String,
}

#[pymethods]
impl PyGeometryColumn {
    fn __repr__(&self) -> String {
        format!(
            "GeometryColumn({:?}, {}, srs_id={})",
            self.column_name, self.geometry_type, self.srs_id
        )
    }
}

/// A feature layer or attribute table of a `GeoPackage`.
///
/// The schema is read once, when the object is made; a column added to the
/// table afterwards is not seen until the layer is looked up again.
#[pyclass(name = "Layer", module = "geopackage_rs", frozen)]
pub struct PyLayer {
    gpkg: Py<PyGeoPackage>,
    name: String,
    attributes: bool,
    fields: Vec<PyField>,
    geometry: Option<GeometryColumn>,
    primary_key: Option<String>,
}

/// Opens the Rust handle for a layer of `gpkg`.
fn open_layer<'g>(
    gpkg: &'g GeoPackage,
    name: &str,
    attributes: bool,
) -> geopackage::Result<Layer<'g>> {
    if attributes {
        gpkg.attributes(name)
    } else {
        gpkg.layer(name)
    }
}

impl PyLayer {
    pub(crate) fn resolve(
        gpkg: &Bound<'_, PyGeoPackage>,
        name: &str,
        attributes: bool,
    ) -> PyGpkgResult<Self> {
        let (fields, geometry, primary_key) = gpkg
            .get()
            .with(gpkg.py(), |g| {
                let layer = open_layer(g, name, attributes)?;
                let geometry = layer.geometry_column().cloned();
                let pk = layer.primary_key_column().map(str::to_owned);
                let fields = layer
                    .schema()
                    .columns
                    .iter()
                    .filter(|c| Some(&c.name) != geometry.as_ref().map(|g| &g.column_name))
                    .filter(|c| Some(c.name.as_str()) != pk.as_deref())
                    .map(|c| PyField {
                        name: c.name.clone(),
                        declared_type: c.declared_type.clone(),
                        column_type: c.column_type.as_ref().map(|t| t.ddl_name()),
                        not_null: c.not_null,
                        default_value: c.default_value.clone(),
                    })
                    .collect();
                Ok((fields, geometry, pk))
            })
            .or_else(|error| {
                // `with` has already turned the error into a Python one; look
                // the layer up again only to choose the exception type.
                let missing = gpkg
                    .get()
                    .with(gpkg.py(), |g| Ok(open_layer(g, name, attributes).err()))?;
                match missing {
                    Some(e) => Err(lookup_error("layer", name, e)),
                    None => Err(error),
                }
            })?;
        Ok(Self {
            gpkg: gpkg.clone().unbind(),
            name: name.to_owned(),
            attributes,
            fields,
            geometry,
            primary_key,
        })
    }

    /// Runs `f` on the Rust handle, with the GIL released.
    fn with<R: Send>(
        &self,
        py: Python<'_>,
        f: impl FnOnce(Layer<'_>) -> geopackage::Result<R> + Send,
    ) -> PyGpkgResult<R> {
        let (name, attributes) = (self.name.as_str(), self.attributes);
        self.gpkg
            .get()
            .with(py, |g| f(open_layer(g, name, attributes)?))
    }
}

/// One row to write: `(fid, wkb, values)`, converted while the GIL is held.
struct NewRow {
    fid: Option<i64>,
    wkb: Option<Vec<u8>>,
    values: Vec<Value>,
}

fn new_row(row: &Bound<'_, PyAny>) -> PyGpkgResult<NewRow> {
    let tuple = row
        .cast::<PyTuple>()
        .map_err(|_not_tuple| PyValueError::new_err("a row is a (fid, wkb, values) tuple"))?;
    if tuple.len() != 3 {
        return Err(PyValueError::new_err("a row is a (fid, wkb, values) tuple"));
    }
    let wkb = tuple.get_item(1)?;
    Ok(NewRow {
        fid: tuple.get_item(0)?.extract()?,
        wkb: if wkb.is_none() {
            None
        } else {
            Some(wkb.extract::<Vec<u8>>()?)
        },
        values: py_to_values(&tuple.get_item(2)?)?,
    })
}

fn write_rows(layer: &Layer<'_>, rows: &[NewRow]) -> geopackage::Result<Vec<i64>> {
    let mut writer = layer.writer()?;
    let mut fids = Vec::with_capacity(rows.len());
    for row in rows {
        let values: Vec<ValueRef<'_>> = row.values.iter().map(ValueRef::from).collect();
        let fid = match &row.wkb {
            Some(wkb) => writer.insert_wkb(row.fid, wkb, &values)?,
            None => writer.insert_row(row.fid, &values)?,
        };
        fids.push(fid);
    }
    writer.commit()?;
    Ok(fids)
}

/// Returns the word that `Layer.spatial_index_status` documents for a status.
/// `SpatialIndexStatus::as_str` gives a phrase for display ("legacy trigger
/// set"), which callers that compare the result do not expect.
fn status_name(status: SpatialIndexStatus) -> &'static str {
    match status {
        SpatialIndexStatus::Absent => "absent",
        SpatialIndexStatus::Current => "current",
        SpatialIndexStatus::Legacy => "legacy",
        SpatialIndexStatus::Stale => "stale",
        // `SpatialIndexStatus` is `#[non_exhaustive]`. A status that this
        // version does not know gets the crate's phrase, so that a caller
        // reports it and does not act on it.
        other => other.as_str(),
    }
}

#[pymethods]
impl PyLayer {
    fn __repr__(&self) -> String {
        format!("Layer({:?})", self.name)
    }

    /// The table name.
    #[getter]
    fn name(&self) -> &str {
        &self.name
    }

    /// ``"features"`` or ``"attributes"``.
    #[getter]
    fn kind(&self) -> &'static str {
        if self.geometry.is_some() {
            "features"
        } else {
            "attributes"
        }
    }

    /// The value columns, in table order. Rows read from and written to the
    /// layer carry their values in this order.
    #[getter]
    fn fields(&self) -> Vec<PyField> {
        self.fields.clone()
    }

    /// The names of :attr:`fields`.
    #[getter]
    fn field_names(&self) -> Vec<String> {
        self.fields.iter().map(|f| f.name.clone()).collect()
    }

    /// The integer primary key column, or ``None`` for a table that uses
    /// SQLite's ``rowid``.
    #[getter]
    fn primary_key(&self) -> Option<&str> {
        self.primary_key.as_deref()
    }

    /// The geometry column, or ``None`` for an attribute table.
    #[getter]
    fn geometry_column(&self) -> Option<PyGeometryColumn> {
        self.geometry.as_ref().map(|g| PyGeometryColumn {
            column_name: g.column_name.clone(),
            geometry_type: g.geometry_type.as_str().to_owned(),
            srs_id: g.srs_id,
            z: zm_name(g.z).to_owned(),
            m: zm_name(g.m).to_owned(),
        })
    }

    /// The number of rows.
    fn count(&self, py: Python<'_>) -> PyGpkgResult<u64> {
        self.with(py, |l| l.count())
    }

    /// The extent recorded in ``gpkg_contents``, as ``(min_x, min_y, max_x,
    /// max_y)``, or ``None``.
    fn extent(&self, py: Python<'_>) -> PyGpkgResult<Option<(f64, f64, f64, f64)>> {
        Ok(self
            .with(py, |l| l.extent())?
            .map(|b| (b.min_x, b.min_y, b.max_x, b.max_y)))
    }

    /// Measures the extent from the geometries and records it.
    fn recompute_extent(&self, py: Python<'_>) -> PyGpkgResult<Option<(f64, f64, f64, f64)>> {
        Ok(self
            .with(py, |l| l.recompute_extent())?
            .map(|b| (b.min_x, b.min_y, b.max_x, b.max_y)))
    }

    /// ``"current"``, ``"legacy"`` (pre-1.4 or mixed triggers), ``"stale"``
    /// (out of step with the table) or ``"absent"``.
    fn spatial_index_status(&self, py: Python<'_>) -> PyGpkgResult<String> {
        self.with(
            py,
            |l| Ok(status_name(l.spatial_index_status()?).to_owned()),
        )
    }

    /// Whether bounding-box reads use an RTree.
    fn has_spatial_index(&self, py: Python<'_>) -> PyGpkgResult<bool> {
        self.with(py, |l| l.has_spatial_index())
    }

    /// Builds an RTree index and the GeoPackage 1.4 triggers on a layer that
    /// has none.
    fn create_spatial_index(&self, py: Python<'_>) -> PyGpkgResult<()> {
        self.with(py, |l| l.create_spatial_index())
    }

    /// Replaces a legacy or stale index with a current one.
    fn repair_spatial_index(&self, py: Python<'_>) -> PyGpkgResult<()> {
        self.with(py, |l| l.repair_spatial_index())
    }

    /// Repopulates the index from the table.
    fn rebuild_spatial_index(&self, py: Python<'_>) -> PyGpkgResult<()> {
        self.with(py, |l| l.rebuild_spatial_index())
    }

    /// Drops the index, its triggers and its extension row.
    fn drop_spatial_index(&self, py: Python<'_>) -> PyGpkgResult<()> {
        self.with(py, |l| l.drop_spatial_index())
    }

    /// Compares the index with the table, as a dictionary with the keys
    /// ``indexable``, ``entries``, ``missing``, ``extra``, ``not_covering``
    /// and ``consistent``.
    fn audit_spatial_index<'py>(&self, py: Python<'py>) -> PyGpkgResult<Bound<'py, PyDict>> {
        let audit = self.with(py, |l| l.audit_spatial_index())?;
        let dict = PyDict::new(py);
        dict.set_item("indexable", audit.indexable)?;
        dict.set_item("entries", audit.entries)?;
        dict.set_item("missing", audit.missing)?;
        dict.set_item("extra", audit.extra)?;
        dict.set_item("not_covering", audit.not_covering)?;
        dict.set_item("consistent", audit.is_consistent())?;
        Ok(dict)
    }

    /// The ``gpkg_extensions`` rows that name this table.
    fn extensions(&self, py: Python<'_>) -> PyGpkgResult<Vec<PyExtension>> {
        let rows = self.with(py, |l| l.extensions())?;
        Ok(rows.iter().map(PyExtension::from_row).collect())
    }

    /// Streams the layer's rows as ``(fid, wkb, values)`` tuples.
    ///
    /// `wkb` is ISO WKB bytes, or ``None`` for a row with no geometry, an
    /// attribute table, or ``geometry=False``. `values` follow
    /// :attr:`fields`, narrowed to `columns` when that is given; the
    /// iterator's ``columns`` attribute names them.
    ///
    /// `bbox` is ``(min_x, min_y, max_x, max_y)`` in the layer's SRS and uses
    /// the RTree when there is one. `where` is a raw SQL condition, trusted
    /// from the caller, with ``?1`` to ``?N`` bound from `params`. Both may be
    /// given together.
    ///
    /// The read runs on its own thread and connection, so it sees committed
    /// data only, and it overlaps with the caller's work on each chunk.
    #[pyo3(signature = (
        bbox = None, r#where = None, params = None, columns = None, *,
        geometry = true, chunk_size = 1000,
    ))]
    fn features(
        &self,
        bbox: Option<(f64, f64, f64, f64)>,
        r#where: Option<String>,
        params: Option<Bound<'_, PyAny>>,
        columns: Option<Vec<String>>,
        geometry: bool,
        chunk_size: usize,
    ) -> PyGpkgResult<PyFeatureIterator> {
        if bbox.is_some() && self.geometry.is_none() {
            return Err(PyValueError::new_err(format!(
                "{} has no geometry column to filter by",
                self.name
            )));
        }
        let params = match params {
            Some(p) => py_to_values(&p)?,
            None => Vec::new(),
        };
        let selected: Vec<String> = match &columns {
            Some(names) => {
                for name in names {
                    if !self.fields.iter().any(|f| &f.name == name) {
                        return Err(crate::gpkg::no_such("column", name));
                    }
                }
                self.fields
                    .iter()
                    .filter(|f| names.contains(&f.name))
                    .map(|f| f.name.clone())
                    .collect()
            }
            None => self.field_names(),
        };
        let query = FeatureQuery {
            path: self.gpkg.get().path().clone(),
            table: self.name.clone(),
            attributes: self.attributes,
            bbox: bbox.map(|(a, b, c, d)| BoundingBox::new(a, b, c, d)),
            where_clause: r#where,
            params,
            columns,
            geometry: geometry && self.geometry.is_some(),
            geometry_column: self.geometry.as_ref().map(|g| g.column_name.clone()),
            chunk_size: chunk_size.max(1),
        };
        PyFeatureIterator::start(query, selected)
    }

    /// The row with feature id `fid`, as a ``(fid, wkb, values)`` tuple, or
    /// ``None``.
    fn get<'py>(&self, py: Python<'py>, fid: i64) -> PyGpkgResult<Option<Bound<'py, PyTuple>>> {
        let pk = self.primary_key.as_deref().unwrap_or("rowid");
        let clause = if pk == "rowid" {
            "rowid = ?1".to_owned()
        } else {
            format!("{} = ?1", crate::convert::quote_ident(pk))
        };
        let geometry = self.geometry.is_some();
        let row = self.with(py, |layer| {
            let layer = if geometry {
                layer
            } else {
                layer.without_geometry()
            };
            let mut found = layer.select(&clause, &[ValueRef::Integer(fid)])?;
            found
                .next()
                .transpose()?
                .map(|feature| {
                    let wkb = match feature.geometry_bytes() {
                        Some(blob) => {
                            let offset = gpb::body_offset(blob)
                                .map_err(|e| geopackage::Error::Core(e.into()))?;
                            Some(blob.get(offset..).unwrap_or_default().to_vec())
                        }
                        None => None,
                    };
                    let values: Vec<Value> = feature.values().map(|v| v.to_value()).collect();
                    Ok((feature.fid(), wkb, values))
                })
                .transpose()
        })?;
        row.map(|(fid, wkb, values): (i64, Option<Vec<u8>>, Vec<Value>)| {
            crate::features::row_tuple(py, fid, wkb.as_deref(), &values)
        })
        .transpose()
    }

    /// Inserts rows, each a ``(fid, wkb, values)`` tuple, and returns their
    /// feature ids.
    ///
    /// `fid` is ``None`` to let SQLite assign one. `wkb` is ISO WKB or
    /// ``None``. `values` follow :attr:`fields`, all of them. Rows are
    /// committed every `batch` rows, so a failure part way leaves the
    /// batches before it in place.
    #[pyo3(signature = (rows, *, batch = 10_000))]
    fn insert(
        &self,
        py: Python<'_>,
        rows: &Bound<'_, PyAny>,
        batch: usize,
    ) -> PyGpkgResult<Vec<i64>> {
        let batch = batch.max(1);
        let mut fids = Vec::new();
        let mut pending = Vec::with_capacity(batch.min(65_536));
        for row in rows.try_iter()? {
            pending.push(new_row(&row?)?);
            if pending.len() >= batch {
                fids.extend(self.with(py, |l| write_rows(&l, &pending))?);
                pending.clear();
            }
        }
        if !pending.is_empty() {
            fids.extend(self.with(py, |l| write_rows(&l, &pending))?);
        }
        Ok(fids)
    }

    /// Sets the named columns of row `fid` from a ``{name: value}``
    /// dictionary. Returns ``False`` if there is no such row.
    fn update_attributes(
        &self,
        py: Python<'_>,
        fid: i64,
        values: &Bound<'_, PyDict>,
    ) -> PyGpkgResult<bool> {
        let mut updates = Vec::with_capacity(values.len());
        for (name, value) in values.iter() {
            updates.push((name.extract::<String>()?, py_to_value(&value)?));
        }
        self.with(py, |layer| {
            let refs: Vec<(&str, ValueRef<'_>)> = updates
                .iter()
                .map(|(n, v)| (n.as_str(), ValueRef::from(v)))
                .collect();
            let mut writer = layer.writer()?;
            let updated = writer.update_columns(fid, &refs)?;
            writer.commit()?;
            Ok(updated)
        })
    }

    /// Replaces the geometry of row `fid` with ISO WKB, keeping its values.
    /// Returns ``False`` if there is no such row.
    fn update_geometry(&self, py: Python<'_>, fid: i64, wkb: Vec<u8>) -> PyGpkgResult<bool> {
        if self.geometry.is_none() {
            return Err(PyValueError::new_err(format!(
                "{} has no geometry column",
                self.name
            )));
        }
        let pk = self.primary_key.as_deref().unwrap_or("rowid").to_owned();
        let (name, attributes) = (self.name.as_str(), self.attributes);
        self.gpkg.get().with(py, |g| {
            let layer = open_layer(g, name, attributes)?;
            let clause = format!("{} = ?1", crate::convert::quote_ident(&pk));
            let current = open_layer(g, name, attributes)?
                .without_geometry()
                .select(&clause, &[ValueRef::Integer(fid)])?
                .next()
                .transpose()?;
            let Some(current) = current else {
                return Ok(false);
            };
            let values: Vec<Value> = current.values().map(|v| v.to_value()).collect();
            let refs: Vec<ValueRef<'_>> = values.iter().map(ValueRef::from).collect();
            let mut writer = layer.writer()?;
            let updated = writer.update_wkb(fid, &wkb, &refs)?;
            writer.commit()?;
            Ok(updated)
        })
    }

    /// Deletes rows by feature id, in one transaction, and returns how many
    /// existed.
    fn delete(&self, py: Python<'_>, fids: Vec<i64>) -> PyGpkgResult<usize> {
        self.with(py, |layer| {
            let mut writer = layer.writer()?;
            let mut deleted = 0;
            for fid in &fids {
                if writer.delete(*fid)? {
                    deleted += 1;
                }
            }
            writer.commit()?;
            Ok(deleted)
        })
    }
}
