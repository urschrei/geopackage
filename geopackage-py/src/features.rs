//! Streaming feature reads.
//!
//! A Python iterator outlives the call that made it, and the Rust cursor
//! borrows its layer, which borrows its connection. Rather than tie the three
//! into one self-referential object, each read runs on a thread of its own,
//! with its own read-only connection, the way `gpkg copy` reads. Rows cross to
//! Python in owned chunks over a bounded channel, so memory stays bounded and
//! the read overlaps with whatever the caller does with each chunk (in QGIS,
//! building `QgsFeature`s).
//!
//! Dropping or closing the iterator drops the receiver; the reader's next
//! send fails and the thread stops.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use geopackage::core::gpb;
use geopackage::{BoundingBox, GeoPackage, Value, ValueRef};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyList, PyTuple};

use crate::convert::{DateTypes, quote_ident, value_to_py};
use crate::{GeoPackageError, PyGpkgResult};

/// Chunks that can wait in the channel before the reader blocks.
const CHANNEL_DEPTH: usize = 4;

/// What a read selects, owned so that it can move to the reader thread.
pub(crate) struct FeatureQuery {
    pub(crate) path: PathBuf,
    pub(crate) table: String,
    pub(crate) attributes: bool,
    pub(crate) bbox: Option<BoundingBox>,
    pub(crate) where_clause: Option<String>,
    pub(crate) params: Vec<Value>,
    pub(crate) columns: Option<Vec<String>>,
    pub(crate) geometry: bool,
    pub(crate) geometry_column: Option<String>,
    pub(crate) chunk_size: usize,
}

/// One row, owned.
pub(crate) struct Row {
    fid: i64,
    wkb: Option<Vec<u8>>,
    values: Vec<Value>,
}

enum Message {
    Rows(Vec<Row>),
    Error(String),
}

/// Builds the Python tuple for one row.
pub(crate) fn row_tuple<'py>(
    py: Python<'py>,
    fid: i64,
    wkb: Option<&[u8]>,
    values: &[Value],
) -> PyGpkgResult<Bound<'py, PyTuple>> {
    let types = DateTypes::new(py)?;
    row_tuple_with(py, &types, fid, wkb, values)
}

fn row_tuple_with<'py>(
    py: Python<'py>,
    types: &DateTypes<'py>,
    fid: i64,
    wkb: Option<&[u8]>,
    values: &[Value],
) -> PyGpkgResult<Bound<'py, PyTuple>> {
    let wkb = match wkb {
        Some(bytes) => PyBytes::new(py, bytes).into_any(),
        None => py.None().into_bound(py),
    };
    let values = values
        .iter()
        .map(|v| value_to_py(py, types, v))
        .collect::<PyResult<Vec<_>>>()?;
    PyTuple::new(
        py,
        [
            fid.into_pyobject(py)?.into_any(),
            wkb,
            PyList::new(py, values)?.into_any(),
        ],
    )
}

/// Runs the read, sending chunks to `tx`. Returns early, without error, when
/// the receiver has gone.
fn read(query: &FeatureQuery, tx: &SyncSender<Message>) -> geopackage::Result<()> {
    let gpkg = GeoPackage::open_read_only_lenient(&query.path)?;
    let mut layer = if query.attributes {
        gpkg.attributes(&query.table)?
    } else {
        gpkg.layer(&query.table)?
    };
    let has_index = query.bbox.is_some() && layer.has_spatial_index()?;
    let pk = layer.primary_key_column().map(str::to_owned);
    match &query.columns {
        Some(names) => {
            let mut names: Vec<&str> = names.iter().map(String::as_str).collect();
            if query.geometry
                && let Some(geom) = &query.geometry_column
            {
                names.push(geom);
            }
            layer = layer.with_columns(&names)?;
        }
        None if !query.geometry => layer = layer.without_geometry(),
        None => {}
    }

    let params: Vec<ValueRef<'_>> = query.params.iter().map(ValueRef::from).collect();
    let mut cursor = match (&query.bbox, &query.where_clause) {
        (None, None) => layer.cursor()?,
        (Some(bbox), None) => layer.cursor_in(*bbox)?,
        (None, Some(clause)) => layer.cursor_select(clause, &params)?,
        (Some(bbox), Some(clause)) => {
            // No cursor takes both, so compose them. `ST_EnvIntersects` is
            // the exact test `features_in` makes on a full scan; the RTree
            // prefilter, when there is an index, is the conservative superset
            // its index path starts from. Both are numbered after the
            // caller's own parameters.
            let n = params.len();
            let geom = quote_ident(query.geometry_column.as_deref().unwrap_or_default());
            let mut sql = format!(
                "({clause}) AND ST_EnvIntersects({geom}, ?{}, ?{}, ?{}, ?{})",
                n + 1,
                n + 2,
                n + 3,
                n + 4
            );
            if has_index && let Some(pk) = &pk {
                let rtree = quote_ident(&format!(
                    "rtree_{}_{}",
                    query.table,
                    query.geometry_column.as_deref().unwrap_or_default()
                ));
                sql.push_str(&format!(
                    " AND {} IN (SELECT id FROM {rtree} WHERE minx <= ?{} AND maxx >= ?{} \
                     AND miny <= ?{} AND maxy >= ?{})",
                    quote_ident(pk),
                    n + 3,
                    n + 1,
                    n + 4,
                    n + 2
                ));
            }
            let mut all = params.clone();
            all.extend([
                ValueRef::Float(bbox.min_x),
                ValueRef::Float(bbox.min_y),
                ValueRef::Float(bbox.max_x),
                ValueRef::Float(bbox.max_y),
            ]);
            layer.cursor_select(&sql, &all)?
        }
    };

    let mut chunk = Vec::with_capacity(query.chunk_size);
    for feature in cursor.features()?.by_ref() {
        let feature = feature?;
        let wkb = match feature.geometry_bytes() {
            Some(blob) => {
                let offset =
                    gpb::body_offset(blob).map_err(|e| geopackage::Error::Core(e.into()))?;
                Some(blob.get(offset..).unwrap_or_default().to_vec())
            }
            None => None,
        };
        chunk.push(Row {
            fid: feature.fid(),
            wkb,
            values: feature.values().map(|v| v.to_value()).collect(),
        });
        if chunk.len() >= query.chunk_size {
            let full = std::mem::replace(&mut chunk, Vec::with_capacity(query.chunk_size));
            if tx.send(Message::Rows(full)).is_err() {
                return Ok(());
            }
        }
    }
    if !chunk.is_empty() && tx.send(Message::Rows(chunk)).is_err() {
        return Ok(());
    }
    Ok(())
}

