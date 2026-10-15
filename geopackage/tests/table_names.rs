//! Property tests over the names of new user tables, drawn from the whole of
//! Unicode, for every path that creates one.
//!
//! The documented outcome of a name depends only on the name: an empty name or
//! one containing NUL is an invalid identifier, a name that begins `gpkg_` or
//! `sqlite_` in any case is reserved, and every other name creates a table
//! that opens by that name and is listed in `gpkg_contents` under it.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::path::Path;

use geopackage::core::coverage::CoverageDatatype;
use geopackage::core::tiles::{TileMatrixSet, ZoomLadder};
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{
    ColumnSpec, CoverageBuilder, Error, GeoPackage, GeometrySpec, TableSchemaBuilder,
    TilePyramidBuilder,
};
use hegel::generators;

/// The documented outcome of creating a table with a given name.
#[derive(Debug, PartialEq, Eq)]
enum Expected {
    InvalidIdentifier,
    Reserved,
    Created,
}

fn expected(name: &str) -> Expected {
    if name.is_empty() || name.contains('\0') {
        return Expected::InvalidIdentifier;
    }
    let reserved = ["gpkg_", "sqlite_"].iter().any(|prefix| {
        name.get(..prefix.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
    });
    if reserved {
        Expected::Reserved
    } else {
        Expected::Created
    }
}

/// Draws a name: any string, or one that begins with a reserved prefix in a
/// drawn mix of cases, which a draw from the whole of Unicode rarely produces.
fn draw_name(tc: &hegel::TestCase) -> String {
    let rest = tc.draw(generators::text());
    if tc.draw(generators::weighted_booleans(0.8)) {
        return rest;
    }
    let prefix = tc.draw(generators::sampled_from(vec!["gpkg_", "sqlite_"]));
    let cased: String = prefix
        .chars()
        .map(|c| {
            if tc.draw(generators::booleans()) {
                c.to_ascii_uppercase()
            } else {
                c
            }
        })
        .collect();
    cased + &rest
}

/// Checks a creation result against the documented outcome for `name`, and
/// returns whether a table was created.
fn check_outcome<T>(name: &str, result: geopackage::Result<T>) -> bool {
    match expected(name) {
        Expected::InvalidIdentifier => {
            assert!(
                matches!(
                    result,
                    Err(Error::Core(geopackage::core::Error::InvalidIdentifier(_)))
                ),
                "{name:?}: {:?}",
                result.err()
            );
            false
        }
        Expected::Reserved => {
            assert!(
                matches!(result, Err(Error::ReservedTablePrefix { .. })),
                "{name:?}: {:?}",
                result.err()
            );
            false
        }
        Expected::Created => {
            result
                .map_err(|error| format!("{name:?}: {error:?}"))
                .unwrap();
            true
        }
    }
}

/// The names in `gpkg_contents`.
fn contents(gpkg: &GeoPackage) -> Vec<String> {
    gpkg.contents()
        .unwrap()
        .into_iter()
        .map(|entry| entry.table_name)
        .collect()
}

fn in_memory() -> GeoPackage {
    GeoPackage::create(Path::new(":memory:")).unwrap()
}

#[hegel::test]
fn a_tile_pyramid_name_has_its_documented_outcome(tc: hegel::TestCase) {
    let gpkg = in_memory();
    let name = draw_name(&tc);
    let matrix_set = TileMatrixSet::new(0, 0.0, 0.0, 1.0, 1.0);
    let matrices = matrix_set.ladder(ZoomLadder::new(0, 0)).unwrap();
    let result =
        gpkg.create_tile_pyramid(&TilePyramidBuilder::new(&name, matrix_set).matrices(matrices));
    if check_outcome(&name, result) {
        assert_eq!(gpkg.tiles(&name).unwrap().table_name(), name);
        assert_eq!(contents(&gpkg), vec![name]);
    }
}

#[hegel::test]
fn a_coverage_name_has_its_documented_outcome(tc: hegel::TestCase) {
    let gpkg = in_memory();
    let name = draw_name(&tc);
    let matrix_set = TileMatrixSet::new(0, 0.0, 0.0, 1.0, 1.0);
    let matrices = matrix_set.ladder(ZoomLadder::new(0, 0)).unwrap();
    let result = gpkg.create_coverage(
        &CoverageBuilder::new(&name, matrix_set, CoverageDatatype::Float).matrices(matrices),
    );
    if check_outcome(&name, result) {
        assert_eq!(gpkg.coverage(&name).unwrap().table_name(), name);
        assert_eq!(contents(&gpkg), vec![name]);
    }
}

/// A feature layer with its spatial index, whose virtual table and triggers
/// are named after the table.
#[hegel::test]
fn a_feature_layer_name_has_its_documented_outcome(tc: hegel::TestCase) {
    let gpkg = in_memory();
    let name = draw_name(&tc);
    let result = gpkg.create_layer(
        &TableSchemaBuilder::new(&name).geometry(GeometrySpec::new(GeometryType::Point, 4326)),
    );
    if check_outcome(&name, result) {
        let layer = gpkg.layer(&name).unwrap();
        assert_eq!(layer.table_name(), name);
        assert!(layer.has_spatial_index().unwrap());
        assert_eq!(contents(&gpkg), vec![name]);
    }
}

#[hegel::test]
fn an_attribute_table_name_has_its_documented_outcome(tc: hegel::TestCase) {
    let gpkg = in_memory();
    let name = draw_name(&tc);
    let result = gpkg.create_attributes_table(
        &TableSchemaBuilder::new(&name).column(ColumnSpec::new("body", ColumnType::Text(None))),
    );
    if check_outcome(&name, result) {
        assert_eq!(gpkg.attributes(&name).unwrap().table_name(), name);
        assert_eq!(contents(&gpkg), vec![name]);
    }
}
