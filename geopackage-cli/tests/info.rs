//! `gpkg info` over the committed fixtures, driven as a subprocess so the test
//! exercises what a user runs rather than the functions behind it.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// The fixtures belong to the `geopackage` crate, one directory up.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("geopackage")
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn info(name: &str) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_gpkg"))
        .arg("info")
        .arg(fixture(name))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "gpkg info {name} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn reports_layers_their_geometry_and_their_index_state() {
    let out = info("gdal_multilayer_1_4.gpkg");
    assert!(out.contains("version: 1.4"), "{out}");
    assert!(out.contains(r#"layer "points" (features)"#), "{out}");
    assert!(out.contains("geometry: geom (POINT, srs_id 4326)"), "{out}");
    assert!(out.contains("WGS 84 geodetic (EPSG:4326)"), "{out}");
    // One indexed layer and one not, so both index states are covered.
    assert!(out.contains("index:    current"), "{out}");
    assert!(out.contains("index:    absent"), "{out}");
    assert!(out.contains("rows:     3"), "{out}");
}

#[test]
fn reports_extensions_with_the_support_level_this_crate_has_for_them() {
    let out = info("gdal_multilayer_1_4.gpkg");
    assert!(out.contains("extensions:"), "{out}");
    assert!(out.contains("gpkg_rtree_index"), "{out}");
    assert!(out.contains("implemented"), "{out}");
}

#[test]
fn a_legacy_file_opens_and_says_why_it_is_legacy() {
    let out = info("legacy_gp10.gpkg");
    assert!(out.contains("version: 1.0"), "{out}");
    // Rendered as a sentence, not as a debug-printed enum.
    assert!(out.contains("warning: file declares"), "{out}");
    assert!(out.contains(r#""GP10""#), "{out}");
    assert!(!out.contains("LegacyApplicationId"), "{out}");
}

#[test]
fn a_tile_pyramid_reports_its_zooms_and_matrix_sizes() {
    let out = info("gdal_tiles.gpkg");
    assert!(out.contains(r#"tiles "tiles""#), "{out}");
    assert!(out.contains("zooms:"), "{out}");
    assert!(out.contains("256x256 px"), "{out}");
    // The file carries no features, and that is stated rather than left blank.
    assert!(out.contains("no feature layers"), "{out}");
}

#[test]
fn a_missing_file_fails_rather_than_printing_nothing() {
    let output = Command::new(env!("CARGO_BIN_EXE_gpkg"))
        .arg("info")
        .arg("definitely-not-here.gpkg")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).starts_with("gpkg: "),
        "{:?}",
        String::from_utf8_lossy(&output.stderr)
    );
}
