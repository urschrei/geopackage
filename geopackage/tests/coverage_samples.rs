//! Decoded coverage samples (the `coverage-samples` feature): decoding, value
//! lookups, per-tile statistics, and Requirement 21.
//!
//! The two GDAL fixtures come from one source grid, where the sample in
//! column `x` and row `y` (from the top) is `(3x + 5y) mod 97 + 0.5`. GDAL
//! stores it as float32 TIFF in `gdal_coverage.gpkg`, and quantises it to
//! 16-bit PNG with a per-tile scale and offset in `gdal_coverage_png.gpkg`.
//! Both files record GDAL's statistics of the grid, so the statistics here are
//! checked against another implementation's.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::io::Cursor;

use geopackage::core::TileError;
use geopackage::core::coverage::CoverageDatatype;
use geopackage::core::tiles::{TileCoord, TileMatrix, TileMatrixSet};
use geopackage::{
    Coverage, CoverageBuilder, Error, Finding, GeoPackage, Interpolation, OpenOptions,
    TileAncillary,
};
use hegel::generators;

fn fixture(name: &str) -> GeoPackage {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    GeoPackage::open_read_only(path).unwrap()
}

/// The source grid of both GDAL fixtures.
fn source(column: usize, row: usize) -> f64 {
    ((column * 3 + row * 5) % 97) as f64 + 0.5
}

/// How far GDAL's quantisation moves a value of the PNG fixture: one step of
/// the tile's scale. Not half a step, because the largest value, 96.5, falls
/// on stored 65535, which is the coverage's `data_null`, so GDAL stores 65534.
/// The slack is for the arithmetic that carries the value back.
fn one_step(tile: &TileAncillary) -> f64 {
    tile.scale * (1.0 + 1e-9)
}

/// The centre of a pixel of the fixtures' 64-pixel, 100-unit grid.
fn centre(column: usize, row: usize) -> (f64, f64) {
    (
        (column as f64 + 0.5) * 100.0,
        6400.0 - (row as f64 + 0.5) * 100.0,
    )
}

/// A float32 LZW TIFF, from `tiff`'s encoder.
fn float_tiff(width: u32, height: u32, samples: &[f32]) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    tiff::encoder::TiffEncoder::new(&mut bytes)
        .unwrap()
        .with_compression(tiff::encoder::Compression::Lzw)
        .write_image::<tiff::encoder::colortype::Gray32Float>(width, height, samples)
        .unwrap();
    bytes.into_inner()
}

/// A signed 16-bit uncompressed TIFF.
fn i16_tiff(width: u32, height: u32, samples: &[i16]) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    tiff::encoder::TiffEncoder::new(&mut bytes)
        .unwrap()
        .write_image::<tiff::encoder::colortype::GrayI16>(width, height, samples)
        .unwrap();
    bytes.into_inner()
}

/// A 16-bit greyscale PNG, from `png`'s encoder.
fn png16(width: u32, height: u32, samples: &[u16]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, width, height);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Sixteen);
    let mut writer = encoder.write_header().unwrap();
    let data: Vec<u8> = samples.iter().flat_map(|s| s.to_be_bytes()).collect();
    writer.write_image_data(&data).unwrap();
    writer.finish().unwrap();
    bytes
}

/// A coverage in a new in-memory file: `columns` by `rows` tiles of `side`
/// pixels, one unit per pixel, with the top-left corner at (0, `rows *
/// side`).
fn new_coverage(
    datatype: CoverageDatatype,
    columns: i64,
    rows: i64,
    side: i64,
    configure: impl FnOnce(CoverageBuilder) -> CoverageBuilder,
) -> GeoPackage {
    new_coverage_with(OpenOptions::new(), datatype, columns, rows, side, configure)
}

