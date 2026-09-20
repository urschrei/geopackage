//! Tiled gridded coverages: opening one, what its ancillary tables say, and
//! what its payloads are allowed to be.
//!
//! The fixture is `gdal_coverage.gpkg` from `scripts/generate_fixtures.py`:
//! one 64-pixel float32 tile, LZW compressed, written by `gdal_translate` with
//! `TILE_FORMAT=TIFF`. Reading a coverage this crate did not write is the
//! whole of what it can do today, so the fixture is the subject of most of
//! what follows.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]
#![expect(
    clippy::float_cmp,
    reason = "the ancillary columns are asserted against the literals GDAL wrote and the scale/offset arithmetic against values chosen to be exact in binary, so exact equality is the property under test rather than an approximation of one"
)]

use geopackage::core::coverage::{CoverageDatatype, SampleType, TiffCompression};
use geopackage::core::tiles::{TileCoord, TileMatrixSet, ZoomLadder};
use geopackage::{
    ContentsDataType, Coverage, Error, GeoPackage, TilePyramidBuilder, core::TileError,
};

fn fixture() -> GeoPackage {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gdal_coverage.gpkg");
    GeoPackage::open_read_only(path).unwrap()
}

fn the_tile(coverage: &Coverage<'_>) -> Vec<u8> {
    coverage
        .get_tile(TileCoord::new(0, 0, 0))
        .unwrap()
        .expect("the fixture's single tile")
}

#[test]
fn a_gdal_written_coverage_opens_and_describes_itself() {
    let gpkg = fixture();
    let coverage = gpkg.coverage("coverage").unwrap();

    assert_eq!(coverage.table_name(), "coverage");
    assert_eq!(coverage.datatype(), Some(CoverageDatatype::Float));
    assert_eq!(coverage.zoom_levels(), vec![0]);
    let matrix = *coverage.matrix(0).unwrap();
    assert_eq!((matrix.tile_width, matrix.tile_height), (64, 64));
    assert_eq!(coverage.matrix_set().srs_id, 3857);
    assert_eq!(coverage.tile_count().unwrap(), 1);
    assert_eq!(coverage.tile_count_at(0).unwrap(), 1);
    // GDAL's own coverage satisfies the grid rules our writes are checked
    // against.
    coverage.validate().unwrap();

    let ancillary = coverage.ancillary();
    assert_eq!(ancillary.tile_matrix_set_name, "coverage");
    assert_eq!(ancillary.datatype, "float");
    // Requirement 11: a float coverage keeps the default scale and offset.
    assert_eq!(ancillary.scale, 1.0);
    assert_eq!(ancillary.offset, 0.0);
    assert_eq!(ancillary.data_null, Some(-9999.0));
    assert_eq!(
        ancillary.grid_cell_encoding.as_deref(),
        Some("grid-value-is-center")
    );
    assert_eq!(ancillary.field_name.as_deref(), Some("Height"));
}

#[test]
fn a_coverage_is_listed_as_a_coverage_and_not_as_a_pyramid() {
    let gpkg = fixture();

    let names: Vec<String> = gpkg
        .coverages()
        .unwrap()
        .iter()
        .map(|coverage| coverage.table_name().to_owned())
        .collect();
    assert_eq!(names, vec!["coverage".to_owned()]);

    // C6: the two data types do not mix. A coverage is not in the pyramid
    // listing, and `tiles()` refuses to open one as a pyramid, which is what
    // keeps a TIFF payload away from a writer that would refuse it.
    assert!(gpkg.tile_pyramids().unwrap().is_empty());
    assert!(matches!(
        gpkg.tiles("coverage"),
        Err(Error::WrongDataType {
            expected: "tiles",
            ..
        })
    ));
    assert_eq!(
        gpkg.contents()
            .unwrap()
            .first()
            .map(|entry| &entry.data_type),
        Some(&ContentsDataType::Coverage)
    );
}

#[test]
fn the_payload_is_checked_against_the_coverage_that_holds_it() {
    let gpkg = fixture();
    let coverage = gpkg.coverage("coverage").unwrap();
    let payload = coverage.check_payload(&the_tile(&coverage)).unwrap();

    assert_eq!(payload.mime_type(), "image/tiff");
    assert_eq!(payload.sample_type(), SampleType::Float32);
    assert_eq!((payload.width(), payload.height()), (64, 64));
    assert!(matches!(
        payload,
        geopackage::core::coverage::CoveragePayload::Tiff(tiff) if tiff.compression == TiffCompression::Lzw
    ));
    // And the payload's size is the one its zoom level declares.
    coverage
        .matrix(0)
        .unwrap()
        .check_payload(&geopackage::core::tiles::probe(&the_tile(&coverage)).unwrap())
        .unwrap();
}

