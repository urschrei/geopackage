//! The `Coverage` class: a tiled gridded coverage, with sample decoding.

use geopackage::core::tiles::{TileCoord, TileMatrix, TileMatrixSet};
use geopackage::{Coverage, CoverageAncillary, GeoPackage, Interpolation, TileAncillary};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::PyGpkgResult;
use crate::gpkg::{PyGeoPackage, lookup_error};

/// One zoom level of a coverage's tile matrix.
#[pyclass(name = "TileMatrix", module = "geopackage_rs", frozen, get_all)]
#[derive(Clone)]
pub struct PyTileMatrix {
    zoom_level: i64,
    matrix_width: i64,
    matrix_height: i64,
    tile_width: i64,
    tile_height: i64,
    pixel_x_size: f64,
    pixel_y_size: f64,
}

impl From<&TileMatrix> for PyTileMatrix {
    fn from(m: &TileMatrix) -> Self {
        Self {
            zoom_level: m.zoom_level,
            matrix_width: m.matrix_width,
            matrix_height: m.matrix_height,
            tile_width: m.tile_width,
            tile_height: m.tile_height,
            pixel_x_size: m.pixel_x_size,
            pixel_y_size: m.pixel_y_size,
        }
    }
}

#[pymethods]
impl PyTileMatrix {
    fn __repr__(&self) -> String {
        format!(
            "TileMatrix(zoom={}, {}x{} tiles of {}x{} px)",
            self.zoom_level,
            self.matrix_width,
            self.matrix_height,
            self.tile_width,
            self.tile_height
        )
    }
}

/// A tiled gridded coverage (OGC 17-066r2): elevation or another measured
/// grid.
///
/// Values are samples after the tile's scale and offset; ``None`` stands for
/// no value (``data_null``, NaN, or a tile the coverage does not store).
#[pyclass(name = "Coverage", module = "geopackage_rs", frozen)]
pub struct PyCoverage {
    gpkg: Py<PyGeoPackage>,
    name: String,
    ancillary: CoverageAncillary,
    set: TileMatrixSet,
    matrices: Vec<TileMatrix>,
}

fn parse_interpolation(name: &str) -> PyGpkgResult<Interpolation> {
    match name.to_ascii_lowercase().as_str() {
        "nearest" => Ok(Interpolation::Nearest),
        "bilinear" => Ok(Interpolation::Bilinear),
        _ => Err(PyValueError::new_err(format!(
            "interpolation {name:?} is not nearest or bilinear"
        ))),
    }
}

/// The ancillary row a tile is read with: its own, or the defaults other
/// readers use when it has none.
fn tile_ancillary(coverage: &Coverage<'_>, coord: TileCoord) -> geopackage::Result<TileAncillary> {
    Ok(coverage
        .tile_ancillary(coord)?
        .unwrap_or_else(TileAncillary::defaults))
}

/// One tile's values: width, height, and values in row-major order.
type TileValues = (usize, usize, Vec<Option<f64>>);

/// One tile's recorded and calculated statistics.
struct TileReport {
    coord: TileCoord,
    recorded: TileAncillary,
    calculated: Option<geopackage::SampleStatistics>,
    error: Option<String>,
}

impl PyCoverage {
    pub(crate) fn resolve(gpkg: &Bound<'_, PyGeoPackage>, name: &str) -> PyGpkgResult<Self> {
        let found = gpkg.get().with(gpkg.py(), |g| match g.coverage(name) {
            Ok(c) => Ok(Ok((
                c.ancillary().clone(),
                *c.matrix_set(),
                c.matrices().to_vec(),
            ))),
            Err(e) => Ok(Err(e)),
        })?;
        let (ancillary, set, matrices) = found.map_err(|e| lookup_error("coverage", name, e))?;
        Ok(Self {
            gpkg: gpkg.clone().unbind(),
            name: name.to_owned(),
            ancillary,
            set,
            matrices,
        })
    }

