//! Property tests over the C Arrow streams: reads with any filter, and a
//! copy through `gpkg_create_layer_from_arrow_schema` and
//! `gpkg_layer_write_arrow`, against the Rust API.
//!
//! One `unsafe` block per call, as the rest of this crate's tests do.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::ffi::{CString, c_char};
use std::path::Path;

use arrow_array::ffi::FFI_ArrowSchema;
use arrow_array::ffi_stream::{ArrowArrayStreamReader, FFI_ArrowArrayStream};
use arrow_array::{RecordBatch, RecordBatchIterator, RecordBatchReader};
use geopackage::arrow::{ArrowBatches, ArrowReadOptions};
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{
    BoundingBox, ColumnSpec, GeoPackage, GeometrySpec, Layer, TableSchemaBuilder, ValueRef,
};
use geopackage_ffi::{
    Status, ValueKind, gpkg_close, gpkg_create, gpkg_create_layer_from_arrow_schema,
    gpkg_error_clear, gpkg_error_t, gpkg_layer_free, gpkg_layer_open, gpkg_layer_read_arrow,
    gpkg_layer_read_arrow_filtered, gpkg_layer_read_arrow_in, gpkg_layer_t, gpkg_layer_write_arrow,
    gpkg_t, gpkg_value_payload, gpkg_value_t,
};
use hegel::generators;

/// An error slot holding a value no call writes on success.
fn sentinel() -> gpkg_error_t {
    gpkg_error_t {
        code: Status::Other,
        message: std::ptr::null_mut(),
    }
}

/// Checks the error protocol for a call that returned `status`.
fn check(status: Status, error: &mut gpkg_error_t) -> Status {
    if status == Status::Ok {
        assert_eq!(error.code, Status::Other, "a success wrote the slot");
        assert!(error.message.is_null(), "a success wrote the slot");
        return status;
    }
    assert!(!error.message.is_null(), "a failure without a message");
    assert_eq!(error.code, status);
    // SAFETY: a slot this library filled in.
    unsafe { gpkg_error_clear(error) };
    *error = sentinel();
    status
}

const COLUMNS: [(&str, ColumnType); 6] = [
    ("t", ColumnType::Text(None)),
    ("i", ColumnType::Integer),
    ("r", ColumnType::Double),
    ("b", ColumnType::Boolean),
    ("x", ColumnType::Blob(None)),
    ("dt", ColumnType::DateTime),
];

fn schema(name: &str) -> TableSchemaBuilder {
    COLUMNS
        .iter()
        .fold(TableSchemaBuilder::new(name), |schema, (column, ty)| {
            schema.column(ColumnSpec::new(*column, ty.clone()))
        })
        .geometry(GeometrySpec::new(GeometryType::Point, 4326))
        .spatial_index(true)
}

/// A little-endian WKB point.
fn point(x: f64, y: f64) -> Vec<u8> {
    let mut bytes = vec![1u8];
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&x.to_le_bytes());
    bytes.extend_from_slice(&y.to_le_bytes());
    bytes
}

/// Fills layer `t` with drawn rows: each value of the column's type or NULL,
/// and a point or no geometry.
fn fill(tc: &hegel::TestCase, gpkg: &GeoPackage) {
    let layer = gpkg.create_layer(&schema("t")).unwrap();
    let mut writer = layer.writer().unwrap();
    let rows = tc.draw(generators::integers::<usize>().min_value(0).max_value(12));
    for _ in 0..rows {
        let text = tc.draw(generators::text().max_size(6));
        let blob = tc.draw(generators::vecs(generators::integers::<u8>()).max_size(4));
        let null = |tc: &hegel::TestCase| tc.draw(generators::weighted_booleans(0.2));
        let values = [
            if null(tc) {
                ValueRef::Null
            } else {
                ValueRef::Text(&text)
            },
            if null(tc) {
                ValueRef::Null
            } else {
                ValueRef::Integer(tc.draw(generators::integers::<i64>().min_value(-5).max_value(5)))
            },
            if null(tc) {
                ValueRef::Null
            } else {
                ValueRef::Float(tc.draw(generators::floats::<f64>().allow_nan(false)))
            },
            if null(tc) {
                ValueRef::Null
            } else {
                ValueRef::Boolean(tc.draw(generators::booleans()))
            },
            if null(tc) {
                ValueRef::Null
            } else {
                ValueRef::Blob(&blob)
            },
            ValueRef::Null,
        ];
        if tc.draw(generators::weighted_booleans(0.8)) {
            let x = tc.draw(generators::floats::<f64>().min_value(-10.0).max_value(10.0));
            let y = tc.draw(generators::floats::<f64>().min_value(-10.0).max_value(10.0));
            writer.insert_wkb(None, &point(x, y), &values).unwrap();
        } else {
            writer.insert_row(None, &values).unwrap();
        }
    }
    writer.commit().unwrap();
}

