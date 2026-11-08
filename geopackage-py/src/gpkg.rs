//! The `GeoPackage` class, and the plain records it returns.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use geopackage::core::types::{ColumnType, GeometryType, ZmFlag};
use geopackage::{
    ColumnSpec, ContentsDataType, GeoPackage, GeometrySpec, OpenOptions, Srs, TableSchemaBuilder,
};
use pyo3::exceptions::{PyKeyError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple};

use crate::coverage::PyCoverage;
use crate::layer::PyLayer;
use crate::{GeoPackageError, IntoPyResult, PyGpkgResult, to_py};

/// An open GeoPackage file.
///
/// ``GeoPackage(path)`` opens an existing file read-write;
/// ``read_only=True`` opens it read-only, and ``lenient=True`` accepts the
/// defects a repair tool has to be able to open (a legacy ``application_id``,
/// a missing ``gpkg_geometry_columns``, an unsupported extension), recording
/// them in :meth:`open_warnings`. :meth:`create` makes a new file.
///
/// The object is a context manager that closes the file on exit.
#[pyclass(name = "GeoPackage", module = "geopackage_rs", frozen)]
pub struct PyGeoPackage {
    inner: Mutex<Option<GeoPackage>>,
    path: PathBuf,
    read_only: bool,
}

impl PyGeoPackage {
    fn guard(&self) -> PyGpkgResult<MutexGuard<'_, Option<GeoPackage>>> {
        self.inner
            .lock()
            .map_err(|_poisoned| GeoPackageError::new_err("the connection's lock is poisoned"))
    }

    /// Runs `f` on the open connection, with the GIL released.
    pub(crate) fn with<R: Send>(
        &self,
        py: Python<'_>,
        f: impl FnOnce(&GeoPackage) -> geopackage::Result<R> + Send,
    ) -> PyGpkgResult<R> {
        py.detach(|| {
            let guard = self.guard()?;
            let gpkg = guard
                .as_ref()
                .ok_or_else(|| GeoPackageError::new_err("the GeoPackage is closed"))?;
            f(gpkg).py()
        })
    }

    /// The file's path, for readers that open their own connection.
    pub(crate) fn path(&self) -> &PathBuf {
        &self.path
    }
}

fn parse_geometry_type(name: &str) -> PyGpkgResult<GeometryType> {
    GeometryType::parse(&name.to_ascii_uppercase())
        .ok_or_else(|| PyValueError::new_err(format!("unknown geometry type {name:?}")))
}

fn parse_zm(flag: &Bound<'_, PyAny>) -> PyGpkgResult<ZmFlag> {
    if let Ok(code) = flag.extract::<u8>() {
        return ZmFlag::from_code(code)
            .ok_or_else(|| PyValueError::new_err(format!("z/m code {code} is not 0, 1 or 2")));
    }
    let text: String = flag.extract()?;
    match text.to_ascii_lowercase().as_str() {
        "prohibited" | "no" => Ok(ZmFlag::Prohibited),
        "mandatory" | "yes" => Ok(ZmFlag::Mandatory),
        "optional" => Ok(ZmFlag::Optional),
        _ => Err(PyValueError::new_err(format!(
            "z/m flag {text:?} is not prohibited, mandatory or optional"
        ))),
    }
}

pub(crate) fn zm_name(flag: ZmFlag) -> &'static str {
    match flag {
        ZmFlag::Mandatory => "mandatory",
        ZmFlag::Optional => "optional",
        _ => "prohibited",
    }
}

/// Reads one `columns` entry of `create_layer`: `(name, type)` or
/// `(name, type, not_null)`.
fn column_spec(entry: &Bound<'_, PyAny>) -> PyGpkgResult<ColumnSpec> {
    let tuple = entry.cast::<PyTuple>().map_err(|_not_tuple| {
        PyValueError::new_err("a column is a (name, type) or (name, type, not_null) tuple")
    })?;
    let name: String = tuple.get_item(0)?.extract()?;
    let type_name: String = tuple.get_item(1)?.extract()?;
    let column_type = ColumnType::parse(&type_name).ok_or_else(|| {
        PyValueError::new_err(format!(
            "column {name:?}: {type_name:?} is not a GeoPackage column type"
        ))
    })?;
    let mut spec = ColumnSpec::new(name, column_type);
    if tuple.len() > 2 && tuple.get_item(2)?.is_truthy()? {
        spec = spec.not_null();
    }
    Ok(spec)
}

