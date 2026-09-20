//! The Abstract Test Suite of the tiled gridded coverage extension (OGC
//! 17-066r2 Annex A), TEST001 to TEST012.
//!
//! There is no executable ETS for this extension: `ets-gpkg12` validates the
//! 1.2 core and tiles, and skips the rest. This file implements the abstract
//! tests instead. Each test follows the method that the standard states, as SQL
//! against the file, not through the API of this crate. A test through the API
//! would only check that the API agrees with itself. These tests check the
//! *file*.
//!
//! Every test runs twice, over
//!
//! - `gdal_coverage.gpkg`, the committed fixture that GDAL wrote. This is the
//!   control: a file from another implementation, and the counterpart of
//!   [`geopackage_writes_a_conformant_coverage`].
//! - a coverage that this crate writes, which is the main subject of the suite.
//!
//! Two of the twelve tests are `Test Type: Capability` with a manual step
//! (TEST005 tells a person to confirm that all coverages are in the result).
//! The TIFF half of TEST012 is the payload profile that
//! [`geopackage_core::coverage`] implements. These tests run the automatic
//! steps, and each test states the steps that remain.
//!
//! The references in each test name are the requirement numbers of 17-066r2.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]
#![expect(
    clippy::float_cmp,
    reason = "TEST009 and TEST011 are written as the standard writes them: \"fail if datatype is float and scale is not 1.0\". The requirement is exact equality with the defaults, and a tolerance here would pass files the abstract test fails"
)]

use std::path::{Path, PathBuf};

use geopackage::core::coverage::{CoverageDatatype, coverage_payload, coverage_tiff};
use geopackage::core::tiles::{TileCoord, TileMatrix, TileMatrixSet};
use geopackage::{CoverageBuilder, GeoPackage, TileAncillary};
use rusqlite::Connection;
use tempfile::TempDir;

/// The two subjects: the GDAL file, and a file from this crate.
///
/// Open as connections, not as [`GeoPackage`] handles, because the abstract
/// tests are statements about the file and are written as SQL.
fn subjects() -> Vec<(&'static str, Connection, Option<TempDir>)> {
    vec![("gdal", Connection::open(gdal_fixture()).unwrap(), None), {
        let (dir, path) = write_ours();
        ("ours", Connection::open(path).unwrap(), Some(dir))
    }]
}

fn gdal_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gdal_coverage.gpkg")
}

