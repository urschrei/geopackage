//! The `gpkg_schema` extension: column descriptions, value constraints, and
//! the opt-in enforcement of those constraints on write.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geo_types::Point;
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{
    ColumnConstraint, ColumnSpec, ConstraintKind, DataColumn, Error, ExtensionSupport, GeoPackage,
    GeometrySpec, OpenOptions, TableSchemaBuilder, Value, ValueRef,
};

/// A layer with a text column, an integer column and a float column, which is
/// enough for one of each constraint form.
fn layered(gpkg: &GeoPackage) {
    gpkg.create_layer(
        &TableSchemaBuilder::new("sites")
            .column(ColumnSpec::new("code", ColumnType::Text(None)))
            .column(ColumnSpec::new("year", ColumnType::Integer))
            .column(ColumnSpec::new("depth", ColumnType::Double))
            .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
    )
    .unwrap();
}

fn create() -> (tempfile::TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    layered(&gpkg);
    (dir, gpkg)
}

fn described(column_name: &str, constraint: Option<&str>) -> DataColumn {
    DataColumn {
        column_name: column_name.to_owned(),
        name: Some(format!("{column_name} (short)")),
        title: Some(format!("The {column_name}")),
        description: Some("described for the test".to_owned()),
        mime_type: None,
        constraint_name: constraint.map(str::to_owned),
    }
}

#[test]
fn a_file_without_the_extension_describes_nothing() {
    let (_dir, gpkg) = create();
    assert_eq!(gpkg.data_columns("sites").unwrap(), Vec::new());
    assert_eq!(gpkg.column_constraints().unwrap(), Vec::new());
    assert_eq!(gpkg.column_constraint("nothing").unwrap(), None);
    // And nothing is attached to the schema.
    let schema = gpkg.table_schema("sites").unwrap();
    assert!(schema.columns.iter().all(|c| c.data_column.is_none()));
}

#[test]
fn a_description_reaches_the_table_schema() {
    let (_dir, gpkg) = create();
    gpkg.set_data_column("sites", &described("code", None))
        .unwrap();

    // The point of attaching it here: a caller reading the schema sees the
    // description without asking a second question.
    let schema = gpkg.table_schema("sites").unwrap();
    let column = schema.column("code").unwrap();
    let data_column = column.data_column.as_ref().unwrap();
    assert_eq!(data_column.name.as_deref(), Some("code (short)"));
    assert_eq!(data_column.title.as_deref(), Some("The code"));
    assert!(schema.column("year").unwrap().data_column.is_none());

    // Creating the tables registers the extension against both of them, per
    // Requirement 141.
    let rows: Vec<_> = gpkg
        .extensions()
        .unwrap()
        .into_iter()
        .filter(|row| row.name == "gpkg_schema")
        .collect();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].support(), ExtensionSupport::Known);
    let tables: Vec<_> = rows.iter().filter_map(|r| r.table_name.clone()).collect();
    assert_eq!(
        tables,
        ["gpkg_data_column_constraints", "gpkg_data_columns"]
    );
}

#[test]
fn describing_a_column_twice_replaces_the_description() {
    let (_dir, gpkg) = create();
    gpkg.set_data_column("sites", &described("code", None))
        .unwrap();
    let mut second = described("code", None);
    second.title = Some("Renamed".to_owned());
    gpkg.set_data_column("sites", &second).unwrap();

    let all = gpkg.data_columns("sites").unwrap();
    assert_eq!(all.len(), 1, "the primary key is (table, column)");
    assert_eq!(all[0].title.as_deref(), Some("Renamed"));
}

#[test]
fn describing_a_column_the_table_lacks_is_an_error() {
    let (_dir, gpkg) = create();
    match gpkg.set_data_column("sites", &described("nonesuch", None)) {
        Err(Error::NoSuchColumn {
            table_name,
            column_name,
        }) => {
            assert_eq!(table_name, "sites");
            assert_eq!(column_name, "nonesuch");
        }
        other => panic!("expected NoSuchColumn, got {other:?}"),
    }
}

#[test]
fn the_three_constraint_forms_round_trip() {
    let (_dir, gpkg) = create();
    let range = ColumnConstraint {
        name: "depth_range".to_owned(),
        kind: ConstraintKind::Range {
            min: 0.0,
            min_is_inclusive: true,
            max: 100.0,
            max_is_inclusive: false,
        },
        description: Some("metres below datum".to_owned()),
    };
    let members = ColumnConstraint {
        name: "odd_years".to_owned(),
        // The spec's own sample enumerates numbers, stored as text.
        kind: ConstraintKind::Enum(vec!["1".to_owned(), "3".to_owned(), "5".to_owned()]),
        description: None,
    };
    let pattern = ColumnConstraint {
        name: "four_digits".to_owned(),
        kind: ConstraintKind::Glob("[1-2][0-9][0-9][0-9]".to_owned()),
        description: None,
    };
    for constraint in [&range, &members, &pattern] {
        gpkg.add_column_constraint(constraint).unwrap();
    }

    assert_eq!(gpkg.column_constraint("depth_range").unwrap(), Some(range));
    assert_eq!(gpkg.column_constraint("odd_years").unwrap(), Some(members));
    assert_eq!(
        gpkg.column_constraint("four_digits").unwrap(),
        Some(pattern)
    );
    // An enum is several rows and one constraint.
    let names: Vec<String> = gpkg
        .column_constraints()
        .unwrap()
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(names, ["depth_range", "four_digits", "odd_years"]);
}

