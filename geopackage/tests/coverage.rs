//! Tiled gridded coverages: the handle, the ancillary tables, and the payload
//! checks.
//!
//! The fixture is `gdal_coverage.gpkg` from `scripts/generate_fixtures.py`:
//! one 64-pixel float32 tile, LZW compressed, written by `gdal_translate` with
//! `TILE_FORMAT=TIFF`. The read tests use this fixture.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]
#![expect(
    clippy::float_cmp,
    reason = "the ancillary columns are asserted against the literals GDAL wrote and the scale/offset arithmetic against values chosen to be exact in binary, so exact equality is the property under test rather than an approximation of one"
)]

use geopackage::core::coverage::{CoverageDatatype, SampleType, TiffCompression};
use geopackage::core::tiles::{TileCoord, TileMatrix, TileMatrixSet, ZoomLadder};
use geopackage::{
    ContentsDataType, Coverage, CoverageBuilder, Error, GeoPackage, TileAncillary,
    TilePyramidBuilder, core::TileError,
};
use hegel::generators;

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
    // The GDAL coverage satisfies the grid rules that this crate applies to
    // its own writes.
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

    // C6: the two data types are separate. A coverage is not in the pyramid
    // list, and `tiles()` does not open a coverage as a pyramid. This keeps a
    // TIFF payload away from the tile writer, which rejects TIFF.
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
    // The size of the payload is the size that its zoom level declares.
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

    // A PNG in a float coverage: Requirement 14 specifies image/tiff, 32-bit
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
    // GDAL calculates the statistics because it decodes the samples. This
    // crate does not decode samples, so the statistics are optional here.
    assert_eq!(tile.min, Some(0.5));
    assert_eq!(tile.max, Some(96.5));
    assert!(tile.mean.is_some() && tile.std_dev.is_some());

    // The arithmetic of the extension, on a sample decoded elsewhere. Both
    // pairs are the defaults, so a stored sample is a value.
    assert_eq!(coverage.value(&tile, 12.5), 12.5);
    // The null is compared with the stored sample, before either pair
    // applies.
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
    // The test checks the arithmetic on values, not on a file: the tile pair
    // first, then the coverage pair.
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
    // offset and no null, so a sample has no meaning. The handle returns an
    // error and does not use default values.
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
    // The list of coverages also returns the error, and does not skip the
    // coverage.
    gpkg.coverages().unwrap_err();
}

// --- the write path -----------------------------------------------------------

/// A payload that this crate can write: a conforming coverage TIFF header of
/// the given size and sample type, built as the core tests build one.
///
/// The payload is a header only. The profile is about tags, and neither side of
/// the write decodes a sample.
fn tiff(width: u32, height: u32, sample_format: u16, bits: u16, compression: u16) -> Vec<u8> {
    let entries: [(u16, u16, u32); 7] = [
        (256, 3, u32::from(u16::try_from(width).unwrap())), // ImageWidth
        (257, 3, u32::from(u16::try_from(height).unwrap())), // ImageLength
        (258, 3, u32::from(bits)),                          // BitsPerSample
        (259, 3, u32::from(compression)),                   // Compression
        (277, 3, 1),                                        // SamplesPerPixel
        (278, 3, u32::from(u16::try_from(height).unwrap())), // RowsPerStrip
        (339, 3, u32::from(sample_format)),                 // SampleFormat
    ];
    let mut bytes = b"II\x2a\x00".to_vec();
    bytes.extend_from_slice(&8u32.to_le_bytes());
    bytes.extend_from_slice(&u16::try_from(entries.len()).unwrap().to_le_bytes());
    for (tag, field_type, value) in entries {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&field_type.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&u16::try_from(value).unwrap().to_le_bytes());
        bytes.extend_from_slice(&[0, 0]);
    }
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes
}

/// A float32 LZW payload of the given size, for a float coverage.
fn float_tile(side: u32) -> Vec<u8> {
    tiff(side, side, 3, 32, 5)
}

