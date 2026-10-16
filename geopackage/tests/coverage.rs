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

use std::collections::BTreeMap;

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

/// A float32 LZW payload of the given size, every sample zero, encoded by
/// `tiff`: a whole image rather than a header, for a test that validates the
/// file. With the `coverage-samples` feature, `validate` decodes coverage
/// payloads, and a header with no image data is then a finding of its own.
fn encoded_float_tile(side: u32) -> Vec<u8> {
    let count = usize::try_from(side * side).unwrap();
    let mut bytes = std::io::Cursor::new(Vec::new());
    tiff::encoder::TiffEncoder::new(&mut bytes)
        .unwrap()
        .with_compression(tiff::encoder::Compression::Lzw)
        .write_image::<tiff::encoder::colortype::Gray32Float>(side, side, &vec![0.0; count])
        .unwrap();
    bytes.into_inner()
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
        .put_tile(TileCoord::new(0, 0, 0), &encoded_float_tile(256))
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

// --- write sequences ----------------------------------------------------------
//
// Two properties over drawn sequences of writer sessions. Each session opens a
// `CoverageWriter`, applies drawn puts and deletes, and then commits or drops
// the writer. Each put has at most one defect, and each defect is one of the
// errors that `CoverageWriter::put_with_ancillary` documents, so the expected
// outcome of every put is known before it runs.

/// The shape of a drawn coverage: one zoom level, level 0, of `columns` by
/// `rows` tiles of `side` pixels.
#[derive(Debug, Clone, Copy)]
struct Grid {
    datatype: CoverageDatatype,
    columns: u16,
    rows: u16,
    side: u16,
}

/// Draws a grid. The grid is small so that drawn addresses often repeat, which
/// makes rewrites and deletes of existing tiles common.
fn draw_grid(tc: &hegel::TestCase) -> Grid {
    Grid {
        datatype: tc.draw(generators::sampled_from(vec![
            CoverageDatatype::Integer,
            CoverageDatatype::Float,
        ])),
        columns: tc.draw(generators::integers::<u16>().min_value(1).max_value(4)),
        rows: tc.draw(generators::integers::<u16>().min_value(1).max_value(4)),
        side: tc.draw(generators::integers::<u16>().min_value(1)),
    }
}

/// A coverage of the drawn grid in SQLite's in-memory database, at one ground
/// unit per pixel.
///
/// In memory for the reason that `in_memory` in `tiles_property.rs` gives:
/// each case builds a container, and nothing these properties check depends
/// on a file.
fn in_memory_coverage(grid: Grid) -> GeoPackage {
    let gpkg = GeoPackage::create(std::path::Path::new(":memory:")).unwrap();
    gpkg.add_epsg_srs(3857).unwrap();
    let set = TileMatrixSet::new(
        3857,
        0.0,
        0.0,
        f64::from(u32::from(grid.columns) * u32::from(grid.side)),
        f64::from(u32::from(grid.rows) * u32::from(grid.side)),
    );
    gpkg.create_coverage(
        &CoverageBuilder::new("dem", set, grid.datatype).matrix(TileMatrix::new(
            0,
            i64::from(grid.columns),
            i64::from(grid.rows),
            i64::from(grid.side),
            i64::from(grid.side),
            1.0,
            1.0,
        )),
    )
    .unwrap();
    gpkg
}

/// A 16-bit greyscale PNG header of the given size: the signature and a
/// complete `IHDR` chunk, with a zero CRC that no reader checks.
fn png16(width: u16, height: u16) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&13_u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&u32::from(width).to_be_bytes());
    bytes.extend_from_slice(&u32::from(height).to_be_bytes());
    bytes.extend_from_slice(&[16, 0, 0, 0, 0]);
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    bytes
}

