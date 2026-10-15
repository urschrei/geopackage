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
    clippy::indexing_slicing,
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
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{
    BoundingBox, ColumnSpec, ConversionOptions, DateTimeParsing, GeoPackage, GeometrySpec,
    NewFeature, StorageStrictness, TableSchemaBuilder, Value, ValueRef,
};
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
                    Ok(offset) => cells.push(Cell::Bytes(blob[offset..].to_vec())),
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

// --- the write path -----------------------------------------------------------

/// The attribute column types of the spec vocabulary, one of each Arrow type
/// a read produces.
const COLUMN_TYPES: [ColumnType; 7] = [
    ColumnType::Boolean,
    ColumnType::Integer,
    ColumnType::Double,
    ColumnType::Text(None),
    ColumnType::Blob(None),
    ColumnType::Date,
    ColumnType::DateTime,
];

/// A layer `t` with a geometry column and one column of each drawn type.
fn typed_layer(tc: &hegel::TestCase, gpkg: &GeoPackage, name: &str) -> Vec<ColumnType> {
    let count = tc.draw(generators::integers::<usize>().min_value(0).max_value(4));
    let types: Vec<ColumnType> = (0..count)
        .map(|_| tc.draw(generators::sampled_from(COLUMN_TYPES.to_vec())))
        .collect();
    let mut builder =
        TableSchemaBuilder::new(name).geometry(GeometrySpec::new(GeometryType::Geometry, 4326));
    for (i, ty) in types.iter().enumerate() {
        builder = builder.column(ColumnSpec::new(format!("c{i}"), ty.clone()));
    }
    gpkg.create_layer(&builder).unwrap();
    types
}

/// The range of `Date32` days and microsecond timestamps a GeoPackage
/// `DATE` and `DATETIME` can hold: years 0 to 9999.
const FIRST_DAY: i32 = -719_528;
const LAST_DAY: i32 = 2_932_896;

/// Draws one Arrow cell of the type a read of `ty` produces, from the whole
/// domain the type can store, NaN and the infinities included for floats.
fn draw_arrow_cell(tc: &hegel::TestCase, ty: &ColumnType) -> Cell {
    if tc.draw(generators::weighted_booleans(0.15)) {
        return Cell::Null;
    }
    match ty {
        ColumnType::Boolean => Cell::Bool(tc.draw(generators::booleans())),
        ColumnType::Integer => Cell::Int(tc.draw(generators::integers::<i64>())),
        ColumnType::Double => Cell::Float(tc.draw(generators::floats::<f64>()).to_bits()),
        ColumnType::Text(_) => Cell::Text(tc.draw(generators::text().max_size(12))),
        ColumnType::Blob(_) => {
            Cell::Bytes(tc.draw(generators::vecs(generators::integers::<u8>()).max_size(12)))
        }
        ColumnType::Date => Cell::Days(
            tc.draw(
                generators::integers::<i32>()
                    .min_value(FIRST_DAY)
                    .max_value(LAST_DAY),
            ),
        ),
        _ => Cell::Micros(
            tc.draw(
                generators::integers::<i64>()
                    .min_value(i64::from(FIRST_DAY) * 86_400_000_000)
                    .max_value((i64::from(LAST_DAY) + 1) * 86_400_000_000 - 1),
            ),
        ),
    }
}

