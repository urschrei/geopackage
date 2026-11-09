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
//!
//! # The write path
//!
//! Edits go through one read-write connection, opened on the first edit.
//! Each of the provider's edit calls (add, delete, change attributes, change
//! geometries) is one transaction, so a call that fails leaves the file as it
//! was before it. QGIS's edit buffer makes those calls in turn when a layer's
//! changes are committed. Reads keep their own read-only connections, and
//! see each committed edit from their next read on.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, sync_channel};
use std::sync::{Arc, Mutex};

use arrow_array::cast::AsArray;
use arrow_array::types::{Date32Type, Float64Type, Int64Type, TimestampMicrosecondType};
use arrow_array::{Array, ArrayRef, RecordBatch};
use arrow_schema::{DataType, TimeUnit};
use geopackage::arrow::ArrowReadOptions;
use geopackage::core::datetime::{Date, DateTime};
use geopackage::core::types::ZmFlag;
use geopackage::{BoundingBox, ConversionOptions, GeoPackage, Layer, Value, ValueRef};

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

    /// One attribute value going to the file. `kind` says which of the other
    /// fields holds it: see `kind` below; dates are days since the epoch and
    /// datetimes microseconds since the epoch, UTC, both in `int_value`.
    struct Cell {
        kind: u8,
        int_value: i64,
        double_value: f64,
        text: String,
        blob: Vec<u8>,
    }

    /// A feature to add: ISO WKB (empty for none) and a cell per field.
    struct NewFeature {
        wkb: Vec<u8>,
        cells: Vec<Cell>,
    }

    /// A new value for field `field` of feature `fid`.
    struct AttributeChange {
        fid: i64,
        field: u32,
        cell: Cell,
    }

    /// A new geometry for feature `fid`, as ISO WKB.
    struct GeometryChange {
        fid: i64,
        wkb: Vec<u8>,
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
        /// Whether the file can be written, so the layer offers editing.
        fn writable(self: &RsLayer) -> bool;
        /// Adds features in one transaction and returns their ids, in order.
        fn add_features(self: &RsLayer, features: &[NewFeature]) -> Result<Vec<i64>>;
        /// Deletes features in one transaction; returns how many existed.
        fn delete_features(self: &RsLayer, fids: &[i64]) -> Result<usize>;
        /// Applies attribute changes in one transaction.
        fn change_attributes(self: &RsLayer, changes: &[AttributeChange]) -> Result<()>;
        /// Replaces geometries in one transaction, keeping each row's values.
        fn change_geometries(self: &RsLayer, changes: &[GeometryChange]) -> Result<()>;

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

        /// Registers the provider with QGIS's registry under `key`; `false` if
        /// a provider with that key is registered already.
        fn register_provider(key: &str) -> bool;
        /// The QGIS version the shim was compiled against, such as 33404.
        fn built_qgis_version() -> i32;
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
    /// Whether the file could be opened for writing when the layer was.
    writable: bool,
    /// The read-write connection edits go through, once there has been one.
    writer: Mutex<Option<GeoPackage>>,
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

/// Opens a layer of `gpkg`. Every connection here is lenient, and so are its
/// layers' value conversions: `DATETIME` text in the common ISO 8601 forms
/// other than GeoPackage's own is read, as OGR reads it.
fn open_layer_handle<'g>(
    gpkg: &'g GeoPackage,
    table: &str,
    attributes: bool,
) -> geopackage::Result<Layer<'g>> {
    let layer = if attributes {
        gpkg.attributes(table)?
    } else {
        gpkg.layer(table)?
    };
    Ok(layer.with_conversion_options(ConversionOptions::lenient()))
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
    // Opening the file for writing, without writing, is the check SQLite's
    // own open makes; a read-only file or mount fails it.
    let writable = std::fs::OpenOptions::new().write(true).open(path).is_ok();
    Ok(Box::new(RsLayer {
        writable,
        writer: Mutex::new(None),
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

impl RsLayer {
    fn writable(&self) -> bool {
        self.writable
    }

    /// Runs `f` on a handle to this layer through the read-write connection,
    /// opening it the first time.
    fn with_writer<R>(
        &self,
        f: impl FnOnce(&GeoPackage, &Layer<'_>) -> Result<R, Error>,
    ) -> Result<R, Error> {
        if !self.writable {
            return Err(format!("{} is not writable", self.path.display()).into());
        }
        let mut slot = self
            .writer
            .lock()
            .map_err(|_poisoned| "writer lock poisoned")?;
        if slot.is_none() {
            *slot = Some(
                geopackage::OpenOptions::new()
                    .lenient(true)
                    .open(&self.path)?,
            );
        }
        let gpkg = slot.as_ref().ok_or("no writer")?;
        let layer = open_layer_handle(gpkg, &self.table, self.attributes)?;
        f(gpkg, &layer)
    }

    fn add_features(&self, features: &[ffi::NewFeature]) -> Result<Vec<i64>, Error> {
        let width = self.info_fields.len();
        self.with_writer(|_, layer| {
            let mut writer = layer.writer()?;
            let mut fids = Vec::with_capacity(features.len());
            for feature in features {
                let mut values = feature
                    .cells
                    .iter()
                    .take(width)
                    .map(to_value)
                    .collect::<Result<Vec<_>, _>>()?;
                values.resize(width, Value::Null);
                let refs: Vec<ValueRef<'_>> = values.iter().map(ValueRef::from).collect();
                let fid = if feature.wkb.is_empty() || self.geometry_column.is_none() {
                    writer.insert_row(None, &refs)?
                } else {
                    writer.insert_wkb(None, &feature.wkb, &refs)?
                };
                fids.push(fid);
            }
            writer.commit()?;
            Ok(fids)
        })
    }

    fn delete_features(&self, fids: &[i64]) -> Result<usize, Error> {
        self.with_writer(|_, layer| {
            let mut writer = layer.writer()?;
            let mut deleted = 0;
            for &fid in fids {
                if writer.delete(fid)? {
                    deleted += 1;
                }
            }
            writer.commit()?;
            Ok(deleted)
        })
    }

    fn change_attributes(&self, changes: &[ffi::AttributeChange]) -> Result<(), Error> {
        // QGIS hands over a map of maps, flattened here: consecutive changes
        // to one feature become one update.
        let mut grouped: Vec<(i64, Vec<(&str, Value)>)> = Vec::new();
        for change in changes {
            let Some(field) = self.info_fields.get(change.field as usize) else {
                return Err(format!("no field {}", change.field).into());
            };
            let value = (field.0.as_str(), to_value(&change.cell)?);
            match grouped.last_mut() {
                Some((fid, values)) if *fid == change.fid => values.push(value),
                _ => grouped.push((change.fid, vec![value])),
            }
        }
        self.with_writer(|_, layer| {
            let mut writer = layer.writer()?;
            for (fid, values) in &grouped {
                let refs: Vec<(&str, ValueRef<'_>)> = values
                    .iter()
                    .map(|(n, v)| (*n, ValueRef::from(v)))
                    .collect();
                writer.update_columns(*fid, &refs)?;
            }
            writer.commit()?;
            Ok(())
        })
    }

    fn change_geometries(&self, changes: &[ffi::GeometryChange]) -> Result<(), Error> {
        if self.geometry_column.is_none() {
            return Err(format!("{} has no geometry column", self.table).into());
        }
        self.with_writer(|gpkg, layer| {
            // The crate replaces a geometry together with the row's values,
            // so each row's current values are read first, in the same
            // transaction as the writes.
            let pk = layer.primary_key_column().unwrap_or("rowid").to_owned();
            let clause = format!("{} = ?1", quote_ident(&pk));
            let values_only =
                open_layer_handle(gpkg, &self.table, self.attributes)?.without_geometry();
            let mut writer = layer.writer()?;
            for change in changes {
                let current = values_only
                    .select(&clause, &[ValueRef::Integer(change.fid)])?
                    .next()
                    .transpose()?;
                let Some(current) = current else {
                    continue;
                };
                let values: Vec<Value> = current.values().map(|v| v.to_value()).collect();
                let refs: Vec<ValueRef<'_>> = values.iter().map(ValueRef::from).collect();
                writer.update_wkb(change.fid, &change.wkb, &refs)?;
            }
            writer.commit()?;
            Ok(())
        })
    }
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A cell from QGIS as a value for the crate.
fn to_value(cell: &ffi::Cell) -> Result<Value, Error> {
    Ok(match cell.kind {
        kind::INT => Value::Integer(cell.int_value),
        kind::DOUBLE => Value::Float(cell.double_value),
        kind::TEXT => Value::Text(cell.text.clone()),
        kind::BOOL => Value::Boolean(cell.int_value != 0),
        kind::BLOB => Value::Blob(cell.blob.clone()),
        kind::DATE => Value::Date(Date::from_days_since_epoch(i32::try_from(cell.int_value)?)?),
        kind::DATETIME => Value::DateTime(DateTime::from_micros_since_epoch(cell.int_value)?),
        _ => Value::Null,
    })
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

/// The key the plugin's Python provider registers. The plugin registers this
/// provider under it in that provider's place, so a project names one
/// provider whichever of the two serves it.
const PLUGIN_KEY: &str = "geopackage_rs";

/// A key of this provider's own, for registering it beside the Python one, as
/// the tests and the comparisons with OGR do.
const SIDE_BY_SIDE_KEY: &str = "geopackage_rs_native";

/// Registers the provider with QGIS under the Python provider's key,
/// `geopackage_rs`, and returns `true`, or returns `false` if a provider
/// with that key is registered already. The QGIS plugin calls this through
/// ctypes, before registering the Python provider, after checking
/// [`geopackage_qgis_qgis_version`] against the running QGIS.
///
/// Call it from a process in which QGIS's core library is loaded and
/// `QgsApplication` is initialised, on the main thread.
#[unsafe(no_mangle)]
pub extern "C" fn geopackage_qgis_register() -> bool {
    ffi::register_provider(PLUGIN_KEY)
}

/// As [`geopackage_qgis_register`], under the key `geopackage_rs_native`, so
/// that it can sit beside the Python provider.
#[unsafe(no_mangle)]
pub extern "C" fn geopackage_qgis_register_side_by_side() -> bool {
    ffi::register_provider(SIDE_BY_SIDE_KEY)
}

/// The QGIS version this library was compiled against, as
/// `Qgis.versionInt()` gives it (33404 for 3.34.4). QGIS's C++ interface
/// changes between minor versions, so the plugin loads a build only into
/// the minor version it was made for.
#[unsafe(no_mangle)]
pub extern "C" fn geopackage_qgis_qgis_version() -> i32 {
    ffi::built_qgis_version()
}