/// A read, as drawn.
#[derive(Debug)]
enum Read {
    All,
    In([f64; 4]),
    Filtered {
        bbox: Option<[f64; 4]>,
        clause: Option<&'static str>,
        params: Vec<i64>,
    },
}

fn draw_bbox(tc: &hegel::TestCase) -> [f64; 4] {
    let edge = || {
        if tc.draw(generators::weighted_booleans(0.05)) {
            f64::NAN
        } else {
            tc.draw(generators::floats::<f64>().min_value(-12.0).max_value(12.0))
        }
    };
    [edge(), edge(), edge(), edge()]
}

fn draw_read(tc: &hegel::TestCase) -> Read {
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(2)) {
        0 => Read::All,
        1 => Read::In(draw_bbox(tc)),
        _ => Read::Filtered {
            bbox: tc.draw(generators::booleans()).then(|| draw_bbox(tc)),
            clause: tc.draw(generators::booleans()).then(|| {
                tc.draw(generators::sampled_from(vec![
                    "1 = 1",
                    "i > ?1",
                    "i = ?1 OR i = ?2",
                    "b",
                    "x IS NULL",
                    "fid >",
                    "absent = 1",
                ]))
            }),
            params: tc.draw(
                generators::vecs(generators::integers::<i64>().min_value(-5).max_value(5))
                    .max_size(2),
            ),
        },
    }
}

/// The batches of a stream, or the message of the error it ended with.
type Drained = (arrow_schema::SchemaRef, Vec<RecordBatch>, Option<String>);

fn drain(reader: impl RecordBatchReader) -> Drained {
    let schema = reader.schema();
    let mut batches = Vec::new();
    for batch in reader {
        match batch {
            Ok(batch) => batches.push(batch),
            Err(error) => return (schema, batches, Some(error.to_string())),
        }
    }
    (schema, batches, None)
}

fn read_c(
    layer: *const gpkg_layer_t,
    read: &Read,
    error: &mut gpkg_error_t,
) -> Result<Drained, Status> {
    let mut stream = FFI_ArrowArrayStream::empty();
    let status = match read {
        // SAFETY: a live layer handle and a writable stream.
        Read::All => unsafe { gpkg_layer_read_arrow(layer, &raw mut stream, error) },
        Read::In([min_x, min_y, max_x, max_y]) => {
            // SAFETY: as above.
            unsafe {
                gpkg_layer_read_arrow_in(
                    layer,
                    *min_x,
                    *min_y,
                    *max_x,
                    *max_y,
                    &raw mut stream,
                    error,
                )
            }
        }
        Read::Filtered {
            bbox,
            clause,
            params,
        } => {
            let clause = clause.map(|text| CString::new(text).unwrap());
            let values: Vec<gpkg_value_t> = params
                .iter()
                .map(|value| gpkg_value_t {
                    kind: ValueKind::Integer,
                    value: gpkg_value_payload { integer: *value },
                })
                .collect();
            // SAFETY: a live layer handle, a bbox of four doubles or NULL, a
            // NUL-terminated clause or NULL, `values` readable for its
            // length, and a writable stream.
            unsafe {
                gpkg_layer_read_arrow_filtered(
                    layer,
                    bbox.as_ref().map_or(std::ptr::null(), |b| b.as_ptr()),
                    clause
                        .as_ref()
                        .map_or(std::ptr::null::<c_char>(), |c| c.as_ptr()),
                    values.as_ptr(),
                    values.len(),
                    &raw mut stream,
                    error,
                )
            }
        }
    };
    match check(status, error) {
        Status::Ok => {
            // SAFETY: a stream this library exported, consumed once.
            let reader = unsafe { ArrowArrayStreamReader::from_raw(&raw mut stream) }
                .map_err(|_import| Status::Other)?;
            Ok(drain(reader))
        }
        failed => Err(failed),
    }
}