    fn with<R: Send>(
        &self,
        py: Python<'_>,
        f: impl FnOnce(&Coverage<'_>) -> geopackage::Result<R> + Send,
    ) -> PyGpkgResult<R> {
        let name = self.name.as_str();
        self.gpkg
            .get()
            .with(py, |g: &GeoPackage| f(&g.coverage(name)?))
    }

    /// The zoom level to read: the one asked for, or the most detailed.
    fn zoom(&self, zoom: Option<i64>) -> PyGpkgResult<i64> {
        match zoom {
            Some(z) if self.matrices.iter().any(|m| m.zoom_level == z) => Ok(z),
            Some(z) => Err(PyValueError::new_err(format!(
                "coverage {:?} has no zoom level {z}",
                self.name
            ))),
            None => self
                .matrices
                .iter()
                .map(|m| m.zoom_level)
                .max()
                .ok_or_else(|| {
                    PyValueError::new_err(format!("coverage {:?} has no zoom levels", self.name))
                }),
        }
    }

    /// Visits every stored tile at `zoom`, or at every level.
    fn each_tile<R: Send>(
        &self,
        py: Python<'_>,
        zoom: Option<i64>,
        mut visit: impl FnMut(&Coverage<'_>, TileCoord, &[u8]) -> geopackage::Result<R> + Send,
    ) -> PyGpkgResult<Vec<R>> {
        self.with(py, |coverage| {
            let mut cursor = match zoom {
                Some(z) => coverage.cursor_at(z)?,
                None => coverage.cursor()?,
            };
            let mut stream = cursor.tiles()?;
            let mut out = Vec::new();
            while let Some(tile) = stream.next()? {
                out.push(visit(coverage, tile.coord(), tile.data())?);
            }
            Ok(out)
        })
    }
}

#[pymethods]
impl PyCoverage {
    fn __repr__(&self) -> String {
        format!("Coverage({:?})", self.name)
    }

    #[getter]
    fn name(&self) -> &str {
        &self.name
    }

    /// ``"integer"`` or ``"float"``.
    #[getter]
    fn datatype(&self) -> &str {
        &self.ancillary.datatype
    }

    #[getter]
    fn scale(&self) -> f64 {
        self.ancillary.scale
    }

    #[getter]
    fn offset(&self) -> f64 {
        self.ancillary.offset
    }

    #[getter]
    fn precision(&self) -> Option<f64> {
        self.ancillary.precision
    }

    /// The stored sample that means no value.
    #[getter]
    fn data_null(&self) -> Option<f64> {
        self.ancillary.data_null
    }

    /// The unit of measure of the values.
    #[getter]
    fn uom(&self) -> Option<&str> {
        self.ancillary.uom.as_deref()
    }

    #[getter]
    fn field_name(&self) -> Option<&str> {
        self.ancillary.field_name.as_deref()
    }

    #[getter]
    fn quantity_definition(&self) -> Option<&str> {
        self.ancillary.quantity_definition.as_deref()
    }

    /// ``"grid-value-is-center"``, ``"grid-value-is-area"`` or
    /// ``"grid-value-is-corner"``, or ``None`` for the default (centre).
    #[getter]
    fn grid_cell_encoding(&self) -> Option<&str> {
        self.ancillary.grid_cell_encoding.as_deref()
    }

    #[getter]
    fn srs_id(&self) -> i32 {
        self.set.srs_id
    }

    /// The tile matrix set's extent, ``(min_x, min_y, max_x, max_y)``.
    #[getter]
    fn extent(&self) -> (f64, f64, f64, f64) {
        (
            self.set.min_x,
            self.set.min_y,
            self.set.max_x,
            self.set.max_y,
        )
    }

    #[getter]
    fn matrices(&self) -> Vec<PyTileMatrix> {
        self.matrices.iter().map(PyTileMatrix::from).collect()
    }

    #[getter]
    fn zoom_levels(&self) -> Vec<i64> {
        self.matrices.iter().map(|m| m.zoom_level).collect()
    }

    /// The number of stored tiles, at one zoom level or in all.
    #[pyo3(signature = (zoom = None))]
    fn tile_count(&self, py: Python<'_>, zoom: Option<i64>) -> PyGpkgResult<i64> {
        self.with(py, |c| match zoom {
            Some(z) => c.tile_count_at(z),
            None => c.tile_count(),
        })
    }

    /// The bounds of one tile, ``(min_x, min_y, max_x, max_y)``.
    fn tile_bounds(&self, zoom: i64, column: i64, row: i64) -> PyGpkgResult<(f64, f64, f64, f64)> {
        let zoom = self.zoom(Some(zoom))?;
        let matrix = self
            .matrices
            .iter()
            .find(|m| m.zoom_level == zoom)
            .ok_or_else(|| PyValueError::new_err("no such zoom level"))?;
        let b = self.set.tile_bounds(matrix, column, row);
        Ok((b.min_x, b.min_y, b.max_x, b.max_y))
    }

    /// Checks the coverage's structure and every payload header against the
    /// extension. Raises `GeoPackageError` on the first problem.
    fn validate(&self, py: Python<'_>) -> PyGpkgResult<()> {
        self.with(py, |c| c.validate())
    }

    /// Values at positions in the coverage's SRS, as a list parallel to
    /// `points` (``(x, y)`` pairs).
    ///
    /// `zoom` defaults to the most detailed level. `interpolation` is
    /// ``"nearest"`` or ``"bilinear"``. Tiles are decoded once and kept for
    /// neighbouring positions, so sort points spatially for the best speed.
    #[pyo3(signature = (points, zoom = None, interpolation = "nearest"))]
    fn values_at(
        &self,
        py: Python<'_>,
        points: Vec<(f64, f64)>,
        zoom: Option<i64>,
        interpolation: &str,
    ) -> PyGpkgResult<Vec<Option<f64>>> {
        let zoom = self.zoom(zoom)?;
        let interpolation = parse_interpolation(interpolation)?;
        self.with(py, |coverage| {
            let mut sampler = coverage.sampler(zoom)?;
            points
                .iter()
                .map(|&(x, y)| sampler.value_at(x, y, interpolation))
                .collect()
        })
    }

    /// The stored bytes of one tile, or ``None``.
    fn tile(
        &self,
        py: Python<'_>,
        zoom: i64,
        column: i64,
        row: i64,
    ) -> PyGpkgResult<Option<Vec<u8>>> {
        self.with(py, |c| c.get_tile(TileCoord::new(zoom, column, row)))
    }

    /// One tile's values, as ``(width, height, values)`` with `values` in
    /// row-major order from the top-left, or ``None`` if the tile is not
    /// stored.
    fn samples(
        &self,
        py: Python<'_>,
        zoom: i64,
        column: i64,
        row: i64,
    ) -> PyGpkgResult<Option<TileValues>> {
        let ancillary = self.ancillary.clone();
        self.with(py, move |coverage| {
            let coord = TileCoord::new(zoom, column, row);
            let Some(samples) = coverage.samples(coord)? else {
                return Ok(None);
            };
            let tile = tile_ancillary(coverage, coord)?;
            let (w, h) = (samples.width(), samples.height());
            let mut values = Vec::with_capacity(w * h);
            for r in 0..h {
                for c in 0..w {
                    values.push(samples.value(c, r, &ancillary, &tile));
                }
            }
            Ok(Some((w, h, values)))
        })
    }

    /// Recorded and calculated statistics for every stored tile, at one zoom
    /// level or all of them.
    ///
    /// Each entry is a dictionary with ``zoom``, ``column``, ``row``, the
    /// recorded ``scale``, ``offset``, ``min``, ``max``, ``mean`` and
    /// ``std_dev`` from ``gpkg_2d_gridded_tile_ancillary`` (``None`` where not
    /// recorded), the ``calculated`` statistics as a dictionary (``min``,
    /// ``max``, ``mean``, ``std_dev``, ``count``) or ``None`` for a tile with
    /// no values, and ``error`` for a tile that cannot be decoded. The
    /// calculated statistics are values, after scale and offset; the recorded
    /// ones are whatever the writer of the file put there.
    #[pyo3(signature = (zoom = None))]
    fn tile_statistics<'py>(
        &self,
        py: Python<'py>,
        zoom: Option<i64>,
    ) -> PyGpkgResult<Vec<Bound<'py, PyDict>>> {
        let reports = self.each_tile(py, zoom, |coverage, coord, data| {
            let recorded = tile_ancillary(coverage, coord)?;
            Ok(match coverage.decode(data) {
                Ok(samples) => TileReport {
                    coord,
                    calculated: coverage.statistics(&samples, &recorded),
                    recorded,
                    error: None,
                },
                Err(error) => TileReport {
                    coord,
                    recorded,
                    calculated: None,
                    error: Some(error.to_string()),
                },
            })
        })?;
        reports
            .iter()
            .map(|report| {
                let d = PyDict::new(py);
                d.set_item("zoom", report.coord.zoom_level)?;
                d.set_item("column", report.coord.column)?;
                d.set_item("row", report.coord.row)?;
                d.set_item("scale", report.recorded.scale)?;
                d.set_item("offset", report.recorded.offset)?;
                d.set_item("min", report.recorded.min)?;
                d.set_item("max", report.recorded.max)?;
                d.set_item("mean", report.recorded.mean)?;
                d.set_item("std_dev", report.recorded.std_dev)?;
                match &report.calculated {
                    Some(s) => {
                        let c = PyDict::new(py);
                        c.set_item("min", s.min)?;
                        c.set_item("max", s.max)?;
                        c.set_item("mean", s.mean)?;
                        c.set_item("std_dev", s.std_dev)?;
                        c.set_item("count", s.count)?;
                        d.set_item("calculated", c)?;
                    }
                    None => d.set_item("calculated", py.None())?,
                }
                d.set_item("error", &report.error)?;
                Ok(d)
            })
            .collect()
    }

