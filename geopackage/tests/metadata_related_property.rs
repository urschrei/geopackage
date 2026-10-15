//! Property tests over the metadata extension and the related tables
//! extension, against models of what a sequence of writes should leave.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::path::Path;

use geo_types::Point;
use geopackage::core::datetime::DateTime;
use geopackage::core::metadata::{MetadataRecord, MetadataScope, ReferenceScope};
use geopackage::core::related::RelationName;
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{
    ColumnSpec, Error, GeoPackage, GeometrySpec, MetadataTarget, NewFeature, NewMetadata,
    NewRelation, TableSchemaBuilder, Value,
};
use hegel::generators;

/// A GeoPackage with a feature layer `pts(fid, name, geom)` of three rows and
/// an attribute table `notes(id, body)` of two.
fn with_tables() -> GeoPackage {
    let gpkg = GeoPackage::create(Path::new(":memory:")).unwrap();
    let pts = gpkg
        .create_layer(
            &TableSchemaBuilder::new("pts")
                .column(ColumnSpec::new("name", ColumnType::Text(None)))
                .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
        )
        .unwrap();
    pts.write_all(
        (1..=3).map(|i| NewFeature::new(Point::new(0.0, 0.0), vec![Value::Text(format!("p{i}"))])),
        0,
    )
    .unwrap();
    gpkg.create_attributes_table(
        &TableSchemaBuilder::new("notes")
            .primary_key("id")
            .column(ColumnSpec::new("body", ColumnType::Text(None))),
    )
    .unwrap();
    gpkg.connection()
        .execute_batch("INSERT INTO notes (id, body) VALUES (1, 'a'), (2, 'b')")
        .unwrap();
    gpkg
}

/// The metadata scopes of spec Table 15, as stored.
const LISTED: [&str; 6] = ["undefined", "dataset", "feature", "tile", "style", "series"];

/// Draws a metadata document with any scope string and any text in each
/// field, NUL included.
fn draw_metadata(tc: &hegel::TestCase) -> NewMetadata {
    let scope = if tc.draw(generators::booleans()) {
        MetadataScope::parse(tc.draw(generators::sampled_from(LISTED.to_vec())))
    } else {
        MetadataScope::parse(&tc.draw(generators::text().max_size(10)))
    };
    let text = |tc: &hegel::TestCase| tc.draw(generators::text().max_size(20));
    NewMetadata::new(scope, text(tc), text(tc)).mime_type(text(tc))
}

/// Every document added reads back exactly, by `metadata_record` and in
/// `metadata`, in `id` order.
#[hegel::test]
fn metadata_documents_read_back_as_written(tc: hegel::TestCase) {
    let gpkg = with_tables();
    let count = tc.draw(generators::integers::<usize>().min_value(0).max_value(6));
    let mut expected = Vec::new();
    for _ in 0..count {
        let document = draw_metadata(&tc);
        let id = gpkg.add_metadata(&document).unwrap();
        let record = MetadataRecord {
            id,
            scope: document.scope.clone(),
            standard_uri: document.standard_uri.clone(),
            mime_type: document.mime_type.clone(),
            metadata: document.metadata.clone(),
        };
        assert_eq!(gpkg.metadata_record(id).unwrap(), Some(record.clone()));
        expected.push(record);
    }
    assert_eq!(gpkg.metadata().unwrap(), expected);
}

/// Draws a target: the whole GeoPackage, or a table, column, row or cell of
/// one of the file's tables or of a table it does not have, with columns and
/// rows that exist or not.
fn draw_target(tc: &hegel::TestCase) -> MetadataTarget {
    let table = tc
        .draw(generators::sampled_from(vec!["pts", "notes", "absent"]))
        .to_owned();
    let column = tc
        .draw(generators::sampled_from(vec![
            "name", "body", "fid", "nope",
        ]))
        .to_owned();
    let row_id = tc.draw(generators::integers::<i64>().min_value(-1).max_value(4));
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(4)) {
        0 => MetadataTarget::GeoPackage,
        1 => MetadataTarget::Table { table_name: table },
        2 => MetadataTarget::Column {
            table_name: table,
            column_name: column,
        },
        3 => MetadataTarget::Row {
            table_name: table,
            row_id,
        },
        _ => MetadataTarget::Cell {
            table_name: table,
            column_name: column,
            row_id,
        },
    }
}

