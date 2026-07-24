//! `Value` conversion from stored SQLite values, driven by declared types.

use geopackage::core::datetime::{Date, DateTime};
use geopackage::core::types::ColumnType;
use geopackage::{ConversionOptions, Error, GeoPackage, Value};

/// A GeoPackage with a feature-style table carrying one column per gpkg type.
fn typed_gpkg() -> (tempfile::TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    gpkg.connection()
        .execute_batch(
            "CREATE TABLE things (\
               fid INTEGER PRIMARY KEY, \
               flag BOOLEAN, \
               count MEDIUMINT, \
               ratio DOUBLE, \
               label TEXT, \
               payload BLOB, \
               born DATE, \
               seen DATETIME, \
               geom POINT, \
               odd VARCHAR(20));",
        )
        .unwrap();
    (dir, gpkg)
}

fn insert(gpkg: &GeoPackage, sql: &str) {
    gpkg.connection().execute_batch(sql).unwrap();
}

#[test]
fn assorted_column_types_convert() {
    let (_dir, gpkg) = typed_gpkg();
    insert(
        &gpkg,
        "INSERT INTO things (fid, flag, count, ratio, label, payload, born, seen, odd) \
         VALUES (1, 1, 42, 3.5, 'hi', x'00ff', '2026-07-24', \
                 '2026-07-24T12:34:56.789Z', 'anything')",
    );

    assert_eq!(
        gpkg.column_values("things", "fid", ConversionOptions::strict())
            .unwrap(),
        vec![Value::Integer(1)]
    );
    assert_eq!(
        gpkg.column_values("things", "flag", ConversionOptions::strict())
            .unwrap(),
        vec![Value::Boolean(true)]
    );
    assert_eq!(
        gpkg.column_values("things", "count", ConversionOptions::strict())
            .unwrap(),
        vec![Value::Integer(42)]
    );
    assert_eq!(
        gpkg.column_values("things", "ratio", ConversionOptions::strict())
            .unwrap(),
        vec![Value::Float(3.5)]
    );
    assert_eq!(
        gpkg.column_values("things", "label", ConversionOptions::strict())
            .unwrap(),
        vec![Value::Text("hi".into())]
    );
    assert_eq!(
        gpkg.column_values("things", "payload", ConversionOptions::strict())
            .unwrap(),
        vec![Value::Blob(vec![0x00, 0xff])]
    );

    assert_eq!(
        gpkg.column_values("things", "born", ConversionOptions::strict())
            .unwrap(),
        vec![Value::Date(Date::parse("2026-07-24").unwrap())]
    );

    let seen = gpkg
        .column_values("things", "seen", ConversionOptions::strict())
        .unwrap();
    assert_eq!(
        seen,
        vec![Value::DateTime(
            DateTime::parse_strict("2026-07-24T12:34:56.789Z").unwrap()
        )]
    );

    // A declared type outside the vocabulary falls back to the storage class.
    assert_eq!(
        gpkg.column_values("things", "odd", ConversionOptions::strict())
            .unwrap(),
        vec![Value::Text("anything".into())]
    );
}

#[test]
fn null_is_null_for_any_type() {
    let (_dir, gpkg) = typed_gpkg();
    insert(&gpkg, "INSERT INTO things (fid) VALUES (1)");
    for col in ["flag", "count", "ratio", "label", "payload", "born", "seen"] {
        assert_eq!(
            gpkg.column_values("things", col, ConversionOptions::strict())
                .unwrap(),
            vec![Value::Null],
            "{col}"
        );
    }
}

#[test]
fn datetime_strict_vs_lenient() {
    let (_dir, gpkg) = typed_gpkg();
    // A second-precision value: valid under lenient, rejected under strict.
    insert(
        &gpkg,
        "INSERT INTO things (fid, seen) VALUES (1, '2026-07-24T12:34:56Z')",
    );

    match gpkg.column_values("things", "seen", ConversionOptions::strict()) {
        Err(Error::InvalidDateTimeValue { column, text, .. }) => {
            assert_eq!(column, "seen");
            assert_eq!(text, "2026-07-24T12:34:56Z");
        }
        other => panic!("expected InvalidDateTimeValue, got {other:?}"),
    }

    let lenient = gpkg
        .column_values("things", "seen", ConversionOptions::lenient())
        .unwrap();
    assert_eq!(
        lenient,
        vec![Value::DateTime(
            DateTime::parse_lenient("2026-07-24T12:34:56Z").unwrap()
        )]
    );
}

#[test]
fn storage_class_mismatch_is_typed_error() {
    let (_dir, gpkg) = typed_gpkg();
    // Store TEXT in an INTEGER-declared column (SQLite permits it).
    insert(
        &gpkg,
        "INSERT INTO things (fid, count) VALUES (1, 'not a number')",
    );
    match gpkg.column_values("things", "count", ConversionOptions::strict()) {
        Err(Error::ValueTypeMismatch {
            column,
            declared,
            found,
        }) => {
            assert_eq!(column, "count");
            assert_eq!(declared, ColumnType::MediumInt);
            assert_eq!(found, "TEXT");
        }
        other => panic!("expected ValueTypeMismatch, got {other:?}"),
    }
}

#[test]
fn integer_widens_to_float() {
    let (_dir, gpkg) = typed_gpkg();
    // A bare integer literal lands in the DOUBLE column with integer affinity.
    insert(&gpkg, "INSERT INTO things (fid, ratio) VALUES (1, 7)");
    assert_eq!(
        gpkg.column_values("things", "ratio", ConversionOptions::strict())
            .unwrap(),
        vec![Value::Float(7.0)]
    );
}

#[test]
fn geometry_column_is_rejected() {
    let (_dir, gpkg) = typed_gpkg();
    insert(&gpkg, "INSERT INTO things (fid) VALUES (1)");
    match gpkg.column_values("things", "geom", ConversionOptions::strict()) {
        Err(Error::GeometryValueUnsupported { column }) => assert_eq!(column, "geom"),
        other => panic!("expected GeometryValueUnsupported, got {other:?}"),
    }
}

#[test]
fn unknown_column_is_typed_error() {
    let (_dir, gpkg) = typed_gpkg();
    match gpkg.column_values("things", "missing", ConversionOptions::strict()) {
        Err(Error::NoSuchColumn {
            table_name,
            column_name,
        }) => {
            assert_eq!(table_name, "things");
            assert_eq!(column_name, "missing");
        }
        other => panic!("expected NoSuchColumn, got {other:?}"),
    }
}