/// An iterator of ``(fid, wkb, values)`` tuples, from `Layer.features`.
#[pyclass(name = "FeatureIterator", module = "geopackage_rs", frozen)]
pub struct PyFeatureIterator {
    rx: Mutex<Option<Receiver<Message>>>,
    buffer: Mutex<VecDeque<Row>>,
    columns: Vec<String>,
}

impl PyFeatureIterator {
    pub(crate) fn start(query: FeatureQuery, columns: Vec<String>) -> PyGpkgResult<Self> {
        let (tx, rx) = sync_channel(CHANNEL_DEPTH);
        let spawned = std::thread::Builder::new()
            .name(format!("gpkg-read-{}", query.table))
            .spawn(move || {
                if let Err(error) = read(&query, &tx) {
                    // The receiver may have gone; then nobody is waiting for
                    // the error either.
                    drop(tx.send(Message::Error(error.to_string())));
                }
            });
        spawned
            .map(drop)
            .map_err(|e| GeoPackageError::new_err(format!("cannot start a reader: {e}")))?;
        Ok(Self {
            rx: Mutex::new(Some(rx)),
            buffer: Mutex::new(VecDeque::new()),
            columns,
        })
    }

    /// Waits for the next chunk, with the GIL released. `None` when the read
    /// is over.
    fn receive(&self, py: Python<'_>) -> PyGpkgResult<Option<Vec<Row>>> {
        let message = py.detach(|| -> PyGpkgResult<Option<Message>> {
            let guard = self
                .rx
                .lock()
                .map_err(|_poisoned| GeoPackageError::new_err("the reader's lock is poisoned"))?;
            Ok(match guard.as_ref() {
                Some(rx) => rx.recv().ok(),
                None => None,
            })
        })?;
        match message {
            Some(Message::Rows(rows)) => Ok(Some(rows)),
            Some(Message::Error(text)) => {
                self.finish();
                Err(GeoPackageError::new_err(text))
            }
            None => {
                self.finish();
                Ok(None)
            }
        }
    }

    fn finish(&self) {
        if let Ok(mut guard) = self.rx.lock() {
            *guard = None;
        }
    }

    fn buffer(&self) -> PyGpkgResult<std::sync::MutexGuard<'_, VecDeque<Row>>> {
        self.buffer
            .lock()
            .map_err(|_poisoned| GeoPackageError::new_err("the reader's lock is poisoned"))
    }
}

#[pymethods]
impl PyFeatureIterator {
    /// The names of the values in each row, in order.
    #[getter]
    fn columns(&self) -> Vec<String> {
        self.columns.clone()
    }

    fn __iter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __next__<'py>(&self, py: Python<'py>) -> PyGpkgResult<Option<Bound<'py, PyTuple>>> {
        loop {
            if let Some(row) = self.buffer()?.pop_front() {
                let types = DateTypes::new(py)?;
                return row_tuple_with(py, &types, row.fid, row.wkb.as_deref(), &row.values)
                    .map(Some);
            }
            match self.receive(py)? {
                Some(rows) => self.buffer()?.extend(rows),
                None => return Ok(None),
            }
        }
    }

    /// Returns up to the next chunk of rows as a list, or an empty list when
    /// the read is over. Cheaper than ``next`` per row for large reads.
    fn next_chunk<'py>(&self, py: Python<'py>) -> PyGpkgResult<Vec<Bound<'py, PyTuple>>> {
        let rows: Vec<Row> = {
            let mut buffer = self.buffer()?;
            buffer.drain(..).collect()
        };
        let rows = if rows.is_empty() {
            self.receive(py)?.unwrap_or_default()
        } else {
            rows
        };
        let types = DateTypes::new(py)?;
        rows.iter()
            .map(|row| row_tuple_with(py, &types, row.fid, row.wkb.as_deref(), &row.values))
            .collect()
    }

    /// Stops the read and releases its connection.
    fn close(&self) -> PyGpkgResult<()> {
        self.finish();
        self.buffer()?.clear();
        Ok(())
    }
}