#[test]
fn a_payload_of_the_wrong_kind_names_the_requirement_it_breaks() {
    let gpkg = fixture();
    let coverage = gpkg.coverage("coverage").unwrap();

    // A PNG under a float coverage: Requirement 14 wants image/tiff, 32-bit
    // floating point.
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    png.extend_from_slice(&13u32.to_be_bytes());
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&64u32.to_be_bytes());
    png.extend_from_slice(&64u32.to_be_bytes());
    png.extend_from_slice(&[16, 0, 0, 0, 0]);
    match coverage.check_payload(&png) {
        Err(Error::Tile(TileError::CoverageProfileViolation { requirement, .. })) => {
            assert_eq!(requirement, 14);
        }
        other => panic!("expected a Requirement 14 violation, got {other:?}"),
    }

    // A JPEG is neither encoding, which is unreadable rather than
    // non-conformant.
    assert!(matches!(
        coverage.check_payload(b"\xff\xd8\xff\xe0 a JPEG"),
        Err(Error::Tile(TileError::UnreadablePayload { .. }))
    ));
}

#[test]
fn a_tile_carries_its_own_scale_offset_and_statistics() {
    let gpkg = fixture();
    let coverage = gpkg.coverage("coverage").unwrap();
    let tile = coverage
        .tile_ancillary(TileCoord::new(0, 0, 0))
        .unwrap()
        .expect("Requirement 10: every tile has an ancillary row");

    // Requirement 11 again, on the per-tile pair this time.
    assert_eq!((tile.scale, tile.offset), (1.0, 0.0));
    // GDAL computes the statistics because it decodes the samples; this crate
    // does not, which is why they are optional here.
    assert_eq!(tile.min, Some(0.5));
    assert_eq!(tile.max, Some(96.5));
    assert!(tile.mean.is_some() && tile.std_dev.is_some());

    // The extension's arithmetic, on a sample decoded elsewhere. Both pairs
    // are defaults here, so a stored sample is already a value.
    assert_eq!(coverage.value(&tile, 12.5), 12.5);
    // ... and the null is matched against the stored sample, untouched by
    // either pair.
    assert!(coverage.is_null(-9999.0));
    assert!(!coverage.is_null(0.5));

    assert_eq!(
        coverage.tile_ancillary(TileCoord::new(0, 5, 5)).unwrap(),
        None,
        "no tile there, so no ancillary row"
    );
}

#[test]
fn the_scale_and_offset_pairs_apply_in_the_spec_s_order() {
    // A coverage whose pairs are not the defaults cannot be written yet, so
    // the arithmetic is checked on values rather than on a file: the tile pair
    // first, the coverage pair second.
    let gpkg = fixture();
    let coverage = gpkg.coverage("coverage").unwrap();
    let mut tile = coverage
        .tile_ancillary(TileCoord::new(0, 0, 0))
        .unwrap()
        .unwrap();
    tile.scale = 2.0;
    tile.offset = 1.0;
    // (10 * 2 + 1) * 1 + 0, since the fixture's coverage pair is the default.
    assert_eq!(coverage.value(&tile, 10.0), 21.0);
}

#[test]
fn the_tiles_of_a_coverage_walk_like_any_other_pyramid() {
    let gpkg = fixture();
    let coverage = gpkg.coverage("coverage").unwrap();
    let mut cursor = coverage.cursor().unwrap();
    let mut stream = cursor.tiles().unwrap();

    let mut seen = 0;
    while let Some(tile) = stream.next().unwrap() {
        seen += 1;
        assert_eq!(tile.coord(), TileCoord::new(0, 0, 0));
        // The payload borrows the row, and is checked without being copied.
        coverage.check_payload(tile.data()).unwrap();
    }
    assert_eq!(seen, 1);
}

#[test]
fn opening_something_that_is_not_a_coverage_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    gpkg.add_epsg_srs(3857).unwrap();
    let set = TileMatrixSet::web_mercator_quad();
    let matrices = set.ladder(ZoomLadder::new(0, 1)).unwrap();
    gpkg.create_tile_pyramid(&TilePyramidBuilder::new("basemap", set).matrices(matrices))
        .unwrap();

    // A tile pyramid is not a coverage ...
    assert!(matches!(
        gpkg.coverage("basemap"),
        Err(Error::WrongDataType {
            expected: "2d-gridded-coverage",
            ..
        })
    ));
    // ... a table that is not there at all is its own error ...
    assert!(matches!(
        gpkg.coverage("absent"),
        Err(Error::NoSuchLayer { .. })
    ));
    // ... and a file with no coverages lists none.
    assert!(gpkg.coverages().unwrap().is_empty());
}

#[test]
fn a_coverage_with_no_ancillary_row_is_refused() {
    // Requirements 1 and 7: without the row there is no datatype, no scale or
    // offset and no null, so a sample is a number with nothing attached. The
    // handle refuses rather than inventing defaults.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hollow.gpkg");
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gdal_coverage.gpkg"),
        &path,
    )
    .unwrap();
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("DELETE FROM gpkg_2d_gridded_coverage_ancillary", [])
            .unwrap();
    }

    let gpkg = GeoPackage::open_read_only(&path).unwrap();
    assert!(matches!(
        gpkg.coverage("coverage"),
        Err(Error::NoCoverageAncillary { .. })
    ));
    // And a listing of coverages surfaces that rather than skipping the file.
    gpkg.coverages().unwrap_err();
}
