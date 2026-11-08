//! A native QGIS vector data provider over the `geopackage` crate.
//!
//! QGIS loads providers as C++ subclasses of `QgsVectorDataProvider`,
//! `QgsAbstractFeatureSource` and `QgsAbstractFeatureIterator`, and Rust
//! cannot subclass a C++ class. So the work is here, in Rust, and
//! `provider.cpp` is a shim: classes that inherit QGIS's types and forward to
//! the functions below through a `cxx` bridge.
//!
//! # The read path
//!
//! An iterator's read runs on a thread of its own, through its own read-only
//! connection, using the crate's Arrow read. That read applies the bounding
//! box (through the RTree), the `WHERE` clause and the column projection, and
//! on a full scan reads on up to four threads. Batches cross a bounded
//! channel to the iterator, whose C++ side reads values by field index. There
//! is no per-row allocation on this side: the C++ shim builds each
//! `QgsFeature` straight from the Arrow arrays, which is the loop OGR's
//! provider runs too.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, sync_channel};
use std::sync::{Arc, Mutex};

use arrow_array::cast::AsArray;
use arrow_array::types::{Date32Type, Float64Type, Int64Type, TimestampMicrosecondType};
use arrow_array::{Array, ArrayRef, RecordBatch};
use arrow_schema::{DataType, TimeUnit};
use geopackage::arrow::ArrowReadOptions;
use geopackage::core::types::ZmFlag;
use geopackage::{BoundingBox, GeoPackage, Layer};

#[cxx::bridge(namespace = "gpkgrs")]
mod ffi {
    /// One value column of a layer.
    struct FieldInfo {
        name: String,
        /// The value kind its values have; see `Kind` in `provider.h`.
        kind: u8,
        declared_type: String,
        length: i32,
    }

    /// What the provider needs to know about its layer.
    struct LayerInfo {
        table: String,
        has_geometry: bool,
        /// QGIS's spelling, such as `MultiPolygonZ`, for
        /// `QgsWkbTypes::parseType`.
        wkb_type: String,
        srs_authid: String,
        srs_wkt: String,
        fields: Vec<FieldInfo>,
    }

    extern "Rust" {
        type RsLayer;
        type RsCursor;

        fn open_layer(path: &str, layer: &str) -> Result<Box<RsLayer>>;
        fn info(self: &RsLayer) -> LayerInfo;
        fn count(self: &RsLayer, subset: &str) -> Result<i64>;
        /// Writes `[min_x, min_y, max_x, max_y]` and returns `true`, or
        /// returns `false` for a layer with no recorded or measurable extent.
        fn extent(self: &RsLayer, out: &mut [f64]) -> Result<bool>;
        /// Starts a read. `bbox` is empty or four values; `where_clause` is
        /// empty for none; `columns` are field indexes, read only when
        /// `all_columns` is false. `synchronous` reads on the calling thread
        /// before returning, for a read known to be small (feature ids).
        fn start(
            self: &RsLayer,
            bbox: &[f64],
            where_clause: &str,
            columns: &[u32],
            all_columns: bool,
            geometry: bool,
            synchronous: bool,
        ) -> Result<Box<RsCursor>>;

        /// Advances to the next row; `false` at the end.
        fn advance(self: &mut RsCursor) -> Result<bool>;
        fn fid(self: &RsCursor) -> i64;
        /// The row's WKB, empty for no geometry.
        fn wkb(self: &RsCursor) -> &[u8];
        /// The kind of field `field`'s value in this row; `0` for NULL or a
        /// field the read did not select.
        fn kind(self: &RsCursor, field: usize) -> u8;
        fn int_value(self: &RsCursor, field: usize) -> i64;
        fn double_value(self: &RsCursor, field: usize) -> f64;
        fn text_value(self: &RsCursor, field: usize) -> &str;
        fn blob_value(self: &RsCursor, field: usize) -> &[u8];
    }

    unsafe extern "C++" {
        include!("geopackage-qgis/src/provider.h");

        /// Registers the provider with QGIS's registry; `false` if a provider
        /// with its key is registered already.
        fn register_provider() -> bool;
    }
}

/// The value kinds that cross to C++. Matches `Kind` in `provider.h`.
mod kind {
    pub const NULL: u8 = 0;
    pub const INT: u8 = 1;
    pub const DOUBLE: u8 = 2;
    pub const TEXT: u8 = 3;
    pub const BOOL: u8 = 4;
    pub const BLOB: u8 = 5;
    /// Days since the epoch, through `int_value`.
    pub const DATE: u8 = 6;
    /// Microseconds since the epoch, UTC, through `int_value`.
    pub const DATETIME: u8 = 7;
}

