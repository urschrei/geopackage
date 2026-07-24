//! Layer and attributes-table creation (Group A of the M2 write path): DDL
//! emission plus `gpkg_contents` / `gpkg_geometry_columns` catalogue rows, and
//! name/srs/geometry-type validation.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geopackage::core::types::{ColumnType, GeometryType, ZmFlag};
use geopackage::{
    ColumnSpec, ContentsDataType, Error, GeoPackage, GeometrySpec, LayerKind, TableSchemaBuilder,
};

fn gpkg() -> (tempfile::TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    (dir, gpkg)
}

#[test]
fn create_feature_layer_registers_catalogue_rows() {
    let (_dir, gpkg) = gpkg();
    let builder = TableSchemaBuilder::new("roads")
        .identifier("Road network")
        .column(ColumnSpec::new("name", ColumnType::Text(Some(64))))
        .column(
            ColumnSpec::new("lanes", ColumnType::MediumInt)
                .not_null()
                .default_value("1"),
        )
        .geometry(
            GeometrySpec::new(GeometryType::LineString, 4326)
                .z(ZmFlag::Optional)
                .m(ZmFlag::Prohibited),
        );
    let layer = gpkg.create_layer(&builder).unwrap();
    assert_eq!(layer.table_name(), "roads");
    assert_eq!(layer.kind(), LayerKind::Feature);
    // An empty layer reads back with no features.
    assert_eq!(layer.features().unwrap().count(), 0);

    // gpkg_contents row.
    let contents = gpkg.contents().unwrap();
    assert_eq!(contents.len(), 1);
    assert_eq!(contents[0].table_name, "roads");
    assert_eq!(contents[0].data_type, ContentsDataType::Features);
    assert_eq!(contents[0].identifier.as_deref(), Some("Road network"));
    assert_eq!(contents[0].srs_id, Some(4326));
    // Bounding box starts empty (no features written yet).
    assert_eq!(contents[0].min_x, None);
    assert_eq!(contents[0].max_y, None);

    // gpkg_geometry_columns row.
    let gc = gpkg.geometry_column("roads").unwrap().unwrap();
    assert_eq!(gc.column_name, "geom");
    assert_eq!(gc.geometry_type, GeometryType::LineString);
    assert_eq!(gc.srs_id, 4326);
    assert_eq!(gc.z, ZmFlag::Optional);
    assert_eq!(gc.m, ZmFlag::Prohibited);

    // The user table round-trips through introspection.
    let schema = gpkg.table_schema("roads").unwrap();
    assert_eq!(schema.primary_key().unwrap().name, "fid");
    assert_eq!(
        schema.column("name").unwrap().column_type,
        Some(ColumnType::Text(Some(64)))
    );
    let lanes = schema.column("lanes").unwrap();
    assert!(lanes.not_null);
    assert_eq!(lanes.default_value.as_deref(), Some("1"));
}

#[test]
fn feature_layer_enumerates() {
    let (_dir, gpkg) = gpkg();
    gpkg.create_layer(
        &TableSchemaBuilder::new("pts").geometry(GeometrySpec::new(GeometryType::Point, 4326)),
    )
    .unwrap();
    let layers = gpkg.layers().unwrap();
    assert_eq!(layers.len(), 1);
    assert_eq!(layers[0].table_name(), "pts");
}

#[test]
fn create_attributes_table() {
    let (_dir, gpkg) = gpkg();
    let builder = TableSchemaBuilder::new("notes")
        .column(ColumnSpec::new("body", ColumnType::Text(None)))
        .column(ColumnSpec::new("created", ColumnType::DateTime));
    let layer = gpkg.create_attributes_table(&builder).unwrap();
    assert_eq!(layer.kind(), LayerKind::Attributes);

    let contents = gpkg.contents().unwrap();
    assert_eq!(contents[0].data_type, ContentsDataType::Attributes);
    assert_eq!(contents[0].srs_id, None);
    // No geometry column is registered.
    assert!(gpkg.geometry_column("notes").unwrap().is_none());
}

