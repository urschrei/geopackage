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

// --- the write path -----------------------------------------------------------

/// A payload this crate can write: a conforming coverage TIFF header of the
/// given size and sample type, built the way the core tests build one.
///
/// Header only, and that is the point: the profile is a statement about tags,
/// and no sample is ever decoded on either side of the write.
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

/// A float32 LZW payload of the given size: what a float coverage holds.
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

/// Every `(tpudt_id, tile id)` pairing in the file, as the two tables record
/// it: the tile ids present, and the ancillary rows pointing at them.
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

    // The catalogue rows the extension asks for (Requirements 1, 2, 5, 6, 7).
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
    // In the order `extensions()` reports them, which is the catalogue's own
    // rather than the order they were written.
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
    // No statistics: this crate decodes no samples, so it has none to record.
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
    // And the arithmetic reads back the way the extension defines it.
    assert_eq!(coverage.value(&ancillary, 4.0), 102.0);
}

#[test]
fn deleting_a_tile_deletes_its_ancillary_row() {
    // The normative table definition has no ON DELETE CASCADE, so the writer
    // is what keeps the two tables in step.
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

    // Requirement 14: a float coverage takes float32 TIFF.
    assert!(matches!(
        writer.put(TileCoord::new(0, 0, 0), &tiff(256, 256, 1, 16, 5)),
        Err(Error::Tile(TileError::CoverageProfileViolation {
            requirement: 14,
            ..
        }))
    ));
    // Requirement 16: one sample per grid cell, checked before anything else
    // about the payload.
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
    // And nothing was written by any of that.
    drop(writer);
    assert_eq!(pairing(&gpkg, &name), (Vec::new(), Vec::new()));
}

#[test]
fn deflate_is_read_but_not_written() {
    // C2: Requirement 18 does not say "only LZW", so a Deflate payload reads;
    // Requirement 15 asks for baseline TIFF, so it does not write.
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

/// Whatever sequence of writes and deletes a caller makes, the two tables
/// agree: every tile has exactly one ancillary row and no row points at a tile
/// that is not there (Requirements 10 and 12).
///
/// The invariant the missing `ON DELETE CASCADE` puts on the writer, checked
/// after every step rather than at the end, so a sequence that breaks it
/// shrinks to the step that did.
#[hegel::test]
fn the_two_tables_stay_in_step_through_write_ops(tc: hegel::TestCase) {
    let (_dir, gpkg, name) = new_coverage(CoverageDatatype::Float);
    let coverage = gpkg.coverage(&name).unwrap();
    let payload = float_tile(256);

    let ops = tc.draw(generators::integers::<usize>().min_value(0).max_value(12));
    for _ in 0..ops {
        // One zoom level of one tile would make every op collide, so the grid
        // is addressed through a 2x2 space the matrix does not have: writes
        // outside it are refused, which is itself a case worth covering.
        let column = tc.draw(generators::integers::<i64>().min_value(0).max_value(1));
        let row = tc.draw(generators::integers::<i64>().min_value(0).max_value(1));
        let coord = TileCoord::new(0, column, row);
        // Either outcome is legitimate: the grid this coverage declares is
        // one tile, so three of the four addresses are outside it and are
        // refused. A refused write leaving nothing behind is part of what the
        // invariant below checks.
        if tc.draw(generators::booleans()) {
            let _outcome = coverage.put_tile(coord, &payload);
        } else {
            let _outcome = coverage.delete_tile(coord);
        }
        let (tiles, rows) = pairing(&gpkg, &name);
        assert_eq!(rows, tiles, "the ancillary rows diverged from the tiles");
    }
}