fn contents_type_name(data_type: &ContentsDataType) -> String {
    match data_type {
        ContentsDataType::Features => "features".to_owned(),
        ContentsDataType::Attributes => "attributes".to_owned(),
        ContentsDataType::Tiles => "tiles".to_owned(),
        ContentsDataType::Coverage => "2d-gridded-coverage".to_owned(),
        other => format!("{other:?}"),
    }
}

#[pymethods]
impl PyGeoPackage {
    #[new]
    #[pyo3(signature = (path, *, read_only = false, lenient = false))]
    fn new(py: Python<'_>, path: PathBuf, read_only: bool, lenient: bool) -> PyGpkgResult<Self> {
        let gpkg = py
            .detach(|| {
                let options = OpenOptions::new().lenient(lenient);
                if read_only {
                    options.open_read_only(&path)
                } else {
                    options.open(&path)
                }
            })
            .py()?;
        Ok(Self {
            inner: Mutex::new(Some(gpkg)),
            path,
            read_only,
        })
    }

    /// Creates a new, empty GeoPackage. Fails if `path` exists.
    #[staticmethod]
    fn create(py: Python<'_>, path: PathBuf) -> PyGpkgResult<Self> {
        let gpkg = py.detach(|| GeoPackage::create(&path)).py()?;
        Ok(Self {
            inner: Mutex::new(Some(gpkg)),
            path,
            read_only: false,
        })
    }

    /// Closes the connection. Later calls raise `GeoPackageError`.
    fn close(&self, py: Python<'_>) -> PyGpkgResult<()> {
        let gpkg = py.detach(|| self.guard().map(|mut guard| guard.take()))?;
        match gpkg {
            Some(gpkg) => py.detach(|| gpkg.close()).py(),
            None => Ok(()),
        }
    }

    fn __enter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    #[pyo3(signature = (*_args))]
    fn __exit__(&self, py: Python<'_>, _args: &Bound<'_, PyTuple>) -> PyGpkgResult<bool> {
        self.close(py)?;
        Ok(false)
    }

    fn __repr__(&self) -> String {
        format!("GeoPackage({:?})", self.path.display().to_string())
    }

    /// The path the file was opened from.
    #[getter(path)]
    fn path_str(&self) -> String {
        self.path.display().to_string()
    }

    /// Whether the file was opened read-only.
    #[getter]
    fn read_only(&self) -> bool {
        self.read_only
    }

    /// Whether the connection is still open.
    #[getter]
    fn is_open(&self) -> PyGpkgResult<bool> {
        Ok(self.guard()?.is_some())
    }

    /// The GeoPackage version the file declares, such as ``"1.4.0"``.
    #[getter]
    fn version(&self, py: Python<'_>) -> PyGpkgResult<String> {
        self.with(py, |g| Ok(g.version().to_string()))
    }

    /// What a lenient open accepted, one message per defect.
    fn open_warnings(&self, py: Python<'_>) -> PyGpkgResult<Vec<String>> {
        self.with(py, |g| {
            Ok(g.open_warnings().iter().map(ToString::to_string).collect())
        })
    }

