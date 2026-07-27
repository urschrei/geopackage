//! The Related Tables Extension (OGC 18-000).

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geopackage::core::related::RelationName;
use geopackage::{ColumnSpec, Error, GeoPackage, GeometrySpec, NewRelation, TableSchemaBuilder};
use geopackage_core::types::{ColumnType, GeometryType};
use tempfile::TempDir;

fn gpkg() -> (TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("r.gpkg")).unwrap();
    gpkg.add_epsg_srs(4326).unwrap();
    gpkg.create_layer(
        &TableSchemaBuilder::new("sites")
            .column(ColumnSpec::new("name", ColumnType::Text(None)))
            .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
    )
    .unwrap();
    gpkg.create_attributes_table(
        &TableSchemaBuilder::new("notes").column(ColumnSpec::new("note", ColumnType::Text(None))),
    )
    .unwrap();
    (dir, gpkg)
}

fn simple(gpkg: &GeoPackage) -> geopackage::Relation {
    gpkg.add_relation(&NewRelation::new(
        "sites",
        "notes",
        RelationName::SimpleAttributes,
        "sites_notes",
    ))
    .unwrap();
    gpkg.relations().unwrap().pop().unwrap()
}

#[test]
fn a_file_without_the_extension_reads_as_empty() {
    let (_dir, gpkg) = gpkg();
    assert!(gpkg.relations().unwrap().is_empty());
    assert!(gpkg.relations_from("sites").unwrap().is_empty());
}

#[test]
fn creating_a_relation_writes_the_rows_gdal_writes() {
    let (_dir, gpkg) = gpkg();
    let id = gpkg
        .add_relation(&NewRelation::new(
            "sites",
            "notes",
            RelationName::SimpleAttributes,
            "sites_notes",
        ))
        .unwrap();
    assert_eq!(id, 1);

    let relation = &gpkg.relations().unwrap()[0];
    assert_eq!(relation.base_table_name, "sites");
    assert_eq!(relation.related_table_name, "notes");
    assert_eq!(relation.relation_name, RelationName::SimpleAttributes);
    assert_eq!(relation.mapping_table_name, "sites_notes");
    // The column default is `id`, per the table definition.
    assert_eq!(relation.base_primary_column, "id");
    assert_eq!(relation.related_primary_column, "id");

    // One row for the catalogue table and one for the mapping table, both
    // read-write, which is what GDAL writes and what the spec's tests look for.
    let rows: Vec<(Option<String>, String)> = gpkg
        .connection()
        .prepare(
            "SELECT table_name, scope FROM gpkg_extensions \
             WHERE extension_name = 'gpkg_related_tables' ORDER BY table_name",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            (
                Some("gpkgext_relations".to_owned()),
                "read-write".to_owned()
            ),
            (Some("sites_notes".to_owned()), "read-write".to_owned()),
        ]
    );

    // Table 3 gives both columns as non-null, which is what we write even
    // though GDAL leaves them nullable.
    let ddl: String = gpkg
        .connection()
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'sites_notes'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(ddl.contains("base_id INTEGER NOT NULL"), "{ddl}");
    assert!(ddl.contains("related_id INTEGER NOT NULL"), "{ddl}");
}

#[test]
fn a_mapping_walks_from_base_to_related() {
    let (_dir, gpkg) = gpkg();
    let relation = simple(&gpkg);

    gpkg.add_mapping(&relation, 1, 10).unwrap();
    gpkg.add_mapping(&relation, 1, 11).unwrap();
    gpkg.add_mapping(&relation, 2, 12).unwrap();

    assert_eq!(gpkg.related_ids(&relation, 1).unwrap(), vec![10, 11]);
    assert_eq!(gpkg.related_ids(&relation, 2).unwrap(), vec![12]);
    assert!(gpkg.related_ids(&relation, 3).unwrap().is_empty());
}

#[test]
fn a_duplicate_pair_is_kept_rather_than_deduplicated() {
    // The spec constrains neither cardinality nor uniqueness, so collapsing
    // duplicates would be inventing a rule it declined to make.
    let (_dir, gpkg) = gpkg();
    let relation = simple(&gpkg);
    gpkg.add_mapping(&relation, 1, 10).unwrap();
    gpkg.add_mapping(&relation, 1, 10).unwrap();
    assert_eq!(gpkg.related_ids(&relation, 1).unwrap(), vec![10, 10]);
}