#[test]
fn a_range_needs_min_below_max() {
    let (_dir, gpkg) = create();
    let backwards = ColumnConstraint {
        name: "backwards".to_owned(),
        kind: ConstraintKind::Range {
            min: 10.0,
            min_is_inclusive: true,
            max: 1.0,
            max_is_inclusive: true,
        },
        description: None,
    };
    // Requirement 111. Writing it would produce a constraint nothing can
    // satisfy, described as though it could be satisfied.
    match gpkg.add_column_constraint(&backwards) {
        Err(Error::InvalidColumnConstraint {
            constraint_name, ..
        }) => assert_eq!(constraint_name, "backwards"),
        other => panic!("expected InvalidColumnConstraint, got {other:?}"),
    }
}

#[test]
fn a_constraint_type_outside_the_three_is_an_error_on_read() {
    let (_dir, gpkg) = create();
    gpkg.add_column_constraint(&ColumnConstraint {
        name: "real".to_owned(),
        kind: ConstraintKind::Glob("*".to_owned()),
        description: None,
    })
    .unwrap();
    // As another implementation might have left it (Requirement 108).
    gpkg.connection()
        .execute(
            "INSERT INTO gpkg_data_column_constraints \
             (constraint_name, constraint_type, value) VALUES ('odd', 'regex', '.*')",
            [],
        )
        .unwrap();
    match gpkg.column_constraint("odd") {
        Err(Error::InvalidColumnConstraint {
            constraint_name, ..
        }) => assert_eq!(constraint_name, "odd"),
        other => panic!("expected InvalidColumnConstraint, got {other:?}"),
    }
}

#[test]
fn the_1_0_inclusivity_column_names_are_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.gpkg");
    let gpkg = GeoPackage::create(&path).unwrap();
    // GeoPackage 1.0 spelled these minIsInclusive/maxIsInclusive; the spec
    // warns that files written then are still about.
    gpkg.connection()
        .execute_batch(
            "CREATE TABLE gpkg_data_column_constraints (\
               constraint_name TEXT NOT NULL, constraint_type TEXT NOT NULL, value TEXT, \
               min NUMERIC, minIsInclusive BOOLEAN, max NUMERIC, maxIsInclusive BOOLEAN, \
               description TEXT);\
             INSERT INTO gpkg_data_column_constraints VALUES \
               ('legacy', 'range', NULL, 1, 0, 10, 1, NULL);",
        )
        .unwrap();

    let constraint = gpkg.column_constraint("legacy").unwrap().unwrap();
    assert_eq!(
        constraint.kind,
        ConstraintKind::Range {
            min: 1.0,
            min_is_inclusive: false,
            max: 10.0,
            max_is_inclusive: true,
        }
    );
}

// --- enforcement -------------------------------------------------------------

/// A file whose `year` column carries a range and `code` a glob, returned
/// opened with enforcement on.
fn constrained(dir: &tempfile::TempDir, enforce: bool) -> GeoPackage {
    let path = dir.path().join("c.gpkg");
    {
        let gpkg = GeoPackage::create(&path).unwrap();
        layered(&gpkg);
        gpkg.add_column_constraint(&ColumnConstraint {
            name: "years".to_owned(),
            kind: ConstraintKind::Range {
                min: 1900.0,
                min_is_inclusive: true,
                max: 2000.0,
                max_is_inclusive: false,
            },
            description: None,
        })
        .unwrap();
        gpkg.add_column_constraint(&ColumnConstraint {
            name: "codes".to_owned(),
            kind: ConstraintKind::Glob("[A-Z][A-Z]-*".to_owned()),
            description: None,
        })
        .unwrap();
        gpkg.set_data_column("sites", &described("year", Some("years")))
            .unwrap();
        gpkg.set_data_column("sites", &described("code", Some("codes")))
            .unwrap();
        gpkg.close().unwrap();
    }
    OpenOptions::new()
        .enforce_column_constraints(enforce)
        .open(&path)
        .unwrap()
}

fn insert(gpkg: &GeoPackage, code: &str, year: i64) -> geopackage::Result<i64> {
    let layer = gpkg.layer("sites")?;
    let mut writer = layer.writer()?;
    let fid = writer.insert(
        None,
        &Point::new(1.0, 2.0),
        &[
            ValueRef::Text(code),
            ValueRef::Integer(year),
            ValueRef::Null,
        ],
    )?;
    writer.commit()?;
    Ok(fid)
}