    /// The rows of ``gpkg_contents``, as dictionaries with the keys
    /// ``table_name``, ``data_type``, ``identifier``, ``srs_id`` and
    /// ``bounds`` (``(min_x, min_y, max_x, max_y)`` or ``None``).
    fn contents<'py>(&self, py: Python<'py>) -> PyGpkgResult<Vec<Bound<'py, PyDict>>> {
        let entries = self.with(py, GeoPackage::contents)?;
        entries
            .into_iter()
            .map(|entry| {
                let dict = PyDict::new(py);
                dict.set_item("table_name", &entry.table_name)?;
                dict.set_item("data_type", contents_type_name(&entry.data_type))?;
                dict.set_item("identifier", &entry.identifier)?;
                dict.set_item("srs_id", entry.srs_id)?;
                let bounds = match (entry.min_x, entry.min_y, entry.max_x, entry.max_y) {
                    (Some(a), Some(b), Some(c), Some(d)) => Some((a, b, c, d)),
                    _ => None,
                };
                dict.set_item("bounds", bounds)?;
                Ok(dict)
            })
            .collect()
    }

    /// The names of the feature layers.
    fn layer_names(&self, py: Python<'_>) -> PyGpkgResult<Vec<String>> {
        self.with(py, |g| {
            Ok(g.layers()?
                .iter()
                .map(|layer| layer.table_name().to_owned())
                .collect())
        })
    }

    /// The names of the attribute tables: non-spatial tables registered in
    /// ``gpkg_contents``.
    fn attribute_table_names(&self, py: Python<'_>) -> PyGpkgResult<Vec<String>> {
        self.with(py, |g| {
            Ok(g.contents()?
                .into_iter()
                .filter(|entry| entry.data_type == ContentsDataType::Attributes)
                .map(|entry| entry.table_name)
                .collect())
        })
    }

    /// The feature layer `name`.
    fn layer(slf: &Bound<'_, Self>, name: &str) -> PyGpkgResult<PyLayer> {
        PyLayer::resolve(slf, name, false)
    }

    /// The attribute table `name`, as a `Layer` with no geometry column.
    fn attributes(slf: &Bound<'_, Self>, name: &str) -> PyGpkgResult<PyLayer> {
        PyLayer::resolve(slf, name, true)
    }

    /// Every feature layer.
    fn layers(slf: &Bound<'_, Self>) -> PyGpkgResult<Vec<PyLayer>> {
        let names = slf.get().layer_names(slf.py())?;
        names
            .iter()
            .map(|name| PyLayer::resolve(slf, name, false))
            .collect()
    }

    /// Creates a feature layer, or an attribute table when `geometry_type` is
    /// ``None``.
    ///
    /// `columns` are ``(name, type)`` or ``(name, type, not_null)`` tuples,
    /// where `type` is a GeoPackage column type such as ``"TEXT"``,
    /// ``"INTEGER"``, ``"DOUBLE"``, ``"DATETIME"`` or ``"TEXT(20)"``. The
    /// primary key and the geometry column are declared by their own
    /// arguments, not in `columns`. `z` and `m` take ``"prohibited"``,
    /// ``"mandatory"`` or ``"optional"``. The SRS must already be in the file;
    /// see :meth:`add_epsg_srs` and :meth:`add_srs`.
    #[pyo3(signature = (
        name, geometry_type = None, srs_id = 0, columns = Vec::new(), *,
        z = None, m = None, spatial_index = true, primary_key = None,
        geometry_column = None,
    ))]
    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors a Python keyword signature"
    )]
    fn create_layer(
        slf: &Bound<'_, Self>,
        name: String,
        geometry_type: Option<&str>,
        srs_id: i32,
        columns: Vec<Bound<'_, PyAny>>,
        z: Option<Bound<'_, PyAny>>,
        m: Option<Bound<'_, PyAny>>,
        spatial_index: bool,
        primary_key: Option<String>,
        geometry_column: Option<String>,
    ) -> PyGpkgResult<PyLayer> {
        let mut builder = TableSchemaBuilder::new(name.clone());
        if let Some(pk) = primary_key {
            builder = builder.primary_key(pk);
        }
        for column in &columns {
            builder = builder.column(column_spec(column)?);
        }
        let is_attributes = geometry_type.is_none();
        if let Some(geometry_type) = geometry_type {
            let mut spec = GeometrySpec::new(parse_geometry_type(geometry_type)?, srs_id);
            if let Some(column) = geometry_column {
                spec = spec.column_name(column);
            }
            if let Some(z) = &z {
                spec = spec.z(parse_zm(z)?);
            }
            if let Some(m) = &m {
                spec = spec.m(parse_zm(m)?);
            }
            builder = builder.geometry(spec).spatial_index(spatial_index);
        }
        slf.get().with(slf.py(), |g| {
            if is_attributes {
                g.create_attributes_table(&builder).map(drop)
            } else {
                g.create_layer(&builder).map(drop)
            }
        })?;
        PyLayer::resolve(slf, &name, is_attributes)
    }

    /// Adds the EPSG registry's definition of `code`. Returns ``False`` if
    /// the file already has a row with that `srs_id`.
    fn add_epsg_srs(&self, py: Python<'_>, code: i32) -> PyGpkgResult<bool> {
        self.with(py, |g| g.add_epsg_srs(code))
    }

    /// Adds an SRS row. Returns ``False`` if the file already has a row with
    /// that `srs_id`, which is then left as it was.
    #[pyo3(signature = (
        srs_id, name, organization, organization_coordsys_id, definition, *,
        description = None, definition_wkt2 = None,
    ))]
    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors a Python keyword signature"
    )]
    fn add_srs(
        &self,
        py: Python<'_>,
        srs_id: i32,
        name: String,
        organization: String,
        organization_coordsys_id: i32,
        definition: String,
        description: Option<String>,
        definition_wkt2: Option<String>,
    ) -> PyGpkgResult<bool> {
        let srs = Srs {
            srs_id,
            name,
            organization,
            organization_coordsys_id,
            definition,
            description,
            definition_wkt2,
            epoch: None,
        };
        self.with(py, |g| g.add_srs(&srs))
    }

    /// The SRS row `srs_id`, or ``None``.
    fn srs(&self, py: Python<'_>, srs_id: i32) -> PyGpkgResult<Option<PySrs>> {
        Ok(self.with(py, |g| g.srs(srs_id))?.map(PySrs))
    }

    /// Every SRS row.
    fn srs_list(&self, py: Python<'_>) -> PyGpkgResult<Vec<PySrs>> {
        Ok(self
            .with(py, GeoPackage::srs_list)?
            .into_iter()
            .map(PySrs)
            .collect())
    }

    /// What this crate can see wrong with the file, most severe first.
    ///
    /// The checks are this crate's, not the OGC test suite's; an empty list
    /// is not a conformance claim.
    fn validate(&self, py: Python<'_>) -> PyGpkgResult<Vec<PyFinding>> {
        let findings = self.with(py, GeoPackage::validate)?;
        Ok(findings
            .iter()
            .map(|finding| {
                let debug = format!("{finding:?}");
                let code = debug
                    .split(|c: char| !c.is_alphanumeric())
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                PyFinding {
                    severity: finding.severity().as_str().to_owned(),
                    code,
                    message: finding.to_string(),
                    table_name: finding.table_name().map(str::to_owned),
                    repair: finding.repair().map(str::to_owned),
                }
            })
            .collect())
    }

    /// The rows of ``gpkg_extensions``, each classified by this crate's
    /// support for it.
    fn extensions(&self, py: Python<'_>) -> PyGpkgResult<Vec<PyExtension>> {
        let rows = self.with(py, GeoPackage::extensions)?;
        Ok(rows.iter().map(PyExtension::from_row).collect())
    }

    /// The names of the tiled gridded coverages.
    fn coverage_names(&self, py: Python<'_>) -> PyGpkgResult<Vec<String>> {
        self.with(py, |g| {
            Ok(g.coverages()?
                .iter()
                .map(|c| c.table_name().to_owned())
                .collect())
        })
    }

    /// The tiled gridded coverage `name`.
    fn coverage(slf: &Bound<'_, Self>, name: &str) -> PyGpkgResult<PyCoverage> {
        PyCoverage::resolve(slf, name)
    }

    /// Creates a tiled gridded coverage.
    ///
    /// `extent` is ``(min_x, min_y, max_x, max_y)`` in `srs_id`; `matrices`
    /// are ``(zoom_level, matrix_width, matrix_height, tile_width,
    /// tile_height)`` tuples, whose pixel sizes follow from the extent.
    /// `datatype` is ``"integer"`` or ``"float"``.
    #[pyo3(signature = (
        name, srs_id, extent, matrices, *, datatype = "integer", scale = 1.0,
        offset = 0.0, data_null = None, uom = None, grid_cell_encoding = None,
    ))]
    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors a Python keyword signature"
    )]
    fn create_coverage(
        slf: &Bound<'_, Self>,
        name: String,
        srs_id: i32,
        extent: (f64, f64, f64, f64),
        matrices: Vec<(i64, i64, i64, i64, i64)>,
        datatype: &str,
        scale: f64,
        offset: f64,
        data_null: Option<f64>,
        uom: Option<String>,
        grid_cell_encoding: Option<String>,
    ) -> PyGpkgResult<PyCoverage> {
        use geopackage::core::coverage::CoverageDatatype;
        use geopackage::core::tiles::{TileMatrix, TileMatrixSet};

        let datatype = CoverageDatatype::parse(datatype).ok_or_else(|| {
            PyValueError::new_err(format!("datatype {datatype:?} is not integer or float"))
        })?;
        let (min_x, min_y, max_x, max_y) = extent;
        let set = TileMatrixSet::new(srs_id, min_x, min_y, max_x, max_y);
        let mut builder = geopackage::CoverageBuilder::new(name.clone(), set, datatype);
        for (zoom, mw, mh, tw, th) in matrices {
            let pixel_x = (max_x - min_x) / (mw * tw) as f64;
            let pixel_y = (max_y - min_y) / (mh * th) as f64;
            builder = builder.matrix(TileMatrix::new(zoom, mw, mh, tw, th, pixel_x, pixel_y));
        }
        if (scale, offset) != (1.0, 0.0) {
            builder = builder.scale(scale, offset);
        }
        if let Some(null) = data_null {
            builder = builder.data_null(null);
        }
        if let Some(uom) = uom {
            builder = builder.uom(uom);
        }
        if let Some(encoding) = grid_cell_encoding {
            builder = builder.grid_cell_encoding(encoding);
        }
        slf.get()
            .with(slf.py(), |g| g.create_coverage(&builder).map(drop))?;
        PyCoverage::resolve(slf, &name)
    }

    /// Runs `PRAGMA integrity_check` and returns its messages; ``["ok"]``
    /// for a sound file.
    fn integrity_check(&self, py: Python<'_>) -> PyGpkgResult<Vec<String>> {
        self.with(py, |g| {
            let mut stmt = g
                .connection()
                .prepare("PRAGMA integrity_check")
                .map_err(geopackage::Error::from)?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(geopackage::Error::from)?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(geopackage::Error::from)
        })
    }
}

