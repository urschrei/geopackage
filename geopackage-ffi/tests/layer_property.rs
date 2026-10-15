//! Property tests over the C layer reads: opening a layer by any name, and
//! what an open layer reports, against the Rust API.
//!
//! One `unsafe` block per call, as the rest of this crate's tests do.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::ffi::{CStr, CString, c_char};

use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{ColumnSpec, GeoPackage, GeometrySpec, Layer, TableSchemaBuilder, ValueRef};
use geopackage_ffi::{
    Status, gpkg_attributes_open, gpkg_close, gpkg_create, gpkg_error_clear, gpkg_error_t,
    gpkg_layer_column_count, gpkg_layer_column_is_primary_key, gpkg_layer_column_name,
    gpkg_layer_column_type, gpkg_layer_count, gpkg_layer_extent, gpkg_layer_free,
    gpkg_layer_geometry_column, gpkg_layer_geometry_type, gpkg_layer_has_spatial_index,
    gpkg_layer_kind, gpkg_layer_name, gpkg_layer_name_at, gpkg_layer_names_count, gpkg_layer_open,
    gpkg_layer_spatial_index_status, gpkg_layer_srs_id, gpkg_layer_t, gpkg_string_free, gpkg_t,
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

/// Takes a string a call returned, or the status of a NULL return. A NULL
/// return that leaves the slot untouched is an absent value, not a failure.
fn take(text: *mut c_char, error: &mut gpkg_error_t) -> Result<Option<String>, Status> {
    if text.is_null() {
        if error.code == Status::Other {
            return Ok(None);
        }
        return Err(check(error.code, error));
    }
    check(Status::Ok, error);
    // SAFETY: a string this library returned.
    let owned = unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned();
    // SAFETY: freed once.
    unsafe { gpkg_string_free(text) };
    Ok(Some(owned))
}

/// Table and column names from several scripts.
const NAMES: [&str; 6] = ["places", "Ünïcödé", "名前", "a b", "x\"y", "rivers"];

const TYPES: [ColumnType; 5] = [
    ColumnType::Integer,
    ColumnType::Text(None),
    ColumnType::Double,
    ColumnType::Blob(None),
    ColumnType::DateTime,
];

/// A little-endian WKB point.
fn point(x: f64, y: f64) -> Vec<u8> {
    let mut bytes = vec![1u8];
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&x.to_le_bytes());
    bytes.extend_from_slice(&y.to_le_bytes());
    bytes
}

/// Creates drawn layers through the Rust API: feature or attribute tables,
/// with drawn columns, geometry type, SRS, index and rows.
fn build(tc: &hegel::TestCase, gpkg: &GeoPackage) {
    let layers = tc.draw(generators::integers::<usize>().min_value(0).max_value(3));
    for _ in 0..layers {
        let name = tc.draw(generators::sampled_from(NAMES.to_vec()));
        let columns = tc.draw(generators::integers::<usize>().min_value(0).max_value(3));
        let mut schema = TableSchemaBuilder::new(name);
        let mut used = Vec::new();
        for _ in 0..columns {
            let column = tc.draw(generators::sampled_from(NAMES.to_vec()));
            if used.contains(&column) {
                continue;
            }
            used.push(column);
            let ty = tc.draw(generators::sampled_from(TYPES.to_vec()));
            schema = schema.column(ColumnSpec::new(column, ty));
        }
        let spatial = tc.draw(generators::booleans());
        let created = if spatial {
            let ty = tc.draw(generators::sampled_from(vec![
                GeometryType::Point,
                GeometryType::Geometry,
            ]));
            let srs = tc.draw(generators::sampled_from(vec![4326, 0, -1]));
            schema = schema
                .geometry(GeometrySpec::new(ty, srs))
                .spatial_index(tc.draw(generators::booleans()));
            gpkg.create_layer(&schema)
        } else {
            gpkg.create_attributes_table(&schema)
        };
        // A name drawn twice already exists.
        let Ok(layer) = created else {
            continue;
        };
        let rows = tc.draw(generators::integers::<usize>().min_value(0).max_value(3));
        let mut writer = layer.writer().unwrap();
        let nulls = vec![ValueRef::Null; used.len()];
        for _ in 0..rows {
            if spatial && tc.draw(generators::booleans()) {
                let x = tc.draw(generators::floats::<f64>().min_value(-1e6).max_value(1e6));
                writer.insert_wkb(None, &point(x, -x), &nulls).unwrap();
            } else {
                writer.insert_row(None, &nulls).unwrap();
            }
        }
        writer.commit().unwrap();
    }
}