/// Draws a payload of the given size that a coverage of `datatype` accepts:
/// every sample type and baseline compression that the profile permits.
fn draw_valid_payload(
    tc: &hegel::TestCase,
    datatype: CoverageDatatype,
    width: u16,
    height: u16,
) -> Vec<u8> {
    let compression = tc.draw(generators::sampled_from(vec![1_u16, 5]));
    match datatype {
        CoverageDatatype::Float => tiff(width.into(), height.into(), 3, 32, compression),
        _ => {
            if tc.draw(generators::booleans()) {
                png16(width, height)
            } else {
                let sample_format = tc.draw(generators::sampled_from(vec![1_u16, 2]));
                let bits = tc.draw(generators::sampled_from(vec![8_u16, 16, 32]));
                tiff(
                    width.into(),
                    height.into(),
                    sample_format,
                    bits,
                    compression,
                )
            }
        }
    }
}

/// Draws an index outside `0..len`, on either side of the range.
fn draw_outside(tc: &hegel::TestCase, len: u16) -> i64 {
    if tc.draw(generators::booleans()) {
        tc.draw(generators::integers::<i64>().max_value(-1))
    } else {
        tc.draw(generators::integers::<i64>().min_value(i64::from(len)))
    }
}

/// Draws an address inside the grid.
fn draw_inside(tc: &hegel::TestCase, grid: Grid) -> TileCoord {
    TileCoord::new(
        0,
        tc.draw(
            generators::integers::<i64>()
                .min_value(0)
                .max_value(i64::from(grid.columns) - 1),
        ),
        tc.draw(
            generators::integers::<i64>()
                .min_value(0)
                .max_value(i64::from(grid.rows) - 1),
        ),
    )
}

/// Draws any `f64` but NaN, which a put rejects (see `Defect::NanAncillary`).
/// The infinities and negative zero are included.
fn draw_non_nan_f64(tc: &hegel::TestCase) -> f64 {
    tc.draw(generators::floats::<f64>().allow_nan(false))
}

/// Draws an ancillary row with no NaN. On an `integer` coverage the scale and
/// offset are any other `f64`. On a `float` coverage they are the defaults
/// (Requirement 11), unless `scaled` is set, in which case at least one
/// differs from them.
fn draw_ancillary(tc: &hegel::TestCase, datatype: CoverageDatatype, scaled: bool) -> TileAncillary {
    let mut ancillary = if datatype == CoverageDatatype::Integer || scaled {
        let (scale, offset) = (draw_non_nan_f64(tc), draw_non_nan_f64(tc));
        if scaled {
            // Compare values, not bits: an offset of -0.0 is the default.
            tc.assume(scale != 1.0 || offset != 0.0);
        }
        TileAncillary::new(scale, offset)
    } else {
        TileAncillary::defaults()
    };
    if tc.draw(generators::booleans()) {
        ancillary = ancillary.with_statistics(
            draw_non_nan_f64(tc),
            draw_non_nan_f64(tc),
            draw_non_nan_f64(tc),
            draw_non_nan_f64(tc),
        );
    }
    ancillary
}

/// What is wrong with a drawn put, if anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Defect {
    None,
    UnknownZoomLevel,
    OutsideGrid,
    WrongDatatype,
    WrongSize,
    Deflate,
    /// A valid payload that ends before the last header field the profile
    /// check reads.
    Truncated,
    /// Bytes that start as neither a TIFF nor a PNG.
    NotAnImage,
    /// A scale or offset other than the defaults, on a `float` coverage.
    ScaledFloat,
    /// A NaN in the ancillary row: in any of its six values on an `integer`
    /// coverage, and in a statistic on a `float` coverage, where a NaN scale
    /// or offset is `ScaledFloat`.
    NanAncillary(&'static str),
}

/// One drawn writer operation.
#[derive(Debug)]
enum Op {
    Put {
        coord: TileCoord,
        data: Vec<u8>,
        ancillary: TileAncillary,
        defect: Defect,
    },
    Delete {
        coord: TileCoord,
    },
}