fn kind_of(data_type: &DataType) -> u8 {
    match data_type {
        DataType::Int64 => kind::INT,
        DataType::Float64 => kind::DOUBLE,
        DataType::Utf8 => kind::TEXT,
        DataType::Boolean => kind::BOOL,
        DataType::Binary => kind::BLOB,
        DataType::Date32 => kind::DATE,
        DataType::Timestamp(TimeUnit::Microsecond, _) => kind::DATETIME,
        _ => kind::TEXT,
    }
}

type Error = Box<dyn std::error::Error + Send + Sync>;

/// A layer of a file, with what its reads need.
pub struct RsLayer {
    path: PathBuf,
    table: String,
    attributes: bool,
    info_fields: Vec<(String, u8, String, i32)>,
    geometry_column: Option<String>,
    wkb_type: String,
    srs_authid: String,
    srs_wkt: String,
    /// For counts and extents, which are cheap single queries.
    gpkg: Mutex<GeoPackage>,
    /// Read-only connections that finished reads hand back, so the next read
    /// skips opening one (about 0.4 ms, which is most of a small read).
    pool: Pool,
}

/// Idle read-only connections to one file.
type Pool = Arc<Mutex<Vec<GeoPackage>>>;

/// Connections kept idle per layer; more than QGIS keeps iterators open at
/// once on one layer in ordinary use.
const POOL_SIZE: usize = 8;

fn qgis_type_name(geometry_type: &str) -> &'static str {
    match geometry_type {
        "POINT" => "Point",
        "LINESTRING" => "LineString",
        "POLYGON" => "Polygon",
        "MULTIPOINT" => "MultiPoint",
        "MULTILINESTRING" => "MultiLineString",
        "MULTIPOLYGON" => "MultiPolygon",
        "GEOMETRYCOLLECTION" => "GeometryCollection",
        "CIRCULARSTRING" => "CircularString",
        "COMPOUNDCURVE" => "CompoundCurve",
        "CURVEPOLYGON" => "CurvePolygon",
        "MULTICURVE" => "MultiCurve",
        "MULTISURFACE" => "MultiSurface",
        _ => "Unknown",
    }
}

fn open_layer_handle<'g>(
    gpkg: &'g GeoPackage,
    table: &str,
    attributes: bool,
) -> geopackage::Result<Layer<'g>> {
    if attributes {
        gpkg.attributes(table)
    } else {
        gpkg.layer(table)
    }
}

fn open_layer(path: &str, layer: &str) -> Result<Box<RsLayer>, Error> {
    let gpkg = GeoPackage::open_read_only_lenient(path)?;
    let mut table = layer.to_owned();
    if table.is_empty() {
        let mut names: Vec<String> = gpkg
            .layers()?
            .iter()
            .map(|l| l.table_name().to_owned())
            .collect();
        names.extend(
            gpkg.contents()?
                .into_iter()
                .filter(|c| c.data_type == geopackage::ContentsDataType::Attributes)
                .map(|c| c.table_name),
        );
        match names.as_slice() {
            [only] => table = only.clone(),
            _ => return Err(format!("name one of the layers: {}", names.join(", ")).into()),
        }
    }
    let attributes = gpkg.layer(&table).is_err();
    let handle = open_layer_handle(&gpkg, &table, attributes)?;
    let schema = handle.arrow_schema()?;
    let geometry = handle.geometry_column().cloned();
    let pk = handle.primary_key_column().map(str::to_owned);
    let mut info_fields = Vec::new();
    for column in &handle.schema().columns {
        if Some(&column.name) == geometry.as_ref().map(|g| &g.column_name)
            || Some(column.name.as_str()) == pk.as_deref()
        {
            continue;
        }
        let kind = schema
            .field_with_name(&column.name)
            .map(|f| kind_of(f.data_type()))
            .unwrap_or(kind::TEXT);
        let declared = column.declared_type.clone();
        let length = declared
            .to_ascii_uppercase()
            .strip_prefix("TEXT(")
            .and_then(|rest| rest.strip_suffix(')'))
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        info_fields.push((column.name.clone(), kind, declared, length));
    }
    let (wkb_type, srs_authid, srs_wkt) = match &geometry {
        Some(g) => {
            let mut name = qgis_type_name(g.geometry_type.as_str()).to_owned();
            if g.z != ZmFlag::Prohibited {
                name.push('Z');
            }
            if g.m != ZmFlag::Prohibited {
                name.push('M');
            }
            let srs = gpkg.srs(g.srs_id)?;
            let authid = srs
                .as_ref()
                .filter(|s| {
                    s.organization_coordsys_id > 0 && !s.organization.eq_ignore_ascii_case("NONE")
                })
                .map(|s| {
                    format!(
                        "{}:{}",
                        s.organization.to_ascii_uppercase(),
                        s.organization_coordsys_id
                    )
                })
                .unwrap_or_default();
            let wkt = srs
                .map(|s| s.definition_wkt2.unwrap_or(s.definition))
                .unwrap_or_default();
            (name, authid, wkt)
        }
        None => ("NoGeometry".to_owned(), String::new(), String::new()),
    };
    drop(handle);
    Ok(Box::new(RsLayer {
        path: PathBuf::from(path),
        table,
        attributes,
        info_fields,
        geometry_column: geometry.map(|g| g.column_name),
        wkb_type,
        srs_authid,
        srs_wkt,
        gpkg: Mutex::new(gpkg),
        pool: Arc::new(Mutex::new(Vec::new())),
    }))
}