/// Draws any valid `DATETIME`: any instant in years 1 to 9998, any precision,
/// and an offset or none.
fn draw_timestamp(tc: &hegel::TestCase) -> DateTime {
    let micros = tc.draw(
        generators::integers::<i64>()
            .min_value(-62_135_596_800_000_000)
            .max_value(253_370_764_800_000_000),
    );
    let mut dt = DateTime::from_micros_since_epoch(micros).unwrap();
    dt.nanosecond += tc.draw(generators::integers::<u32>().min_value(0).max_value(999));
    dt.offset_minutes = match tc.draw(generators::integers::<u8>().min_value(0).max_value(2)) {
        0 => None,
        1 => Some(0),
        _ => Some(
            tc.draw(
                generators::integers::<i16>()
                    .min_value(-1439)
                    .max_value(1439),
            ),
        ),
    };
    dt
}

/// A reference as the model keeps it: the target's four stored columns, the
/// timestamp text and the two ids.
type ModelReference = (
    ReferenceScope,
    Option<String>,
    Option<String>,
    Option<i64>,
    String,
    i64,
    Option<i64>,
);

fn columns_of(target: &MetadataTarget) -> (Option<String>, Option<String>, Option<i64>) {
    match target {
        MetadataTarget::GeoPackage => (None, None, None),
        MetadataTarget::Table { table_name } => (Some(table_name.clone()), None, None),
        MetadataTarget::Column {
            table_name,
            column_name,
        } => (Some(table_name.clone()), Some(column_name.clone()), None),
        MetadataTarget::Row { table_name, row_id } => {
            (Some(table_name.clone()), None, Some(*row_id))
        }
        MetadataTarget::Cell {
            table_name,
            column_name,
            row_id,
        } => (
            Some(table_name.clone()),
            Some(column_name.clone()),
            Some(*row_id),
        ),
    }
}

/// `add_metadata_reference` has the documented outcome for any target, record
/// and parent; the references it accepts read back with their timestamp in
/// the strict DATETIME form; `metadata_for` returns exactly those of its
/// target; and the file validates clean.
#[hegel::test]
fn metadata_references_match_a_model(tc: hegel::TestCase) {
    let gpkg = with_tables();
    let records: Vec<i64> = (0..tc.draw(generators::integers::<usize>().min_value(1).max_value(4)))
        .map(|_| gpkg.add_metadata(&draw_metadata(&tc)).unwrap())
        .collect();
    let draw_id = |tc: &hegel::TestCase| {
        if tc.draw(generators::weighted_booleans(0.9)) {
            tc.draw(generators::sampled_from(records.clone()))
        } else {
            tc.draw(generators::integers::<i64>().min_value(-1).max_value(20))
        }
    };

    let mut model: Vec<ModelReference> = Vec::new();
    let steps = tc.draw(generators::integers::<usize>().min_value(0).max_value(8));
    for _ in 0..steps {
        let target = draw_target(&tc);
        let md_file_id = draw_id(&tc);
        let parent = if tc.draw(generators::booleans()) {
            None
        } else if tc.draw(generators::weighted_booleans(0.2)) {
            Some(md_file_id)
        } else {
            Some(draw_id(&tc))
        };
        let timestamp = draw_timestamp(&tc);
        let result = gpkg.add_metadata_reference(md_file_id, &target, timestamp, parent);

        let missing = std::iter::once(md_file_id)
            .chain(parent)
            .find(|id| !records.contains(id));
        let table_missing = target.table_name().is_some_and(|t| t == "absent");
        let (table_name, column_name, row_id) = columns_of(&target);
        let columns: &[&str] = match table_name.as_deref() {
            Some("pts") => &["fid", "name", "geom"],
            Some("notes") => &["id", "body"],
            _ => &[],
        };
        let rows = match table_name.as_deref() {
            Some("pts") => 1..=3,
            _ => 1..=2,
        };
        let column_missing = column_name
            .as_deref()
            .is_some_and(|c| !columns.contains(&c));
        let row_missing = row_id.is_some_and(|r| !rows.contains(&r));
        if parent == Some(md_file_id) {
            tc.event("SelfParentedMetadata");
            assert!(
                matches!(result, Err(Error::SelfParentedMetadata { .. })),
                "{result:?}"
            );
        } else if let Some(id) = missing {
            tc.event("NoSuchMetadata");
            assert!(
                matches!(result, Err(Error::NoSuchMetadata { id: got }) if got == id),
                "{result:?}"
            );
        } else if table_missing {
            tc.event("NoSuchTable");
            assert!(
                matches!(result, Err(Error::NoSuchTable { .. })),
                "{result:?}"
            );
        } else if column_missing {
            tc.event("NoSuchColumn");
            assert!(
                matches!(result, Err(Error::NoSuchColumn { .. })),
                "{result:?}"
            );
        } else if row_missing {
            tc.event("NoSuchRow");
            assert!(matches!(result, Err(Error::NoSuchRow { .. })), "{result:?}");
        } else {
            tc.event("reference written");
            result.unwrap();
            model.push((
                target.scope(),
                table_name,
                column_name,
                row_id,
                timestamp.format_strict().unwrap(),
                md_file_id,
                parent,
            ));
        }
    }

    let stored: Vec<ModelReference> = gpkg
        .metadata_references()
        .unwrap()
        .into_iter()
        .map(|r| {
            (
                r.scope,
                r.table_name,
                r.column_name,
                r.row_id_value,
                r.timestamp,
                r.md_file_id,
                r.md_parent_id,
            )
        })
        .collect();
    assert_eq!(stored, model);

    let target = draw_target(&tc);
    let (table_name, column_name, row_id) = columns_of(&target);
    let for_target: Vec<&ModelReference> = model
        .iter()
        .filter(|r| {
            r.0 == target.scope() && r.1 == table_name && r.2 == column_name && r.3 == row_id
        })
        .collect();
    assert_eq!(
        gpkg.metadata_for(&target).unwrap().len(),
        for_target.len(),
        "{target:?}"
    );
    assert_eq!(gpkg.validate().unwrap(), Vec::new());
}

