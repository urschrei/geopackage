//! Tile pyramids (M4): creation and the catalogue rows it writes, the
//! validation it applies, and the handle that opens one back.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geopackage::core::TileError;
use geopackage::core::tiles::{TileCoord, TileMatrix, TileMatrixSet, ZoomLadder};
use geopackage::core::types::GeometryType;
use geopackage::{
    BoundingBox, ContentsDataType, Error, GeoPackage, GeometrySpec, TableSchemaBuilder,
    TilePyramid, TilePyramidBuilder,
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

/// Insert a tile through raw SQL, which is how a pyramid written by another
/// implementation arrives: the read path is exercised without the write path.
fn insert_tile(pyramid: &TilePyramid<'_>, coord: TileCoord, data: &[u8]) {
    pyramid
        .gpkg()
        .connection()
        .execute(
            &format!(
                "INSERT INTO {} (zoom_level, tile_column, tile_row, tile_data) \
                 VALUES (?1, ?2, ?3, ?4)",
                pyramid.table_name()
            ),
            rusqlite::params![coord.zoom_level, coord.column, coord.row, data],
        )
        .unwrap();
}

/// A PNG header of the given size, with a zeroed CRC: enough for the payload
/// probe, which reads headers and decodes nothing.
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&13_u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    bytes
}

/// A WebP container in its extended (`VP8X`) form, whose dimensions are stored
/// one less than the real size.
fn webp(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::from(*b"RIFF");
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(b"WEBP");
    bytes.extend_from_slice(b"VP8X");
    bytes.extend_from_slice(&10_u32.to_le_bytes());
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    bytes.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
    bytes.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
    bytes
}

/// A little-endian baseline TIFF header with one IFD naming the image size.
/// TIFF is a coverage-extension payload, which a plain tile pyramid may not
/// hold.
fn tiff(width: u16, height: u16) -> Vec<u8> {
    let mut bytes = Vec::from(*b"II");
    bytes.extend_from_slice(&42_u16.to_le_bytes());
    bytes.extend_from_slice(&8_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    for (tag, value) in [(256_u16, width), (257_u16, height)] {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&3_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&u32::from(value).to_le_bytes());
    }
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes
}

