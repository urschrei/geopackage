//! Property tests over the type vocabularies: geometry type names, the set of
//! geometry types, declared column types, the `z`/`m` flag codes, and the
//! version pragmas.

use std::collections::BTreeSet;

use geopackage_core::GpkgVersion;
use geopackage_core::types::{ColumnType, GeometryType, GeometryTypeSet, ZmFlag};
use geopackage_core::version::{APPLICATION_ID_GP10, APPLICATION_ID_GP11, APPLICATION_ID_GPKG};
use hegel::generators;

/// Every geometry type with its spec name, in declaration order, which is the
/// order of their ISO WKB base codes.
const TYPES: [(GeometryType, &str); 15] = [
    (GeometryType::Geometry, "GEOMETRY"),
    (GeometryType::Point, "POINT"),
    (GeometryType::LineString, "LINESTRING"),
    (GeometryType::Polygon, "POLYGON"),
    (GeometryType::MultiPoint, "MULTIPOINT"),
    (GeometryType::MultiLineString, "MULTILINESTRING"),
    (GeometryType::MultiPolygon, "MULTIPOLYGON"),
    (GeometryType::GeometryCollection, "GEOMETRYCOLLECTION"),
    (GeometryType::CircularString, "CIRCULARSTRING"),
    (GeometryType::CompoundCurve, "COMPOUNDCURVE"),
    (GeometryType::CurvePolygon, "CURVEPOLYGON"),
    (GeometryType::MultiCurve, "MULTICURVE"),
    (GeometryType::MultiSurface, "MULTISURFACE"),
    (GeometryType::Curve, "CURVE"),
    (GeometryType::Surface, "SURFACE"),
];

fn draw_type(tc: &hegel::TestCase) -> (usize, GeometryType, &'static str) {
    let (index, (ty, name)) = tc.draw(generators::sampled_from(
        TYPES.iter().copied().enumerate().collect::<Vec<_>>(),
    ));
    (index, ty, name)
}

/// Changes the ASCII case of each letter of `name` as drawn.
fn recase(tc: &hegel::TestCase, name: &str) -> String {
    name.chars()
        .map(|c| {
            if tc.draw(generators::booleans()) {
                c.to_ascii_lowercase()
            } else {
                c.to_ascii_uppercase()
            }
        })
        .collect()
}

/// Draws a name: a spec name in a drawn case, the same with a character
/// changed or a space around it, or any text.
fn draw_name_like(tc: &hegel::TestCase) -> String {
    let (_, _, name) = draw_type(tc);
    let recased = recase(tc, name);
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(3)) {
        0 => recased,
        1 => format!(" {recased}"),
        2 => {
            let at = tc.draw(
                generators::integers::<usize>()
                    .min_value(0)
                    .max_value(recased.len()),
            );
            let mut chars: Vec<char> = recased.chars().collect();
            chars.insert(at, tc.draw(generators::characters()));
            chars.into_iter().collect()
        }
        _ => tc.draw(generators::text()),
    }
}

/// `GeometryType::parse` accepts a string exactly when its ASCII upper case
/// is a spec name, and returns that type.
#[hegel::test]
fn geometry_type_parse_accepts_exactly_the_names_in_any_ascii_case(tc: hegel::TestCase) {
    let text = draw_name_like(&tc);
    let expected = TYPES
        .iter()
        .find(|(_, name)| text.to_ascii_uppercase() == *name)
        .map(|(ty, _)| *ty);
    assert_eq!(GeometryType::parse(&text), expected, "{text:?}");
}

/// Each type's `as_str` and `Display` are its spec name, which parses back to
/// it, and only the eight core types need no extension row.
#[hegel::test]
fn a_geometry_type_names_itself(tc: hegel::TestCase) {
    let (index, ty, name) = draw_type(&tc);
    assert_eq!(ty.as_str(), name);
    assert_eq!(ty.to_string(), name);
    assert_eq!(GeometryType::parse(name), Some(ty));
    assert_eq!(ty.is_extension(), index >= 8);
}

/// A `GeometryTypeSet` behaves as a set of types under any sequence of
/// inserts and unions, and iterates in declaration order.
#[hegel::test]
fn geometry_type_set_behaves_as_a_set(tc: hegel::TestCase) {
    let mut set = GeometryTypeSet::new();
    let mut model: BTreeSet<usize> = BTreeSet::new();
    let steps = tc.draw(generators::integers::<usize>().min_value(0).max_value(20));
    for _ in 0..steps {
        if tc.draw(generators::booleans()) {
            let (index, ty, _) = draw_type(&tc);
            set.insert(ty);
            model.insert(index);
        } else {
            let mut other = GeometryTypeSet::new();
            let count = tc.draw(generators::integers::<usize>().min_value(0).max_value(4));
            for _ in 0..count {
                let (index, ty, _) = draw_type(&tc);
                other.insert(ty);
                model.insert(index);
            }
            set.extend(other);
        }
        assert_eq!(set.is_empty(), model.is_empty());
        for (index, (ty, _)) in TYPES.iter().enumerate() {
            assert_eq!(set.contains(*ty), model.contains(&index), "{ty:?}");
        }
        let iterated: Vec<GeometryType> = set.iter().collect();
        let expected: Vec<GeometryType> = model.iter().map(|&index| TYPES[index].0).collect();
        assert_eq!(iterated, expected);
    }
}