/// Draws a name to open: one in the file, one that may be, the same in other
/// case, or one that is not UTF-8.
fn draw_query(tc: &hegel::TestCase, tables: &[String]) -> CString {
    if !tables.is_empty() && tc.draw(generators::booleans()) {
        return CString::new(tc.draw(generators::sampled_from(tables.to_vec()))).unwrap();
    }
    let name = tc.draw(generators::sampled_from(NAMES.to_vec()));
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(3)) {
        0 => CString::new(name.to_uppercase()).unwrap(),
        1 => CString::new([b'a', 0xFF]).unwrap(),
        _ => CString::new(name).unwrap(),
    }
}

/// What the C API reports of one column: its name, its declared type, and
/// whether it is the primary key.
type ColumnReport = (
    Result<Option<String>, Status>,
    Result<Option<String>, Status>,
    bool,
);

/// Everything the C API reports of an open layer, in the order of the calls.
#[derive(Debug, PartialEq)]
struct Report {
    name: Result<Option<String>, Status>,
    kind: Result<Option<String>, Status>,
    columns: Vec<ColumnReport>,
    geometry_column: Result<Option<String>, Status>,
    geometry_type: Result<Option<String>, Status>,
    srs_id: Result<i32, Status>,
    extent: Result<[f64; 4], Status>,
    index_status: Result<Option<String>, Status>,
    has_index: Result<bool, Status>,
    count: Result<u64, Status>,
}

fn report_c(layer: *const gpkg_layer_t, error: &mut gpkg_error_t) -> Report {
    // SAFETY (every call below): a live layer handle, writable
    // out-parameters and a writable error slot.
    let mut count = usize::MAX;
    // SAFETY: as above.
    let status = unsafe { gpkg_layer_column_count(layer, &raw mut count, error) };
    assert_eq!(check(status, error), Status::Ok);
    // One index past the end, to check the out-of-range answers.
    let columns = (0..=count)
        .map(|index| {
            // SAFETY: as above.
            let name = take(
                unsafe { gpkg_layer_column_name(layer, index, error) },
                error,
            );
            // SAFETY: as above.
            let ty = take(
                unsafe { gpkg_layer_column_type(layer, index, error) },
                error,
            );
            // SAFETY: as above.
            let pk = unsafe { gpkg_layer_column_is_primary_key(layer, index) };
            (name, ty, pk)
        })
        .collect();
    let mut srs_id = 0;
    // SAFETY: as above.
    let status = unsafe { gpkg_layer_srs_id(layer, &raw mut srs_id, error) };
    let srs_id = match check(status, error) {
        Status::Ok => Ok(srs_id),
        failed => Err(failed),
    };
    let mut extent = [0.0; 4];
    let [min_x, min_y, max_x, max_y] = &mut extent;
    // SAFETY: as above.
    let status = unsafe {
        gpkg_layer_extent(
            layer,
            std::ptr::from_mut(min_x),
            std::ptr::from_mut(min_y),
            std::ptr::from_mut(max_x),
            std::ptr::from_mut(max_y),
            error,
        )
    };
    let extent = match check(status, error) {
        Status::Ok => Ok(extent),
        failed => Err(failed),
    };
    let mut has_index = false;
    // SAFETY: as above.
    let status = unsafe { gpkg_layer_has_spatial_index(layer, &raw mut has_index, error) };
    let has_index = match check(status, error) {
        Status::Ok => Ok(has_index),
        failed => Err(failed),
    };
    let mut rows = 0;
    // SAFETY: as above.
    let status = unsafe { gpkg_layer_count(layer, &raw mut rows, error) };
    let count = match check(status, error) {
        Status::Ok => Ok(rows),
        failed => Err(failed),
    };
    Report {
        // SAFETY: as above.
        name: take(unsafe { gpkg_layer_name(layer, error) }, error),
        // SAFETY: as above.
        kind: take(unsafe { gpkg_layer_kind(layer, error) }, error),
        columns,
        // SAFETY: as above.
        geometry_column: take(unsafe { gpkg_layer_geometry_column(layer, error) }, error),
        // SAFETY: as above.
        geometry_type: take(unsafe { gpkg_layer_geometry_type(layer, error) }, error),
        srs_id,
        extent,
        // SAFETY: as above.
        index_status: take(
            unsafe { gpkg_layer_spatial_index_status(layer, error) },
            error,
        ),
        has_index,
        count,
    }
}