fn read_rust(layer: &Layer<'_>, read: &Read) -> Result<Drained, Status> {
    let options = ArrowReadOptions::default().with_threads(1);
    let bbox =
        |[min_x, min_y, max_x, max_y]: [f64; 4]| BoundingBox::new(min_x, min_y, max_x, max_y);
    let opened: geopackage::Result<ArrowBatches<'_>> = match read {
        Read::All => layer.read_arrow(options),
        Read::In(b) => layer.read_arrow_in(bbox(*b), options),
        Read::Filtered {
            bbox: b,
            clause,
            params,
        } => {
            let values: Vec<ValueRef<'_>> = params.iter().map(|v| ValueRef::Integer(*v)).collect();
            match (b, clause) {
                (Some(b), Some(clause)) => {
                    layer.read_arrow_in_where(bbox(*b), clause, &values, options)
                }
                (None, Some(clause)) => layer.read_arrow_where(clause, &values, options),
                // The documented rule for parameters without a clause.
                (_, None) if !values.is_empty() => return Err(Status::InvalidArgument),
                (Some(b), None) => layer.read_arrow_in(bbox(*b), options),
                (None, None) => layer.read_arrow(options),
            }
        }
    };
    opened.map(drain).map_err(|error| Status::from(&error))
}

/// Every read through a C stream gives the schema, batches and end-of-stream
/// error that the same read through the Rust API gives, or the same status
/// when it cannot open.
#[hegel::test]
fn c_streams_read_what_the_rust_api_reads(tc: hegel::TestCase) {
    let mut error = sentinel();
    let path = CString::new(":memory:").unwrap();
    // SAFETY: a NUL-terminated path and a writable error slot.
    let gpkg: *mut gpkg_t = unsafe { gpkg_create(path.as_ptr(), &raw mut error) };
    assert!(!gpkg.is_null());
    // SAFETY: a live container handle; `gpkg_t` is the container.
    let rust: &GeoPackage = unsafe { (*gpkg).gpkg() };
    fill(&tc, rust);
    let rust_layer = rust.layer("t").unwrap();
    let name = CString::new("t").unwrap();
    // SAFETY: a live container and a NUL-terminated name.
    let layer = unsafe { gpkg_layer_open(gpkg, name.as_ptr(), &raw mut error) };
    assert!(!layer.is_null());

    for _ in 0..3 {
        let read = draw_read(&tc);
        let c = read_c(layer, &read, &mut error);
        let r = read_rust(&rust_layer, &read);
        let outcome = match &c {
            Ok((_, _, None)) => "read",
            Ok((_, _, Some(_))) => "stream error",
            Err(_) => "open error",
        };
        tc.event(outcome);
        match (c, r) {
            (Ok((c_schema, c_batches, c_error)), Ok((r_schema, r_batches, r_error))) => {
                assert_eq!(c_schema, r_schema, "{read:?}");
                assert_eq!(c_batches, r_batches, "{read:?}");
                match (c_error, r_error) {
                    (None, None) => {}
                    (Some(c), Some(r)) => assert!(c.contains(&r), "{c} lacks {r}"),
                    (c, r) => panic!("{read:?}: {c:?} against {r:?}"),
                }
            }
            (Err(c), Err(r)) => assert_eq!(c, r, "{read:?}"),
            (c, r) => panic!("{read:?}: {:?} against {:?}", c.map(|_| ()), r.map(|_| ())),
        }
    }
    // SAFETY: a live layer handle, freed once.
    unsafe { gpkg_layer_free(layer) };
    // SAFETY: a live container with no children.
    let status = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(check(status, &mut error), Status::Ok);
}

