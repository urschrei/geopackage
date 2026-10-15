//! Property tests over the metadata and related-tables vocabularies: metadata
//! scopes, reference scopes and relation names, as strings a file stores and
//! as the values this crate writes.

use geopackage_core::metadata::{MetadataScope, ReferenceScope, ReferenceTargets};
use geopackage_core::related::RelationName;
use hegel::generators;

/// The `md_scope` names of spec Table 15.
const LISTED: [&str; 20] = [
    "undefined",
    "fieldSession",
    "collectionSession",
    "series",
    "dataset",
    "featureType",
    "feature",
    "attributeType",
    "attribute",
    "tile",
    "model",
    "catalog",
    "schema",
    "taxonomy",
    "software",
    "service",
    "collectionHardware",
    "nonGeographicDataset",
    "dimensionGroup",
    "style",
];

/// Draws a string near a vocabulary: one of its words, the same with its case
/// or a character changed, or any text.
fn draw_near(tc: &hegel::TestCase, words: &[&str]) -> String {
    let word = tc.draw(generators::sampled_from(words.to_vec())).to_owned();
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(3)) {
        0 => word,
        1 => word.to_ascii_uppercase(),
        2 => format!("{word}{}", tc.draw(generators::characters())),
        _ => tc.draw(generators::text()),
    }
}

/// `MetadataScope::parse` keeps any string as written, and recognises
/// exactly the names Table 15 lists, in their exact case.
#[hegel::test]
fn a_metadata_scope_string_round_trips(tc: hegel::TestCase) {
    let text = draw_near(&tc, &LISTED);
    let scope = MetadataScope::parse(&text);
    assert_eq!(scope.as_str(), text);
    assert_eq!(scope.to_string(), text);
    assert_eq!(
        scope.is_listed(),
        LISTED.contains(&text.as_str()),
        "{text:?}"
    );
}

/// Draws any `MetadataScope` value a caller can build: a listed scope, or
/// `Other` with any text.
fn draw_metadata_scope(tc: &hegel::TestCase) -> MetadataScope {
    if tc.draw(generators::booleans()) {
        MetadataScope::parse(tc.draw(generators::sampled_from(LISTED.to_vec())))
    } else {
        MetadataScope::Other(draw_near(tc, &LISTED))
    }
}

/// A `MetadataScope` value reads back as itself from the string it writes,
/// except `Other` holding a listed name, which reads back as that name's
/// variant, as documented; `is_listed` agrees with what is read back.
#[hegel::test]
fn a_metadata_scope_value_round_trips(tc: hegel::TestCase) {
    let scope = draw_metadata_scope(&tc);
    let back = MetadataScope::parse(scope.as_str());
    match &scope {
        MetadataScope::Other(text) if LISTED.contains(&text.as_str()) => {
            assert!(back.is_listed(), "{scope:?} read back as {back:?}");
        }
        _ => assert_eq!(back, scope),
    }
    assert_eq!(scope.is_listed(), back.is_listed());
}

/// `ReferenceScope::parse` accepts exactly the five lowercase names, and each
/// names itself and the target columns Requirements 97 to 99 require.
#[hegel::test]
fn reference_scopes_are_exactly_the_five(tc: hegel::TestCase) {
    let names = ["geopackage", "table", "column", "row", "row/col"];
    let text = draw_near(&tc, &names);
    match ReferenceScope::parse(&text) {
        Some(scope) => {
            assert!(names.contains(&text.as_str()), "{text:?}");
            assert_eq!(scope.as_str(), text);
            let targets = scope.targets();
            assert_eq!(
                targets,
                ReferenceTargets {
                    table_name: text != "geopackage",
                    column_name: text == "column" || text == "row/col",
                    row_id_value: text == "row" || text == "row/col",
                }
            );
        }
        None => assert!(!names.contains(&text.as_str()), "{text:?}"),
    }
}

/// The relation names of the five requirements classes.
const CLASSES: [&str; 5] = [
    "media",
    "simple_attributes",
    "features",
    "attributes",
    "tiles",
];

/// `RelationName::parse` keeps any string as written.
#[hegel::test]
fn a_relation_name_string_round_trips(tc: hegel::TestCase) {
    let text = if tc.draw(generators::booleans()) {
        draw_near(&tc, &CLASSES)
    } else {
        format!("x-{}", tc.draw(generators::text()))
    };
    assert_eq!(RelationName::parse(&text).as_string(), text);
}

/// Draws any `RelationName` value a caller can build: a class, an extension
/// with any author and name, or `Other` with any text.
fn draw_relation_name(tc: &hegel::TestCase) -> RelationName {
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(2)) {
        0 => RelationName::parse(tc.draw(generators::sampled_from(CLASSES.to_vec()))),
        1 => RelationName::Extended {
            author: tc.draw(generators::text().max_size(6)),
            name: tc.draw(generators::text().max_size(6)),
        },
        _ => RelationName::Other(draw_near(tc, &CLASSES)),
    }
}

/// Whether the documentation accepts a value: a class; an extension whose
/// author is non-empty without `_` and whose name is non-empty; or `Other`
/// whose text is a class or such an extension.
fn documented_conformant(name: &RelationName) -> bool {
    match name {
        RelationName::Extended { author, name } => {
            !author.is_empty() && !author.contains('_') && !name.is_empty()
        }
        RelationName::Other(text) => {
            CLASSES.contains(&text.as_str())
                || text.strip_prefix("x-").is_some_and(|rest| {
                    rest.split_once('_')
                        .is_some_and(|(author, name)| !author.is_empty() && !name.is_empty())
                })
        }
        _ => true,
    }
}

/// `is_conformant` follows the documented rule for any value.
#[hegel::test]
fn is_conformant_follows_the_documented_rule(tc: hegel::TestCase) {
    let name = draw_relation_name(&tc);
    assert_eq!(
        name.is_conformant(),
        documented_conformant(&name),
        "{name:?}"
    );
}

/// A conformant value other than `Other` reads back as itself from the
/// string it writes.
#[hegel::test]
fn a_conformant_relation_name_round_trips(tc: hegel::TestCase) {
    let name = draw_relation_name(&tc);
    tc.assume(name.is_conformant() && !matches!(name, RelationName::Other(_)));
    assert_eq!(RelationName::parse(&name.as_string()), name);
}

/// A conformant value writes a string that a reader also finds conformant,
/// so `add_relation` never stores a name `validate` would flag.
#[hegel::test]
fn a_conformant_name_reads_back_conformant(tc: hegel::TestCase) {
    let name = draw_relation_name(&tc);
    tc.assume(name.is_conformant());
    assert!(
        RelationName::parse(&name.as_string()).is_conformant(),
        "{name:?} writes {:?}",
        name.as_string()
    );
}
