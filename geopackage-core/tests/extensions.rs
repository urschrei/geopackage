//! Property tests over extension names and scopes: the strings a
//! `gpkg_extensions` row stores and the values this crate writes.

use geopackage_core::extensions::{Extension, ExtensionScope, ExtensionSupport};
use geopackage_core::types::GeometryType;
use hegel::generators;

/// The current spelling of every extension this crate identifies, apart from
/// the geometry types.
const CANONICAL: [&str; 12] = [
    "gpkg_rtree_index",
    "gpkg_geometry_type_trigger",
    "gpkg_srs_id_trigger",
    "gpkg_zoom_other",
    "gpkg_webp",
    "gpkg_metadata",
    "gpkg_schema",
    "gpkg_crs_wkt",
    "gpkg_crs_wkt_1_1",
    "gpkg_2d_gridded_coverage",
    "gpkg_related_tables",
    "gdal_aspatial",
];

/// Earlier spellings, each with the current spelling it maps to.
const ALIASES: [(&str, &str); 3] = [
    ("2d_gridded_coverage", "gpkg_2d_gridded_coverage"),
    ("gpkg_elevation_tiles", "gpkg_2d_gridded_coverage"),
    ("related_tables", "gpkg_related_tables"),
];

/// The geometry types an Annex F.1 name may carry.
const EXTENSION_TYPES: [&str; 7] = [
    "CIRCULARSTRING",
    "COMPOUNDCURVE",
    "CURVEPOLYGON",
    "MULTICURVE",
    "MULTISURFACE",
    "CURVE",
    "SURFACE",
];

/// Changes the ASCII case of each letter of `text` as drawn.
fn recase(tc: &hegel::TestCase, text: &str) -> String {
    text.chars()
        .map(|c| {
            if tc.draw(generators::booleans()) {
                c.to_ascii_lowercase()
            } else {
                c.to_ascii_uppercase()
            }
        })
        .collect()
}

/// Draws a name near the vocabulary: a current spelling, an alias, a
/// geometry type name in any case (core types included), any of them with a
/// character changed, or any text.
fn draw_name(tc: &hegel::TestCase) -> String {
    let base = match tc.draw(generators::integers::<u8>().min_value(0).max_value(3)) {
        0 => tc
            .draw(generators::sampled_from(CANONICAL.to_vec()))
            .to_owned(),
        1 => tc
            .draw(generators::sampled_from(ALIASES.to_vec()))
            .0
            .to_owned(),
        2 => {
            let ty = tc.draw(generators::sampled_from(
                EXTENSION_TYPES
                    .iter()
                    .chain(["POINT", "POLYGON", "GEOMETRY"].iter())
                    .copied()
                    .collect::<Vec<_>>(),
            ));
            format!("gpkg_geom_{}", recase(tc, ty))
        }
        _ => return tc.draw(generators::text().max_size(24)),
    };
    if tc.draw(generators::weighted_booleans(0.2)) {
        format!("{base}{}", tc.draw(generators::characters()))
    } else {
        base
    }
}

/// The name a reader writes back for a stored name, by the documented
/// rules: an alias becomes its current spelling, an extension geometry type
/// is spelled in upper case, and every other name is kept as written.
fn expected_name(name: &str) -> String {
    if let Some((_, current)) = ALIASES.iter().find(|(alias, _)| *alias == name) {
        return (*current).to_owned();
    }
    if let Some(ty) = name.strip_prefix("gpkg_geom_")
        && EXTENSION_TYPES.contains(&ty.to_ascii_uppercase().as_str())
    {
        return format!("gpkg_geom_{}", ty.to_ascii_uppercase());
    }
    name.to_owned()
}

/// `Extension::from_name` keeps a name as written, except that it
/// normalises an alias to its current spelling and an extension geometry
/// type to upper case; it identifies exactly the known names.
#[hegel::test]
fn an_extension_name_reads_as_documented(tc: hegel::TestCase) {
    let name = draw_name(&tc);
    let extension = Extension::from_name(&name);
    let expected = expected_name(&name);
    assert_eq!(extension.name(), expected, "{name:?}");
    let known = CANONICAL.contains(&expected.as_str())
        || expected
            .strip_prefix("gpkg_geom_")
            .is_some_and(|ty| EXTENSION_TYPES.contains(&ty));
    assert_eq!(!matches!(extension, Extension::Other(_)), known, "{name:?}");
}

/// Draws any `Extension` value a caller can build, including a geometry
/// type that is not an extension type and `Other` holding a known name.
fn draw_extension(tc: &hegel::TestCase) -> Extension {
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(2)) {
        0 => Extension::from_name(&draw_name(tc)),
        1 => {
            let ty = tc.draw(generators::sampled_from(
                [
                    "POINT",
                    "GEOMETRY",
                    "MULTIPOLYGON",
                    "CIRCULARSTRING",
                    "CURVE",
                    "MULTISURFACE",
                ]
                .to_vec(),
            ));
            Extension::GeometryType(GeometryType::parse(ty).unwrap_or(GeometryType::Geometry))
        }
        _ => Extension::Other(draw_name(tc)),
    }
}

/// An extension's support level is the level of what a reader identifies
/// from the name it writes, so a row this crate writes is classified the
/// same way when read.
#[hegel::test]
fn support_agrees_with_a_reader(tc: hegel::TestCase) {
    let extension = draw_extension(&tc);
    let read_back = Extension::from_name(&extension.name());
    assert_eq!(extension.support(), read_back.support(), "{extension:?}");
    assert_eq!(
        extension.is_removed(),
        extension.support() == ExtensionSupport::Removed
    );
}

/// A name a reader identifies reads back as the same extension from the
/// name it writes.
#[hegel::test]
fn an_identified_extension_round_trips(tc: hegel::TestCase) {
    let extension = Extension::from_name(&draw_name(&tc));
    assert_eq!(Extension::from_name(&extension.name()), extension);
}

/// `ExtensionScope::parse` keeps any value as written, recognises exactly
/// the two lowercase scopes, and only `write-only` spares readers.
#[hegel::test]
fn an_extension_scope_reads_as_documented(tc: hegel::TestCase) {
    let text = match tc.draw(generators::integers::<u8>().min_value(0).max_value(2)) {
        0 => tc
            .draw(generators::sampled_from(vec!["read-write", "write-only"]))
            .to_owned(),
        1 => recase(
            &tc,
            tc.draw(generators::sampled_from(vec!["read-write", "write-only"])),
        ),
        _ => tc.draw(generators::text().max_size(12)),
    };
    let scope = ExtensionScope::parse(&text);
    assert_eq!(scope.as_str(), text);
    assert_eq!(scope.to_string(), text);
    let recognised = text == "read-write" || text == "write-only";
    assert_eq!(!matches!(scope, ExtensionScope::Other(_)), recognised);
    assert_eq!(scope.affects_readers(), text != "write-only");
    assert!(scope.affects_writers());
}