impl RsLayer {
    fn info(&self) -> ffi::LayerInfo {
        ffi::LayerInfo {
            table: self.table.clone(),
            has_geometry: self.geometry_column.is_some(),
            wkb_type: self.wkb_type.clone(),
            srs_authid: self.srs_authid.clone(),
            srs_wkt: self.srs_wkt.clone(),
            fields: self
                .info_fields
                .iter()
                .map(|(name, kind, declared, length)| ffi::FieldInfo {
                    name: name.clone(),
                    kind: *kind,
                    declared_type: declared.clone(),
                    length: *length,
                })
                .collect(),
        }
    }

    fn with_gpkg<R>(&self, f: impl FnOnce(&GeoPackage) -> Result<R, Error>) -> Result<R, Error> {
        let gpkg = self
            .gpkg
            .lock()
            .map_err(|_poisoned| "connection lock poisoned")?;
        f(&gpkg)
    }

    fn count(&self, subset: &str) -> Result<i64, Error> {
        self.with_gpkg(|gpkg| {
            let layer = open_layer_handle(gpkg, &self.table, self.attributes)?;
            if subset.is_empty() {
                return Ok(i64::try_from(layer.count()?)?);
            }
            let layer = layer.with_columns(&[])?;
            let mut total = 0usize;
            for batch in layer.read_arrow_where(subset, &[], ArrowReadOptions::default())? {
                total += batch?.num_rows();
            }
            Ok(i64::try_from(total)?)
        })
    }

