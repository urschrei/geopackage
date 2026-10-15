//! Property tests over the columnar read path.
//!
//! The oracle for `read_arrow` is the row path, `Layer::features`, over the
//! same table: the two read the same stored values, and the module
//! documentation of `geopackage::arrow` gives the type each value becomes. The
//! tables are built with SQL, so the stored values are not limited to what this
//! crate writes: every declared type in the vocabulary and several outside it,
//! and every storage class in every column.

#![cfg(feature = "arrow")]
#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::path::Path;

use arrow_array::cast::AsArray;
use arrow_array::types::{Date32Type, Float64Type, Int64Type, TimestampMicrosecondType};
use arrow_array::{Array, ArrayRef, RecordBatch};
use arrow_schema::DataType;
use geo_types::Point;
use geopackage::arrow::ArrowReadOptions;
use geopackage::core::geometry::encode_gpb;
use geopackage::core::gpb::body_offset;
use geopackage::{ConversionOptions, DateTimeParsing, GeoPackage, StorageStrictness, ValueRef};
use hegel::generators;
use rusqlite::types::Value as Sql;

/// Declared column types: the spec vocabulary, and names outside it that real
/// files use, which map by SQLite's affinity rules. The empty name declares no
/// type.
const DECLARED: [&str; 22] = [
    "BOOLEAN",
    "TINYINT",
    "SMALLINT",
    "MEDIUMINT",
    "INT",
    "INTEGER",
    "FLOAT",
    "DOUBLE",
    "REAL",
    "TEXT",
    "TEXT(8)",
    "BLOB",
    "BLOB(4)",
    "DATE",
    "DATETIME",
    "VARCHAR(20)",
    "BIGINT",
    "NUMERIC",
    "DECIMAL(5,2)",
    "CLOB",
    "FLOATING POINT",
    "",
];

/// One cell as both paths can express it, compared exactly.
#[derive(Debug, Clone, PartialEq)]
enum Cell {
    Null,
    Bool(bool),
    Int(i64),
    /// The bits of an `f64`, so that the comparison is exact.
    Float(u64),
    Text(String),
    Bytes(Vec<u8>),
    Days(i32),
    Micros(i64),
}

/// Draws text that is a date, a strict or lenient `DATETIME`, a number, or
/// anything.
fn draw_text(tc: &hegel::TestCase) -> String {
    let year = tc.draw(generators::integers::<u16>().min_value(0).max_value(9999));
    let month = tc.draw(generators::integers::<u8>().min_value(1).max_value(12));
    let day = tc.draw(generators::integers::<u8>().min_value(1).max_value(28));
    let date = format!("{year:04}-{month:02}-{day:02}");
    let time = format!(
        "{:02}:{:02}:{:02}",
        tc.draw(generators::integers::<u8>().min_value(0).max_value(23)),
        tc.draw(generators::integers::<u8>().min_value(0).max_value(59)),
        tc.draw(generators::integers::<u8>().min_value(0).max_value(59)),
    );
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(5)) {
        0 => date,
        1 => format!(
            "{date}T{time}.{:03}Z",
            tc.draw(generators::integers::<u16>().min_value(0).max_value(999))
        ),
        2 => format!(
            "{date} {time}.{}+01:30",
            tc.draw(
                generators::integers::<u32>()
                    .min_value(0)
                    .max_value(999_999_999)
            )
        ),
        3 => tc.draw(generators::integers::<i64>()).to_string(),
        4 => tc
            .draw(generators::floats::<f64>().allow_nan(false))
            .to_string(),
        _ => tc.draw(generators::text().max_size(12)),
    }
}

/// Draws a value of any storage class to bind into a column.
fn draw_value(tc: &hegel::TestCase) -> Sql {
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(5)) {
        0 => Sql::Null,
        1 => Sql::Integer(tc.draw(generators::integers::<i64>().min_value(-1).max_value(2))),
        2 => Sql::Integer(tc.draw(generators::integers::<i64>())),
        3 => Sql::Real(tc.draw(generators::floats::<f64>())),
        4 => Sql::Text(draw_text(tc)),
        _ => Sql::Blob(tc.draw(generators::vecs(generators::integers::<u8>()).max_size(12))),
    }
}