/// A row of ``gpkg_spatial_ref_sys``.
#[pyclass(name = "Srs", module = "geopackage_rs", frozen)]
pub struct PySrs(pub(crate) Srs);

#[pymethods]
impl PySrs {
    #[getter]
    fn srs_id(&self) -> i32 {
        self.0.srs_id
    }
    #[getter]
    fn name(&self) -> &str {
        &self.0.name
    }
    #[getter]
    fn organization(&self) -> &str {
        &self.0.organization
    }
    #[getter]
    fn organization_coordsys_id(&self) -> i32 {
        self.0.organization_coordsys_id
    }
    /// The WKT1 definition, or ``"undefined"``.
    #[getter]
    fn definition(&self) -> &str {
        &self.0.definition
    }
    /// The WKT2 definition, when the file has the ``gpkg_crs_wkt_1_1``
    /// column.
    #[getter]
    fn definition_wkt2(&self) -> Option<&str> {
        self.0.definition_wkt2.as_deref()
    }
    #[getter]
    fn description(&self) -> Option<&str> {
        self.0.description.as_deref()
    }
    #[getter]
    fn epoch(&self) -> Option<f64> {
        self.0.epoch
    }
    /// ``"EPSG:4326"`` style, or ``None`` for an SRS no authority defines.
    #[getter]
    fn authid(&self) -> Option<String> {
        (self.0.organization_coordsys_id > 0 && !self.0.organization.eq_ignore_ascii_case("NONE"))
            .then(|| {
                format!(
                    "{}:{}",
                    self.0.organization.to_ascii_uppercase(),
                    self.0.organization_coordsys_id
                )
            })
    }
    fn __repr__(&self) -> String {
        format!("Srs({}, {:?})", self.0.srs_id, self.0.name)
    }
}