/// A coverage that this crate writes, with the payload of the fixture: a
/// float32 LZW TIFF that GDAL encoded. The suite therefore tests the container
/// from this crate, not a payload that this crate made.
fn write_ours() -> (TempDir, PathBuf) {
    let tile = {
        let gpkg = GeoPackage::open_read_only(gdal_fixture()).unwrap();
        let coverage = gpkg.coverage("coverage").unwrap();
        coverage.get_tile(TileCoord::new(0, 0, 0)).unwrap().unwrap()
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ours.gpkg");
    {
        let gpkg = GeoPackage::create(&path).unwrap();
        gpkg.add_epsg_srs(3857).unwrap();
        let matrix_set = TileMatrixSet::new(3857, 0.0, 0.0, 64.0, 64.0);
        let coverage = gpkg
            .create_coverage(
                &CoverageBuilder::new("elevation", matrix_set, CoverageDatatype::Float)
                    .matrix(TileMatrix::new(0, 1, 1, 64, 64, 1.0, 1.0))
                    .data_null(-9999.0)
                    .uom("m"),
            )
            .unwrap();
        let mut writer = coverage.writer().unwrap();
        writer
            .put_with_ancillary(
                TileCoord::new(0, 0, 0),
                &tile,
                &TileAncillary::defaults().with_statistics(0.5, 96.5, 48.53, 27.99),
            )
            .unwrap();
        writer.commit().unwrap();
        gpkg.close().unwrap();
    }
    (dir, path)
}

/// The column names of a table, as `PRAGMA table_info` gives them.
fn columns(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    stmt.query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// Returns the `gpkg_contents.table_name` of every coverage. Almost every test
/// starts with this query.
fn coverage_tables(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT table_name FROM gpkg_contents WHERE data_type = '2d-gridded-coverage'")
        .unwrap();
    stmt.query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn count(conn: &Connection, sql: &str, params: &[&dyn rusqlite::ToSql]) -> i64 {
    conn.query_row(sql, params, |row| row.get(0)).unwrap()
}

/// TEST001, Requirement 1: the coverage ancillary table has Table 27's
/// columns.
#[test]
fn test001_coverage_ancillary_table_definition() {
    for (who, conn, _dir) in subjects() {
        let found = columns(&conn, "gpkg_2d_gridded_coverage_ancillary");
        for column in [
            "id",
            "tile_matrix_set_name",
            "datatype",
            "scale",
            "offset",
            "precision",
            "data_null",
            "grid_cell_encoding",
            "uom",
            "field_name",
            "quantity_definition",
        ] {
            assert!(
                found.iter().any(|name| name == column),
                "{who}: gpkg_2d_gridded_coverage_ancillary has no {column} column ({found:?})"
            );
        }
    }
}

/// TEST002, Requirement 2: the tile ancillary table has its columns.
#[test]
fn test002_tile_ancillary_table_definition() {
    for (who, conn, _dir) in subjects() {
        let found = columns(&conn, "gpkg_2d_gridded_tile_ancillary");
        for column in [
            "id",
            "tpudt_name",
            "tpudt_id",
            "scale",
            "offset",
            "min",
            "max",
            "mean",
            "std_dev",
        ] {
            assert!(
                found.iter().any(|name| name == column),
                "{who}: gpkg_2d_gridded_tile_ancillary has no {column} column ({found:?})"
            );
        }
    }
}

/// TEST003, Requirement 3: the file contains the EPSG:4979 row.
///
/// A coverage file contains the geographic 3D row whether or not the coverage
/// uses it.
#[test]
fn test003_spatial_ref_sys_has_the_4979_row() {
    for (who, conn, _dir) in subjects() {
        let rows = count(
            &conn,
            "SELECT COUNT(*) FROM gpkg_spatial_ref_sys WHERE organization_coordsys_id = 4979 \
             AND (organization = 'EPSG' OR organization = 'epsg')",
            &[],
        );
        assert!(rows > 0, "{who}: no EPSG:4979 row in gpkg_spatial_ref_sys");
    }
}

/// TEST004, Requirement 4: every coverage has exactly one tile matrix set.
#[test]
fn test004_every_coverage_has_one_tile_matrix_set() {
    for (who, conn, _dir) in subjects() {
        let tables = coverage_tables(&conn);
        assert!(!tables.is_empty(), "{who}: no coverage to test");
        for table in tables {
            assert_eq!(
                count(
                    &conn,
                    "SELECT COUNT(*) FROM gpkg_tile_matrix_set WHERE table_name = ?1",
                    &[&table],
                ),
                1,
                "{who}: {table} does not have exactly one gpkg_tile_matrix_set row"
            );
        }
    }
}

/// TEST005, Requirement 5: every coverage is a `2d-gridded-coverage` row in
/// `gpkg_contents`.
///
/// The standard makes the last step manual ("manually inspect that all
/// elevation data is accounted for in the result set"). A test cannot do that
/// step for a file from another author. This test checks the automatic part:
/// the query returns the coverages that the test knows the file contains.
#[test]
fn test005_coverages_are_listed_in_contents() {
    for (who, conn, _dir) in subjects() {
        let tables = coverage_tables(&conn);
        let expected = if who == "gdal" {
            "coverage"
        } else {
            "elevation"
        };
        assert_eq!(
            tables,
            vec![expected.to_owned()],
            "{who}: the contents list is not the coverage in this file"
        );
        // No table with coverage tiles has a different data type: each table
        // registered for the extension is in the result.
        let registered: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT table_name FROM gpkg_extensions \
                     WHERE extension_name = 'gpkg_2d_gridded_coverage' \
                     AND column_name = 'tile_data'",
                )
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(registered, tables, "{who}: a coverage is not in contents");
    }
}

/// TEST006, Requirement 6: the three `gpkg_extensions` rows.
#[test]
fn test006_extension_rows() {
    for (who, conn, _dir) in subjects() {
        for table in [
            "gpkg_2d_gridded_coverage_ancillary",
            "gpkg_2d_gridded_tile_ancillary",
        ] {
            let scope: String = conn
                .query_row(
                    "SELECT scope FROM gpkg_extensions \
                     WHERE extension_name = 'gpkg_2d_gridded_coverage' AND table_name = ?1 \
                     AND column_name IS NULL",
                    [table],
                    |row| row.get(0),
                )
                .unwrap_or_else(|e| panic!("{who}: no extension row for {table}: {e}"));
            assert_eq!(scope, "read-write", "{who}: {table} scope");
        }
        for table in coverage_tables(&conn) {
            let (column, definition, scope): (String, String, String) = conn
                .query_row(
                    "SELECT column_name, definition, scope FROM gpkg_extensions \
                     WHERE extension_name = 'gpkg_2d_gridded_coverage' AND table_name = ?1",
                    [&table],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap_or_else(|e| panic!("{who}: no extension row for {table}: {e}"));
            assert_eq!(column, "tile_data", "{who}: {table} column_name");
            assert_eq!(scope, "read-write", "{who}: {table} scope");
            // Table 3 gives the r1 URL, as the r2 spec source does, and each
            // file writes it.
            assert_eq!(
                definition, "http://docs.opengeospatial.org/is/17-066r1/17-066r1.html",
                "{who}: {table} definition"
            );
        }
    }
}

/// TEST007, Requirement 7: one ancillary row for each coverage.
#[test]
fn test007_every_coverage_has_one_ancillary_row() {
    for (who, conn, _dir) in subjects() {
        for table in coverage_tables(&conn) {
            assert_eq!(
                count(
                    &conn,
                    "SELECT COUNT(*) FROM gpkg_2d_gridded_coverage_ancillary \
                     WHERE tile_matrix_set_name = ?1",
                    &[&table],
                ),
                1,
                "{who}: {table} does not have exactly one coverage ancillary row"
            );
        }
    }
}

/// TEST008, Requirement 8: every ancillary row names a tile matrix set.
#[test]
fn test008_ancillary_rows_reference_a_tile_matrix_set() {
    for (who, conn, _dir) in subjects() {
        let names: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT tile_matrix_set_name FROM gpkg_2d_gridded_coverage_ancillary")
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert!(!names.is_empty(), "{who}: no ancillary rows to test");
        for name in names {
            assert_eq!(
                count(
                    &conn,
                    "SELECT count(*) FROM gpkg_tile_matrix_set WHERE table_name = ?1",
                    &[&name],
                ),
                1,
                "{who}: {name} is not a tile matrix set"
            );
        }
    }
}

/// TEST009, Requirements 9 and 11: the datatype is `integer` or `float`, and a
/// `float` coverage keeps the default scale and offset.
#[test]
fn test009_coverage_ancillary_values() {
    for (who, conn, _dir) in subjects() {
        let mut stmt = conn
            .prepare(
                "SELECT datatype, scale, offset FROM gpkg_2d_gridded_coverage_ancillary \
                 WHERE tile_matrix_set_name IN \
                 (SELECT table_name FROM gpkg_contents WHERE data_type = '2d-gridded-coverage')",
            )
            .unwrap();
        let rows: Vec<(String, f64, f64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(!rows.is_empty(), "{who}: no rows to test");
        for (datatype, scale, offset) in rows {
            assert!(
                datatype == "integer" || datatype == "float",
                "{who}: datatype {datatype:?}"
            );
            if datatype == "float" {
                assert!(scale == 1.0, "{who}: float coverage with scale {scale}");
                assert!(offset == 0.0, "{who}: float coverage with offset {offset}");
            }
        }
    }
}

/// TEST010, Requirements 10 and 12: every tile has an ancillary row.
#[test]
fn test010_every_tile_has_an_ancillary_row() {
    for (who, conn, _dir) in subjects() {
        for table in coverage_tables(&conn) {
            let orphans = count(
                &conn,
                &format!(
                    "SELECT COUNT(*) FROM \"{table}\" t \
                     LEFT OUTER JOIN gpkg_2d_gridded_tile_ancillary a \
                     ON t.id = a.tpudt_id AND a.tpudt_name = ?1 \
                     WHERE a.tpudt_id IS NULL"
                ),
                &[&table],
            );
            assert_eq!(
                orphans, 0,
                "{who}: {table} has {orphans} tile(s) with no ancillary row"
            );
        }
    }
}

/// TEST011, Requirement 11: every tile ancillary row names an existing
/// coverage, and a `float` coverage keeps the defaults.
#[test]
fn test011_tile_ancillary_values() {
    for (who, conn, _dir) in subjects() {
        let mut stmt = conn
            .prepare("SELECT tpudt_name, scale, offset FROM gpkg_2d_gridded_tile_ancillary")
            .unwrap();
        let rows: Vec<(String, f64, f64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(!rows.is_empty(), "{who}: no tile ancillary rows to test");
        for (tpudt_name, scale, offset) in rows {
            assert!(
                !columns(&conn, &format!("\"{tpudt_name}\"")).is_empty(),
                "{who}: {tpudt_name} is not a table or view"
            );
            let datatype: String = conn
                .query_row(
                    "SELECT datatype FROM gpkg_2d_gridded_coverage_ancillary \
                     WHERE tile_matrix_set_name = ?1",
                    [&tpudt_name],
                    |row| row.get(0),
                )
                .unwrap_or_else(|e| panic!("{who}: no coverage row for {tpudt_name}: {e}"));
            if datatype == "float" {
                assert!(scale == 1.0, "{who}: float tile with scale {scale}");
                assert!(offset == 0.0, "{who}: float tile with offset {offset}");
            }
        }
    }
}

/// TEST012, Requirements 13, 14 and the TIFF encoding class: every payload has
/// the encoding that the `datatype` of its coverage specifies.
///
/// Step (c) of the standard is "fail if `tile_data` is not a valid TIFF image
/// as per requirements 115-121". In the numbering of 17-066r2, that is the TIFF
/// Encoding requirements class: Requirements 15 to 20, which [`coverage_tiff`]
/// checks. The code of this workspace implements this one abstract test, so the
/// test calls the public function and does not implement the check again.
#[test]
fn test012_payloads_match_the_datatype() {
    for (who, conn, _dir) in subjects() {
        for table in coverage_tables(&conn) {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT t.datatype AS datatype, u.id AS id, u.tile_data AS tile_data \
                     FROM gpkg_2d_gridded_coverage_ancillary t, \"{table}\" u \
                     WHERE t.tile_matrix_set_name = ?1"
                ))
                .unwrap();
            let rows: Vec<(String, i64, Vec<u8>)> = stmt
                .query_map([&table], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert!(!rows.is_empty(), "{who}: {table} has no tiles to test");
            for (datatype, id, tile_data) in rows {
                let payload = coverage_payload(&tile_data)
                    .unwrap_or_else(|e| panic!("{who}: {table} tile {id} is unreadable: {e}"));
                let datatype = CoverageDatatype::parse(&datatype)
                    .unwrap_or_else(|| panic!("{who}: {table} declares datatype {datatype:?}"));
                datatype
                    .check_payload(&payload)
                    .unwrap_or_else(|e| panic!("{who}: {table} tile {id}: {e}"));
                if datatype == CoverageDatatype::Float {
                    // All of step (c): the TIFF encoding class, on the
                    // payload, not on what the file says about the payload.
                    coverage_tiff(&tile_data)
                        .unwrap_or_else(|e| panic!("{who}: {table} tile {id}: {e}"));
                }
            }
        }
    }
}

/// The suite, as one result for a file that this crate wrote.
///
/// Each of the twelve tests above runs over every subject. This test gives the
/// summary: a coverage that this workspace writes passes the abstract test
/// suite of the extension, for all the steps that do not need a person.
#[test]
fn geopackage_writes_a_conformant_coverage() {
    let (_dir, path) = write_ours();
    let gpkg = GeoPackage::open_read_only(&path).unwrap();
    // This crate detects no defect in the file.
    assert_eq!(gpkg.validate().unwrap(), Vec::new());
    // The file also round trips through the reader that the abstract tests
    // above check the file for.
    let coverage = gpkg.coverage("elevation").unwrap();
    assert_eq!(coverage.datatype(), Some(CoverageDatatype::Float));
    assert!(
        coverage
            .tile_ancillary(TileCoord::new(0, 0, 0))
            .unwrap()
            .is_some()
    );
}