/// Draws a value that suits the declared type: what a conforming writer
/// stores, by the type's affinity for the names outside the vocabulary.
fn draw_fitting_value(tc: &hegel::TestCase, declared: &str) -> Sql {
    let upper = declared.to_ascii_uppercase();
    let text = |tc: &hegel::TestCase| loop {
        let text = draw_text(tc);
        if tc.draw(generators::booleans()) || text.parse::<f64>().is_err() {
            return text;
        }
    };
    match upper.as_str() {
        "BOOLEAN" => Sql::Integer(tc.draw(generators::integers::<i64>().min_value(0).max_value(1))),
        "DATE" | "DATETIME" => Sql::Text(draw_text(tc)),
        _ if upper.contains("INT") => Sql::Integer(tc.draw(generators::integers::<i64>())),
        _ if ["CHAR", "CLOB", "TEXT"].iter().any(|t| upper.contains(t)) => Sql::Text(text(tc)),
        _ if upper.contains("BLOB") || upper.is_empty() => {
            Sql::Blob(tc.draw(generators::vecs(generators::integers::<u8>()).max_size(12)))
        }
        _ => Sql::Real(tc.draw(generators::floats::<f64>())),
    }
}

/// Draws a geometry cell: usually a GPB point, sometimes NULL, a blob that is
/// not GPB, or a value of another storage class.
fn draw_geometry(tc: &hegel::TestCase) -> Sql {
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(9)) {
        0 => Sql::Null,
        1 => Sql::Blob(tc.draw(generators::vecs(generators::integers::<u8>()).max_size(12))),
        2 => Sql::Text("POINT(1 2)".into()),
        _ => {
            let point = Point::new(
                tc.draw(generators::floats::<f64>().min_value(-1e3).max_value(1e3)),
                tc.draw(generators::floats::<f64>().min_value(-1e3).max_value(1e3)),
            );
            Sql::Blob(encode_gpb(&point, 4326).unwrap().0)
        }
    }
}

/// Creates a feature table `t` with a geometry column and one column of each
/// drawn declared type, registered in the catalogue, and fills it with drawn
/// rows. Returns the declared types.
fn draw_table(tc: &hegel::TestCase, gpkg: &GeoPackage) -> Vec<&'static str> {
    let count = tc.draw(generators::integers::<usize>().min_value(1).max_value(5));
    let declared: Vec<&'static str> = (0..count)
        .map(|_| tc.draw(generators::sampled_from(DECLARED.to_vec())))
        .collect();
    let columns: String = declared
        .iter()
        .enumerate()
        .map(|(i, ty)| format!(", c{i} {ty}"))
        .collect();
    let conn = gpkg.connection();
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS gpkg_geometry_columns (\
           table_name TEXT NOT NULL, column_name TEXT NOT NULL, \
           geometry_type_name TEXT NOT NULL, srs_id INTEGER NOT NULL, \
           z TINYINT NOT NULL, m TINYINT NOT NULL);\
         CREATE TABLE t (fid INTEGER PRIMARY KEY, geom GEOMETRY{columns});\
         INSERT INTO gpkg_contents (table_name, data_type, srs_id) VALUES ('t', 'features', 4326);\
         INSERT INTO gpkg_geometry_columns VALUES ('t', 'geom', 'GEOMETRY', 4326, 0, 0);"
    ))
    .unwrap();

    let rows = tc.draw(generators::integers::<i64>().min_value(0).max_value(12));
    let placeholders: String = (0..count).map(|i| format!(", ?{}", i + 3)).collect();
    let sql = format!("INSERT INTO t VALUES (?1, ?2{placeholders})");
    let mut fid = 0;
    for _ in 0..rows {
        // Gaps in the key, which pagination must read through.
        fid += tc.draw(generators::integers::<i64>().min_value(1).max_value(3));
        let mut values = vec![Sql::Integer(fid), draw_geometry(tc)];
        values.extend(declared.iter().map(|ty| {
            if tc.draw(generators::weighted_booleans(0.9)) {
                draw_fitting_value(tc, ty)
            } else {
                draw_value(tc)
            }
        }));
        conn.execute(&sql, rusqlite::params_from_iter(values))
            .unwrap();
    }
    declared
}

