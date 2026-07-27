//! Tile pyramids (M4): creation and the catalogue rows it writes, the
//! validation it applies, and the handle that opens one back.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geopackage::core::TileError;
use geopackage::core::tiles::{TileMatrix, TileMatrixSet, ZoomLadder};
use geopackage::core::types::GeometryType;
use geopackage::{
    ContentsDataType, Error, GeoPackage, GeometrySpec, TableSchemaBuilder, TilePyramidBuilder,
};

fn gpkg() -> (tempfile::TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    gpkg.add_epsg_srs(3857).unwrap();
    (dir, gpkg)
}

/// A web mercator pyramid of `levels` zoom levels, starting at zero.
fn quad_builder(name: &str, levels: i64) -> TilePyramidBuilder {
    let matrix_set = TileMatrixSet::web_mercator_quad();
    let matrices = matrix_set.ladder(ZoomLadder::new(0, levels - 1)).unwrap();
    TilePyramidBuilder::new(name, matrix_set).matrices(matrices)
}

#[test]
fn create_registers_catalogue_rows() {
    let (_dir, gpkg) = gpkg();
    let pyramid = gpkg
        .create_tile_pyramid(&quad_builder("basemap", 4).identifier("Base map"))
        .unwrap();
    assert_eq!(pyramid.zoom_levels(), vec![0, 1, 2, 3]);
    assert_eq!(pyramid.matrix(3).unwrap().matrix_width, 8);
    assert!(pyramid.matrix(4).is_none());

    let contents = gpkg.contents().unwrap();
    let entry = contents
        .iter()
        .find(|c| c.table_name == "basemap")
        .expect("a gpkg_contents row");
    assert_eq!(entry.data_type, ContentsDataType::Tiles);
    assert_eq!(entry.identifier.as_deref(), Some("Base map"));
    assert_eq!(entry.srs_id, Some(3857));
    // For tiles the recorded extent is the matrix set's, which is exact.
    assert_eq!(entry.min_x, Some(pyramid.matrix_set().min_x));
    assert_eq!(entry.max_y, Some(pyramid.matrix_set().max_y));

    let conn = gpkg.connection();
    let levels: i64 = conn
        .query_row(
            "SELECT count(*) FROM gpkg_tile_matrix WHERE table_name = 'basemap'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(levels, 4);
    let srs: i64 = conn
        .query_row(
            "SELECT srs_id FROM gpkg_tile_matrix_set WHERE table_name = 'basemap'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(srs, 3857);
    // The user table carries the spec's column set, and its uniqueness
    // constraint is what makes a tile address a key.
    conn.execute(
        "INSERT INTO basemap (zoom_level, tile_column, tile_row, tile_data) VALUES (0, 0, 0, x'00')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO basemap (zoom_level, tile_column, tile_row, tile_data) VALUES (0, 0, 0, x'00')",
        [],
    )
    .unwrap_err();
}

#[test]
fn create_rejects_names_the_spec_reserves_or_the_file_uses() {
    let (_dir, gpkg) = gpkg();
    assert!(matches!(
        gpkg.create_tile_pyramid(&quad_builder("gpkg_tiles", 2)),
        Err(Error::ReservedTablePrefix { .. })
    ));
    gpkg.create_tile_pyramid(&quad_builder("basemap", 2))
        .unwrap();
    assert!(matches!(
        gpkg.create_tile_pyramid(&quad_builder("basemap", 2)),
        Err(Error::TableAlreadyExists { .. })
    ));
}

#[test]
fn create_requires_a_registered_srs() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    assert!(matches!(
        gpkg.create_tile_pyramid(&quad_builder("basemap", 2)),
        Err(Error::UnknownSrs { srs_id: 3857 })
    ));
}

#[test]
fn create_checks_the_specs_consistency_rules() {
    let (_dir, gpkg) = gpkg();
    let matrix_set = TileMatrixSet::web_mercator_quad();
    // Half the columns needed to span the extent at that pixel size.
    let short = TileMatrix::new(0, 1, 2, 256, 256, matrix_set.width() / 256.0, 1.0);
    assert!(matches!(
        gpkg.create_tile_pyramid(&TilePyramidBuilder::new("basemap", matrix_set).matrix(short)),
        Err(Error::Tile(TileError::ExtentMismatch { .. }))
    ));
    // Nothing was left behind by the rejected creation.
    assert!(matches!(
        gpkg.tiles("basemap"),
        Err(Error::NoSuchLayer { .. })
    ));
}

#[test]
fn zoom_other_needs_an_explicit_opt_in() {
    let (_dir, gpkg) = gpkg();
    let matrix_set = TileMatrixSet::web_mercator_quad();
    let width = matrix_set.width();
    // Zoom 1 triples the grid rather than doubling it, which is legal only
    // under gpkg_zoom_other.
    let matrices = [
        TileMatrix::new(0, 1, 1, 256, 256, width / 256.0, width / 256.0),
        TileMatrix::new(1, 3, 3, 256, 256, width / 768.0, width / 768.0),
    ];
    let builder = TilePyramidBuilder::new("basemap", matrix_set).matrices(matrices);
    assert!(matches!(
        gpkg.create_tile_pyramid(&builder),
        Err(Error::ZoomOtherNotEnabled { .. })
    ));

    gpkg.create_tile_pyramid(&builder.clone().allow_zoom_other(true))
        .unwrap();
    let registered: i64 = gpkg
        .connection()
        .query_row(
            "SELECT count(*) FROM gpkg_extensions \
             WHERE table_name = 'basemap' AND column_name = 'tile_data' \
             AND extension_name = 'gpkg_zoom_other' AND scope = 'read-write'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(registered, 1);
}

#[test]
fn a_power_of_two_pyramid_registers_no_extension() {
    let (_dir, gpkg) = gpkg();
    gpkg.create_tile_pyramid(&quad_builder("basemap", 5))
        .unwrap();
    let extensions: i64 = gpkg
        .connection()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'gpkg_extensions'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        extensions, 0,
        "a plain pyramid should not even create the extensions table"
    );
}

#[test]
fn pyramids_open_and_enumerate() {
    let (_dir, gpkg) = gpkg();
    gpkg.create_tile_pyramid(&quad_builder("basemap", 3))
        .unwrap();
    gpkg.create_tile_pyramid(&quad_builder("overlay", 2))
        .unwrap();
    gpkg.create_layer(
        &TableSchemaBuilder::new("roads")
            .geometry(GeometrySpec::new(GeometryType::LineString, 3857)),
    )
    .unwrap();

    let names: Vec<String> = gpkg
        .tile_pyramids()
        .unwrap()
        .iter()
        .map(|p| p.table_name().to_owned())
        .collect();
    assert_eq!(names, vec!["basemap".to_owned(), "overlay".to_owned()]);

    let reopened = gpkg.tiles("basemap").unwrap();
    assert_eq!(reopened.zoom_levels(), vec![0, 1, 2]);
    assert_eq!(reopened.matrix_set().srs_id, 3857);
    reopened.validate().unwrap();

    // A feature layer is not a pyramid, and the other way round.
    assert!(matches!(
        gpkg.tiles("roads"),
        Err(Error::WrongDataType {
            expected: "tiles",
            ..
        })
    ));
    assert!(matches!(
        gpkg.layer("basemap"),
        Err(Error::WrongDataType {
            expected: "features",
            ..
        })
    ));
    assert!(matches!(
        gpkg.tiles("nothing"),
        Err(Error::NoSuchLayer { .. })
    ));
}

#[test]
fn a_pyramid_with_no_matrix_set_row_is_an_error() {
    let (_dir, gpkg) = gpkg();
    gpkg.create_tile_pyramid(&quad_builder("basemap", 2))
        .unwrap();
    gpkg.connection()
        .execute(
            "DELETE FROM gpkg_tile_matrix_set WHERE table_name = 'basemap'",
            [],
        )
        .unwrap();
    assert!(matches!(
        gpkg.tiles("basemap"),
        Err(Error::NoTileMatrixSet { .. })
    ));
}