/// [`new_coverage`], in a file opened with `options`.
fn new_coverage_with(
    options: OpenOptions,
    datatype: CoverageDatatype,
    columns: i64,
    rows: i64,
    side: i64,
    configure: impl FnOnce(CoverageBuilder) -> CoverageBuilder,
) -> GeoPackage {
    let gpkg = options.create(std::path::Path::new(":memory:")).unwrap();
    gpkg.add_epsg_srs(3857).unwrap();
    let set = TileMatrixSet::new(
        3857,
        0.0,
        0.0,
        (columns * side) as f64,
        (rows * side) as f64,
    );
    gpkg.create_coverage(&configure(
        CoverageBuilder::new("dem", set, datatype)
            .matrix(TileMatrix::new(0, columns, rows, side, side, 1.0, 1.0)),
    ))
    .unwrap();
    gpkg
}

fn put(coverage: &Coverage<'_>, column: i64, row: i64, payload: &[u8]) {
    coverage
        .put_tile(TileCoord::new(0, column, row), payload)
        .unwrap();
}

fn value_at(coverage: &Coverage<'_>, x: f64, y: f64, interpolation: Interpolation) -> Option<f64> {
    coverage.value_at(0, x, y, interpolation).unwrap()
}

// Decoding.

#[test]
fn a_gdal_float_tile_decodes_to_its_source_grid() {
    let gpkg = fixture("gdal_coverage.gpkg");
    let coverage = gpkg.coverage("coverage").unwrap();
    let samples = coverage.samples(TileCoord::new(0, 0, 0)).unwrap().unwrap();

    assert_eq!((samples.width(), samples.height()), (64, 64));
    for row in 0..64 {
        for column in 0..64 {
            assert_eq!(
                samples.get(column, row),
                Some(source(column, row)),
                "{column}, {row}"
            );
        }
    }
    assert_eq!(samples.get(64, 0), None);
    assert_eq!(samples.get(0, 64), None);
    assert_eq!(samples.as_slice().len(), 64 * 64);
}

#[test]
fn a_gdal_png_tile_decodes_to_its_quantised_source_grid() {
    let gpkg = fixture("gdal_coverage_png.gpkg");
    let coverage = gpkg.coverage("elevation").unwrap();
    let coord = TileCoord::new(0, 0, 0);
    let samples = coverage.samples(coord).unwrap().unwrap();
    let tile = coverage.tile_ancillary(coord).unwrap().unwrap();

    for row in 0..64 {
        for column in 0..64 {
            let value = coverage.value(&tile, samples.get(column, row).unwrap());
            assert!(
                (value - source(column, row)).abs() <= one_step(&tile),
                "{column}, {row}: {value}"
            );
        }
    }
}

#[test]
fn every_sample_type_the_profile_permits_decodes() {
    let gpkg = new_coverage(CoverageDatatype::Integer, 1, 1, 2, |b| b);
    let coverage = gpkg.coverage("dem").unwrap();

    let signed = coverage
        .decode(&i16_tiff(2, 2, &[-32768, -1, 0, 32767]))
        .unwrap();
    assert_eq!(signed.into_vec(), vec![-32768.0, -1.0, 0.0, 32767.0]);

    let png = coverage.decode(&png16(2, 2, &[0, 1, 256, 65535])).unwrap();
    assert_eq!(png.into_vec(), vec![0.0, 1.0, 256.0, 65535.0]);
}

#[test]
fn a_payload_the_coverage_does_not_permit_is_not_decoded() {
    let gpkg = fixture("gdal_coverage.gpkg");
    let coverage = gpkg.coverage("coverage").unwrap();
    // A float coverage may not hold a PNG (Requirement 14).
    let error = coverage.decode(&png16(2, 2, &[0; 4])).unwrap_err();
    assert!(
        matches!(
            error,
            Error::Tile(TileError::CoverageProfileViolation {
                requirement: 14,
                ..
            })
        ),
        "{error:?}"
    );
}