#[test]
fn relations_from_finds_a_base_table_case_insensitively() {
    let (_dir, gpkg) = gpkg();
    simple(&gpkg);
    for spelling in ["sites", "Sites", "SITES"] {
        assert_eq!(
            gpkg.relations_from(spelling).unwrap().len(),
            1,
            "{spelling}"
        );
    }
    assert!(gpkg.relations_from("notes").unwrap().is_empty());
}

#[test]
fn every_defined_requirements_class_can_be_written() {
    let (_dir, gpkg) = gpkg();
    for (index, name) in [
        RelationName::Media,
        RelationName::SimpleAttributes,
        RelationName::Features,
        RelationName::Attributes,
        RelationName::Tiles,
        RelationName::Extended {
            author: "acme".to_owned(),
            name: "inspections".to_owned(),
        },
    ]
    .into_iter()
    .enumerate()
    {
        gpkg.add_relation(&NewRelation::new(
            "sites",
            "notes",
            name.clone(),
            format!("map_{index}"),
        ))
        .unwrap();
    }
    let written: Vec<RelationName> = gpkg
        .relations()
        .unwrap()
        .into_iter()
        .map(|relation| relation.relation_name)
        .collect();
    assert_eq!(written.len(), 6);
    assert!(written.iter().all(RelationName::is_conformant));
    assert_eq!(written[5].as_string(), "x-acme_inspections");
}

#[test]
fn a_relation_name_requirement_8_rejects_is_refused_on_write() {
    let (_dir, gpkg) = gpkg();
    let result = gpkg.add_relation(&NewRelation::new(
        "sites",
        "notes",
        RelationName::Other("photos".to_owned()),
        "map",
    ));
    match result {
        Err(Error::NonConformantRelationName { relation_name }) => {
            assert_eq!(relation_name, "photos");
        }
        other => panic!("expected NonConformantRelationName, got {other:?}"),
    }
    // Nothing was created on the way to the refusal.
    assert!(gpkg.relations().unwrap().is_empty());
}

#[test]
fn both_ends_must_be_in_gpkg_contents() {
    // Requirements 5 and 6.
    let (_dir, gpkg) = gpkg();
    for (base, related, missing) in [
        ("nowhere", "notes", "nowhere"),
        ("sites", "nowhere", "nowhere"),
    ] {
        match gpkg.add_relation(&NewRelation::new(base, related, RelationName::Media, "map")) {
            Err(Error::NoSuchTable { table_name }) => assert_eq!(table_name, missing),
            other => panic!("expected NoSuchTable, got {other:?}"),
        }
    }
}

#[test]
fn a_mapping_table_name_already_in_use_is_refused() {
    let (_dir, gpkg) = gpkg();
    match gpkg.add_relation(&NewRelation::new(
        "sites",
        "notes",
        RelationName::Media,
        "notes",
    )) {
        Err(Error::TableAlreadyExists { table_name }) => assert_eq!(table_name, "notes"),
        other => panic!("expected TableAlreadyExists, got {other:?}"),
    }
}

#[test]
fn a_non_default_primary_column_is_recorded() {
    // A GeoPackage feature table conventionally keys on `fid`, not the `id`
    // the column default assumes.
    let (_dir, gpkg) = gpkg();
    gpkg.add_relation(
        &NewRelation::new("sites", "notes", RelationName::Media, "sites_media")
            .base_primary_column("fid"),
    )
    .unwrap();
    let relation = &gpkg.relations().unwrap()[0];
    assert_eq!(relation.base_primary_column, "fid");
    assert_eq!(relation.related_primary_column, "id");
}

#[test]
fn an_unknown_relation_type_still_reads_and_walks() {
    // Reading must not depend on recognising the relation type: a file written
    // by someone else's extension is still a base table, a related table and a
    // mapping table.
    let (_dir, gpkg) = gpkg();
    let relation = simple(&gpkg);
    gpkg.connection()
        .execute(
            "UPDATE gpkgext_relations SET relation_name = 'photos' WHERE id = ?1",
            [relation.id],
        )
        .unwrap();

    let read = &gpkg.relations().unwrap()[0];
    assert_eq!(read.relation_name, RelationName::Other("photos".to_owned()));
    assert!(!read.relation_name.is_conformant());

    gpkg.add_mapping(read, 1, 42).unwrap();
    assert_eq!(gpkg.related_ids(read, 1).unwrap(), vec![42]);
}