/// `metadata_ancestors` walks the parent references from a record, nearest
/// first, and reports a cycle when the walk returns to a record it has seen.
#[hegel::test]
fn metadata_ancestors_follow_the_parent_chain(tc: hegel::TestCase) {
    let gpkg = with_tables();
    let count = tc.draw(generators::integers::<usize>().min_value(1).max_value(6));
    let ids: Vec<i64> = (0..count)
        .map(|_| gpkg.add_metadata(&draw_metadata(&tc)).unwrap())
        .collect();
    // Each record gets at most one parent other than itself.
    let mut parent_of = std::collections::HashMap::new();
    for &id in &ids {
        if tc.draw(generators::booleans()) {
            let parent = tc.draw(generators::sampled_from(ids.clone()));
            if parent != id {
                gpkg.add_metadata_reference(
                    id,
                    &MetadataTarget::GeoPackage,
                    DateTime::from_micros_since_epoch(0).unwrap(),
                    Some(parent),
                )
                .unwrap();
                parent_of.insert(id, parent);
            }
        }
    }
    let start = tc.draw(generators::sampled_from(ids.clone()));
    let mut seen = vec![start];
    let mut chain = Vec::new();
    let mut current = start;
    let mut cycle = None;
    while let Some(&parent) = parent_of.get(&current) {
        if seen.contains(&parent) {
            cycle = Some(parent);
            break;
        }
        seen.push(parent);
        chain.push(parent);
        current = parent;
    }
    let result = gpkg.metadata_ancestors(start);
    match cycle {
        Some(id) => assert!(
            matches!(result, Err(Error::MetadataCycle { id: got }) if got == id),
            "{result:?}"
        ),
        None => assert_eq!(
            result.unwrap().iter().map(|r| r.id).collect::<Vec<_>>(),
            chain
        ),
    }
}

/// Draws a relation name a caller may pass: a class, an extension, or
/// anything.
fn draw_relation_name(tc: &hegel::TestCase) -> RelationName {
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(2)) {
        0 => RelationName::parse(tc.draw(generators::sampled_from(vec![
            "media",
            "simple_attributes",
            "features",
            "attributes",
            "tiles",
        ]))),
        1 => RelationName::Extended {
            author: tc.draw(generators::text().max_size(4)),
            name: tc.draw(generators::text().max_size(4)),
        },
        _ => RelationName::parse(&tc.draw(generators::text().max_size(6))),
    }
}