/// Every row of a table, as SQLite stores it.
fn table(gpkg: &GeoPackage, name: &str) -> Vec<Vec<rusqlite::types::Value>> {
    let mut stmt = gpkg
        .connection()
        .prepare(&format!("SELECT * FROM \"{name}\" ORDER BY fid"))
        .unwrap();
    let width = stmt.column_count();
    stmt.query_map([], |row| (0..width).map(|i| row.get(i)).collect())
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// Copying a layer's batches into a layer made from their schema gives the
/// same layer and the same table through the C API as through the Rust API,
/// for any batch size.
#[hegel::test]
fn a_c_copy_matches_a_rust_copy(tc: hegel::TestCase) {
    let source = GeoPackage::create(Path::new(":memory:")).unwrap();
    fill(&tc, &source);
    let batches: Vec<RecordBatch> = source
        .layer("t")
        .unwrap()
        .read_arrow(ArrowReadOptions::with_batch_size(
            tc.draw(generators::integers::<usize>().min_value(1).max_value(5)),
        ))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let arrow_schema = source
        .layer("t")
        .unwrap()
        .read_arrow(ArrowReadOptions::default())
        .unwrap()
        .schema();
    let batch_size = tc.draw(generators::integers::<usize>().min_value(0).max_value(4));
    let spatial_index = tc.draw(generators::booleans());

    let mut error = sentinel();
    let path = CString::new(":memory:").unwrap();
    // SAFETY: a NUL-terminated path and a writable error slot.
    let gpkg: *mut gpkg_t = unsafe { gpkg_create(path.as_ptr(), &raw mut error) };
    assert!(!gpkg.is_null());
    let name = CString::new("copy").unwrap();
    let ffi_schema = FFI_ArrowSchema::try_from(arrow_schema.as_ref()).unwrap();
    // SAFETY: a live container, a NUL-terminated name and a readable schema.
    let status = unsafe {
        gpkg_create_layer_from_arrow_schema(
            gpkg,
            name.as_ptr(),
            &raw const ffi_schema,
            spatial_index,
            &raw mut error,
        )
    };
    assert_eq!(check(status, &mut error), Status::Ok);
    // SAFETY: a live container and a NUL-terminated name.
    let layer = unsafe { gpkg_layer_open(gpkg, name.as_ptr(), &raw mut error) };
    assert!(!layer.is_null());
    let reader =
        RecordBatchIterator::new(batches.clone().into_iter().map(Ok), arrow_schema.clone());
    let mut stream = FFI_ArrowArrayStream::new(Box::new(reader));
    let mut rows = u64::MAX;
    // SAFETY: a live layer handle, a live stream the call consumes, and a
    // writable out-parameter.
    let status = unsafe {
        gpkg_layer_write_arrow(
            layer,
            &raw mut stream,
            batch_size,
            &raw mut rows,
            &raw mut error,
        )
    };
    assert_eq!(check(status, &mut error), Status::Ok);

    let rust = GeoPackage::create(Path::new(":memory:")).unwrap();
    let builder = TableSchemaBuilder::new("copy")
        .from_arrow_schema(&arrow_schema)
        .unwrap()
        .spatial_index(spatial_index);
    let copy = rust.create_layer(&builder).unwrap();
    let reader = RecordBatchIterator::new(batches.into_iter().map(Ok), arrow_schema);
    let fids = copy.write_arrow(reader, batch_size).unwrap();

    assert_eq!(rows, u64::try_from(fids.len()).unwrap());
    // SAFETY: a live container handle.
    let written: &GeoPackage = unsafe { (*gpkg).gpkg() };
    let ours = written.layer("copy").unwrap();
    assert_eq!(ours.schema().columns.len(), copy.schema().columns.len());
    for (a, b) in ours.schema().columns.iter().zip(&copy.schema().columns) {
        assert_eq!((&a.name, &a.declared_type), (&b.name, &b.declared_type));
    }
    assert_eq!(
        ours.has_spatial_index().unwrap(),
        copy.has_spatial_index().unwrap()
    );
    assert_eq!(table(written, "copy"), table(&rust, "copy"));
    drop(ours);
    // SAFETY: a live layer handle, freed once.
    unsafe { gpkg_layer_free(layer) };
    // SAFETY: a live container with no children.
    let status = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(check(status, &mut error), Status::Ok);
}