#[test]
fn data_that_does_not_decode_is_unreadable() {
    let gpkg = fixture("gdal_coverage.gpkg");
    let coverage = gpkg.coverage("coverage").unwrap();
    // GDAL writes the IFD first and the strip after it, at byte 158, so a cut
    // inside the strip keeps the header and loses the data.
    let mut payload = coverage.get_tile(TileCoord::new(0, 0, 0)).unwrap().unwrap();
    payload.truncate(400);
    coverage.check_payload(&payload).unwrap();
    let error = coverage.decode(&payload).unwrap_err();
    assert!(
        matches!(error, Error::Tile(TileError::UnreadablePayload { .. })),
        "{error:?}"
    );
}

// Statistics.

#[test]
fn statistics_match_what_gdal_recorded() {
    for (file, table, quantised) in [
        ("gdal_coverage.gpkg", "coverage", false),
        ("gdal_coverage_png.gpkg", "elevation", true),
    ] {
        let gpkg = fixture(file);
        let coverage = gpkg.coverage(table).unwrap();
        let coord = TileCoord::new(0, 0, 0);
        let tile = coverage.tile_ancillary(coord).unwrap().unwrap();
        // GDAL records the statistics of its float source, and the PNG holds
        // that source quantised.
        let tolerance = if quantised { one_step(&tile) } else { 1e-9 };
        let samples = coverage.samples(coord).unwrap().unwrap();
        let statistics = coverage.statistics(&samples, &tile).unwrap();

        assert_eq!(statistics.count, 64 * 64, "{file}");
        for (name, ours, gdal) in [
            ("min", statistics.min, tile.min.unwrap()),
            ("max", statistics.max, tile.max.unwrap()),
            ("mean", statistics.mean, tile.mean.unwrap()),
            ("std_dev", statistics.std_dev, tile.std_dev.unwrap()),
        ] {
            assert!(
                (ours - gdal).abs() <= tolerance,
                "{file} {name}: {ours} against GDAL's {gdal}"
            );
        }
    }
}

#[test]
fn statistics_are_of_values_and_leave_out_data_null() {
    let gpkg = new_coverage(CoverageDatatype::Integer, 1, 1, 2, |b| {
        b.scale(1.0, 100.0).data_null(65535.0)
    });
    let coverage = gpkg.coverage("dem").unwrap();
    let samples = coverage.decode(&png16(2, 2, &[0, 1, 3, 65535])).unwrap();
    // Stored 0, 1, 3 become 10, 12, 16 by the tile's pair, and 110, 112, 116
    // by the coverage's.
    let statistics = coverage
        .statistics(&samples, &TileAncillary::new(2.0, 10.0))
        .unwrap();
    assert_eq!(statistics.count, 3);
    assert_eq!((statistics.min, statistics.max), (110.0, 116.0));
    assert_eq!(statistics.mean, 338.0 / 3.0);
    let variance = [110.0, 112.0, 116.0_f64]
        .iter()
        .map(|v| (v - 338.0 / 3.0).powi(2))
        .sum::<f64>()
        / 3.0;
    assert_eq!(statistics.std_dev, variance.sqrt());

    let all_null = coverage.decode(&png16(2, 2, &[65535; 4])).unwrap();
    assert_eq!(
        coverage.statistics(&all_null, &TileAncillary::defaults()),
        None
    );
}

#[test]
fn a_writer_records_statistics_it_calculates() {
    let gpkg = new_coverage(CoverageDatatype::Float, 1, 1, 2, |b| b.data_null(-9999.0));
    let coverage = gpkg.coverage("dem").unwrap();
    let mut writer = coverage.writer().unwrap();
    // Statistics in the row passed in are replaced.
    let stale = TileAncillary::defaults().with_statistics(0.0, 0.0, 0.0, 0.0);
    writer
        .put_with_statistics(
            TileCoord::new(0, 0, 0),
            &float_tiff(2, 2, &[1.0, 2.0, 3.0, -9999.0]),
            &stale,
        )
        .unwrap();
    writer.commit().unwrap();

    let tile = coverage
        .tile_ancillary(TileCoord::new(0, 0, 0))
        .unwrap()
        .unwrap();
    assert_eq!(
        (tile.min, tile.max, tile.mean),
        (Some(1.0), Some(3.0), Some(2.0))
    );
    assert_eq!(tile.std_dev, Some((2.0_f64 / 3.0).sqrt()));
}