    /// Decodes every stored tile and checks Requirement 21 (no NaN or
    /// infinite samples), which a header read cannot. Returns ``(zoom,
    /// column, row, message)`` for each tile that fails or cannot be
    /// decoded; an empty list means every tile passed.
    #[pyo3(signature = (zoom = None))]
    fn check_samples(
        &self,
        py: Python<'_>,
        zoom: Option<i64>,
    ) -> PyGpkgResult<Vec<(i64, i64, i64, String)>> {
        let found = self.each_tile(py, zoom, |coverage, coord, data| {
            let problem = match coverage.decode(data) {
                Ok(samples) => coverage.check_samples(&samples).err(),
                Err(error) => Some(error),
            };
            Ok(problem.map(|e| (coord.zoom_level, coord.column, coord.row, e.to_string())))
        })?;
        Ok(found.into_iter().flatten().collect())
    }

    /// Writes one tile's payload: a 16-bit greyscale PNG for an ``integer``
    /// coverage, or a 32-bit float TIFF, as the extension requires.
    ///
    /// With ``statistics=True`` the tile's ancillary row gets its min, max,
    /// mean and standard deviation, calculated from the samples.
    #[pyo3(signature = (zoom, column, row, data, *, statistics = true))]
    fn put_tile(
        &self,
        py: Python<'_>,
        zoom: i64,
        column: i64,
        row: i64,
        data: Vec<u8>,
        statistics: bool,
    ) -> PyGpkgResult<()> {
        self.with(py, |coverage| {
            let coord = TileCoord::new(zoom, column, row);
            let mut writer = coverage.writer()?;
            if statistics {
                writer.put_with_statistics(coord, &data, &TileAncillary::defaults())?;
            } else {
                writer.put(coord, &data)?;
            }
            writer.commit()
        })
    }
}