/// Draws a declared column type of any kind, with any length.
fn draw_column_type(tc: &hegel::TestCase) -> ColumnType {
    let length = || {
        if tc.draw(generators::booleans()) {
            None
        } else {
            Some(tc.draw(generators::integers::<u32>()))
        }
    };
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(12)) {
        0 => ColumnType::Boolean,
        1 => ColumnType::TinyInt,
        2 => ColumnType::SmallInt,
        3 => ColumnType::MediumInt,
        4 => ColumnType::Integer,
        5 => ColumnType::Float,
        6 => ColumnType::Double,
        7 => ColumnType::Text(length()),
        8 => ColumnType::Blob(length()),
        9 => ColumnType::Date,
        10 => ColumnType::DateTime,
        _ => ColumnType::Geometry(draw_type(tc).1),
    }
}

/// A column type's DDL spelling parses back to it, in any ASCII case and
/// with whitespace around it, as `PRAGMA table_info` may report it.
#[hegel::test]
fn a_column_type_parses_from_its_ddl_name(tc: hegel::TestCase) {
    let ty = draw_column_type(&tc);
    let ddl = ty.ddl_name();
    let spaced = format!(
        "{}{}{}",
        " ".repeat(tc.draw(generators::integers::<usize>().min_value(0).max_value(2))),
        recase(&tc, &ddl),
        " ".repeat(tc.draw(generators::integers::<usize>().min_value(0).max_value(2))),
    );
    assert_eq!(ColumnType::parse(&ddl), Some(ty.clone()), "{ddl:?}");
    assert_eq!(ColumnType::parse(&spaced), Some(ty), "{spaced:?}");
}

/// `ZmFlag::from_code` accepts exactly the codes 0, 1 and 2, and `code`
/// inverts it.
#[hegel::test]
fn zm_flag_codes_round_trip(tc: hegel::TestCase) {
    let code = tc.draw(generators::integers::<u8>());
    match ZmFlag::from_code(code) {
        Some(flag) => {
            assert!(code <= 2);
            assert_eq!(flag.code(), code);
        }
        None => assert!(code > 2),
    }
}

/// `from_pragmas` classifies every pair of pragmas as documented: GP10 and
/// GP11 by `application_id`, GPKG by the `user_version` hundreds with every
/// version above 1.4 read as 1.4, and anything else as no GeoPackage.
#[hegel::test]
fn from_pragmas_classifies_every_pair(tc: hegel::TestCase) {
    let application_id = if tc.draw(generators::booleans()) {
        tc.draw(generators::sampled_from(vec![
            APPLICATION_ID_GP10,
            APPLICATION_ID_GP11,
            APPLICATION_ID_GPKG,
        ]))
    } else {
        tc.draw(generators::integers::<u32>())
    };
    let user_version = if tc.draw(generators::booleans()) {
        tc.draw(
            generators::integers::<u32>()
                .min_value(10_000)
                .max_value(10_600),
        )
    } else {
        tc.draw(generators::integers::<u32>())
    };
    let expected = match application_id {
        APPLICATION_ID_GP10 => Some(GpkgVersion::V1_0),
        APPLICATION_ID_GP11 => Some(GpkgVersion::V1_1),
        APPLICATION_ID_GPKG => match user_version / 100 {
            102 => Some(GpkgVersion::V1_2),
            103 => Some(GpkgVersion::V1_3),
            104.. => Some(GpkgVersion::V1_4),
            _ => None,
        },
        _ => None,
    };
    assert_eq!(
        GpkgVersion::from_pragmas(application_id, user_version),
        expected,
        "{application_id:#x} {user_version}"
    );
}

/// The `user_version` this crate writes for a version reads back as that
/// version.
#[hegel::test]
fn a_written_user_version_reads_back(tc: hegel::TestCase) {
    let version = tc.draw(generators::sampled_from(vec![
        GpkgVersion::V1_2,
        GpkgVersion::V1_3,
        GpkgVersion::V1_4,
    ]));
    let user_version = version.user_version().unwrap_or_default();
    assert_eq!(
        GpkgVersion::from_pragmas(APPLICATION_ID_GPKG, user_version),
        Some(version)
    );
}