/// What the documentation says each call reports, from the Rust layer.
fn report_rust(layer: &Layer<'_>) -> Report {
    let schema = layer.schema();
    let mut columns: Vec<_> = schema
        .columns
        .iter()
        .map(|column| {
            (
                Ok(Some(column.name.clone())),
                Ok(Some(column.declared_type.clone())),
                column.is_primary_key(),
            )
        })
        .collect();
    columns.push((Err(Status::NotFound), Err(Status::NotFound), false));
    let geometry = layer.geometry_column();
    let status_of = |error: &geopackage::Error| Status::from(error);
    Report {
        name: Ok(Some(layer.table_name().to_owned())),
        kind: Ok(Some(layer.kind().as_str().to_owned())),
        columns,
        geometry_column: Ok(geometry.map(|g| g.column_name.clone())),
        geometry_type: Ok(geometry.map(|g| g.geometry_type.to_string())),
        srs_id: geometry.map(|g| g.srs_id).ok_or(Status::NotFound),
        extent: match layer.extent() {
            Ok(Some(b)) => Ok([b.min_x, b.min_y, b.max_x, b.max_y]),
            Ok(None) => Err(Status::NotFound),
            Err(error) => Err(status_of(&error)),
        },
        index_status: layer
            .spatial_index_status()
            .map(|status| Some(status.as_str().to_owned()))
            .map_err(|error| status_of(&error)),
        has_index: layer.has_spatial_index().map_err(|error| status_of(&error)),
        count: layer.count().map_err(|error| status_of(&error)),
    }
}

/// The layer list, and opening a layer by any name as a feature layer or as
/// an attribute table, agree with the Rust API; an open layer reports what
/// the Rust layer reports, and an index past the end is `NOT_FOUND`.
#[hegel::test]
fn layer_reads_agree_with_the_rust_api(tc: hegel::TestCase) {
    let mut error = sentinel();
    let path = CString::new(":memory:").unwrap();
    // SAFETY: a NUL-terminated path and a writable error slot.
    let gpkg: *mut gpkg_t = unsafe { gpkg_create(path.as_ptr(), &raw mut error) };
    assert!(!gpkg.is_null());
    // SAFETY: a live container handle; `gpkg_t` is the container.
    let rust: &GeoPackage = unsafe { (*gpkg).gpkg() };
    build(&tc, rust);

    let names: Vec<String> = rust
        .layers()
        .unwrap()
        .iter()
        .map(|layer| layer.table_name().to_owned())
        .collect();
    let mut count = usize::MAX;
    // SAFETY: a live container and a writable out-parameter.
    let status = unsafe { gpkg_layer_names_count(gpkg, &raw mut count, &raw mut error) };
    assert_eq!(check(status, &mut error), Status::Ok);
    assert_eq!(count, names.len());
    for index in 0..=count {
        // SAFETY: a live container.
        let name = take(
            unsafe { gpkg_layer_name_at(gpkg, index, &raw mut error) },
            &mut error,
        );
        let expected = names.get(index).cloned().map(Some).ok_or(Status::NotFound);
        assert_eq!(name, expected);
    }

    let tables: Vec<String> = rust
        .contents()
        .unwrap()
        .into_iter()
        .map(|entry| entry.table_name)
        .collect();
    for _ in 0..3 {
        let query = draw_query(&tc, &tables);
        let attributes = tc.draw(generators::booleans());
        let layer = if attributes {
            // SAFETY: a live container and a NUL-terminated name.
            unsafe { gpkg_attributes_open(gpkg, query.as_ptr(), &raw mut error) }
        } else {
            // SAFETY: as above.
            unsafe { gpkg_layer_open(gpkg, query.as_ptr(), &raw mut error) }
        };
        let expected = match query.to_str() {
            Err(_not_utf8) => Err(Status::BadArgument),
            Ok(name) if attributes => rust.attributes(name).map_err(|e| Status::from(&e)),
            Ok(name) => rust.layer(name).map_err(|e| Status::from(&e)),
        };
        if layer.is_null() {
            let status = check(error.code, &mut error);
            tc.event(format!("open {status:?}"));
            assert_eq!(Err(status), expected.map(|_| ()), "{query:?}");
            continue;
        }
        check(Status::Ok, &mut error);
        tc.event("open Ok");
        let rust_layer = expected.unwrap();
        assert_eq!(
            report_c(layer, &mut error),
            report_rust(&rust_layer),
            "{query:?}"
        );
        // SAFETY: a live layer handle, freed once.
        unsafe { gpkg_layer_free(layer) };
    }
    // SAFETY: a live container with no children.
    let status = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(check(status, &mut error), Status::Ok);
}