#[test]
fn constraints_are_not_enforced_unless_asked_for() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = constrained(&dir, false);
    assert!(!gpkg.enforces_column_constraints());
    // The spec makes these advisory, so a file may hold values its own
    // constraints forbid, and the default write path does not judge.
    insert(&gpkg, "not a code", 1234).unwrap();
    assert_eq!(gpkg.layer("sites").unwrap().features().unwrap().count(), 1);
}

#[test]
fn an_enforced_range_refuses_a_value_outside_it() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = constrained(&dir, true);
    assert!(gpkg.enforces_column_constraints());
    insert(&gpkg, "IE-01", 1950).unwrap();

    match insert(&gpkg, "IE-02", 1899) {
        Err(Error::ColumnConstraintViolation {
            table_name,
            column_name,
            constraint_name,
            ..
        }) => {
            assert_eq!(table_name, "sites");
            assert_eq!(column_name, "year");
            assert_eq!(constraint_name, "years");
        }
        other => panic!("expected ColumnConstraintViolation, got {other:?}"),
    }
    // The exclusive upper bound is exclusive.
    insert(&gpkg, "IE-03", 2000).unwrap_err();
    insert(&gpkg, "IE-04", 1999).unwrap();
    // Two rows: the two that satisfied both constraints. A refused row leaves
    // nothing behind, its writer's transaction never having been committed.
    assert_eq!(gpkg.layer("sites").unwrap().features().unwrap().count(), 2);
}

#[test]
fn an_enforced_glob_refuses_a_value_it_does_not_match() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = constrained(&dir, true);
    insert(&gpkg, "IE-01", 1950).unwrap();
    match insert(&gpkg, "ie-01", 1950) {
        Err(Error::ColumnConstraintViolation { column_name, .. }) => {
            assert_eq!(column_name, "code", "GLOB is case sensitive");
        }
        other => panic!("expected ColumnConstraintViolation, got {other:?}"),
    }
}

#[test]
fn null_satisfies_every_constraint() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = constrained(&dir, true);
    let layer = gpkg.layer("sites").unwrap();
    let mut writer = layer.writer().unwrap();
    // A constraint says what a value may be, not that there has to be one.
    writer
        .insert(
            None,
            &Point::new(1.0, 2.0),
            &[ValueRef::Null, ValueRef::Null, ValueRef::Null],
        )
        .unwrap();
    writer.commit().unwrap();
    assert_eq!(layer.features().unwrap().count(), 1);
}

#[test]
fn enforcement_covers_the_bulk_and_partial_update_paths() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = constrained(&dir, true);
    let layer = gpkg.layer("sites").unwrap();

    // write_all, which is a different path from the row writer: an option that
    // only covered one of them would read as "checked unless you went fast".
    let rows = vec![geopackage::NewFeature::new(
        Point::new(1.0, 2.0),
        vec![
            Value::Text("IE-09".to_owned()),
            Value::Integer(2500),
            Value::Null,
        ],
    )];
    match layer.write_all(rows, 0) {
        Err(Error::ColumnConstraintViolation { column_name, .. }) => {
            assert_eq!(column_name, "year");
        }
        other => panic!("expected ColumnConstraintViolation, got {other:?}"),
    }

    insert(&gpkg, "IE-01", 1950).unwrap();
    let mut writer = layer.writer().unwrap();
    // And the partial update, which names its columns rather than passing a
    // whole row.
    match writer.update_columns(1, &[("year", ValueRef::Integer(1899))]) {
        Err(Error::ColumnConstraintViolation { column_name, .. }) => {
            assert_eq!(column_name, "year");
        }
        other => panic!("expected ColumnConstraintViolation, got {other:?}"),
    }
    assert!(
        writer
            .update_columns(1, &[("year", ValueRef::Integer(1901))])
            .unwrap()
    );
    writer.commit().unwrap();
}

#[cfg(feature = "arrow")]
#[test]
fn enforcement_covers_the_columnar_write_path() {
    let dir = tempfile::tempdir().unwrap();
    // A source with the same columns and no constraints, holding a year the
    // target's constraint forbids.
    let source = GeoPackage::create(dir.path().join("s.gpkg")).unwrap();
    layered(&source);
    insert(&source, "IE-01", 2500).unwrap();
    let batches: Vec<_> = source
        .layer("sites")
        .unwrap()
        .read_arrow(geopackage::arrow::ArrowReadOptions::default())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    // The columnar path binds values without building a Value per cell, so it
    // reaches the writer in a third representation. An option that skipped it
    // would be enforcement in name only.
    let target = constrained(&dir, true);
    match target
        .layer("sites")
        .unwrap()
        .write_arrow(batches.into_iter().map(Ok), 0)
    {
        Err(Error::ColumnConstraintViolation { column_name, .. }) => {
            assert_eq!(column_name, "year");
        }
        other => panic!("expected ColumnConstraintViolation, got {other:?}"),
    }
    assert_eq!(
        target.layer("sites").unwrap().features().unwrap().count(),
        0
    );
}
