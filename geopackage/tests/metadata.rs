//! The `gpkg_metadata` extension (Annex F.8): records, the references that
//! attach them, and the parent graph.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geopackage::core::datetime::DateTime;
use geopackage::core::metadata::{MetadataScope, ReferenceScope};
use geopackage::{
    ColumnSpec, Error, GeoPackage, GeometrySpec, MetadataTarget, NewMetadata, TableSchemaBuilder,
};
use geopackage_core::types::{ColumnType, GeometryType};
use tempfile::TempDir;

fn stamp() -> DateTime {
    DateTime::parse_strict("2020-01-01T00:00:00.000Z").unwrap()
}

fn gpkg() -> (TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("m.gpkg")).unwrap();
    gpkg.add_epsg_srs(4326).unwrap();
    gpkg.create_layer(
        &TableSchemaBuilder::new("roads")
            .column(ColumnSpec::new("name", ColumnType::Text(None)))
            .geometry(GeometrySpec::new(GeometryType::LineString, 4326)),
    )
    .unwrap();
    (dir, gpkg)
}

fn iso(gpkg: &GeoPackage, body: &str) -> i64 {
    gpkg.add_metadata(&NewMetadata::new(
        MetadataScope::Dataset,
        "http://www.isotc211.org/2005/gmd",
        body,
    ))
    .unwrap()
}

#[test]
fn a_file_without_the_extension_reads_as_empty_rather_than_failing() {
    let (_dir, gpkg) = gpkg();
    assert!(gpkg.metadata().unwrap().is_empty());
    assert!(gpkg.metadata_references().unwrap().is_empty());
    assert_eq!(gpkg.metadata_record(1).unwrap(), None);
}