    fn extent(&self, out: &mut [f64]) -> Result<bool, Error> {
        self.with_gpkg(|gpkg| {
            let layer = open_layer_handle(gpkg, &self.table, self.attributes)?;
            match layer.extent()? {
                Some(b) if out.len() >= 4 => {
                    out.copy_from_slice(&[b.min_x, b.min_y, b.max_x, b.max_y][..out.len().min(4)]);
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
    }

    fn start(
        &self,
        bbox: &[f64],
        where_clause: &str,
        columns: &[u32],
        all_columns: bool,
        geometry: bool,
        synchronous: bool,
    ) -> Result<Box<RsCursor>, Error> {
        let names: Vec<String> = if all_columns {
            self.info_fields.iter().map(|f| f.0.clone()).collect()
        } else {
            columns
                .iter()
                .filter_map(|&i| self.info_fields.get(i as usize).map(|f| f.0.clone()))
                .collect()
        };
        let bbox = match bbox {
            [a, b, c, d] => Some(BoundingBox::new(*a, *b, *c, *d)),
            _ => None,
        };
        let geometry = geometry && self.geometry_column.is_some();
        let request = Request {
            path: self.path.clone(),
            table: self.table.clone(),
            attributes: self.attributes,
            columns: (!all_columns || !geometry).then(|| names.clone()),
            geometry_column: self.geometry_column.clone().filter(|_| geometry),
            bbox,
            where_clause: (!where_clause.is_empty()).then(|| where_clause.to_owned()),
            pool: Arc::clone(&self.pool),
        };
        let source = if synchronous {
            let mut batches = VecDeque::new();
            request.run(&mut |batch| {
                batches.push_back(batch);
                true
            })?;
            Source::Ready(batches)
        } else {
            let (tx, rx) = sync_channel(4);
            std::thread::Builder::new()
                .name(format!("gpkg-qgis-{}", self.table))
                .spawn(move || {
                    let result = request.run(&mut |batch| tx.send(batch).is_ok());
                    if let Err(error) = result {
                        drop(tx.send(Err(error.to_string())));
                    }
                })?;
            Source::Channel(rx)
        };
        let field_index = self
            .info_fields
            .iter()
            .map(|f| f.0.clone())
            .collect::<Vec<_>>();
        Ok(Box::new(RsCursor {
            source: Some(source),
            batch: None,
            row: 0,
            rows: 0,
            fids: None,
            geometry: None,
            columns: vec![None; field_index.len()],
            field_names: field_index,
            geometry_name: self.geometry_column.clone(),
        }))
    }
}

/// What one read selects, owned so it can move to the reader thread.
struct Request {
    path: PathBuf,
    table: String,
    attributes: bool,
    /// `Some` to project: the value columns to read.
    columns: Option<Vec<String>>,
    /// The geometry column, when the read includes it.
    geometry_column: Option<String>,
    bbox: Option<BoundingBox>,
    where_clause: Option<String>,
    pool: Pool,
}

impl Request {
    /// Runs the read, handing each batch to `sink` until it returns `false`.
    /// Borrows a pooled connection, or opens one, and gives it back after.
    fn run(&self, sink: &mut dyn FnMut(Result<RecordBatch, String>) -> bool) -> Result<(), Error> {
        let pooled = self.pool.lock().ok().and_then(|mut idle| idle.pop());
        let gpkg = match pooled {
            Some(gpkg) => gpkg,
            None => GeoPackage::open_read_only_lenient(&self.path)?,
        };
        let result = self.read(&gpkg, sink);
        if let Ok(mut idle) = self.pool.lock()
            && idle.len() < POOL_SIZE
        {
            idle.push(gpkg);
        }
        result
    }

    fn read(
        &self,
        gpkg: &GeoPackage,
        sink: &mut dyn FnMut(Result<RecordBatch, String>) -> bool,
    ) -> Result<(), Error> {
        let mut layer = open_layer_handle(gpkg, &self.table, self.attributes)?;
        if let Some(columns) = &self.columns {
            let mut names: Vec<&str> = columns.iter().map(String::as_str).collect();
            if let Some(geometry) = &self.geometry_column {
                names.push(geometry);
            }
            layer = layer.with_columns(&names)?;
        }
        let options = ArrowReadOptions::default();
        // The crate's reads, as they are. A box goes to the RTree once, then
        // a page of candidates per query (or the exact scan, with no index);
        // a box that selects every row is read unfiltered, as OGR's driver
        // does.
        let where_clause = self.where_clause.as_deref();
        let batches = match (self.bbox, where_clause) {
            (Some(b), Some(w)) => layer.read_arrow_in_where(b, w, &[], options)?,
            (Some(b), None) => layer.read_arrow_in(b, options)?,
            (None, Some(w)) => layer.read_arrow_where(w, &[], options)?,
            (None, None) => layer.read_arrow(options)?,
        };
        for batch in batches {
            if !sink(batch.map_err(|e| e.to_string())) {
                break; // the iterator was closed
            }
        }
        Ok(())
    }
}

/// One decoded column of the current batch.
#[derive(Clone)]
enum Column {
    Int(ArrayRef),
    Double(ArrayRef),
    Text(ArrayRef),
    Bool(ArrayRef),
    Blob(ArrayRef),
    Date(ArrayRef),
    DateTime(ArrayRef),
}

/// Where a cursor's batches come from.
enum Source {
    /// A reader thread, for reads of unknown size.
    Channel(Receiver<Result<RecordBatch, String>>),
    /// Read already, on the calling thread.
    Ready(VecDeque<Result<RecordBatch, String>>),
}

impl Source {
    /// The next batch, or `None` at the end.
    fn next(&mut self) -> Option<Result<RecordBatch, String>> {
        match self {
            Self::Channel(rx) => rx.recv().ok(),
            Self::Ready(batches) => batches.pop_front(),
        }
    }
}

/// A read in progress: the current batch and the row in it.
pub struct RsCursor {
    source: Option<Source>,
    batch: Option<RecordBatch>,
    row: usize,
    rows: usize,
    fids: Option<ArrayRef>,
    geometry: Option<ArrayRef>,
    /// Indexed by field index; `None` for a field the read did not select.
    columns: Vec<Option<Column>>,
    field_names: Vec<String>,
    geometry_name: Option<String>,
}

impl RsCursor {
    fn load(&mut self, batch: RecordBatch) {
        let schema = batch.schema();
        for column in &mut self.columns {
            *column = None;
        }
        self.geometry = None;
        // The first column is always the primary key.
        self.fids = batch.columns().first().cloned();
        for (field, array) in schema.fields().iter().zip(batch.columns()) {
            if Some(field.name()) == self.geometry_name.as_ref() {
                self.geometry = Some(array.clone());
                continue;
            }
            let Some(index) = self.field_names.iter().position(|n| n == field.name()) else {
                continue;
            };
            let array = array.clone();
            let column = match field.data_type() {
                DataType::Int64 => Column::Int(array),
                DataType::Float64 => Column::Double(array),
                DataType::Boolean => Column::Bool(array),
                DataType::Binary => Column::Blob(array),
                DataType::Date32 => Column::Date(array),
                DataType::Timestamp(TimeUnit::Microsecond, _) => Column::DateTime(array),
                _ => Column::Text(array),
            };
            if let Some(slot) = self.columns.get_mut(index) {
                *slot = Some(column);
            }
        }
        self.rows = batch.num_rows();
        self.row = 0;
        self.batch = Some(batch);
    }

    fn advance(&mut self) -> Result<bool, Error> {
        if self.batch.is_some() && self.row + 1 < self.rows {
            self.row += 1;
            return Ok(true);
        }
        loop {
            let Some(source) = &mut self.source else {
                return Ok(false);
            };
            match source.next() {
                Some(Ok(batch)) => {
                    if batch.num_rows() == 0 {
                        continue;
                    }
                    self.load(batch);
                    return Ok(true);
                }
                Some(Err(message)) => {
                    self.source = None;
                    return Err(message.into());
                }
                None => {
                    self.source = None;
                    self.batch = None;
                    return Ok(false);
                }
            }
        }
    }

    fn fid(&self) -> i64 {
        self.fids
            .as_ref()
            .map_or(0, |a| a.as_primitive::<Int64Type>().value(self.row))
    }

    fn wkb(&self) -> &[u8] {
        match &self.geometry {
            Some(a) if !a.is_null(self.row) => a.as_binary::<i32>().value(self.row),
            _ => &[],
        }
    }

    fn column(&self, field: usize) -> Option<&Column> {
        self.columns.get(field).and_then(Option::as_ref)
    }

    fn kind(&self, field: usize) -> u8 {
        let Some(column) = self.column(field) else {
            return kind::NULL;
        };
        let (array, value_kind) = match column {
            Column::Int(a) => (a, kind::INT),
            Column::Double(a) => (a, kind::DOUBLE),
            Column::Text(a) => (a, kind::TEXT),
            Column::Bool(a) => (a, kind::BOOL),
            Column::Blob(a) => (a, kind::BLOB),
            Column::Date(a) => (a, kind::DATE),
            Column::DateTime(a) => (a, kind::DATETIME),
        };
        if array.is_null(self.row) {
            kind::NULL
        } else {
            value_kind
        }
    }

    fn int_value(&self, field: usize) -> i64 {
        match self.column(field) {
            Some(Column::Int(a)) => a.as_primitive::<Int64Type>().value(self.row),
            Some(Column::Bool(a)) => i64::from(a.as_boolean().value(self.row)),
            Some(Column::Date(a)) => i64::from(a.as_primitive::<Date32Type>().value(self.row)),
            Some(Column::DateTime(a)) => {
                a.as_primitive::<TimestampMicrosecondType>().value(self.row)
            }
            _ => 0,
        }
    }

    fn double_value(&self, field: usize) -> f64 {
        match self.column(field) {
            Some(Column::Double(a)) => a.as_primitive::<Float64Type>().value(self.row),
            _ => 0.0,
        }
    }

    fn text_value(&self, field: usize) -> &str {
        match self.column(field) {
            Some(Column::Text(a)) => a.as_string::<i32>().value(self.row),
            _ => "",
        }
    }

    fn blob_value(&self, field: usize) -> &[u8] {
        match self.column(field) {
            Some(Column::Blob(a)) => a.as_binary::<i32>().value(self.row),
            _ => &[],
        }
    }
}

/// Registers the provider with QGIS. The QGIS plugin calls this through
/// ctypes after loading the library into a running QGIS; it returns `true`
/// if the provider was registered, `false` if it already was.
///
/// # Safety
///
/// Call it from a process in which QGIS's core library is loaded and
/// `QgsApplication` is initialised, on the main thread.
#[unsafe(no_mangle)]
pub extern "C" fn geopackage_qgis_register() -> bool {
    ffi::register_provider()
}