/// A column name argument, as drawn.
#[derive(Debug)]
enum Column {
    Name(CString),
    NotUtf8,
    Null,
}

/// Opening a layer with a projection of any column names agrees with the
/// Rust API: a NULL or non-UTF-8 name is `BAD_ARGUMENT`, any other outcome is
/// the Rust outcome, and an open projected layer reports what the projected
/// Rust layer reports.
#[hegel::test]
fn projected_opens_agree_with_the_rust_api(tc: hegel::TestCase) {
    let mut error = sentinel();
    let path = CString::new(":memory:").unwrap();
    // SAFETY: a NUL-terminated path and a writable error slot.
    let gpkg: *mut gpkg_t = unsafe { gpkg_create(path.as_ptr(), &raw mut error) };
    assert!(!gpkg.is_null());
    // SAFETY: a live container handle; `gpkg_t` is the container.
    let rust: &GeoPackage = unsafe { (*gpkg).gpkg() };
    build(&tc, rust);
    let tables: Vec<String> = rust
        .contents()
        .unwrap()
        .into_iter()
        .map(|entry| entry.table_name)
        .collect();
    // Mostly a table in the file, opened as the kind it is.
    let (query, attributes) = if !tables.is_empty() && tc.draw(generators::weighted_booleans(0.85))
    {
        let table = tc.draw(generators::sampled_from(tables.clone()));
        let spatial = rust.layer(&table).is_ok();
        (CString::new(table).unwrap(), !spatial)
    } else {
        (draw_query(&tc, &tables), tc.draw(generators::booleans()))
    };
    let count = tc.draw(generators::integers::<usize>().min_value(0).max_value(4));
    // The queried table's own columns, if it has any, and some others.
    let own: Vec<String> = query
        .to_str()
        .ok()
        .and_then(|name| rust.layer(name).or_else(|_| rust.attributes(name)).ok())
        .map(|layer| {
            layer
                .schema()
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect()
        })
        .unwrap_or_default();
    let mut candidates: Vec<String> = own.clone();
    candidates.extend(own);
    candidates.extend(["geom", "fid", "absent", "Ünïcödé"].map(str::to_owned));
    let columns: Vec<Column> = (0..count)
        .map(
            |_| match tc.draw(generators::integers::<u8>().min_value(0).max_value(29)) {
                0 => Column::NotUtf8,
                1 => Column::Null,
                _ => Column::Name(
                    CString::new(tc.draw(generators::sampled_from(candidates.clone()))).unwrap(),
                ),
            },
        )
        .collect();
    let not_utf8 = CString::new([b'c', 0xFF]).unwrap();
    let pointers: Vec<*const c_char> = columns
        .iter()
        .map(|column| match column {
            Column::Name(name) => name.as_ptr(),
            Column::NotUtf8 => not_utf8.as_ptr(),
            Column::Null => std::ptr::null(),
        })
        .collect();
    let layer = if attributes {
        // SAFETY: a live container, a NUL-terminated name, and `pointers`
        // readable for its length, each NULL or NUL-terminated.
        unsafe {
            geopackage_ffi::gpkg_attributes_open_with_columns(
                gpkg,
                query.as_ptr(),
                pointers.as_ptr(),
                pointers.len(),
                &raw mut error,
            )
        }
    } else {
        // SAFETY: as above.
        unsafe {
            geopackage_ffi::gpkg_layer_open_with_columns(
                gpkg,
                query.as_ptr(),
                pointers.as_ptr(),
                pointers.len(),
                &raw mut error,
            )
        }
    };
    let expected = match query.to_str() {
        Err(_not_utf8) => Err(Status::BadArgument),
        Ok(name) => {
            let opened = if attributes {
                rust.attributes(name)
            } else {
                rust.layer(name)
            };
            // Every argument is checked before the layer is looked up.
            match (
                opened,
                columns.iter().any(|c| !matches!(c, Column::Name(_))),
            ) {
                (_, true) => Err(Status::BadArgument),
                (Err(e), false) => Err(Status::from(&e)),
                (Ok(layer), false) => {
                    let names: Vec<&str> = columns
                        .iter()
                        .filter_map(|c| match c {
                            Column::Name(name) => name.to_str().ok(),
                            _ => None,
                        })
                        .collect();
                    layer.with_columns(&names).map_err(|e| Status::from(&e))
                }
            }
        }
    };
    if layer.is_null() {
        let status = check(error.code, &mut error);
        tc.event(format!("open {status:?}"));
        assert_eq!(Err(status), expected.map(|_| ()), "{query:?} {columns:?}");
    } else {
        check(Status::Ok, &mut error);
        tc.event("open Ok");
        let rust_layer = expected.unwrap();
        assert_eq!(
            report_c(layer, &mut error),
            report_rust(&rust_layer),
            "{query:?} {columns:?}"
        );
        // SAFETY: a live layer handle, freed once.
        unsafe { gpkg_layer_free(layer) };
    }
    // SAFETY: a live container with no children.
    let status = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(check(status, &mut error), Status::Ok);
}