/// Draws conversion options: the default, or either setting changed.
fn draw_options(tc: &hegel::TestCase) -> ConversionOptions {
    ConversionOptions::default()
        .with_datetime(tc.draw(generators::sampled_from(vec![
            DateTimeParsing::Strict,
            DateTimeParsing::Lenient,
        ])))
        .with_storage(tc.draw(generators::sampled_from(vec![
            StorageStrictness::Lenient,
            StorageStrictness::Strict,
        ])))
}

/// The cell an Arrow array of `data_type` holds for a row-path value, by the
/// documented mapping, or `None` where the value's storage class does not fit
/// the array (`Error::ArrowValueMismatch`).
fn expected_cell(value: ValueRef<'_>, data_type: &DataType) -> Option<Cell> {
    Some(match (value, data_type) {
        (ValueRef::Null, _) => Cell::Null,
        (ValueRef::Boolean(b), DataType::Boolean) => Cell::Bool(b),
        (ValueRef::Integer(i), DataType::Boolean) => Cell::Bool(i != 0),
        (ValueRef::Integer(i), DataType::Int64) => Cell::Int(i),
        (ValueRef::Float(f), DataType::Float64) => Cell::Float(f.to_bits()),
        (ValueRef::Integer(i), DataType::Float64) => Cell::Float((i as f64).to_bits()),
        (ValueRef::Text(s), DataType::Utf8) => Cell::Text(s.to_owned()),
        (ValueRef::Blob(b), DataType::Binary) => Cell::Bytes(b.to_vec()),
        (ValueRef::Date(d), DataType::Date32) => Cell::Days(d.days_since_epoch()),
        (ValueRef::DateTime(dt), DataType::Timestamp(..)) => {
            Cell::Micros(dt.micros_since_epoch().unwrap())
        }
        _ => return None,
    })
}

/// The cell at `row` of an Arrow array.
fn arrow_cell(array: &ArrayRef, row: usize) -> Cell {
    if array.is_null(row) {
        return Cell::Null;
    }
    match array.data_type() {
        DataType::Boolean => Cell::Bool(array.as_boolean().value(row)),
        DataType::Int64 => Cell::Int(array.as_primitive::<Int64Type>().value(row)),
        DataType::Float64 => Cell::Float(array.as_primitive::<Float64Type>().value(row).to_bits()),
        DataType::Utf8 => Cell::Text(array.as_string::<i32>().value(row).to_owned()),
        DataType::Binary => Cell::Bytes(array.as_binary::<i32>().value(row).to_vec()),
        DataType::Date32 => Cell::Days(array.as_primitive::<Date32Type>().value(row)),
        DataType::Timestamp(..) => {
            Cell::Micros(array.as_primitive::<TimestampMicrosecondType>().value(row))
        }
        // A type the mapping never produces, which then compares unequal.
        other => Cell::Text(format!("unexpected Arrow type {other}")),
    }
}

/// A row as the row path reads it, mapped to the cells an Arrow batch of
/// `schema` should hold, or `None` if Arrow should reject the row.
type Row = Option<Vec<Cell>>;

