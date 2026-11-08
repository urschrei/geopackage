//! Python bindings for the `geopackage` crate.
//!
//! The native module is `geopackage_rs._geopackage_rs`; the `geopackage_rs`
//! package in `python/` re-exports it with type stubs. The surface is
//! deliberately the one a QGIS plugin needs: open and create, layer schemas,
//! streaming feature reads, batched writes, validation, spatial index
//! maintenance, the extension catalogue, SRS definitions and tiled gridded
//! coverages with sample decoding.
//!
//! # Handles and lifetimes
//!
//! The Rust API borrows: a `Layer<'a>` borrows its `GeoPackage`, and a cursor
//! borrows its layer. Python objects cannot carry lifetimes, so the Python
//! `Layer` and `Coverage` hold a reference to their `GeoPackage` and a table
//! name, and resolve the Rust handle per call. The connection is behind a
//! mutex, so one `GeoPackage` object can be shared between threads, and
//! calls on it serialise.
//!
//! Feature reads are the exception, because a Python iterator outlives any
//! one call. Each read runs on its own thread with its own read-only
//! connection, as `gpkg copy` does, and hands rows over in chunks; see
//! the `features` module.

use pyo3::prelude::*;

mod convert;
mod coverage;
mod features;
mod gpkg;
mod layer;

pyo3::create_exception!(
    _geopackage_rs,
    GeoPackageError,
    pyo3::exceptions::PyException,
    "An error reported by the geopackage crate."
);

/// Converts a crate error into the Python exception.
pub(crate) fn to_py(error: impl std::fmt::Display) -> PyErr {
    GeoPackageError::new_err(error.to_string())
}

/// The result type of the binding code.
pub(crate) type PyGpkgResult<T> = Result<T, PyErr>;

/// Extension trait: `.py()` on a crate result maps its error to Python.
pub(crate) trait IntoPyResult<T> {
    fn py(self) -> PyGpkgResult<T>;
}

impl<T, E: std::fmt::Display> IntoPyResult<T> for Result<T, E> {
    fn py(self) -> PyGpkgResult<T> {
        self.map_err(to_py)
    }
}

#[pymodule]
fn _geopackage_rs(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("GeoPackageError", m.py().get_type::<GeoPackageError>())?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add_class::<gpkg::PyGeoPackage>()?;
    m.add_class::<gpkg::PySrs>()?;
    m.add_class::<gpkg::PyFinding>()?;
    m.add_class::<gpkg::PyExtension>()?;
    m.add_class::<layer::PyLayer>()?;
    m.add_class::<layer::PyField>()?;
    m.add_class::<layer::PyGeometryColumn>()?;
    m.add_class::<features::PyFeatureIterator>()?;
    m.add_class::<coverage::PyCoverage>()?;
    m.add_class::<coverage::PyTileMatrix>()?;
    Ok(())
}
