//! `Value` conversion from stored SQLite values, driven by declared types.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geopackage::core::datetime::{Date, DateTime};
use geopackage::core::types::ColumnType;
use geopackage::{ConversionOptions, Error, GeoPackage, StorageStrictness, Value};

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

/// An integer written to a `DOUBLE` column reads back as a float, under either
/// strictness.
///
/// Not because conversion widens it: `FLOAT`, `DOUBLE` and `REAL` all give the
/// column REAL affinity, and REAL affinity converts the integer to floating
/// point on the way in, so what conversion sees is already a REAL. The widening
/// arm in `value_from_ref` exists for a value that reaches it without that
/// having happened, which reading a table column does not produce. This test
/// pins the affinity behaviour that makes that so, since it is the reason the
/// arm is unreachable here.
#[test]
fn integer_written_to_a_double_column_reads_as_float() {
    let (_dir, gpkg) = typed_gpkg();
    insert(&gpkg, "INSERT INTO things (fid, ratio) VALUES (1, 7)");

    let stored: String = gpkg
        .connection()
        .query_row("SELECT typeof(ratio) FROM things", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        stored, "real",
        "REAL affinity should have converted on insert"
    );

    for options in [
        ConversionOptions::strict(),
        ConversionOptions::lenient(),
        ConversionOptions::default(),
    ] {
        assert_eq!(
            gpkg.column_values("things", "ratio", options).unwrap(),
            vec![Value::Float(7.0)]
        );
    }
}

/// A `BOOLEAN` column holding an integer other than 0 or 1 reads as `true` by
/// default and is an error under strict conversion.
///
/// Unlike the `DOUBLE` case above this is reachable: SQLite gives a
/// `BOOLEAN`-declared column no affinity, so the integer is stored as written.
#[test]
fn non_boolean_integer_is_lenient_by_default_and_strict_on_request() {
    let (_dir, gpkg) = typed_gpkg();
    insert(&gpkg, "INSERT INTO things (fid, flag) VALUES (1, 7)");

    let stored: String = gpkg
        .connection()
        .query_row("SELECT typeof(flag) FROM things", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        stored, "integer",
        "BOOLEAN carries no affinity to convert it"
    );

    assert_eq!(
        gpkg.column_values("things", "flag", ConversionOptions::default())
            .unwrap(),
        vec![Value::Boolean(true)],
        "the default reads a non-zero integer as true"
    );
    assert_eq!(
        gpkg.column_values("things", "flag", ConversionOptions::lenient())
            .unwrap(),
        vec![Value::Boolean(true)]
    );

    match gpkg.column_values("things", "flag", ConversionOptions::strict()) {
        Err(Error::NonBooleanInteger { column, value }) => {
            assert_eq!(column, "flag");
            assert_eq!(value, 7);
        }
        other => panic!("expected NonBooleanInteger, got {other:?}"),
    }
}

/// Strictness applies only to the values that need interpreting: a conformant
/// 0 or 1 reads the same under either setting.
#[test]
fn conformant_booleans_are_unaffected_by_strictness() {
    let (_dir, gpkg) = typed_gpkg();
    insert(&gpkg, "INSERT INTO things (fid, flag) VALUES (1, 0)");
    insert(&gpkg, "INSERT INTO things (fid, flag) VALUES (2, 1)");
    for options in [ConversionOptions::strict(), ConversionOptions::lenient()] {
        assert_eq!(
            gpkg.column_values("things", "flag", options).unwrap(),
            vec![Value::Boolean(false), Value::Boolean(true)]
        );
    }
}

/// The layer read path carries lenient value interpretation by default, so a
/// file with a non-conformant `BOOLEAN` still reads.
///
/// `Layer` seeds itself from `ConversionOptions::default()` rather than
/// `strict()`. The two used to be the same value, and this is what would break
/// silently were the layer ever seeded from `strict()` again: every feature read
/// of such a file would fail instead of returning the row.
#[test]
fn layer_reads_are_lenient_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("a.gpkg")).unwrap();
    gpkg.connection()
        .execute_batch(
            "CREATE TABLE notes (id INTEGER PRIMARY KEY, flag BOOLEAN);\
             INSERT INTO gpkg_contents (table_name, data_type, srs_id) \
               VALUES ('notes', 'attributes', 0);\
             INSERT INTO notes (id, flag) VALUES (1, 7);",
        )
        .unwrap();

    let layer = gpkg.attributes("notes").unwrap();
    assert_eq!(layer.conversion_options(), ConversionOptions::default());
    let features: Vec<_> = layer
        .features()
        .unwrap()
        .map(|f| f.unwrap().value("flag").map(|v| v.to_owned()))
        .collect();
    assert_eq!(features, vec![Some(Value::Boolean(true))]);

    // The same read, asked to be strict, rejects it.
    let strict = gpkg
        .attributes("notes")
        .unwrap()
        .with_conversion_options(ConversionOptions::strict());
    let first = strict.features().unwrap().next().unwrap();
    assert!(matches!(first, Err(Error::NonBooleanInteger { .. })));
}

/// The two axes are independent: strict `DATETIME` parsing with lenient value
/// interpretation is the default, and either can be set on its own.
#[test]
fn datetime_and_storage_strictness_are_independent() {
    let (_dir, gpkg) = typed_gpkg();
    insert(&gpkg, "INSERT INTO things (fid, flag) VALUES (1, 7)");

    assert_eq!(
        ConversionOptions::default(),
        ConversionOptions::strict().with_storage(StorageStrictness::Lenient),
        "the default is strict datetimes with lenient values"
    );

    // Lenient datetimes need not mean lenient values.
    let mixed = ConversionOptions::lenient().with_storage(StorageStrictness::Strict);
    assert!(matches!(
        gpkg.column_values("things", "flag", mixed),
        Err(Error::NonBooleanInteger { .. })
    ));
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