fn draw_put(tc: &hegel::TestCase, grid: Grid) -> Op {
    let mut defects = vec![
        Defect::UnknownZoomLevel,
        Defect::OutsideGrid,
        Defect::WrongDatatype,
        Defect::WrongSize,
        Defect::Deflate,
        Defect::Truncated,
        Defect::NotAnImage,
    ];
    let nan_fields = match grid.datatype {
        CoverageDatatype::Float => {
            defects.push(Defect::ScaledFloat);
            vec!["min", "max", "mean", "std_dev"]
        }
        _ => vec!["scale", "offset", "min", "max", "mean", "std_dev"],
    };
    defects.extend(nan_fields.into_iter().map(Defect::NanAncillary));
    // Half the puts are valid, so that the model stores, rewrites and deletes
    // tiles often. The other half divide between the defects.
    let defect = if tc.draw(generators::booleans()) {
        Defect::None
    } else {
        tc.draw(generators::sampled_from(defects))
    };
    tc.event(format!("put with defect {defect:?}"));

    let coord = match defect {
        Defect::UnknownZoomLevel => {
            let zoom_level = tc.draw(generators::integers::<i64>());
            tc.assume(zoom_level != 0);
            TileCoord::new(zoom_level, 0, 0)
        }
        Defect::OutsideGrid => {
            let inside = draw_inside(tc, grid);
            match tc.draw(generators::integers::<u8>().min_value(0).max_value(2)) {
                0 => TileCoord::new(0, draw_outside(tc, grid.columns), inside.row),
                1 => TileCoord::new(0, inside.column, draw_outside(tc, grid.rows)),
                _ => TileCoord::new(
                    0,
                    draw_outside(tc, grid.columns),
                    draw_outside(tc, grid.rows),
                ),
            }
        }
        _ => draw_inside(tc, grid),
    };

    let side = grid.side;
    let data = match defect {
        Defect::WrongDatatype => {
            let other = match grid.datatype {
                CoverageDatatype::Float => CoverageDatatype::Integer,
                _ => CoverageDatatype::Float,
            };
            draw_valid_payload(tc, other, side, side)
        }
        Defect::WrongSize => {
            let width = tc.draw(generators::integers::<u16>().min_value(1));
            let height = tc.draw(generators::integers::<u16>().min_value(1));
            tc.assume((width, height) != (side, side));
            draw_valid_payload(tc, grid.datatype, width, height)
        }
        Defect::Deflate => {
            let compression = tc.draw(generators::sampled_from(vec![8_u16, 0x80B2]));
            match grid.datatype {
                CoverageDatatype::Float => tiff(side.into(), side.into(), 3, 32, compression),
                _ => tiff(side.into(), side.into(), 1, 16, compression),
            }
        }
        Defect::Truncated => {
            let mut full = draw_valid_payload(tc, grid.datatype, side, side);
            // `coverage_png` reads through the colour type, byte 25.
            // `coverage_tiff` reads the whole first IFD, which ends where the
            // payloads that `tiff` builds end.
            let read = if full.starts_with(b"\x89PNG") {
                26
            } else {
                full.len()
            };
            let len = tc.draw(
                generators::integers::<usize>()
                    .min_value(0)
                    .max_value(read - 1),
            );
            full.truncate(len);
            full
        }
        Defect::NotAnImage => {
            let first = tc.draw(generators::integers::<u8>());
            tc.assume(![b'I', b'M', 0x89].contains(&first));
            let mut bytes = vec![first];
            bytes.extend(tc.draw(generators::vecs(generators::integers::<u8>()).max_size(64)));
            bytes
        }
        _ => draw_valid_payload(tc, grid.datatype, side, side),
    };

    let mut ancillary = draw_ancillary(tc, grid.datatype, defect == Defect::ScaledFloat);
    if let Defect::NanAncillary(field) = defect {
        let nan = Some(f64::NAN);
        match field {
            "scale" => ancillary.scale = f64::NAN,
            "offset" => ancillary.offset = f64::NAN,
            "min" => ancillary.min = nan,
            "max" => ancillary.max = nan,
            "mean" => ancillary.mean = nan,
            _ => ancillary.std_dev = nan,
        }
    }
    Op::Put {
        coord,
        data,
        ancillary,
        defect,
    }
}