// Requirement 21.

#[test]
fn requirement_21_is_checked_where_samples_are_decoded() {
    let gpkg = new_coverage(CoverageDatatype::Float, 1, 1, 2, |b| b);
    let coverage = gpkg.coverage("dem").unwrap();
    let nan = float_tiff(2, 2, &[1.0, 2.0, 3.0, f32::NAN]);
    let infinite = float_tiff(2, 2, &[f32::INFINITY, 2.0, 3.0, 4.0]);

    for (payload, at) in [(&nan, "column 1, row 1"), (&infinite, "column 0, row 0")] {
        let error = coverage
            .check_samples(&coverage.decode(payload).unwrap())
            .unwrap_err();
        let Error::Tile(TileError::CoverageProfileViolation {
            requirement: 21,
            detail,
        }) = &error
        else {
            panic!("{error:?}");
        };
        assert!(detail.contains(at), "{detail}");

        let mut writer = coverage.writer().unwrap();
        let error = writer
            .put_with_statistics(TileCoord::new(0, 0, 0), payload, &TileAncillary::defaults())
            .unwrap_err();
        assert!(
            matches!(
                error,
                Error::Tile(TileError::CoverageProfileViolation {
                    requirement: 21,
                    ..
                })
            ),
            "{error:?}"
        );
    }
    coverage
        .check_samples(
            &coverage
                .decode(&float_tiff(2, 2, &[1.0, 2.0, 3.0, 4.0]))
                .unwrap(),
        )
        .unwrap();
}

#[test]
fn validate_reports_a_tile_that_breaks_requirement_21() {
    let gpkg = new_coverage(CoverageDatatype::Float, 2, 1, 2, |b| b);
    let coverage = gpkg.coverage("dem").unwrap();
    // `put` does not decode, so it writes what `put_with_statistics` refuses.
    put(
        &coverage,
        0,
        0,
        &float_tiff(2, 2, &[1.0, f32::NAN, 3.0, 4.0]),
    );
    put(&coverage, 1, 0, &float_tiff(2, 2, &[1.0, 2.0, 3.0, 4.0]));

    let findings = gpkg.validate().unwrap();
    let [
        Finding::CoveragePayloadNonConformant {
            table_name,
            requirement: 21,
            tiles: 1,
            ..
        },
    ] = findings.as_slice()
    else {
        panic!("{findings:#?}");
    };
    assert_eq!(table_name, "dem");
}

#[test]
fn validate_finds_nothing_in_the_samples_of_the_gdal_fixtures() {
    for file in ["gdal_coverage.gpkg", "gdal_coverage_png.gpkg"] {
        assert_eq!(fixture(file).validate().unwrap(), Vec::new(), "{file}");
    }
}

// Value lookups.

#[test]
fn nearest_returns_the_sample_of_the_cell_a_position_is_in() {
    let gpkg = fixture("gdal_coverage.gpkg");
    let coverage = gpkg.coverage("coverage").unwrap();
    let mut sampler = coverage.sampler(0).unwrap();
    for row in 0..64 {
        for column in 0..64 {
            let (x, y) = centre(column, row);
            // Anywhere in the cell, not only at its centre.
            for (dx, dy) in [(0.0, 0.0), (-49.0, 49.0), (49.0, -49.0)] {
                assert_eq!(
                    sampler
                        .value_at(x + dx, y + dy, Interpolation::Nearest)
                        .unwrap(),
                    Some(source(column, row)),
                    "{column}, {row}"
                );
            }
        }
    }
    // The extent is inclusive at all four edges.
    assert_eq!(
        value_at(&coverage, 0.0, 6400.0, Interpolation::Nearest),
        Some(source(0, 0))
    );
    assert_eq!(
        value_at(&coverage, 6400.0, 0.0, Interpolation::Nearest),
        Some(source(63, 63))
    );
}