/// A new file with one coverage over a 256-unit square, one zoom level of a
/// single 256-pixel tile.
fn new_coverage(datatype: CoverageDatatype) -> (tempfile::TempDir, GeoPackage, String) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("dem.gpkg")).unwrap();
    gpkg.add_epsg_srs(3857).unwrap();
    let set = TileMatrixSet::new(3857, 0.0, 0.0, 256.0, 256.0);
    let coverage = gpkg
        .create_coverage(
            &CoverageBuilder::new("dem", set, datatype)
                .matrix(TileMatrix::new(0, 1, 1, 256, 256, 1.0, 1.0))
                .data_null(-9999.0)
                .uom("m"),
        )
        .unwrap();
    let name = coverage.table_name().to_owned();
    (dir, gpkg, name)
}

/// Returns the tile ids in the file, and the `tpudt_id` values of the
/// ancillary rows.
fn pairing(gpkg: &GeoPackage, table: &str) -> (Vec<i64>, Vec<i64>) {
    let conn = gpkg.connection();
    let tiles: Vec<i64> = conn
        .prepare(&format!("SELECT id FROM {table} ORDER BY id"))
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let rows: Vec<i64> = conn
        .prepare(
            "SELECT tpudt_id FROM gpkg_2d_gridded_tile_ancillary \
             WHERE tpudt_name = ?1 ORDER BY tpudt_id",
        )
        .unwrap()
        .query_map([table], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    (tiles, rows)
}

#[test]
fn a_coverage_this_crate_writes_reads_back_as_one() {
    let (_dir, gpkg, name) = new_coverage(CoverageDatatype::Float);
    let coverage = gpkg.coverage(&name).unwrap();

    // The catalogue rows that the extension specifies (Requirements 1, 2, 5,
    // 6, 7).
    assert_eq!(
        gpkg.contents().unwrap().first().map(|e| &e.data_type),
        Some(&ContentsDataType::Coverage)
    );
    assert_eq!(coverage.datatype(), Some(CoverageDatatype::Float));
    assert_eq!(coverage.ancillary().uom.as_deref(), Some("m"));
    assert_eq!(coverage.ancillary().data_null, Some(-9999.0));
    let registered: Vec<(Option<String>, Option<String>)> = gpkg
        .extensions()
        .unwrap()
        .into_iter()
        .filter(|row| row.name == "gpkg_2d_gridded_coverage")
        .map(|row| (row.table_name, row.column_name))
        .collect();
    // In the order of `extensions()`, which is the catalogue order, not the
    // order of the writes.
    assert_eq!(
        registered,
        vec![
            (Some("dem".to_owned()), Some("tile_data".to_owned())),
            (Some("gpkg_2d_gridded_coverage_ancillary".to_owned()), None),
            (Some("gpkg_2d_gridded_tile_ancillary".to_owned()), None),
        ]
    );
    // And it is still not a pyramid.
    assert!(gpkg.tile_pyramids().unwrap().is_empty());
    coverage.validate().unwrap();
    assert_eq!(gpkg.validate().unwrap(), Vec::new());
}

#[test]
fn every_written_tile_gets_an_ancillary_row() {
    // Requirement 10, on the path that writes one tile at a time.
    let (_dir, gpkg, name) = new_coverage(CoverageDatatype::Float);
    let coverage = gpkg.coverage(&name).unwrap();
    coverage
        .put_tile(TileCoord::new(0, 0, 0), &float_tile(256))
        .unwrap();

    let (tiles, rows) = pairing(&gpkg, &name);
    assert_eq!(tiles.len(), 1);
    assert_eq!(rows, tiles);
    let ancillary = coverage
        .tile_ancillary(TileCoord::new(0, 0, 0))
        .unwrap()
        .unwrap();
    assert_eq!((ancillary.scale, ancillary.offset), (1.0, 0.0));
    // No statistics: this crate does not decode samples, so it has no
    // statistics to record.
    assert_eq!(ancillary.min, None);
    assert_eq!(gpkg.validate().unwrap(), Vec::new());
}

#[test]
fn a_caller_that_decoded_the_samples_can_record_them() {
    let (_dir, gpkg, name) = new_coverage(CoverageDatatype::Integer);
    let coverage = gpkg.coverage(&name).unwrap();
    let mut writer = coverage.writer().unwrap();
    writer
        .put_with_ancillary(
            TileCoord::new(0, 0, 0),
            &tiff(256, 256, 1, 16, 5),
            &TileAncillary::new(0.5, 100.0).with_statistics(1.0, 9.0, 5.0, 2.0),
        )
        .unwrap();
    writer.commit().unwrap();

    let ancillary = coverage
        .tile_ancillary(TileCoord::new(0, 0, 0))
        .unwrap()
        .unwrap();
    assert_eq!((ancillary.scale, ancillary.offset), (0.5, 100.0));
    assert_eq!(
        (
            ancillary.min,
            ancillary.max,
            ancillary.mean,
            ancillary.std_dev
        ),
        (Some(1.0), Some(9.0), Some(5.0), Some(2.0))
    );
    // The arithmetic gives the result that the extension defines.
    assert_eq!(coverage.value(&ancillary, 4.0), 102.0);
}

#[test]
fn deleting_a_tile_deletes_its_ancillary_row() {
    // The normative table definition has no ON DELETE CASCADE, so the writer
    // keeps the two tables consistent.
    let (_dir, gpkg, name) = new_coverage(CoverageDatatype::Float);
    let coverage = gpkg.coverage(&name).unwrap();
    coverage
        .put_tile(TileCoord::new(0, 0, 0), &float_tile(256))
        .unwrap();
    assert!(coverage.delete_tile(TileCoord::new(0, 0, 0)).unwrap());

    assert_eq!(pairing(&gpkg, &name), (Vec::new(), Vec::new()));
    assert!(!coverage.delete_tile(TileCoord::new(0, 0, 0)).unwrap());
    assert_eq!(gpkg.validate().unwrap(), Vec::new());
}

#[test]
fn rewriting_a_tile_keeps_one_ancillary_row() {
    let (_dir, gpkg, name) = new_coverage(CoverageDatatype::Float);
    let coverage = gpkg.coverage(&name).unwrap();
    for _ in 0..3 {
        coverage
            .put_tile(TileCoord::new(0, 0, 0), &float_tile(256))
            .unwrap();
    }
    let (tiles, rows) = pairing(&gpkg, &name);
    assert_eq!((tiles.len(), rows.len()), (1, 1));
    assert_eq!(rows, tiles);
}

#[test]
fn a_float_coverage_may_not_scale_its_samples() {
    // Requirement 11, on the coverage row ...
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("dem.gpkg")).unwrap();
    gpkg.add_epsg_srs(3857).unwrap();
    let set = TileMatrixSet::new(3857, 0.0, 0.0, 256.0, 256.0);
    assert!(matches!(
        gpkg.create_coverage(
            &CoverageBuilder::new("dem", set, CoverageDatatype::Float)
                .matrix(TileMatrix::new(0, 1, 1, 256, 256, 1.0, 1.0))
                .scale(2.0, 0.0),
        ),
        Err(Error::FloatCoverageScaled { .. })
    ));

    // ... and on a tile's.
    let (_dir, gpkg, name) = new_coverage(CoverageDatatype::Float);
    let coverage = gpkg.coverage(&name).unwrap();
    let mut writer = coverage.writer().unwrap();
    assert!(matches!(
        writer.put_with_ancillary(
            TileCoord::new(0, 0, 0),
            &float_tile(256),
            &TileAncillary::new(2.0, 0.0)
        ),
        Err(Error::FloatCoverageScaled { .. })
    ));
}