fn draw_op(tc: &hegel::TestCase, grid: Grid) -> Op {
    if tc.draw(generators::booleans()) {
        draw_put(tc, grid)
    } else {
        // Usually an address in the grid, so that the delete can find a tile.
        let coord = if tc.draw(generators::weighted_booleans(0.9)) {
            draw_inside(tc, grid)
        } else {
            TileCoord::new(
                tc.draw(generators::integers::<i64>()),
                tc.draw(generators::integers::<i64>()),
                tc.draw(generators::integers::<i64>()),
            )
        };
        Op::Delete { coord }
    }
}

/// Returns `true` if `result` is the documented outcome of a put with `defect`.
fn put_outcome_is_documented(grid: Grid, defect: Defect, result: &geopackage::Result<()>) -> bool {
    let datatype_requirement = match grid.datatype {
        CoverageDatatype::Float => 14,
        _ => 13,
    };
    match defect {
        Defect::None => result.is_ok(),
        Defect::UnknownZoomLevel => matches!(result, Err(Error::UnknownZoomLevel { .. })),
        Defect::OutsideGrid => matches!(
            result,
            Err(Error::Tile(TileError::CoordOutsideMatrix { .. }))
        ),
        Defect::WrongDatatype => matches!(
            result,
            Err(Error::Tile(TileError::CoverageProfileViolation { requirement, .. }))
                if *requirement == datatype_requirement
        ),
        Defect::WrongSize => matches!(
            result,
            Err(Error::Tile(TileError::PayloadSizeMismatch { .. }))
        ),
        Defect::Deflate => matches!(result, Err(Error::UnwritableCoveragePayload { .. })),
        Defect::Truncated | Defect::NotAnImage => matches!(
            result,
            Err(Error::Tile(TileError::UnreadablePayload { .. }))
        ),
        Defect::ScaledFloat => matches!(result, Err(Error::FloatCoverageScaled { .. })),
        Defect::NanAncillary(field) => matches!(
            result,
            Err(Error::NanAncillaryValue { column, .. })
                if *column == format!("gpkg_2d_gridded_tile_ancillary.{field}")
        ),
    }
}

/// What a session step reports to the property that runs it.
enum Step<'a> {
    Put {
        coord: TileCoord,
        ancillary: &'a TileAncillary,
        defect: Defect,
        result: &'a geopackage::Result<()>,
    },
    Delete {
        coord: TileCoord,
        result: &'a geopackage::Result<bool>,
    },
    /// The writer committed.
    Committed,
    /// The writer was dropped without a commit.
    RolledBack,
}