#[test]
fn there_is_no_value_outside_the_extent() {
    let gpkg = fixture("gdal_coverage.gpkg");
    let coverage = gpkg.coverage("coverage").unwrap();
    for (x, y) in [
        (-0.001, 100.0),
        (6400.001, 100.0),
        (100.0, -0.001),
        (100.0, 6400.001),
        (f64::NAN, 100.0),
        (100.0, f64::INFINITY),
    ] {
        for interpolation in [Interpolation::Nearest, Interpolation::Bilinear] {
            assert_eq!(value_at(&coverage, x, y, interpolation), None, "{x}, {y}");
        }
    }
}

#[test]
fn bilinear_interpolates_between_sample_centres() {
    let gpkg = fixture("gdal_coverage.gpkg");
    let coverage = gpkg.coverage("coverage").unwrap();
    let mut sampler = coverage.sampler(0).unwrap();
    let mut bilinear = |x, y| sampler.value_at(x, y, Interpolation::Bilinear).unwrap();

    // On a centre, the sample itself.
    let (x, y) = centre(10, 20);
    assert_eq!(bilinear(x, y), Some(source(10, 20)));
    // Halfway to the next centre across, and down.
    assert_eq!(
        bilinear(x + 50.0, y),
        Some((source(10, 20) + source(11, 20)) / 2.0)
    );
    assert_eq!(
        bilinear(x, y - 50.0),
        Some((source(10, 20) + source(10, 21)) / 2.0)
    );
    // In the middle of four centres, their mean.
    let four = source(10, 20) + source(11, 20) + source(10, 21) + source(11, 21);
    assert_eq!(bilinear(x + 50.0, y - 50.0), Some(four / 4.0));
    // A quarter of the way across, three quarters of the near sample.
    assert_eq!(
        bilinear(x + 25.0, y),
        Some(0.75 * source(10, 20) + 0.25 * source(11, 20))
    );
    // Beyond the outermost centres there is nothing to interpolate towards.
    assert_eq!(bilinear(1.0, 6399.0), Some(source(0, 0)));
    assert_eq!(bilinear(6399.0, 1.0), Some(source(63, 63)));
}

#[test]
fn bilinear_on_the_png_fixture_carries_each_sample_s_scale() {
    let gpkg = fixture("gdal_coverage_png.gpkg");
    let coverage = gpkg.coverage("elevation").unwrap();
    let tile = coverage
        .tile_ancillary(TileCoord::new(0, 0, 0))
        .unwrap()
        .unwrap();
    let (x, y) = centre(30, 40);
    let expected = (source(30, 40) + source(31, 40)) / 2.0;
    let value = value_at(&coverage, x + 50.0, y, Interpolation::Bilinear).unwrap();
    assert!((value - expected).abs() <= one_step(&tile), "{value}");
}

#[test]
fn lookups_cross_tile_edges_and_stop_at_missing_tiles() {
    // Three tiles of 2 by 2 in a row, the last one missing. Each tile's
    // samples are its column number plus 0, 1, 2, 3.
    let gpkg = new_coverage(CoverageDatatype::Float, 3, 1, 2, |b| b);
    let coverage = gpkg.coverage("dem").unwrap();
    put(&coverage, 0, 0, &float_tiff(2, 2, &[0.0, 1.0, 2.0, 3.0]));
    put(
        &coverage,
        1,
        0,
        &float_tiff(2, 2, &[10.0, 11.0, 12.0, 13.0]),
    );

    // Between the last column of tile 0 (1.0) and the first of tile 1 (10.0).
    assert_eq!(
        value_at(&coverage, 2.0, 1.5, Interpolation::Bilinear),
        Some(5.5)
    );
    // In the missing tile, and next to it.
    assert_eq!(value_at(&coverage, 5.0, 1.5, Interpolation::Nearest), None);
    assert_eq!(value_at(&coverage, 4.0, 1.5, Interpolation::Bilinear), None);
    // On the last centre before it, the missing neighbour has no weight.
    assert_eq!(
        value_at(&coverage, 3.5, 1.5, Interpolation::Bilinear),
        Some(11.0)
    );
}