#[test]
fn a_payload_the_coverage_may_not_hold_is_refused_on_write() {
    let (_dir, gpkg, name) = new_coverage(CoverageDatatype::Float);
    let coverage = gpkg.coverage(&name).unwrap();
    let mut writer = coverage.writer().unwrap();

    // Requirement 14: a float coverage accepts float32 TIFF only.
    assert!(matches!(
        writer.put(TileCoord::new(0, 0, 0), &tiff(256, 256, 1, 16, 5)),
        Err(Error::Tile(TileError::CoverageProfileViolation {
            requirement: 14,
            ..
        }))
    ));
    // Requirement 16: one sample per grid cell.
    let mut multi_band = float_tile(256);
    // SamplesPerPixel is the fifth entry: 8 header + 2 count + 4 * 12.
    multi_band.splice(66..68, 3u16.to_le_bytes());
    assert!(matches!(
        writer.put(TileCoord::new(0, 0, 0), &multi_band),
        Err(Error::Tile(TileError::CoverageProfileViolation {
            requirement: 16,
            ..
        }))
    ));
    // The zoom level's pixel size still applies.
    assert!(matches!(
        writer.put(TileCoord::new(0, 0, 0), &float_tile(128)),
        Err(Error::Tile(TileError::PayloadSizeMismatch { .. }))
    ));
    // The writer wrote none of these payloads.
    drop(writer);
    assert_eq!(pairing(&gpkg, &name), (Vec::new(), Vec::new()));
}

