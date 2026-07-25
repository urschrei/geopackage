//! The Arrow schema a layer derives from its table schema.

#![cfg(feature = "arrow")]
#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use arrow_schema::{DataType, TimeUnit};
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{ColumnSpec, GeoPackage, GeometrySpec, TableSchemaBuilder};

/// A feature layer carrying one column per spec type, plus a column whose
/// declared type is outside the vocabulary.
fn typed_layer() -> (tempfile::TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    let builder = TableSchemaBuilder::new("things")
        .column(ColumnSpec::new("flag", ColumnType::Boolean))
        .column(ColumnSpec::new("tiny", ColumnType::TinyInt))
        .column(ColumnSpec::new("small", ColumnType::SmallInt))
        .column(ColumnSpec::new("medium", ColumnType::MediumInt))
        .column(ColumnSpec::new("big", ColumnType::Integer))
        .column(ColumnSpec::new("ratio", ColumnType::Float))
        .column(ColumnSpec::new("precise", ColumnType::Double))
        .column(ColumnSpec::new("label", ColumnType::Text(Some(64))))
        .column(ColumnSpec::new("payload", ColumnType::Blob(None)))
        .column(ColumnSpec::new("born", ColumnType::Date))
        .column(ColumnSpec::new("seen", ColumnType::DateTime))
        .column(ColumnSpec::new("required", ColumnType::Text(None)).not_null())
        .geometry(GeometrySpec::new(GeometryType::Point, 4326));
    gpkg.create_layer(&builder).unwrap();
    // A declared type outside the spec vocabulary, which `create_layer` has no
    // way to express, so it is added directly.
    gpkg.connection()
        .execute_batch("ALTER TABLE things ADD COLUMN odd VARCHAR(20)")
        .unwrap();
    (dir, gpkg)
}

#[test]
fn attribute_types_map_as_documented() {
    let (_dir, gpkg) = typed_layer();
    let schema = gpkg.layer("things").unwrap().arrow_schema().unwrap();

    let ty = |name: &str| schema.field_with_name(name).unwrap().data_type().clone();

    assert_eq!(ty("flag"), DataType::Boolean);
    // Every declared integer width collapses to Int64, because SQLite does not
    // enforce the width and a narrower Arrow type could truncate.
    for name in ["tiny", "small", "medium", "big"] {
        assert_eq!(ty(name), DataType::Int64, "{name}");
    }
    for name in ["ratio", "precise"] {
        assert_eq!(ty(name), DataType::Float64, "{name}");
    }
    // The declared TEXT length is informative and is not carried into Arrow.
    assert_eq!(ty("label"), DataType::Utf8);
    assert_eq!(ty("payload"), DataType::Binary);
    assert_eq!(ty("born"), DataType::Date32);
    assert_eq!(
        ty("seen"),
        DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
    );
}

#[test]
fn out_of_vocabulary_types_fall_back_to_sqlite_affinity() {
    let (_dir, gpkg) = typed_layer();
    let schema = gpkg.layer("things").unwrap().arrow_schema().unwrap();
    // VARCHAR(20) is not in the spec vocabulary, so `ColumnType::parse` gives
    // None. SQLite gives it TEXT affinity, and so do we: refusing to read it
    // would make the Arrow path useless on files written by other tools.
    assert_eq!(
        schema.field_with_name("odd").unwrap().data_type(),
        &DataType::Utf8
    );
}

#[test]
fn geometry_column_carries_the_geoarrow_extension() {
    let (_dir, gpkg) = typed_layer();
    let schema = gpkg.layer("things").unwrap().arrow_schema().unwrap();
    let geom = schema.field_with_name("geom").unwrap();

    assert_eq!(geom.data_type(), &DataType::Binary);
    assert_eq!(
        geom.metadata()
            .get("ARROW:extension:name")
            .map(String::as_str),
        Some("geoarrow.wkb")
    );
    let crs = geom.metadata().get("ARROW:extension:metadata").unwrap();
    assert!(
        crs.contains("EPSG:4326"),
        "the layer's srs_id should reach the field metadata, got {crs}"
    );
}

#[test]
fn undefined_srs_carries_no_crs() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("u.gpkg")).unwrap();
    // srs_id 0 is the spec's undefined-geographic value, so there is no CRS to
    // name and the metadata must not invent one.
    gpkg.create_layer(
        &TableSchemaBuilder::new("pts").geometry(GeometrySpec::new(GeometryType::Point, 0)),
    )
    .unwrap();
    let schema = gpkg.layer("pts").unwrap().arrow_schema().unwrap();
    let geom = schema.field_with_name("geom").unwrap();
    assert_eq!(
        geom.metadata()
            .get("ARROW:extension:metadata")
            .map(String::as_str),
        Some("{}")
    );
}

#[test]
fn nullability_follows_the_table() {
    let (_dir, gpkg) = typed_layer();
    let schema = gpkg.layer("things").unwrap().arrow_schema().unwrap();
    let nullable = |name: &str| schema.field_with_name(name).unwrap().is_nullable();

    // An INTEGER PRIMARY KEY is SQLite's rowid alias and cannot hold NULL,
    // whether or not the column was declared NOT NULL.
    assert!(!nullable("fid"), "the primary key cannot be null");
    assert!(!nullable("required"), "a NOT NULL column is not nullable");
    assert!(nullable("label"), "an unconstrained column is nullable");
    assert!(nullable("geom"), "an unconstrained geometry is nullable");
}

#[test]
fn field_order_matches_the_table() {
    let (_dir, gpkg) = typed_layer();
    let layer = gpkg.layer("things").unwrap();
    let schema = layer.arrow_schema().unwrap();

    let arrow: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    let table: Vec<&str> = layer
        .schema()
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(
        arrow, table,
        "the geometry column stays where the table puts it"
    );
}