/// Runs between one and four drawn writer sessions against `coverage`, and
/// calls `check` after each operation and at the end of each session.
fn run_write_sessions(
    tc: &hegel::TestCase,
    grid: Grid,
    coverage: &Coverage<'_>,
    mut check: impl FnMut(Step<'_>),
) {
    let sessions = tc.draw(generators::integers::<usize>().min_value(1).max_value(4));
    for _ in 0..sessions {
        let mut writer = coverage.writer().unwrap();
        let ops = tc.draw(generators::integers::<usize>().min_value(0).max_value(8));
        for _ in 0..ops {
            let op = draw_op(tc, grid);
            match &op {
                Op::Put {
                    coord,
                    data,
                    ancillary,
                    defect,
                } => {
                    let result = writer.put_with_ancillary(*coord, data, ancillary);
                    check(Step::Put {
                        coord: *coord,
                        ancillary,
                        defect: *defect,
                        result: &result,
                    });
                }
                Op::Delete { coord } => {
                    let result = writer.delete(*coord);
                    check(Step::Delete {
                        coord: *coord,
                        result: &result,
                    });
                }
            }
        }
        if tc.draw(generators::booleans()) {
            writer.commit().unwrap();
            check(Step::Committed);
        } else {
            drop(writer);
            check(Step::RolledBack);
        }
    }
}

/// For any sequence of writes, deletes, commits and rollbacks, the two tables
/// agree: each tile has exactly one ancillary row, and no row points to a tile
/// that does not exist (Requirements 10 and 12).
///
/// The table definition has no `ON DELETE CASCADE`, so the writer must keep
/// this invariant. The test checks it after each step, not only at the end, so
/// a failing sequence shrinks to the step that broke it.
#[hegel::test]
fn the_two_tables_stay_in_step_through_write_ops(tc: hegel::TestCase) {
    let grid = draw_grid(&tc);
    let gpkg = in_memory_coverage(grid);
    let coverage = gpkg.coverage("dem").unwrap();
    run_write_sessions(&tc, grid, &coverage, |_| {
        let (tiles, rows) = pairing(&gpkg, "dem");
        assert_eq!(rows, tiles, "the ancillary rows diverged from the tiles");
    });
}

/// An ancillary row compared by the bits of its values, so that the
/// comparison is exact.
type AncillaryBits = (u64, u64, Option<u64>, Option<u64>, Option<u64>, Option<u64>);

/// Returns the bits of each value, with a negative zero as zero: SQLite stores
/// it as zero, as the `TileAncillary` docs state.
fn ancillary_bits(ancillary: &TileAncillary) -> AncillaryBits {
    let bits = |value: f64| if value == 0.0 { 0 } else { value.to_bits() };
    (
        bits(ancillary.scale),
        bits(ancillary.offset),
        ancillary.min.map(bits),
        ancillary.max.map(bits),
        ancillary.mean.map(bits),
        ancillary.std_dev.map(bits),
    )
}

/// The tiles in the coverage, each with its ancillary row read back through
/// [`Coverage::tile_ancillary`].
fn stored(coverage: &Coverage<'_>) -> BTreeMap<(i64, i64, i64), Option<AncillaryBits>> {
    let addresses: Vec<(i64, i64, i64)> = coverage
        .gpkg()
        .connection()
        .prepare("SELECT zoom_level, tile_column, tile_row FROM dem")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    addresses
        .into_iter()
        .map(|(zoom_level, column, row)| {
            let ancillary = coverage
                .tile_ancillary(TileCoord::new(zoom_level, column, row))
                .unwrap();
            (
                (zoom_level, column, row),
                ancillary.as_ref().map(ancillary_bits),
            )
        })
        .collect()
}

/// Every put has the outcome its defect documents, every delete reports
/// whether a tile was there, and after each session the coverage holds exactly
/// the tiles and ancillary rows of the committed sessions.
///
/// The oracle is a map of the puts that should succeed, kept apart from the
/// coverage: a session's changes enter it on commit and are discarded on
/// rollback.
#[hegel::test]
fn writer_sessions_match_a_model(tc: hegel::TestCase) {
    let grid = draw_grid(&tc);
    let gpkg = in_memory_coverage(grid);
    let coverage = gpkg.coverage("dem").unwrap();

    let mut committed: BTreeMap<(i64, i64, i64), Option<AncillaryBits>> = BTreeMap::new();
    let mut pending = committed.clone();
    run_write_sessions(&tc, grid, &coverage, |step| match step {
        Step::Put {
            coord,
            ancillary,
            defect,
            result,
        } => {
            assert!(
                put_outcome_is_documented(grid, defect, result),
                "a put with defect {defect:?} returned {result:?}"
            );
            if defect == Defect::None {
                pending.insert(
                    (coord.zoom_level, coord.column, coord.row),
                    Some(ancillary_bits(ancillary)),
                );
            }
        }
        Step::Delete { coord, result } => {
            let existed = pending
                .remove(&(coord.zoom_level, coord.column, coord.row))
                .is_some();
            assert_eq!(result.as_ref().ok(), Some(&existed), "delete at {coord:?}");
        }
        Step::Committed => {
            committed = pending.clone();
            assert_eq!(stored(&coverage), committed, "after a commit");
        }
        Step::RolledBack => {
            pending = committed.clone();
            assert_eq!(stored(&coverage), committed, "after a rollback");
        }
    });
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

// --- creation -----------------------------------------------------------------

/// Draws an optional `f64` from the whole domain, NaN included.
fn draw_any_optional_f64(tc: &hegel::TestCase) -> Option<f64> {
    tc.draw(generators::optional(generators::floats::<f64>()))
}

/// Creating a coverage rejects a `float` coverage whose scale or offset is
/// not the default (Requirement 11) and an ancillary row with a NaN, and
/// otherwise creates a coverage whose ancillary row reads back as written.
///
/// A `float` coverage with a NaN scale or offset breaks both rules, and the
/// docs do not order the two checks, so either error is accepted there.
#[hegel::test]
fn coverage_creation_checks_its_ancillary_row(tc: hegel::TestCase) {
    let gpkg = GeoPackage::create(std::path::Path::new(":memory:")).unwrap();
    let datatype = tc.draw(generators::sampled_from(vec![
        CoverageDatatype::Integer,
        CoverageDatatype::Float,
    ]));
    let (scale, offset) = if tc.draw(generators::booleans()) {
        (1.0, 0.0)
    } else {
        let any = || tc.draw(generators::floats::<f64>());
        (any(), any())
    };
    let (precision, data_null) = (draw_any_optional_f64(&tc), draw_any_optional_f64(&tc));
    let set = TileMatrixSet::new(0, 0.0, 0.0, 1.0, 1.0);
    let mut builder = CoverageBuilder::new("dem", set, datatype)
        .matrix(TileMatrix::new(0, 1, 1, 1, 1, 1.0, 1.0))
        .scale(scale, offset);
    if let Some(precision) = precision {
        builder = builder.precision(precision);
    }
    if let Some(data_null) = data_null {
        builder = builder.data_null(data_null);
    }
    let result = gpkg.create_coverage(&builder);

    let scaled_float = datatype == CoverageDatatype::Float && (scale != 1.0 || offset != 0.0);
    let nan = [
        ("gpkg_2d_gridded_coverage_ancillary.scale", Some(scale)),
        ("gpkg_2d_gridded_coverage_ancillary.offset", Some(offset)),
        ("gpkg_2d_gridded_coverage_ancillary.precision", precision),
        ("gpkg_2d_gridded_coverage_ancillary.data_null", data_null),
    ]
    .into_iter()
    .find(|(_, value)| value.is_some_and(f64::is_nan))
    .map(|(column, _)| column);
    let float_error = matches!(result, Err(Error::FloatCoverageScaled { .. }));
    let nan_error = matches!(
        &result,
        Err(Error::NanAncillaryValue { column, .. }) if Some(*column) == nan
    );
    match (scaled_float, nan) {
        (true, Some(_)) => assert!(float_error || nan_error, "{:?}", result.err()),
        (true, None) => assert!(float_error, "{:?}", result.err()),
        (false, Some(_)) => assert!(nan_error, "{:?}", result.err()),
        (false, None) => {
            drop(result.unwrap());
            let coverage = gpkg.coverage("dem").unwrap();
            let ancillary = coverage.ancillary();
            assert_eq!(
                (
                    ancillary.scale,
                    ancillary.offset,
                    ancillary.precision,
                    ancillary.data_null
                ),
                (scale, offset, precision, data_null)
            );
        }
    }
}