#[test]
fn deflate_is_read_but_not_written() {
    // C2: Requirement 18 does not say "only LZW", so a read accepts a Deflate
    // payload. Requirement 15 specifies baseline TIFF, so a write rejects it.
    let (_dir, gpkg, name) = new_coverage(CoverageDatatype::Float);
    let coverage = gpkg.coverage(&name).unwrap();
    let deflate = tiff(256, 256, 3, 32, 8);
    coverage.check_payload(&deflate).unwrap();

    let mut writer = coverage.writer().unwrap();
    assert!(matches!(
        writer.put(TileCoord::new(0, 0, 0), &deflate),
        Err(Error::UnwritableCoveragePayload { .. })
    ));
}

/// For any sequence of writes and deletes, the two tables agree: each tile has
/// exactly one ancillary row, and no row points to a tile that does not exist
/// (Requirements 10 and 12).
///
/// The table definition has no `ON DELETE CASCADE`, so the writer must keep
/// this invariant. The test checks it after each step, not only at the end, so
/// a failing sequence shrinks to the step that broke it.
#[hegel::test]
fn the_two_tables_stay_in_step_through_write_ops(tc: hegel::TestCase) {
    let (_dir, gpkg, name) = new_coverage(CoverageDatatype::Float);
    let coverage = gpkg.coverage(&name).unwrap();
    let payload = float_tile(256);

    let ops = tc.draw(generators::integers::<usize>().min_value(0).max_value(12));
    for _ in 0..ops {
        // With one zoom level of one tile, every operation would use the same
        // address. The test therefore uses a 2x2 address space that is larger
        // than the matrix. The writer rejects a write outside the matrix, and
        // the test covers that case too.
        let column = tc.draw(generators::integers::<i64>().min_value(0).max_value(1));
        let row = tc.draw(generators::integers::<i64>().min_value(0).max_value(1));
        let coord = TileCoord::new(0, column, row);
        // Both results are valid: the grid of this coverage is one tile, so
        // three of the four addresses are outside it and the writer rejects
        // them. The invariant below also checks that a rejected write leaves
        // nothing.
        if tc.draw(generators::booleans()) {
            let _outcome = coverage.put_tile(coord, &payload);
        } else {
            let _outcome = coverage.delete_tile(coord);
        }
        let (tiles, rows) = pairing(&gpkg, &name);
        assert_eq!(rows, tiles, "the ancillary rows diverged from the tiles");
    }
}

// --- the PNG encoding ---------------------------------------------------------

fn png_fixture() -> GeoPackage {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/gdal_coverage_png.gpkg");
    GeoPackage::open_read_only(path).unwrap()
}

#[test]
fn an_integer_coverage_may_store_png() {
    // The other encoding of Requirement 13, from another encoder. GDAL writes
    // it when it quantises a float source to 16-bit unsigned values.
    let gpkg = png_fixture();
    let coverage = gpkg.coverage("elevation").unwrap();

    assert_eq!(coverage.datatype(), Some(CoverageDatatype::Integer));
    let payload = coverage
        .check_payload(&coverage.get_tile(TileCoord::new(0, 0, 0)).unwrap().unwrap())
        .unwrap();
    assert_eq!(payload.mime_type(), "image/png");
    // Requirement 13 permits one PNG form only, so the encoding gives the
    // sample type.
    assert_eq!(payload.sample_type(), SampleType::Unsigned(16));
    assert_eq!((payload.width(), payload.height()), (64, 64));
    assert_eq!(gpkg.validate().unwrap(), Vec::new());
}