/// `gpkg_layer_repair_spatial_index` does what its documentation says for
/// each index state: a stale or legacy index becomes current and consistent,
/// and a current or absent one is left as it is; the call succeeds either way.
#[hegel::test]
fn repair_follows_the_documented_rules(tc: hegel::TestCase) {
    let mut error = sentinel();
    let path = CString::new(":memory:").unwrap();
    // SAFETY: a NUL-terminated path and a writable error slot.
    let gpkg: *mut gpkg_t = unsafe { gpkg_create(path.as_ptr(), &raw mut error) };
    assert!(!gpkg.is_null());
    // SAFETY: a live container handle.
    let rust: &GeoPackage = unsafe { (*gpkg).gpkg() };
    let layer = rust
        .create_layer(
            &TableSchemaBuilder::new("pts")
                .geometry(GeometrySpec::new(GeometryType::Point, 4326))
                .spatial_index(true),
        )
        .unwrap();
    let mut writer = layer.writer().unwrap();
    let rows = tc.draw(generators::integers::<usize>().min_value(0).max_value(5));
    for i in 0..rows {
        let x = f64::from(u32::try_from(i).unwrap());
        writer.insert_wkb(None, &point(x, x), &[]).unwrap();
    }
    writer.commit().unwrap();
    let sql = |statement: &str| rust.connection().execute_batch(statement).unwrap();
    let triggers = |pattern: &str| -> Vec<String> {
        rust.connection()
            .prepare("SELECT name FROM sqlite_master WHERE type = 'trigger' AND name LIKE ?1")
            .unwrap()
            .query_map([pattern], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(4)) {
        0 => {}
        1 => layer.drop_spatial_index().unwrap(),
        2 => {
            for trigger in triggers("rtree_pts_geom_%") {
                sql(&format!("DROP TRIGGER \"{trigger}\""));
            }
        }
        3 => sql("DROP TABLE rtree_pts_geom"),
        _ => sql("CREATE TRIGGER rtree_pts_geom_update1 AFTER UPDATE ON pts BEGIN SELECT 1; END"),
    }
    let before = layer.spatial_index_status().unwrap();
    tc.event(before.as_str());
    let name = CString::new("pts").unwrap();
    // SAFETY: a live container and a NUL-terminated name.
    let handle = unsafe { gpkg_layer_open(gpkg, name.as_ptr(), &raw mut error) };
    assert!(!handle.is_null());
    // SAFETY: a live layer handle.
    let status = unsafe { geopackage_ffi::gpkg_layer_repair_spatial_index(handle, &raw mut error) };
    assert_eq!(check(status, &mut error), Status::Ok, "{before:?}");
    let after = layer.spatial_index_status().unwrap();
    match before {
        geopackage::SpatialIndexStatus::Stale | geopackage::SpatialIndexStatus::Legacy => {
            assert_eq!(after, geopackage::SpatialIndexStatus::Current);
            assert!(layer.audit_spatial_index().unwrap().is_consistent());
        }
        _ => assert_eq!(after, before),
    }
    // SAFETY: a live layer handle, freed once.
    unsafe { gpkg_layer_free(handle) };
    drop(layer);
    // SAFETY: a live container with no children.
    let status = unsafe { gpkg_close(gpkg, &raw mut error) };
    assert_eq!(check(status, &mut error), Status::Ok);
}