/// `add_relation` has the documented outcome for any tables, name and mapping
/// table; `relations` and `relations_from` list exactly the relationships it
/// accepted; mappings read back through `related_ids`; and the file validates
/// clean.
#[hegel::test]
fn relations_match_a_model(tc: hegel::TestCase) {
    let gpkg = with_tables();
    let mut model = Vec::new();
    let steps = tc.draw(generators::integers::<usize>().min_value(0).max_value(5));
    for step in 0..steps {
        let table = |tc: &hegel::TestCase| -> String {
            tc.draw(generators::sampled_from(vec!["pts", "notes", "absent"]))
                .to_owned()
        };
        let (base, related) = (table(&tc), table(&tc));
        let mapping = if tc.draw(generators::weighted_booleans(0.3)) {
            // A table the file already has, or a name the checks reject.
            tc.draw(generators::sampled_from(vec![
                "pts", "notes", "map_0", "", "sqlite_m", "GPKG_m",
            ]))
            .to_owned()
        } else {
            format!("map_{step}")
        };
        let name = draw_relation_name(&tc);
        let relation = NewRelation::new(&base, &related, name.clone(), &mapping);
        let exists = gpkg
            .connection()
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = ?1 COLLATE NOCASE",
                [&mapping],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            > 0;
        let result = gpkg.add_relation(&relation);
        if !name.is_conformant() {
            tc.event("NonConformantRelationName");
            assert!(
                matches!(result, Err(Error::NonConformantRelationName { .. })),
                "{result:?}"
            );
        } else if mapping.is_empty() {
            tc.event("InvalidIdentifier");
            assert!(matches!(result, Err(Error::Core(_))), "{result:?}");
        } else if mapping == "sqlite_m" || mapping == "GPKG_m" {
            tc.event("ReservedTablePrefix");
            assert!(
                matches!(result, Err(Error::ReservedTablePrefix { .. })),
                "{result:?}"
            );
        } else if base == "absent" || related == "absent" {
            tc.event("NoSuchTable");
            assert!(
                matches!(result, Err(Error::NoSuchTable { .. })),
                "{result:?}"
            );
        } else if exists {
            tc.event("TableAlreadyExists");
            assert!(
                matches!(result, Err(Error::TableAlreadyExists { .. })),
                "{result:?}"
            );
        } else {
            tc.event("relation written");
            let id = result.unwrap();
            // A reader sees the name as its written string parses.
            let read_back = RelationName::parse(&name.as_string());
            model.push((id, base, related, read_back, mapping));
        }
    }

    let relations = gpkg.relations().unwrap();
    let listed: Vec<_> = relations
        .iter()
        .map(|r| {
            (
                r.id,
                r.base_table_name.clone(),
                r.related_table_name.clone(),
                r.relation_name.clone(),
                r.mapping_table_name.clone(),
            )
        })
        .collect();
    assert_eq!(listed, model);
    for base in ["pts", "notes"] {
        let from: Vec<i64> = gpkg
            .relations_from(base)
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect();
        let expected: Vec<i64> = model.iter().filter(|m| m.1 == base).map(|m| m.0).collect();
        assert_eq!(from, expected);
    }

    // Mappings: drawn pairs, read back by base id as the multiset added.
    for relation in &relations {
        let mut pairs: Vec<(i64, i64)> = Vec::new();
        for _ in 0..tc.draw(generators::integers::<usize>().min_value(0).max_value(6)) {
            let pair = (
                tc.draw(generators::integers::<i64>().min_value(1).max_value(3)),
                tc.draw(generators::integers::<i64>()),
            );
            gpkg.add_mapping(relation, pair.0, pair.1).unwrap();
            pairs.push(pair);
        }
        for base_id in 1..=3 {
            let mut got = gpkg.related_ids(relation, base_id).unwrap();
            let mut want: Vec<i64> = pairs
                .iter()
                .filter(|p| p.0 == base_id)
                .map(|p| p.1)
                .collect();
            got.sort_unstable();
            want.sort_unstable();
            assert_eq!(got, want);
        }
    }
    assert_eq!(gpkg.validate().unwrap(), Vec::new());
}