/// Builds an Arrow array of `data_type` from cells.
fn array_of(data_type: &DataType, cells: &[Cell]) -> ArrayRef {
    use arrow_array::{
        BinaryArray, BooleanArray, Date32Array, Float64Array, Int64Array, StringArray,
        TimestampMicrosecondArray,
    };
    macro_rules! column {
        ($variant:ident, $array:ty) => {
            std::sync::Arc::new(<$array>::from(
                cells
                    .iter()
                    .map(|cell| match cell {
                        Cell::$variant(v) => Some(v.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
            )) as ArrayRef
        };
    }
    match data_type {
        DataType::Boolean => column!(Bool, BooleanArray),
        DataType::Int64 => column!(Int, Int64Array),
        DataType::Float64 => std::sync::Arc::new(Float64Array::from(
            cells
                .iter()
                .map(|cell| match cell {
                    Cell::Float(bits) => Some(f64::from_bits(*bits)),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        )),
        DataType::Utf8 => column!(Text, StringArray),
        DataType::Binary => std::sync::Arc::new(BinaryArray::from(
            cells
                .iter()
                .map(|cell| match cell {
                    Cell::Bytes(b) => Some(b.as_slice()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        )),
        DataType::Date32 => column!(Days, Date32Array),
        _ => std::sync::Arc::new(
            TimestampMicrosecondArray::from(
                cells
                    .iter()
                    .map(|cell| match cell {
                        Cell::Micros(v) => Some(*v),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
            )
            .with_timezone("UTC"),
        ),
    }
}

/// Draws the rows of a write to a layer of `schema`, each in field order:
/// increasing feature ids with gaps, a WKB geometry or null, and a cell for
/// each attribute `c<i>` of type `types[i]`.
fn draw_arrow_rows(
    tc: &hegel::TestCase,
    schema: &arrow_schema::Schema,
    types: &[ColumnType],
) -> Vec<Vec<Cell>> {
    let rows = tc.draw(generators::integers::<usize>().min_value(0).max_value(12));
    let mut fid = 0;
    (0..rows)
        .map(|_| {
            fid += tc.draw(generators::integers::<i64>().min_value(1).max_value(3));
            schema
                .fields()
                .iter()
                .map(|field| match field.name().as_str() {
                    "fid" => Cell::Int(fid),
                    "geom" => {
                        if tc.draw(generators::weighted_booleans(0.15)) {
                            Cell::Null
                        } else {
                            let point = Point::new(
                                tc.draw(generators::floats::<f64>().min_value(-1e3).max_value(1e3)),
                                tc.draw(generators::floats::<f64>().min_value(-1e3).max_value(1e3)),
                            );
                            let blob = encode_gpb(&point, 4326).unwrap().0;
                            Cell::Bytes(blob[body_offset(&blob).unwrap()..].to_vec())
                        }
                    }
                    name => {
                        let index: usize = name.trim_start_matches('c').parse().unwrap();
                        draw_arrow_cell(tc, &types[index])
                    }
                })
                .collect()
        })
        .collect()
}

/// Splits rows into record batches of `schema` of drawn sizes.
fn batches_of(
    tc: &hegel::TestCase,
    schema: &arrow_schema::SchemaRef,
    rows: &[Vec<Cell>],
) -> Vec<RecordBatch> {
    let mut batches = Vec::new();
    let mut start = 0;
    while start < rows.len() {
        let size = tc.draw(
            generators::integers::<usize>()
                .min_value(1)
                .max_value(rows.len() - start),
        );
        let chunk = &rows[start..start + size];
        let columns: Vec<ArrayRef> = schema
            .fields()
            .iter()
            .enumerate()
            .map(|(i, field)| {
                let cells: Vec<Cell> = chunk.iter().map(|row| row[i].clone()).collect();
                array_of(field.data_type(), &cells)
            })
            .collect();
        batches.push(RecordBatch::try_new(std::sync::Arc::clone(schema), columns).unwrap());
        start += size;
    }
    batches
}

/// Reads a layer's rows back as cells.
fn read_back(gpkg: &GeoPackage, name: &str) -> Vec<Vec<Cell>> {
    gpkg.layer(name)
        .unwrap()
        .read_arrow(ArrowReadOptions::default().with_threads(1))
        .unwrap()
        .flat_map(|batch| batch_rows(&batch.unwrap()))
        .collect()
}

/// A cell as a write stores it, by the documented storage rules: SQLite
/// stores a NaN as NULL and a negative zero as zero, and a `DATETIME` is
/// stored in the strict form, floored to the millisecond.
fn as_stored(cell: &Cell) -> Cell {
    match cell {
        Cell::Float(bits) => {
            let value = f64::from_bits(*bits);
            if value.is_nan() {
                Cell::Null
            } else if value == 0.0 {
                Cell::Float(0.0_f64.to_bits())
            } else {
                cell.clone()
            }
        }
        Cell::Micros(micros) => Cell::Micros(micros.div_euclid(1_000) * 1_000),
        other => other.clone(),
    }
}

/// A write of drawn record batches reads back as the same rows, every cell
/// exactly as the documented storage rules leave it, and returns the feature
/// ids it wrote.
#[hegel::test]
fn write_arrow_reads_back_as_written(tc: hegel::TestCase) {
    let gpkg = GeoPackage::create(Path::new(":memory:")).unwrap();
    let types = typed_layer(&tc, &gpkg, "t");
    let layer = gpkg.layer("t").unwrap();
    let schema = layer.arrow_schema().unwrap();
    let rows = draw_arrow_rows(&tc, &schema, &types);
    let batches = batches_of(&tc, &schema, &rows);
    let batch_size = tc.draw(generators::integers::<usize>().min_value(0).max_value(5));

    let fids = layer
        .write_arrow(batches.into_iter().map(Ok), batch_size)
        .unwrap();
    let expected_fids: Vec<i64> = rows
        .iter()
        .filter_map(|row| match row[0] {
            Cell::Int(fid) => Some(fid),
            _ => None,
        })
        .collect();
    assert_eq!(fids, expected_fids);
    let stored: Vec<Vec<Cell>> = rows
        .iter()
        .map(|row| row.iter().map(as_stored).collect())
        .collect();
    assert_eq!(read_back(&gpkg, "t"), stored, "{types:?}");
}

/// Arrow types a producer may send, with a one-cell array of each.
fn arrow_types() -> Vec<DataType> {
    use arrow_schema::TimeUnit;
    vec![
        DataType::Boolean,
        DataType::Int8,
        DataType::Int16,
        DataType::Int32,
        DataType::Int64,
        DataType::UInt8,
        DataType::UInt16,
        DataType::UInt32,
        DataType::UInt64,
        DataType::Float32,
        DataType::Float64,
        DataType::Utf8,
        DataType::LargeUtf8,
        DataType::Binary,
        DataType::LargeBinary,
        DataType::Date32,
        DataType::Timestamp(TimeUnit::Second, Some("UTC".into())),
        DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
        DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
        DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
        DataType::Timestamp(TimeUnit::Microsecond, None),
    ]
}

/// A one-cell array of `data_type` holding a small value that every
/// conversion can represent.
fn one_cell(data_type: &DataType) -> ArrayRef {
    use arrow_array::*;
    use arrow_schema::TimeUnit;
    use std::sync::Arc;
    match data_type {
        DataType::Boolean => Arc::new(BooleanArray::from(vec![true])),
        DataType::Int8 => Arc::new(Int8Array::from(vec![1])),
        DataType::Int16 => Arc::new(Int16Array::from(vec![1])),
        DataType::Int32 => Arc::new(Int32Array::from(vec![1])),
        DataType::Int64 => Arc::new(Int64Array::from(vec![1])),
        DataType::UInt8 => Arc::new(UInt8Array::from(vec![1])),
        DataType::UInt16 => Arc::new(UInt16Array::from(vec![1])),
        DataType::UInt32 => Arc::new(UInt32Array::from(vec![1])),
        DataType::UInt64 => Arc::new(UInt64Array::from(vec![1])),
        DataType::Float32 => Arc::new(Float32Array::from(vec![1.5])),
        DataType::Float64 => Arc::new(Float64Array::from(vec![1.5])),
        DataType::Utf8 => Arc::new(StringArray::from(vec!["a"])),
        DataType::LargeUtf8 => Arc::new(LargeStringArray::from(vec!["a"])),
        DataType::Binary => Arc::new(BinaryArray::from(vec![b"a".as_slice()])),
        DataType::LargeBinary => Arc::new(LargeBinaryArray::from(vec![b"a".as_slice()])),
        DataType::Date32 => Arc::new(Date32Array::from(vec![1])),
        DataType::Timestamp(unit, tz) => {
            let array: ArrayRef = match unit {
                TimeUnit::Second => {
                    Arc::new(TimestampSecondArray::from(vec![1]).with_timezone_opt(tz.clone()))
                }
                TimeUnit::Millisecond => {
                    Arc::new(TimestampMillisecondArray::from(vec![1]).with_timezone_opt(tz.clone()))
                }
                TimeUnit::Microsecond => {
                    Arc::new(TimestampMicrosecondArray::from(vec![1]).with_timezone_opt(tz.clone()))
                }
                TimeUnit::Nanosecond => {
                    Arc::new(TimestampNanosecondArray::from(vec![1]).with_timezone_opt(tz.clone()))
                }
            };
            array
        }
        // A type `arrow_types` never lists.
        _ => std::sync::Arc::new(arrow_array::NullArray::new(1)) as ArrayRef,
    }
}

/// Whether the documented write rule accepts `data_type` into a column of
/// `declared`: the type a read produces, a narrower or unsigned integer, a
/// narrower float, the large forms of text and binary, or a timestamp in any
/// unit.
fn fits(declared: &ColumnType, data_type: &DataType) -> bool {
    match declared {
        ColumnType::Boolean => *data_type == DataType::Boolean,
        ColumnType::TinyInt
        | ColumnType::SmallInt
        | ColumnType::MediumInt
        | ColumnType::Integer => {
            matches!(
                data_type,
                DataType::Int8
                    | DataType::Int16
                    | DataType::Int32
                    | DataType::Int64
                    | DataType::UInt8
                    | DataType::UInt16
                    | DataType::UInt32
                    | DataType::UInt64
            )
        }
        ColumnType::Float | ColumnType::Double => {
            matches!(data_type, DataType::Float32 | DataType::Float64)
        }
        ColumnType::Text(_) => matches!(data_type, DataType::Utf8 | DataType::LargeUtf8),
        ColumnType::Blob(_) => matches!(data_type, DataType::Binary | DataType::LargeBinary),
        ColumnType::Date => *data_type == DataType::Date32,
        ColumnType::DateTime => matches!(data_type, DataType::Timestamp(..)),
        _ => false,
    }
}

/// `write_arrow` writes a cell of an Arrow type into a column exactly when the
/// type fits the column's declared type, and otherwise fails and writes
/// nothing.
#[hegel::test]
fn write_arrow_accepts_exactly_the_fitting_types(tc: hegel::TestCase) {
    let gpkg = GeoPackage::create(Path::new(":memory:")).unwrap();
    let declared = tc.draw(generators::sampled_from(vec![
        ColumnType::Boolean,
        ColumnType::TinyInt,
        ColumnType::SmallInt,
        ColumnType::MediumInt,
        ColumnType::Integer,
        ColumnType::Float,
        ColumnType::Double,
        ColumnType::Text(None),
        ColumnType::Blob(None),
        ColumnType::Date,
        ColumnType::DateTime,
    ]));
    gpkg.create_attributes_table(
        &TableSchemaBuilder::new("t").column(ColumnSpec::new("c", declared.clone())),
    )
    .unwrap();
    let data_type = tc.draw(generators::sampled_from(arrow_types()));
    let schema = std::sync::Arc::new(arrow_schema::Schema::new(vec![arrow_schema::Field::new(
        "c",
        data_type.clone(),
        true,
    )]));
    let batch = RecordBatch::try_new(schema, vec![one_cell(&data_type)]).unwrap();
    let result = gpkg
        .attributes("t")
        .unwrap()
        .write_arrow(vec![Ok(batch)], 0);
    let rows: i64 = gpkg
        .connection()
        .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
        .unwrap();
    if fits(&declared, &data_type) {
        assert!(result.is_ok(), "{declared:?} <- {data_type}: {result:?}");
        assert_eq!(rows, 1);
    } else {
        assert!(result.is_err(), "{declared:?} <- {data_type} was written");
        assert_eq!(rows, 0, "{declared:?} <- {data_type}");
    }
}

/// A layer made by `from_arrow_schema` accepts a batch of the schema it was
/// made from.
#[hegel::test]
fn a_layer_from_an_arrow_schema_accepts_that_schema(tc: hegel::TestCase) {
    let gpkg = GeoPackage::create(Path::new(":memory:")).unwrap();
    let data_type = tc.draw(generators::sampled_from(arrow_types()));
    let schema = std::sync::Arc::new(arrow_schema::Schema::new(vec![arrow_schema::Field::new(
        "c",
        data_type.clone(),
        true,
    )]));
    let builder = TableSchemaBuilder::new("t")
        .from_arrow_schema(&schema)
        .unwrap();
    gpkg.create_attributes_table(&builder).unwrap();
    let batch = RecordBatch::try_new(schema, vec![one_cell(&data_type)]).unwrap();
    let result = gpkg
        .attributes("t")
        .unwrap()
        .write_arrow(vec![Ok(batch)], 0);
    assert!(result.is_ok(), "{data_type}: {result:?}");
}

// --- read variants ------------------------------------------------------------

/// A points layer `p` with a text, an integer and a float column, and drawn
/// rows: feature ids with gaps, points in `[-50, 50]`, and nulls. The spatial
/// index is drawn too. Returns the number of rows.
fn draw_point_layer(tc: &hegel::TestCase, gpkg: &GeoPackage) -> usize {
    let layer = gpkg
        .create_layer(
            &TableSchemaBuilder::new("p")
                .column(ColumnSpec::new("name", ColumnType::Text(None)))
                .column(ColumnSpec::new("n", ColumnType::Integer))
                .column(ColumnSpec::new("x", ColumnType::Double))
                .geometry(GeometrySpec::new(GeometryType::Point, 4326))
                .spatial_index(tc.draw(generators::booleans())),
        )
        .unwrap();
    let count = tc.draw(generators::integers::<usize>().min_value(0).max_value(60));
    let mut fid = 0;
    let rows: Vec<NewFeature<Point<f64>>> = (0..count)
        .map(|_| {
            fid += tc.draw(generators::integers::<i64>().min_value(1).max_value(4));
            let maybe = |tc: &hegel::TestCase, value: Value| {
                if tc.draw(generators::weighted_booleans(0.1)) {
                    Value::Null
                } else {
                    value
                }
            };
            let values = vec![
                maybe(
                    tc,
                    Value::Text(
                        tc.draw(generators::sampled_from(vec!["a", "ab", "b", ""]))
                            .into(),
                    ),
                ),
                maybe(
                    tc,
                    Value::Integer(
                        tc.draw(generators::integers::<i64>().min_value(-5).max_value(5)),
                    ),
                ),
                maybe(
                    tc,
                    Value::Float(
                        tc.draw(generators::floats::<f64>().min_value(-1e3).max_value(1e3)),
                    ),
                ),
            ];
            let point = Point::new(
                tc.draw(generators::floats::<f64>().min_value(-50.0).max_value(50.0)),
                tc.draw(generators::floats::<f64>().min_value(-50.0).max_value(50.0)),
            );
            if tc.draw(generators::weighted_booleans(0.1)) {
                NewFeature::attributes(values).with_fid(fid)
            } else {
                NewFeature::new(point, values).with_fid(fid)
            }
        })
        .collect();
    layer.write_all(rows, 0).unwrap();
    count
}

/// The rows of a stream of batches, failing on an error.
fn arrow_rows(batches: geopackage::arrow::ArrowBatches<'_>) -> Vec<Vec<Cell>> {
    batches
        .flat_map(|batch| batch_rows(&batch.unwrap()))
        .collect()
}

/// Rows from the row path, as the cells of `schema`: the key, the geometry's
/// WKB body, and each value by the documented mapping.
fn feature_rows(features: geopackage::Features, schema: &arrow_schema::Schema) -> Vec<Vec<Cell>> {
    features
        .map(|feature| {
            let feature = feature.unwrap();
            schema
                .fields()
                .iter()
                .map(|field| match field.name().as_str() {
                    "fid" => Cell::Int(feature.fid()),
                    "geom" => match feature.geometry_bytes() {
                        Some(blob) => Cell::Bytes(blob[body_offset(blob).unwrap()..].to_vec()),
                        None => Cell::Null,
                    },
                    name => expected_cell(feature.value(name).unwrap(), field.data_type()).unwrap(),
                })
                .collect()
        })
        .collect()
}

/// A threaded read of a file returns the rows a single-threaded read does,
/// in key order. Where the key spans at least two batches, so that threads
/// are used, each batch spans fewer than `batch_size` key values; otherwise
/// each holds at most `batch_size` rows, as the threading documentation
/// states.
///
/// Each case creates a file and opens worker connections, which costs far
/// more per case on Windows CI (issue #44), so this property runs 200 cases
/// rather than the suite's 2000.
#[hegel::test(test_cases = 200)]
fn a_threaded_read_matches_a_single_threaded_one(tc: hegel::TestCase) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("p.gpkg")).unwrap();
    draw_point_layer(&tc, &gpkg);
    let layer = gpkg.layer("p").unwrap();
    let batch_size = tc.draw(generators::integers::<usize>().min_value(1).max_value(8));
    let threads = tc.draw(generators::integers::<usize>().min_value(2).max_value(4));
    let options = ArrowReadOptions::with_batch_size(batch_size);

    let single = arrow_rows(layer.read_arrow(options.with_threads(1)).unwrap());
    // Threads are used only when the key spans at least two batches.
    let all_keys: Vec<i64> = single
        .iter()
        .filter_map(|row| match row[0] {
            Cell::Int(key) => Some(key),
            _ => None,
        })
        .collect();
    let threaded_read = match (all_keys.first(), all_keys.last()) {
        (Some(first), Some(last)) => last - first >= 2 * batch_size as i64 - 1,
        _ => false,
    };
    tc.event(if threaded_read {
        "threaded"
    } else {
        "single-threaded"
    });
    let mut threaded = Vec::new();
    for batch in layer.read_arrow(options.with_threads(threads)).unwrap() {
        let rows = batch_rows(&batch.unwrap());
        let keys: Vec<i64> = rows
            .iter()
            .filter_map(|row| match row[0] {
                Cell::Int(key) => Some(key),
                _ => None,
            })
            .collect();
        match (keys.first(), keys.last()) {
            (Some(first), Some(last)) if threaded_read => assert!(
                last - first < batch_size as i64,
                "{keys:?} in one batch of {batch_size}"
            ),
            _ => assert!(
                keys.len() <= batch_size,
                "{keys:?} in one batch of {batch_size}"
            ),
        }
        threaded.extend(rows);
    }
    assert_eq!(threaded, single);
}

/// A single-threaded read yields batches of at most `batch_size` rows, and
/// geometry bytes over `max_batch_bytes` only in a batch of one row, and
/// loses nothing: together they are the row path's rows in key order.
#[hegel::test]
fn batches_respect_their_size_and_byte_ceiling(tc: hegel::TestCase) {
    let gpkg = GeoPackage::create(Path::new(":memory:")).unwrap();
    draw_point_layer(&tc, &gpkg);
    let layer = gpkg.layer("p").unwrap();
    let batch_size = tc.draw(generators::integers::<usize>().min_value(1).max_value(8));
    let max_bytes = tc.draw(generators::integers::<usize>().min_value(1).max_value(120));
    let options = ArrowReadOptions::with_batch_size(batch_size)
        .with_max_batch_bytes(max_bytes)
        .with_threads(1);
    let schema = layer.arrow_schema().unwrap();
    let geometry = schema.index_of("geom").unwrap();

    let mut all = Vec::new();
    for batch in layer.read_arrow(options).unwrap() {
        let batch = batch.unwrap();
        assert!(
            (1..=batch_size).contains(&batch.num_rows()),
            "{} rows",
            batch.num_rows()
        );
        let bytes = batch.column(geometry).as_binary::<i32>().values().len();
        assert!(
            bytes <= max_bytes || batch.num_rows() == 1,
            "{bytes} bytes over {max_bytes} in {} rows",
            batch.num_rows()
        );
        all.extend(batch_rows(&batch));
    }
    assert_eq!(all, feature_rows(layer.features().unwrap(), &schema));
}

/// Draws a query box over `[-60, 60]`, usually ordered, sometimes with a
/// NaN edge.
fn draw_box(tc: &hegel::TestCase) -> BoundingBox {
    let edge = |tc: &hegel::TestCase| {
        if tc.draw(generators::weighted_booleans(0.05)) {
            f64::NAN
        } else {
            tc.draw(generators::floats::<f64>().min_value(-60.0).max_value(60.0))
        }
    };
    let axis = |tc: &hegel::TestCase| {
        let (a, b) = (edge(tc), edge(tc));
        if b < a && tc.draw(generators::weighted_booleans(0.9)) {
            (b, a)
        } else {
            (a, b)
        }
    };
    let (min_x, max_x) = axis(tc);
    let (min_y, max_y) = axis(tc);
    BoundingBox::new(min_x, min_y, max_x, max_y)
}

/// Draws a `WHERE` clause over the layer's columns, with its parameters.
fn draw_clause(tc: &hegel::TestCase) -> (&'static str, Vec<ValueRef<'static>>) {
    let int = |tc: &hegel::TestCase| {
        ValueRef::Integer(tc.draw(generators::integers::<i64>().min_value(-6).max_value(6)))
    };
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(4)) {
        0 => ("n > ?1", vec![int(tc)]),
        1 => (
            "name LIKE ?1",
            vec![ValueRef::Text(
                tc.draw(generators::sampled_from(vec!["a%", "%b", ""])),
            )],
        ),
        2 => ("x IS NULL OR geom IS NULL", vec![]),
        3 => (
            "fid % ?1 = 0",
            vec![ValueRef::Integer(tc.draw(
                generators::integers::<i64>().min_value(1).max_value(4),
            ))],
        ),
        _ => ("n BETWEEN ?1 AND ?2", vec![int(tc), int(tc)]),
    }
}

/// The rows sorted by their key, the first cell.
fn by_key(mut rows: Vec<Vec<Cell>>) -> Vec<Vec<Cell>> {
    rows.sort_by_key(|row| match row[0] {
        Cell::Int(key) => key,
        _ => i64::MIN,
    });
    rows
}

/// `read_arrow_in` returns the rows `features_in` returns, cell for cell, in
/// key order, with or without a spatial index. `features_in` promises no
/// order, so its rows are sorted first.
#[hegel::test]
fn read_arrow_in_matches_features_in(tc: hegel::TestCase) {
    let gpkg = GeoPackage::create(Path::new(":memory:")).unwrap();
    draw_point_layer(&tc, &gpkg);
    let layer = gpkg.layer("p").unwrap();
    let schema = layer.arrow_schema().unwrap();
    let bbox = draw_box(&tc);
    let batch_size = tc.draw(generators::integers::<usize>().min_value(1).max_value(8));
    let arrow = arrow_rows(
        layer
            .read_arrow_in(bbox, ArrowReadOptions::with_batch_size(batch_size))
            .unwrap(),
    );
    assert_eq!(
        arrow,
        by_key(feature_rows(layer.features_in(bbox).unwrap(), &schema)),
        "{bbox:?}"
    );
}

/// `read_arrow_where` returns the rows `select` returns for the same clause
/// and parameters.
#[hegel::test]
fn read_arrow_where_matches_select(tc: hegel::TestCase) {
    let gpkg = GeoPackage::create(Path::new(":memory:")).unwrap();
    draw_point_layer(&tc, &gpkg);
    let layer = gpkg.layer("p").unwrap();
    let schema = layer.arrow_schema().unwrap();
    let (clause, params) = draw_clause(&tc);
    let batch_size = tc.draw(generators::integers::<usize>().min_value(1).max_value(8));
    let arrow = arrow_rows(
        layer
            .read_arrow_where(
                clause,
                &params,
                ArrowReadOptions::with_batch_size(batch_size),
            )
            .unwrap(),
    );
    assert_eq!(
        arrow,
        feature_rows(layer.select(clause, &params).unwrap(), &schema),
        "{clause}"
    );
}

/// `read_arrow_in_where` returns the rows of `features_in` that `select`
/// also returns, in key order.
#[hegel::test]
fn read_arrow_in_where_is_the_intersection(tc: hegel::TestCase) {
    let gpkg = GeoPackage::create(Path::new(":memory:")).unwrap();
    draw_point_layer(&tc, &gpkg);
    let layer = gpkg.layer("p").unwrap();
    let schema = layer.arrow_schema().unwrap();
    let bbox = draw_box(&tc);
    let (clause, params) = draw_clause(&tc);
    let selected = feature_rows(layer.select(clause, &params).unwrap(), &schema);
    let expected: Vec<Vec<Cell>> = by_key(feature_rows(layer.features_in(bbox).unwrap(), &schema))
        .into_iter()
        .filter(|row| selected.contains(row))
        .collect();
    let arrow = arrow_rows(
        layer
            .read_arrow_in_where(bbox, clause, &params, ArrowReadOptions::with_batch_size(3))
            .unwrap(),
    );
    assert_eq!(arrow, expected, "{bbox:?} {clause}");
}

/// A projected layer reads exactly the projected columns of a full read: the
/// key always, the geometry only if kept, and the named value columns in
/// table order.
#[hegel::test]
fn a_projected_read_is_the_projection_of_a_full_read(tc: hegel::TestCase) {
    let gpkg = GeoPackage::create(Path::new(":memory:")).unwrap();
    draw_point_layer(&tc, &gpkg);
    let full_layer = gpkg.layer("p").unwrap();
    let full_schema = full_layer.arrow_schema().unwrap();
    let full = arrow_rows(
        full_layer
            .read_arrow(ArrowReadOptions::default().with_threads(1))
            .unwrap(),
    );

    let names: Vec<&str> = ["name", "n", "x"]
        .into_iter()
        .filter(|_| tc.draw(generators::booleans()))
        .collect();
    let mut layer = gpkg.layer("p").unwrap().with_columns(&names).unwrap();
    if tc.draw(generators::booleans()) {
        layer = layer.without_geometry();
    }
    let schema = layer.arrow_schema().unwrap();
    let kept: Vec<usize> = schema
        .fields()
        .iter()
        .map(|field| full_schema.index_of(field.name()).unwrap())
        .collect();
    let expected: Vec<Vec<Cell>> = full
        .iter()
        .map(|row| kept.iter().map(|&i| row[i].clone()).collect())
        .collect();
    let projected = arrow_rows(
        layer
            .read_arrow(ArrowReadOptions::default().with_threads(1))
            .unwrap(),
    );
    assert_eq!(projected, expected, "{names:?}");
}

/// `write_arrow` and `write_all` store the same rows the same way: the two
/// tables are equal value for value and storage class for storage class.
#[hegel::test]
fn write_arrow_stores_what_write_all_stores(tc: hegel::TestCase) {
    let gpkg = GeoPackage::create(Path::new(":memory:")).unwrap();
    let types = typed_layer(&tc, &gpkg, "a");
    let mut builder =
        TableSchemaBuilder::new("b").geometry(GeometrySpec::new(GeometryType::Geometry, 4326));
    for (i, ty) in types.iter().enumerate() {
        builder = builder.column(ColumnSpec::new(format!("c{i}"), ty.clone()));
    }
    gpkg.create_layer(&builder).unwrap();
    let layer = gpkg.layer("a").unwrap();
    let schema = layer.arrow_schema().unwrap();
    let rows = draw_arrow_rows(&tc, &schema, &types);
    layer
        .write_arrow(batches_of(&tc, &schema, &rows).into_iter().map(Ok), 0)
        .unwrap();

    let other = gpkg.layer("b").unwrap();
    let mut writer = other.writer().unwrap();
    for row in &rows {
        let mut fid = None;
        let mut wkb = None;
        let mut values = Vec::new();
        for (field, cell) in schema.fields().iter().zip(row) {
            match (field.name().as_str(), cell) {
                ("fid", Cell::Int(id)) => fid = Some(*id),
                ("geom", Cell::Bytes(body)) => wkb = Some(body.clone()),
                ("geom", _) => {}
                (_, cell) => values.push(value_of(cell)),
            }
        }
        let refs: Vec<ValueRef<'_>> = values.iter().map(ValueRef::from).collect();
        match wkb {
            Some(body) => drop(writer.insert_wkb(fid, &body, &refs).unwrap()),
            None => drop(writer.insert_row(fid, &refs).unwrap()),
        }
    }
    writer.commit().unwrap();

    let dump = |table: &str| -> Vec<Vec<String>> {
        let columns: String = schema
            .fields()
            .iter()
            .map(|f| format!("typeof({0}), quote({0})", f.name()))
            .collect::<Vec<_>>()
            .join(", ");
        gpkg.connection()
            .prepare(&format!("SELECT {columns} FROM {table} ORDER BY fid"))
            .unwrap()
            .query_map([], |row| {
                (0..schema.fields().len() * 2)
                    .map(|i| {
                        row.get::<_, rusqlite::types::Value>(i)
                            .map(|v| format!("{v:?}"))
                    })
                    .collect()
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(dump("a"), dump("b"), "{types:?}");
}

/// The row-path value for a drawn Arrow cell of a typed column.
fn value_of(cell: &Cell) -> Value {
    use geopackage::core::datetime::{Date, DateTime};
    match cell {
        Cell::Null => Value::Null,
        Cell::Bool(b) => Value::Boolean(*b),
        Cell::Int(i) => Value::Integer(*i),
        Cell::Float(bits) => Value::Float(f64::from_bits(*bits)),
        Cell::Text(s) => Value::Text(s.clone()),
        Cell::Bytes(b) => Value::Blob(b.clone()),
        Cell::Days(d) => Value::Date(Date::from_days_since_epoch(*d).unwrap()),
        Cell::Micros(m) => Value::DateTime(DateTime::from_micros_since_epoch(*m).unwrap()),
    }
}