#[test]
fn a_null_sample_has_no_value_and_poisons_an_interpolation() {
    let gpkg = new_coverage(CoverageDatatype::Float, 1, 1, 2, |b| b.data_null(-9999.0));
    let coverage = gpkg.coverage("dem").unwrap();
    put(
        &coverage,
        0,
        0,
        &float_tiff(2, 2, &[1.0, -9999.0, 3.0, 4.0]),
    );

    assert_eq!(value_at(&coverage, 1.5, 1.5, Interpolation::Nearest), None);
    assert_eq!(value_at(&coverage, 1.0, 1.5, Interpolation::Bilinear), None);
    assert_eq!(
        value_at(&coverage, 0.5, 0.5, Interpolation::Bilinear),
        Some(3.0)
    );
}

#[test]
fn an_integer_lookup_applies_both_scale_and_offset_pairs() {
    let gpkg = new_coverage(CoverageDatatype::Integer, 1, 1, 2, |b| b.scale(10.0, 5.0));
    let coverage = gpkg.coverage("dem").unwrap();
    let mut writer = coverage.writer().unwrap();
    writer
        .put_with_ancillary(
            TileCoord::new(0, 0, 0),
            &png16(2, 2, &[1, 2, 3, 4]),
            &TileAncillary::new(0.5, 1.0),
        )
        .unwrap();
    writer.commit().unwrap();
    // Stored 1: (1 * 0.5 + 1) * 10 + 5.
    assert_eq!(
        value_at(&coverage, 0.5, 1.5, Interpolation::Nearest),
        Some(20.0)
    );
}

#[test]
fn the_cell_encoding_places_the_samples() {
    // One tile of 2 by 2 over the square (0, 0) to (2, 2).
    let samples = [1.0, 2.0, 3.0, 4.0];
    let open = |encoding: &str| {
        let gpkg = new_coverage(CoverageDatatype::Float, 1, 1, 2, |b| {
            b.grid_cell_encoding(encoding)
        });
        put(
            &gpkg.coverage("dem").unwrap(),
            0,
            0,
            &float_tiff(2, 2, &samples),
        );
        gpkg
    };

    // Corner: each sample sits on the lower-left corner of its cell, so the
    // four are at (0, 1), (1, 1), (0, 0) and (1, 0).
    let gpkg = open("grid-value-is-corner");
    let coverage = gpkg.coverage("dem").unwrap();
    assert_eq!(
        value_at(&coverage, 0.0, 1.0, Interpolation::Bilinear),
        Some(1.0)
    );
    assert_eq!(
        value_at(&coverage, 1.0, 0.0, Interpolation::Bilinear),
        Some(4.0)
    );
    assert_eq!(
        value_at(&coverage, 0.5, 0.5, Interpolation::Bilinear),
        Some(2.5)
    );
    assert_eq!(
        value_at(&coverage, 0.9, 0.1, Interpolation::Nearest),
        Some(4.0)
    );

    // Area: one value across the whole cell, so nothing to interpolate.
    let gpkg = open("grid-value-is-area");
    let coverage = gpkg.coverage("dem").unwrap();
    assert_eq!(
        value_at(&coverage, 0.9, 1.9, Interpolation::Bilinear),
        Some(1.0)
    );
    assert_eq!(
        value_at(&coverage, 1.1, 1.9, Interpolation::Bilinear),
        Some(2.0)
    );

    // Centre, and a value the extension does not define, read as centre.
    for encoding in ["grid-value-is-center", "grid-value-is-elsewhere"] {
        let gpkg = open(encoding);
        let coverage = gpkg.coverage("dem").unwrap();
        assert_eq!(
            value_at(&coverage, 1.0, 1.0, Interpolation::Bilinear),
            Some(2.5),
            "{encoding}"
        );
    }
}