#[test]
fn adding_a_record_creates_the_tables_and_registers_both() {
    let (_dir, gpkg) = gpkg();
    let id = iso(&gpkg, "<gmd:MD_Metadata/>");
    assert_eq!(id, 1);

    let rows: Vec<(String, Option<String>, String)> = gpkg
        .connection()
        .prepare(
            "SELECT extension_name, table_name, scope FROM gpkg_extensions \
             WHERE extension_name = 'gpkg_metadata' ORDER BY table_name",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();

    // One row per table, both read-write, as GDAL writes and as Annex F.8's
    // test expects to find.
    assert_eq!(
        rows,
        vec![
            (
                "gpkg_metadata".to_owned(),
                Some("gpkg_metadata".to_owned()),
                "read-write".to_owned()
            ),
            (
                "gpkg_metadata".to_owned(),
                Some("gpkg_metadata_reference".to_owned()),
                "read-write".to_owned()
            ),
        ]
    );
}

#[test]
fn a_record_round_trips_including_its_payload() {
    let (_dir, gpkg) = gpkg();
    let body = "<gmd:MD_Metadata><gmd:fileIdentifier>x</gmd:fileIdentifier></gmd:MD_Metadata>";
    let id = gpkg
        .add_metadata(
            &NewMetadata::new(MetadataScope::Series, "http://example.invalid/std", body)
                .mime_type("application/xml"),
        )
        .unwrap();

    let record = gpkg.metadata_record(id).unwrap().expect("the record");
    assert_eq!(record.scope, MetadataScope::Series);
    assert_eq!(record.standard_uri, "http://example.invalid/std");
    assert_eq!(record.mime_type, "application/xml");
    // Stored as written: no parse, no reformat.
    assert_eq!(record.metadata, body);
}

#[test]
fn an_unlisted_scope_survives_a_round_trip() {
    // Requirement 94 permits scopes outside Table 15, so one must not be
    // rejected on write or lost on read.
    let (_dir, gpkg) = gpkg();
    let scope = MetadataScope::Other("x-acme_survey".to_owned());
    let id = gpkg
        .add_metadata(&NewMetadata::new(
            scope.clone(),
            "http://example.invalid/std",
            "{}",
        ))
        .unwrap();
    assert_eq!(gpkg.metadata_record(id).unwrap().unwrap().scope, scope);
}

#[test]
fn every_reference_scope_round_trips_through_its_target() {
    let (_dir, gpkg) = gpkg();
    let id = iso(&gpkg, "<gmd:MD_Metadata/>");

    let targets = [
        MetadataTarget::GeoPackage,
        MetadataTarget::Table {
            table_name: "roads".to_owned(),
        },
        MetadataTarget::Column {
            table_name: "roads".to_owned(),
            column_name: "name".to_owned(),
        },
        MetadataTarget::Row {
            table_name: "roads".to_owned(),
            row_id: 7,
        },
        MetadataTarget::Cell {
            table_name: "roads".to_owned(),
            column_name: "name".to_owned(),
            row_id: 7,
        },
    ];
    for target in &targets {
        gpkg.add_metadata_reference(id, target, stamp(), None)
            .unwrap();
    }

    for target in &targets {
        let found = gpkg.metadata_for(target).unwrap();
        assert_eq!(found.len(), 1, "{target:?}");
        assert_eq!(found[0].scope, target.scope());
        assert_eq!(found[0].md_file_id, id);
    }

    // The stored NULL pattern is Requirements 97 to 99, not our choice.
    type StoredReference = (String, Option<String>, Option<String>, Option<i64>);
    let stored: Vec<StoredReference> = gpkg
        .connection()
        .prepare(
            "SELECT reference_scope, table_name, column_name, row_id_value \
             FROM gpkg_metadata_reference ORDER BY reference_scope",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        stored,
        vec![
            (
                "column".to_owned(),
                Some("roads".to_owned()),
                Some("name".to_owned()),
                None
            ),
            ("geopackage".to_owned(), None, None, None),
            ("row".to_owned(), Some("roads".to_owned()), None, Some(7)),
            (
                "row/col".to_owned(),
                Some("roads".to_owned()),
                Some("name".to_owned()),
                Some(7)
            ),
            ("table".to_owned(), Some("roads".to_owned()), None, None),
        ]
    );
}

#[test]
fn the_timestamp_is_written_in_the_spec_datetime_form() {
    let (_dir, gpkg) = gpkg();
    let id = iso(&gpkg, "<gmd:MD_Metadata/>");
    gpkg.add_metadata_reference(id, &MetadataTarget::GeoPackage, stamp(), None)
        .unwrap();

    let stored: String = gpkg
        .connection()
        .query_row("SELECT timestamp FROM gpkg_metadata_reference", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(stored, "2020-01-01T00:00:00.000Z");
    // Requirement 100 puts it in the same form as every other DATETIME.
    assert_eq!(DateTime::parse_strict(&stored).unwrap(), stamp());
}

#[test]
fn a_reference_to_an_absent_record_is_refused() {
    let (_dir, gpkg) = gpkg();
    match gpkg.add_metadata_reference(99, &MetadataTarget::GeoPackage, stamp(), None) {
        Err(Error::NoSuchMetadata { id }) => assert_eq!(id, 99),
        other => panic!("expected NoSuchMetadata, got {other:?}"),
    }
}

#[test]
fn a_reference_to_a_table_outside_gpkg_contents_is_refused() {
    // Requirement 97: every scope but `geopackage` names a gpkg_contents table.
    let (_dir, gpkg) = gpkg();
    let id = iso(&gpkg, "<gmd:MD_Metadata/>");
    let target = MetadataTarget::Table {
        table_name: "nowhere".to_owned(),
    };
    match gpkg.add_metadata_reference(id, &target, stamp(), None) {
        Err(Error::NoSuchTable { table_name }) => assert_eq!(table_name, "nowhere"),
        other => panic!("expected NoSuchTable, got {other:?}"),
    }
}

#[test]
fn a_record_cannot_be_its_own_parent() {
    // Requirement 102.
    let (_dir, gpkg) = gpkg();
    let id = iso(&gpkg, "<gmd:MD_Metadata/>");
    match gpkg.add_metadata_reference(id, &MetadataTarget::GeoPackage, stamp(), Some(id)) {
        Err(Error::SelfParentedMetadata { md_file_id }) => assert_eq!(md_file_id, id),
        other => panic!("expected SelfParentedMetadata, got {other:?}"),
    }
}

#[test]
fn ancestors_walk_the_parent_chain_nearest_first() {
    let (_dir, gpkg) = gpkg();
    let grandparent = iso(&gpkg, "<gmd:MD_Metadata>gp</gmd:MD_Metadata>");
    let parent = iso(&gpkg, "<gmd:MD_Metadata>p</gmd:MD_Metadata>");
    let child = iso(&gpkg, "<gmd:MD_Metadata>c</gmd:MD_Metadata>");

    gpkg.add_metadata_reference(
        parent,
        &MetadataTarget::Table {
            table_name: "roads".to_owned(),
        },
        stamp(),
        Some(grandparent),
    )
    .unwrap();
    gpkg.add_metadata_reference(child, &MetadataTarget::GeoPackage, stamp(), Some(parent))
        .unwrap();

    let ids: Vec<i64> = gpkg
        .metadata_ancestors(child)
        .unwrap()
        .into_iter()
        .map(|record| record.id)
        .collect();
    assert_eq!(ids, vec![parent, grandparent]);

    // The root has none, and asking about an absent record is an error.
    assert!(gpkg.metadata_ancestors(grandparent).unwrap().is_empty());
    assert!(matches!(
        gpkg.metadata_ancestors(404),
        Err(Error::NoSuchMetadata { id: 404 })
    ));
}

#[test]
fn a_parent_cycle_is_reported_rather_than_looped_on() {
    // Requirement 102 forbids only the one-step cycle, so a two-step one is a
    // file that has to be survived. Written through raw SQL because the API
    // refuses the self-parent case that would be the one-step version.
    let (_dir, gpkg) = gpkg();
    let first = iso(&gpkg, "<gmd:MD_Metadata>1</gmd:MD_Metadata>");
    let second = iso(&gpkg, "<gmd:MD_Metadata>2</gmd:MD_Metadata>");
    gpkg.add_metadata_reference(first, &MetadataTarget::GeoPackage, stamp(), Some(second))
        .unwrap();
    gpkg.connection()
        .execute(
            "INSERT INTO gpkg_metadata_reference \
             (reference_scope, timestamp, md_file_id, md_parent_id) \
             VALUES ('geopackage', '2020-01-01T00:00:00.000Z', ?1, ?2)",
            [second, first],
        )
        .unwrap();

    assert!(matches!(
        gpkg.metadata_ancestors(first),
        Err(Error::MetadataCycle { .. })
    ));
}

#[test]
fn the_gdal_written_fixture_reads() {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("gdal_multilayer_1_4.gpkg");
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("gdal_multilayer_1_4.gpkg");
    std::fs::copy(src, &dst).unwrap();
    let gpkg = GeoPackage::open(&dst).unwrap();

    let records = gpkg.metadata().unwrap();
    assert!(!records.is_empty(), "the fixture contains metadata");
    assert!(records.iter().all(|r| r.scope == MetadataScope::Dataset));
    assert!(records.iter().all(|r| r.mime_type == "text/xml"));

    let references = gpkg.metadata_references().unwrap();
    assert!(!references.is_empty());
    // GDAL attaches one per layer at table scope.
    assert!(
        references
            .iter()
            .all(|r| r.scope == ReferenceScope::Table && r.table_name.is_some())
    );

    // And the targets resolve back.
    let lines = gpkg
        .metadata_for(&MetadataTarget::Table {
            table_name: "lines".to_owned(),
        })
        .unwrap();
    assert_eq!(lines.len(), 1);
}