#[test]
fn a_per_tile_scale_and_offset_is_what_carries_quantised_samples_back() {
    // A pair other than the defaults, on an integer coverage: the coverage
    // pair is the default, and the tile pair converts the samples.
    let gpkg = png_fixture();
    let coverage = gpkg.coverage("elevation").unwrap();
    assert_eq!(
        (coverage.ancillary().scale, coverage.ancillary().offset),
        (1.0, 0.0)
    );

    let tile = coverage
        .tile_ancillary(TileCoord::new(0, 0, 0))
        .unwrap()
        .unwrap();
    assert!(tile.scale < 1.0 && tile.offset == 0.5, "{tile:?}");
    // GDAL recorded the range that it quantised, so the test can check the
    // arithmetic against the GDAL values: the smallest stored sample gives the
    // minimum, and the largest gives the maximum.
    let (min, max) = (tile.min.unwrap(), tile.max.unwrap());
    assert_eq!(coverage.value(&tile, 0.0), min);
    let largest = ((max - tile.offset) / tile.scale).round();
    assert!(
        (coverage.value(&tile, largest) - max).abs() < 1e-3,
        "{} is not {max}",
        coverage.value(&tile, largest)
    );
}

#[test]
fn the_two_encodings_are_told_apart_by_what_the_payload_is() {
    // A float coverage does not permit the PNG, and an integer coverage does
    // not permit the float32 TIFF: Requirements 13 and 14, checked against two
    // files from another implementation, not against payloads that this crate
    // built.
    let png_gpkg = png_fixture();
    let png_coverage = png_gpkg.coverage("elevation").unwrap();
    let png_tile = png_coverage
        .get_tile(TileCoord::new(0, 0, 0))
        .unwrap()
        .unwrap();

    let tiff_gpkg = fixture();
    let tiff_coverage = tiff_gpkg.coverage("coverage").unwrap();
    let tiff_tile = the_tile(&tiff_coverage);

    match tiff_coverage.check_payload(&png_tile) {
        Err(Error::Tile(TileError::CoverageProfileViolation { requirement, .. })) => {
            assert_eq!(requirement, 14, "a PNG in a float coverage");
        }
        other => panic!("expected a Requirement 14 violation, got {other:?}"),
    }
    match png_coverage.check_payload(&tiff_tile) {
        Err(Error::Tile(TileError::CoverageProfileViolation { requirement, .. })) => {
            assert_eq!(requirement, 13, "float samples in an integer coverage");
        }
        other => panic!("expected a Requirement 13 violation, got {other:?}"),
    }
}

#[test]
fn a_png_coverage_round_trips_through_this_crate() {
    // The write path also accepts the other encoding, with its per-tile pair.
    let source = png_fixture();
    let source_coverage = source.coverage("elevation").unwrap();
    let payload = source_coverage
        .get_tile(TileCoord::new(0, 0, 0))
        .unwrap()
        .unwrap();
    let ancillary = source_coverage
        .tile_ancillary(TileCoord::new(0, 0, 0))
        .unwrap()
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("ours.gpkg")).unwrap();
    gpkg.add_epsg_srs(3857).unwrap();
    let set = TileMatrixSet::new(3857, 0.0, 0.0, 6400.0, 6400.0);
    let coverage = gpkg
        .create_coverage(
            &CoverageBuilder::new("elevation", set, CoverageDatatype::Integer)
                .matrix(TileMatrix::new(0, 1, 1, 64, 64, 100.0, 100.0))
                .data_null(65535.0),
        )
        .unwrap();
    let mut writer = coverage.writer().unwrap();
    writer
        .put_with_ancillary(TileCoord::new(0, 0, 0), &payload, &ancillary)
        .unwrap();
    writer.commit().unwrap();

    let written = gpkg.coverage("elevation").unwrap();
    assert_eq!(
        written.tile_ancillary(TileCoord::new(0, 0, 0)).unwrap(),
        Some(ancillary),
        "the per-tile pair and statistics survive the round trip"
    );
    assert_eq!(gpkg.validate().unwrap(), Vec::new());
}