#[test]
fn a_lookup_refuses_a_tile_of_the_wrong_size() {
    let gpkg = new_coverage(CoverageDatatype::Float, 1, 1, 4, |b| b);
    // The writer checks the size, so write past it.
    gpkg.connection()
        .execute(
            "INSERT INTO dem (zoom_level, tile_column, tile_row, tile_data) VALUES (0, 0, 0, ?1)",
            [float_tiff(2, 2, &[1.0; 4])],
        )
        .unwrap();
    let coverage = gpkg.coverage("dem").unwrap();
    let error = coverage
        .value_at(0, 1.0, 1.0, Interpolation::Nearest)
        .unwrap_err();
    assert!(
        matches!(error, Error::Tile(TileError::PayloadSizeMismatch { .. })),
        "{error:?}"
    );
    // A read of the tile itself does not reject it.
    assert_eq!(
        coverage
            .samples(TileCoord::new(0, 0, 0))
            .unwrap()
            .unwrap()
            .width(),
        2
    );
}

/// A PNG whose header declares far more samples than its data holds: the
/// 192 by 4,294,705,216 the fuzzer found, which a decoder that trusted the
/// header would allocate about 1.6 TB for.
fn lying_png() -> Vec<u8> {
    let mut bytes = png16(2, 2, &[0; 4]);
    // The width and height of the IHDR chunk, after the signature and the
    // chunk's length and type.
    let size = [192_u32, 4_294_705_216].map(u32::to_be_bytes);
    bytes.splice(16..24, size.into_iter().flatten());
    bytes
}

#[test]
fn a_tile_larger_than_its_zoom_level_is_refused_before_it_is_decoded() {
    let gpkg = new_coverage(CoverageDatatype::Integer, 1, 1, 4, |b| b);
    gpkg.connection()
        .execute(
            "INSERT INTO dem (zoom_level, tile_column, tile_row, tile_data) VALUES (0, 0, 0, ?1)",
            [lying_png()],
        )
        .unwrap();
    let coverage = gpkg.coverage("dem").unwrap();
    for error in [
        coverage.samples(TileCoord::new(0, 0, 0)).unwrap_err(),
        coverage
            .value_at(0, 1.0, 1.0, Interpolation::Nearest)
            .unwrap_err(),
        coverage
            .writer()
            .unwrap()
            .put_with_statistics(
                TileCoord::new(0, 0, 0),
                &lying_png(),
                &TileAncillary::defaults(),
            )
            .unwrap_err(),
    ] {
        assert!(
            matches!(error, Error::Tile(TileError::PayloadSizeMismatch { .. })),
            "{error:?}"
        );
    }
    // Without a zoom level to compare with, the limit refuses it.
    assert!(matches!(
        coverage.decode(&lying_png()).unwrap_err(),
        Error::Tile(TileError::PayloadTooLarge { .. })
    ));
    // `validate` does not decode it, so it reports nothing about its samples.
    let findings = gpkg.validate().unwrap();
    assert!(
        !findings
            .iter()
            .any(|finding| matches!(finding, Finding::CoveragePayloadNonConformant { .. })),
        "{findings:#?}"
    );
}

