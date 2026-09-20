//! The tiled gridded coverage extension's Abstract Test Suite (OGC 17-066r2
//! Annex A), TEST001 to TEST012.
//!
//! There is no executable ETS for this extension — `ets-gpkg12` validates 1.2
//! core and tiles and skips the rest — so the abstract tests are the nearest
//! thing to one, and this file is them: each test follows the method the
//! standard states, as SQL against the file, rather than through this crate's
//! API. Going through the API would check that the API agrees with itself; the
//! point here is to check the *file*.
//!
//! Every test runs twice, over
//!
//! - `gdal_coverage.gpkg`, the committed fixture GDAL wrote, which is the
//!   control: a file another implementation produced and believes in, and
//!   [`geopackage_writes_a_conformant_coverage`]'s counterpart.
//! - a coverage this crate writes, which is what the suite is really for.
//!
//! Two of the twelve are `Test Type: Capability` with a manual step
//! (TEST005 asks a human to confirm every coverage is accounted for), and
//! TEST012's TIFF half is exactly the payload profile
//! [`geopackage_core::coverage`] implements, so they are run as far as they
//! can be automatically and what is left is said out loud in the test.
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

/// The two subjects: GDAL's file, and ours.
///
/// Held open as connections rather than as [`GeoPackage`] handles, because the
/// abstract tests are statements about the file and are written as SQL.
fn subjects() -> Vec<(&'static str, Connection, Option<TempDir>)> {
    vec![("gdal", Connection::open(gdal_fixture()).unwrap(), None), {
        let (dir, path) = write_ours();
        ("ours", Connection::open(path).unwrap(), Some(dir))
    }]
}

fn gdal_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gdal_coverage.gpkg")
}

/// A coverage this crate writes, carrying the fixture's own payload: a float32
/// LZW TIFF that GDAL encoded, so the suite judges our container rather than
/// our ability to make up a payload.
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

/// The `gpkg_contents.table_name` of every coverage, which almost every test
/// starts by asking for.
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

/// TEST003, Requirement 3: the file carries the EPSG:4979 row.
///
/// The one requirement this crate did not meet when the suite was first run:
/// a coverage file carries the geographic 3D row whether or not the coverage
/// uses it, and `create_coverage` now adds it.
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
/// The standard marks the last step manual ("manually inspect that all
/// elevation data is accounted for in the result set"), which no test can do
/// for a file it did not author. What is checked here is the half that can be:
/// the query returns what this test knows the file to contain.
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
            "{who}: the contents listing is not the coverage this file holds"
        );
        // And nothing that holds coverage tiles is hiding under another data
        // type: a table registered for the extension is one of these.
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
            // Table 3 gives the r1 URL, which is what the r2 spec source still
            // prints; both files write it.
            assert_eq!(
                definition, "http://docs.opengeospatial.org/is/17-066r1/17-066r1.html",
                "{who}: {table} definition"
            );
        }
    }
}

/// TEST007, Requirement 7: one ancillary row per coverage.
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

/// TEST011, Requirement 11: every tile ancillary row names a real coverage,
/// and a `float` one keeps the defaults.
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

/// TEST012, Requirements 13, 14 and the TIFF encoding class: every payload is
/// the encoding its coverage's `datatype` calls for.
///
/// The standard's step (c) is "fail if `tile_data` is not a valid TIFF image
/// as per requirements 115-121", which in 17-066r2's own numbering is the TIFF
/// Encoding requirements class — Requirements 15 to 20, and exactly what
/// [`coverage_tiff`] checks. This is the one abstract test that this
/// workspace's own code is the implementation of, so it is run through the
/// public entry point rather than reimplemented here.
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
                    // Step (c) in full: the TIFF encoding class, on the
                    // payload rather than on what the file says about it.
                    coverage_tiff(&tile_data)
                        .unwrap_or_else(|e| panic!("{who}: {table} tile {id}: {e}"));
                }
            }
        }
    }
}

/// The suite, as a single verdict on a file this crate wrote.
///
/// The twelve tests above each run over both subjects; this is the sentence
/// they add up to, and the one worth quoting: a coverage this workspace writes
/// passes the extension's abstract test suite as far as it can be run without
/// a human.
#[test]
fn geopackage_writes_a_conformant_coverage() {
    let (_dir, path) = write_ours();
    let gpkg = GeoPackage::open_read_only(&path).unwrap();
    // Nothing this crate can detect is wrong with it ...
    assert_eq!(gpkg.validate().unwrap(), Vec::new());
    // ... and it round trips through the reader that the abstract tests above
    // check the file for.
    let coverage = gpkg.coverage("elevation").unwrap();
    assert_eq!(coverage.datatype(), Some(CoverageDatatype::Float));
    assert!(
        coverage
            .tile_ancillary(TileCoord::new(0, 0, 0))
            .unwrap()
            .is_some()
    );
}