#[test]
fn geometry_columns_created_lazily_on_first_feature_table() {
    let (_dir, gpkg) = gpkg();
    // A fresh GeoPackage has no gpkg_geometry_columns table.
    assert!(gpkg.geometry_columns().unwrap().is_empty());
    gpkg.create_layer(
        &TableSchemaBuilder::new("a").geometry(GeometrySpec::new(GeometryType::Point, 4326)),
    )
    .unwrap();
    // Creating a second feature table reuses the now-existing catalogue table.
    gpkg.create_layer(
        &TableSchemaBuilder::new("b").geometry(GeometrySpec::new(GeometryType::Polygon, 4326)),
    )
    .unwrap();
    assert_eq!(gpkg.geometry_columns().unwrap().len(), 2);
}

#[test]
fn rejects_gpkg_prefix() {
    let (_dir, gpkg) = gpkg();
    match gpkg.create_attributes_table(&TableSchemaBuilder::new("gpkg_mine")) {
        Err(Error::ReservedTablePrefix { table_name }) => assert_eq!(table_name, "gpkg_mine"),
        other => panic!("expected ReservedTablePrefix, got {other:?}"),
    }
    // Case-insensitively.
    assert!(matches!(
        gpkg.create_attributes_table(&TableSchemaBuilder::new("GPKG_x")),
        Err(Error::ReservedTablePrefix { .. })
    ));
}

#[test]
fn rejects_duplicate_table() {
    let (_dir, gpkg) = gpkg();
    gpkg.create_attributes_table(&TableSchemaBuilder::new("t"))
        .unwrap();
    match gpkg.create_attributes_table(&TableSchemaBuilder::new("t")) {
        Err(Error::TableAlreadyExists { table_name }) => assert_eq!(table_name, "t"),
        other => panic!("expected TableAlreadyExists, got {other:?}"),
    }
}

#[test]
fn rejects_unregistered_srs() {
    let (_dir, gpkg) = gpkg();
    let builder =
        TableSchemaBuilder::new("pts").geometry(GeometrySpec::new(GeometryType::Point, 3857));
    match gpkg.create_layer(&builder) {
        Err(Error::UnknownSrs { srs_id }) => assert_eq!(srs_id, 3857),
        other => panic!("expected UnknownSrs, got {other:?}"),
    }
    // After registering the SRS, creation succeeds.
    gpkg.add_epsg_srs(3857).unwrap();
    gpkg.create_layer(&builder).unwrap();
}

#[test]
fn rejects_extension_geometry_type() {
    let (_dir, gpkg) = gpkg();
    let builder = TableSchemaBuilder::new("curves")
        .geometry(GeometrySpec::new(GeometryType::CurvePolygon, 4326));
    match gpkg.create_layer(&builder) {
        Err(Error::ExtensionGeometryUnsupported { geometry_type }) => {
            assert_eq!(geometry_type, GeometryType::CurvePolygon);
        }
        other => panic!("expected ExtensionGeometryUnsupported, got {other:?}"),
    }
}

#[test]
fn create_layer_without_geometry_is_rejected() {
    let (_dir, gpkg) = gpkg();
    match gpkg.create_layer(&TableSchemaBuilder::new("x")) {
        Err(Error::MissingGeometrySpec { table_name }) => assert_eq!(table_name, "x"),
        other => panic!("expected MissingGeometrySpec, got {other:?}"),
    }
}

#[test]
fn create_attributes_with_geometry_is_rejected() {
    let (_dir, gpkg) = gpkg();
    let builder =
        TableSchemaBuilder::new("x").geometry(GeometrySpec::new(GeometryType::Point, 4326));
    match gpkg.create_attributes_table(&builder) {
        Err(Error::UnexpectedGeometrySpec { table_name }) => assert_eq!(table_name, "x"),
        other => panic!("expected UnexpectedGeometrySpec, got {other:?}"),
    }
}