#[test]
fn the_decode_limit_is_an_open_option() {
    let tile = float_tiff(4, 4, &[f32::NAN; 16]);
    let limited = new_coverage_with(
        OpenOptions::new().max_decoded_samples(15),
        CoverageDatatype::Float,
        1,
        1,
        4,
        |b| b,
    );
    let coverage = limited.coverage("dem").unwrap();
    put(&coverage, 0, 0, &tile);
    let too_large = |error: Error| {
        assert!(
            matches!(
                error,
                Error::Tile(TileError::PayloadTooLarge {
                    width: 4,
                    height: 4,
                    limit: 15
                })
            ),
            "{error:?}"
        );
    };
    too_large(coverage.samples(TileCoord::new(0, 0, 0)).unwrap_err());
    too_large(coverage.decode(&tile).unwrap_err());
    too_large(
        coverage
            .writer()
            .unwrap()
            .put_with_statistics(TileCoord::new(0, 0, 0), &tile, &TileAncillary::defaults())
            .unwrap_err(),
    );
    // The NaN samples are not decoded, so not reported.
    assert_eq!(limited.validate().unwrap(), Vec::new());

    let enough = new_coverage_with(
        OpenOptions::new().max_decoded_samples(16),
        CoverageDatatype::Float,
        1,
        1,
        4,
        |b| b,
    );
    let coverage = enough.coverage("dem").unwrap();
    put(&coverage, 0, 0, &tile);
    assert_eq!(
        coverage
            .samples(TileCoord::new(0, 0, 0))
            .unwrap()
            .unwrap()
            .width(),
        4
    );
    assert!(matches!(
        enough.validate().unwrap().as_slice(),
        [Finding::CoveragePayloadNonConformant {
            requirement: 21,
            ..
        }]
    ));
}

#[test]
fn a_lookup_names_an_undeclared_zoom_level() {
    let gpkg = fixture("gdal_coverage.gpkg");
    let coverage = gpkg.coverage("coverage").unwrap();
    let error = coverage
        .value_at(3, 100.0, 100.0, Interpolation::Nearest)
        .unwrap_err();
    assert!(
        matches!(error, Error::UnknownZoomLevel { zoom_level: 3, .. }),
        "{error:?}"
    );
}

/// For any grid of finite samples and any position in the extent, a bilinear
/// value lies between the smallest and largest sample, and on a sample's
/// centre it is that sample, which is also the nearest value there.
#[hegel::test]
fn bilinear_values_are_bounded_by_the_samples(tc: hegel::TestCase) {
    let side = tc.draw(generators::integers::<u32>().min_value(1).max_value(8));
    let count = (side * side) as usize;
    let samples: Vec<f32> = tc.draw(
        generators::vecs(
            generators::floats::<f32>()
                .min_value(-1e6)
                .max_value(1e6)
                .allow_nan(false)
                .allow_infinity(false),
        )
        .min_size(count)
        .max_size(count),
    );
    let gpkg = new_coverage(CoverageDatatype::Float, 1, 1, i64::from(side), |b| b);
    let coverage = gpkg.coverage("dem").unwrap();
    put(&coverage, 0, 0, &float_tiff(side, side, &samples));

    let extent = f64::from(side);
    let x = tc.draw(generators::floats::<f64>().min_value(0.0).max_value(extent));
    let y = tc.draw(generators::floats::<f64>().min_value(0.0).max_value(extent));
    let low = samples.iter().copied().fold(f32::INFINITY, f32::min);
    let high = samples.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let value = value_at(&coverage, x, y, Interpolation::Bilinear).unwrap();
    // A weighted mean in f64 of float32 samples: bounded up to rounding.
    let slack = 1e-9 * f64::from(low.abs().max(high.abs())).max(1.0);
    assert!(
        f64::from(low) - slack <= value && value <= f64::from(high) + slack,
        "{value} outside [{low}, {high}]"
    );

    let column = tc.draw(generators::integers::<u32>().max_value(side - 1));
    let row = tc.draw(generators::integers::<u32>().max_value(side - 1));
    let (cx, cy) = (f64::from(column) + 0.5, extent - f64::from(row) - 0.5);
    let sample = f64::from(samples[(row * side + column) as usize]);
    assert_eq!(
        value_at(&coverage, cx, cy, Interpolation::Bilinear),
        Some(sample)
    );
    assert_eq!(
        value_at(&coverage, cx, cy, Interpolation::Nearest),
        Some(sample)
    );
}