/// One thing `GeoPackage.validate` found.
#[pyclass(name = "Finding", module = "geopackage_rs", frozen, get_all)]
pub struct PyFinding {
    /// ``"error"``, ``"warning"`` or ``"advisory"``.
    severity: String,
    /// The kind of finding, such as ``"SpatialIndexOutOfStep"``; stable
    /// enough to filter on.
    code: String,
    message: String,
    table_name: Option<String>,
    /// The library operation that puts it right, if there is one.
    repair: Option<String>,
}

#[pymethods]
impl PyFinding {
    fn __repr__(&self) -> String {
        format!("Finding({}: {})", self.severity, self.message)
    }
}

/// A row of ``gpkg_extensions``.
#[pyclass(name = "Extension", module = "geopackage_rs", frozen, get_all)]
pub struct PyExtension {
    name: String,
    table_name: Option<String>,
    column_name: Option<String>,
    definition: String,
    /// ``"read-write"`` or ``"write-only"``.
    scope: String,
    /// ``"implemented"``, ``"known"``, ``"removed"`` or ``"unrecognised"``.
    support: String,
}

impl PyExtension {
    pub(crate) fn from_row(row: &geopackage::ExtensionRow) -> Self {
        Self {
            name: row.name.clone(),
            table_name: row.table_name.clone(),
            column_name: row.column_name.clone(),
            definition: row.definition.clone(),
            scope: row.scope.as_str().to_owned(),
            support: row.support().as_str().to_owned(),
        }
    }
}

#[pymethods]
impl PyExtension {
    fn __repr__(&self) -> String {
        format!("Extension({:?}, {})", self.name, self.support)
    }
}

/// Raised when a name is not in the file, so that Python callers can catch
/// `KeyError` as they would for a dictionary.
pub(crate) fn no_such(kind: &str, name: &str) -> PyErr {
    PyKeyError::new_err(format!("no {kind} named {name:?}"))
}

/// Maps a crate error to `KeyError` when it says the table is missing.
pub(crate) fn lookup_error(kind: &str, name: &str, error: geopackage::Error) -> PyErr {
    match error {
        geopackage::Error::NoSuchTable { .. } | geopackage::Error::NoSuchLayer { .. } => {
            no_such(kind, name)
        }
        other => to_py(other),
    }
}