/// Every tile address a scan yields, in the order it yields them.
fn scanned(cursor: &mut geopackage::TileCursor<'_>) -> Vec<(i64, i64, i64)> {
    let mut stream = cursor.tiles().unwrap();
    let mut out = Vec::new();
    while let Some(tile) = stream.next().unwrap() {
        let coord = tile.coord();
        out.push((coord.zoom_level, coord.column, coord.row));
    }
    out
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
fn a_tile_reads_back_by_address() {
    let (_dir, gpkg) = gpkg();
    let pyramid = gpkg
        .create_tile_pyramid(&quad_builder("basemap", 3))
        .unwrap();
    insert_tile(&pyramid, TileCoord::new(1, 1, 0), b"north east");

    assert_eq!(
        pyramid
            .get_tile(TileCoord::new(1, 1, 0))
            .unwrap()
            .as_deref(),
        Some(b"north east".as_slice())
    );
    assert!(pyramid.has_tile(TileCoord::new(1, 1, 0)).unwrap());
    assert_eq!(pyramid.get_tile(TileCoord::new(1, 0, 0)).unwrap(), None);
    assert!(!pyramid.has_tile(TileCoord::new(1, 0, 0)).unwrap());
    // An address outside the grid is absent, not an error: only writing one is.
    assert_eq!(pyramid.get_tile(TileCoord::new(1, 99, 99)).unwrap(), None);

    let mut buffer = vec![b'x'; 64];
    assert!(
        pyramid
            .get_tile_into(TileCoord::new(1, 1, 0), &mut buffer)
            .unwrap()
    );
    assert_eq!(buffer, b"north east");
    assert!(
        !pyramid
            .get_tile_into(TileCoord::new(1, 0, 0), &mut buffer)
            .unwrap()
    );
    assert_eq!(buffer, b"north east", "an absent tile leaves the buffer be");

    assert_eq!(pyramid.tile_count().unwrap(), 1);
    assert_eq!(pyramid.tile_count_at(1).unwrap(), 1);
    assert_eq!(pyramid.tile_count_at(0).unwrap(), 0);
}

#[test]
fn a_scan_walks_the_pyramid_in_matrix_order() {
    let (_dir, gpkg) = gpkg();
    let pyramid = gpkg
        .create_tile_pyramid(&quad_builder("basemap", 3))
        .unwrap();
    // Inserted out of order: zoom 1's south-east tile first.
    for coord in [
        TileCoord::new(1, 1, 1),
        TileCoord::new(0, 0, 0),
        TileCoord::new(1, 0, 1),
        TileCoord::new(1, 1, 0),
        TileCoord::new(1, 0, 0),
    ] {
        insert_tile(&pyramid, coord, b"tile");
    }

    assert_eq!(
        scanned(&mut pyramid.cursor().unwrap()),
        vec![(0, 0, 0), (1, 0, 0), (1, 1, 0), (1, 0, 1), (1, 1, 1),],
        "zoom level, then north to south, then west to east"
    );
    assert_eq!(scanned(&mut pyramid.cursor_at(0).unwrap()), vec![(0, 0, 0)]);

    // A scan lends its payloads rather than copying them.
    let mut cursor = pyramid.cursor().unwrap();
    let mut stream = cursor.tiles().unwrap();
    let mut bytes = 0;
    stream
        .for_each(|tile| {
            bytes += tile.data().len();
            Ok(())
        })
        .unwrap();
    assert_eq!(bytes, 5 * b"tile".len());
}

#[test]
fn a_bounding_box_selects_the_tiles_it_touches() {
    let (_dir, gpkg) = gpkg();
    let pyramid = gpkg
        .create_tile_pyramid(&quad_builder("basemap", 3))
        .unwrap();
    for coord in [
        TileCoord::new(1, 0, 0),
        TileCoord::new(1, 1, 0),
        TileCoord::new(1, 0, 1),
        TileCoord::new(1, 1, 1),
    ] {
        insert_tile(&pyramid, coord, b"tile");
    }

    // The north-west quadrant of the web mercator quad is tile (0, 0) at zoom 1.
    let north_west = BoundingBox::new(-15_000_000.0, 5_000_000.0, -5_000_000.0, 15_000_000.0);
    assert_eq!(
        scanned(&mut pyramid.cursor_in(1, north_west).unwrap()),
        vec![(1, 0, 0)]
    );
    // A box spanning the origin touches all four.
    let centre = BoundingBox::new(-1_000_000.0, -1_000_000.0, 1_000_000.0, 1_000_000.0);
    assert_eq!(scanned(&mut pyramid.cursor_in(1, centre).unwrap()).len(), 4);
    // A box outside the pyramid's extent selects nothing.
    let elsewhere = BoundingBox::new(30_000_000.0, 30_000_000.0, 31_000_000.0, 31_000_000.0);
    assert!(scanned(&mut pyramid.cursor_in(1, elsewhere).unwrap()).is_empty());

    assert!(matches!(
        pyramid.cursor_in(7, north_west),
        Err(Error::UnknownZoomLevel { zoom_level: 7, .. })
    ));
}

#[test]
fn a_written_tile_reads_back_unchanged() {
    let (_dir, gpkg) = gpkg();
    let pyramid = gpkg
        .create_tile_pyramid(&quad_builder("basemap", 3))
        .unwrap();
    let tile = png(256, 256);
    pyramid.put_tile(TileCoord::new(1, 1, 0), &tile).unwrap();
    assert_eq!(
        pyramid
            .get_tile(TileCoord::new(1, 1, 0))
            .unwrap()
            .as_deref(),
        Some(tile.as_slice())
    );

    // Writing the same address again replaces the payload and keeps one row.
    let mut replacement = png(256, 256);
    replacement.extend_from_slice(b"second");
    pyramid
        .put_tile(TileCoord::new(1, 1, 0), &replacement)
        .unwrap();
    assert_eq!(pyramid.tile_count().unwrap(), 1);
    assert_eq!(
        pyramid
            .get_tile(TileCoord::new(1, 1, 0))
            .unwrap()
            .as_deref(),
        Some(replacement.as_slice())
    );

    assert!(pyramid.delete_tile(TileCoord::new(1, 1, 0)).unwrap());
    assert!(!pyramid.delete_tile(TileCoord::new(1, 1, 0)).unwrap());
    assert_eq!(pyramid.tile_count().unwrap(), 0);
}

#[test]
fn a_write_is_checked_against_the_pyramid_it_lands_in() {
    let (_dir, gpkg) = gpkg();
    let pyramid = gpkg
        .create_tile_pyramid(&quad_builder("basemap", 2))
        .unwrap();
    let tile = png(256, 256);

    assert!(matches!(
        pyramid.put_tile(TileCoord::new(9, 0, 0), &tile),
        Err(Error::UnknownZoomLevel { zoom_level: 9, .. })
    ));
    // Zoom 1 is a two by two grid, so column 2 is off the end of it.
    assert!(matches!(
        pyramid.put_tile(TileCoord::new(1, 2, 0), &tile),
        Err(Error::Tile(TileError::CoordOutsideMatrix { .. }))
    ));
    assert!(matches!(
        pyramid.put_tile(TileCoord::new(0, 0, 0), &png(512, 512)),
        Err(Error::Tile(TileError::PayloadSizeMismatch {
            expected_width: 256,
            actual_width: 512,
            ..
        }))
    ));
    assert!(matches!(
        pyramid.put_tile(TileCoord::new(0, 0, 0), b"not an image"),
        Err(Error::Tile(TileError::UnreadablePayload { .. }))
    ));
    assert!(matches!(
        pyramid.put_tile(TileCoord::new(0, 0, 0), &tiff(256, 256)),
        Err(Error::TileFormatNotAllowed { .. })
    ));
    assert_eq!(pyramid.tile_count().unwrap(), 0, "nothing was stored");
}

#[test]
fn a_webp_payload_registers_the_extension_once() {
    let (_dir, gpkg) = gpkg();
    let pyramid = gpkg
        .create_tile_pyramid(&quad_builder("basemap", 2))
        .unwrap();
    let registrations = || -> i64 {
        let conn = gpkg.connection();
        let catalogue: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'gpkg_extensions'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // No extension table at all is no registration, and is the state a
        // pyramid of PNGs should stay in.
        if catalogue == 0 {
            return 0;
        }
        conn.query_row(
            "SELECT count(*) FROM gpkg_extensions \
             WHERE table_name = 'basemap' AND column_name = 'tile_data' \
             AND extension_name = 'gpkg_webp' AND scope = 'read-write'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };

    pyramid
        .put_tile(TileCoord::new(0, 0, 0), &png(256, 256))
        .unwrap();
    assert_eq!(registrations(), 0, "a PNG registers nothing");

    pyramid
        .put_tile(TileCoord::new(1, 0, 0), &webp(256, 256))
        .unwrap();
    assert_eq!(registrations(), 1);
    pyramid
        .put_tile(TileCoord::new(1, 1, 0), &webp(256, 256))
        .unwrap();
    assert_eq!(registrations(), 1, "and not once per tile");
}

#[test]
fn a_batch_write_commits_in_batches() {
    let (_dir, gpkg) = gpkg();
    let pyramid = gpkg
        .create_tile_pyramid(&quad_builder("basemap", 4))
        .unwrap();
    let tile = png(256, 256);
    let tiles: Vec<(TileCoord, &[u8])> = (0..8)
        .map(|column| (TileCoord::new(3, column, 0), tile.as_slice()))
        .collect();
    assert_eq!(pyramid.write_all(tiles, 3).unwrap(), 8);
    assert_eq!(pyramid.tile_count_at(3).unwrap(), 8);

    // A failure part-way leaves earlier batches written and rolls back the one
    // in flight.
    let bad: Vec<(TileCoord, Vec<u8>)> = vec![
        (TileCoord::new(2, 0, 0), tile.clone()),
        (TileCoord::new(2, 1, 0), tile.clone()),
        (TileCoord::new(2, 2, 0), png(512, 512)),
        (TileCoord::new(2, 3, 0), tile.clone()),
    ];
    pyramid.write_all(bad, 2).unwrap_err();
    assert_eq!(
        pyramid.tile_count_at(2).unwrap(),
        2,
        "the committed batch survives, the failing one does not"
    );
}

#[test]
fn a_dropped_writer_writes_nothing() {
    let (_dir, gpkg) = gpkg();
    let pyramid = gpkg
        .create_tile_pyramid(&quad_builder("basemap", 2))
        .unwrap();
    {
        let mut writer = pyramid.writer().unwrap();
        writer.put(TileCoord::new(0, 0, 0), &png(256, 256)).unwrap();
    }
    assert_eq!(pyramid.tile_count().unwrap(), 0);

    let before: String = gpkg
        .connection()
        .query_row(
            "SELECT last_change FROM gpkg_contents WHERE table_name = 'basemap'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let writer = pyramid.writer().unwrap();
    writer.commit().unwrap();
    let after: String = gpkg
        .connection()
        .query_row(
            "SELECT last_change FROM gpkg_contents WHERE table_name = 'basemap'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(before, after, "a writer that wrote nothing stamps nothing");
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