/// Reads every row through `Layer::features` and maps it by the documented
/// mapping. The fields after the key are, in table order, the geometry and the
/// value columns. A geometry blob that is not GPB fails the Arrow read, which
/// must remove its header; any other geometry cell reads as on the row path.
fn expected_rows(
    gpkg: &GeoPackage,
    options: ConversionOptions,
    schema: &arrow_schema::Schema,
) -> Vec<Result<Row, String>> {
    let layer = gpkg.layer("t").unwrap().with_conversion_options(options);
    let types: Vec<&DataType> = schema.fields().iter().map(|f| f.data_type()).collect();
    let geometry_blobs: Vec<Option<Vec<u8>>> = gpkg
        .connection()
        .prepare("SELECT geom FROM t ORDER BY fid")
        .unwrap()
        .query_map([], |row| {
            Ok(match row.get_ref(0)? {
                rusqlite::types::ValueRef::Blob(b) => Some(b.to_vec()),
                // NULL, or a value of another storage class, which both paths
                // read as no geometry.
                _ => None,
            })
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    layer
        .features()
        .unwrap()
        .zip(geometry_blobs)
        .map(|(feature, geometry)| {
            let feature = feature.map_err(|e| e.to_string())?;
            let mut cells = vec![Cell::Int(feature.fid())];
            match geometry {
                None => cells.push(Cell::Null),
                Some(blob) => match body_offset(&blob) {
                    Ok(offset) => {
                        cells.push(Cell::Bytes(blob.get(offset..).unwrap_or_default().to_vec()))
                    }
                    _ => return Ok(None),
                },
            }
            for (value, data_type) in feature.values().zip(types.iter().skip(2)) {
                match expected_cell(value, data_type) {
                    Some(cell) => cells.push(cell),
                    None => return Ok(None),
                }
            }
            Ok(Some(cells))
        })
        .collect()
}

/// The rows of a batch, as cells.
fn batch_rows(batch: &RecordBatch) -> Vec<Vec<Cell>> {
    (0..batch.num_rows())
        .map(|row| {
            batch
                .columns()
                .iter()
                .map(|array| arrow_cell(array, row))
                .collect()
        })
        .collect()
}

/// `read_arrow` agrees with the row path on every row of any table: each row
/// the row path reads arrives with the cell the documented mapping gives its
/// value, and the read fails, after delivering the rows before it, at the
/// first row that the row path cannot read or whose value does not fit its
/// Arrow type.
#[hegel::test]
fn read_arrow_agrees_with_the_row_path(tc: hegel::TestCase) {
    let gpkg = GeoPackage::create(Path::new(":memory:")).unwrap();
    let declared = draw_table(&tc, &gpkg);
    let options = draw_options(&tc);
    let layer = gpkg.layer("t").unwrap().with_conversion_options(options);
    let schema = layer.arrow_schema().unwrap();
    let expected = expected_rows(&gpkg, options, &schema);

    let batch_size = tc.draw(generators::integers::<usize>().min_value(1).max_value(5));
    let mut delivered: Vec<Vec<Cell>> = Vec::new();
    let mut failed = false;
    for batch in layer
        .read_arrow(ArrowReadOptions::with_batch_size(batch_size).with_threads(1))
        .unwrap()
    {
        match batch {
            Ok(batch) => {
                assert_eq!(batch.schema(), schema);
                delivered.extend(batch_rows(&batch));
            }
            Err(_) => {
                failed = true;
                break;
            }
        }
    }

    let first_bad = expected
        .iter()
        .position(|row| !matches!(row, Ok(Some(_))))
        .unwrap_or(expected.len());
    let good: Vec<Vec<Cell>> = expected[..first_bad]
        .iter()
        .map(|row| row.clone().unwrap().unwrap())
        .collect();
    tc.event(match (first_bad, expected.len()) {
        (_, 0) => "an empty table",
        (bad, len) if bad == len => "every row reads",
        (0, _) => "the first row fails",
        _ => "a later row fails",
    });
    let context = format!("{declared:?} {options:?} expected {expected:?}");
    assert_eq!(failed, first_bad < expected.len(), "{context}");
    assert!(delivered.len() <= good.len(), "{context}");
    assert_eq!(delivered, good[..delivered.len()], "{context}");
    if !failed {
        assert_eq!(delivered, good, "{context}");
    }
}
