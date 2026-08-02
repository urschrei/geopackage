//! `GeoPackage::validate`: the checks, their severities, and what every
//! committed fixture reports.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::path::{Path, PathBuf};

use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{ColumnSpec, Finding, GeoPackage, GeometrySpec, Severity, TableSchemaBuilder};
use tempfile::TempDir;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn open_fixture(name: &str) -> (TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join(name);
    std::fs::copy(fixtures_dir().join(name), &dst).unwrap();
    let gpkg = GeoPackage::open_lenient(&dst).unwrap();
    (dir, gpkg)
}

fn gpkg() -> (TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("v.gpkg")).unwrap();
    gpkg.add_epsg_srs(4326).unwrap();
    (dir, gpkg)
}

fn with_layer() -> (TempDir, GeoPackage) {
    let (dir, gpkg) = gpkg();
    gpkg.create_layer(
        &TableSchemaBuilder::new("roads")
            .column(ColumnSpec::new("name", ColumnType::Text(None)))
            .geometry(GeometrySpec::new(GeometryType::LineString, 4326)),
    )
    .unwrap();
    (dir, gpkg)
}

#[test]
fn a_file_this_crate_just_wrote_has_nothing_to_report() {
    let (_dir, gpkg) = with_layer();
    assert_eq!(gpkg.validate().unwrap(), Vec::new());
}

#[test]
fn an_unindexed_layer_is_an_advisory_rather_than_a_defect() {
    let (_dir, gpkg) = gpkg();
    gpkg.create_layer(
        &TableSchemaBuilder::new("roads")
            .geometry(GeometrySpec::new(GeometryType::LineString, 4326))
            .spatial_index(false),
    )
    .unwrap();

    let findings = gpkg.validate().unwrap();
    assert_eq!(
        findings,
        vec![Finding::NoSpatialIndex {
            table_name: "roads".to_owned()
        }]
    );
    assert_eq!(findings[0].severity(), Severity::Advisory);
    assert!(findings[0].repair().is_some());
}

#[test]
fn a_contents_row_with_no_table_is_an_error_that_names_a_repair() {
    let (_dir, gpkg) = with_layer();
    gpkg.connection()
        .execute(
            "INSERT INTO gpkg_contents (table_name, data_type, identifier) \
             VALUES ('ghost', 'attributes', 'ghost')",
            [],
        )
        .unwrap();

    let findings = gpkg.validate().unwrap();
    let finding = findings
        .iter()
        .find(|f| matches!(f, Finding::MissingContentsTable { .. }))
        .expect("the dangling row");
    assert_eq!(finding.severity(), Severity::Error);
    assert_eq!(finding.table_name(), Some("ghost"));
    assert!(finding.repair().is_some());
}

#[test]
fn an_out_of_step_spatial_index_is_an_error() {
    let (_dir, gpkg) = with_layer();
    let layer = gpkg.layer("roads").unwrap();
    let mut writer = layer.writer().unwrap();
    writer
        .insert(
            None,
            &geo_types::Line::new(
                geo_types::Coord { x: 0.0, y: 0.0 },
                geo_types::Coord { x: 1.0, y: 1.0 },
            ),
            &[geopackage::ValueRef::Text("a")],
        )
        .unwrap();
    writer.commit().unwrap();
    assert_eq!(gpkg.validate().unwrap(), Vec::new());

    // Empty the index behind the triggers' back, as a tool writing rows
    // without maintaining it would.
    gpkg.connection()
        .execute("DELETE FROM rtree_roads_geom", [])
        .unwrap();

    let findings = gpkg.validate().unwrap();
    let finding = findings
        .iter()
        .find(|f| matches!(f, Finding::SpatialIndexOutOfStep { .. }))
        .expect("the emptied index");
    assert_eq!(finding.severity(), Severity::Error);
    assert_eq!(finding.table_name(), Some("roads"));
    assert!(finding.repair().unwrap().contains("rebuild_spatial_index"));
}

#[test]
fn a_removed_extension_is_reported_as_a_warning() {
    let (_dir, gpkg) = with_layer();
    gpkg.connection()
        .execute(
            "INSERT INTO gpkg_extensions \
             (table_name, column_name, extension_name, definition, scope) \
             VALUES ('roads', 'geom', 'gpkg_srs_id_trigger', 'x', 'read-write')",
            [],
        )
        .unwrap();

    let findings = gpkg.validate().unwrap();
    let finding = findings
        .iter()
        .find(|f| matches!(f, Finding::RemovedExtension { .. }))
        .expect("the 2016 removal");
    assert_eq!(finding.severity(), Severity::Warning);
    // Nothing this crate can do: the row belongs to whoever wrote it.
    assert_eq!(finding.repair(), None);
}

#[test]
fn an_unrecognised_extension_is_reported_rather_than_ignored() {
    let (_dir, gpkg) = with_layer();
    gpkg.connection()
        .execute(
            "INSERT INTO gpkg_extensions \
             (table_name, column_name, extension_name, definition, scope) \
             VALUES ('roads', 'geom', 'acme_secret_sauce', 'x', 'read-write')",
            [],
        )
        .unwrap();

    assert!(
        gpkg.validate()
            .unwrap()
            .iter()
            .any(|f| matches!(f, Finding::UnrecognisedExtension { .. }))
    );
}

#[test]
fn a_dangling_metadata_reference_is_an_error() {
    use geopackage::core::metadata::MetadataScope;
    use geopackage::{MetadataTarget, NewMetadata};

    let (_dir, gpkg) = with_layer();
    let id = gpkg
        .add_metadata(&NewMetadata::new(
            MetadataScope::Dataset,
            "http://example.invalid/std",
            "{}",
        ))
        .unwrap();
    gpkg.add_metadata_reference(
        id,
        &MetadataTarget::GeoPackage,
        geopackage::core::datetime::DateTime::parse_strict("2020-01-01T00:00:00.000Z").unwrap(),
        None,
    )
    .unwrap();
    assert_eq!(gpkg.validate().unwrap(), Vec::new());

    // Point it at a record that is not there. The DDL's foreign key forbids
    // this, so it takes turning enforcement off, which is how such a file
    // arrives in practice: SQLite defaults foreign keys off, and the writers
    // that produce GeoPackages do not turn them on.
    gpkg.connection()
        .execute_batch(
            "PRAGMA foreign_keys = OFF; \
             UPDATE gpkg_metadata_reference SET md_file_id = 404; \
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();
    let findings = gpkg.validate().unwrap();
    assert!(findings.contains(&Finding::DanglingMetadataReference { md_id: 404 }));
    assert_eq!(findings[0].severity(), Severity::Error);
}

#[test]
fn a_relation_whose_mapping_table_is_gone_is_an_error() {
    use geopackage::NewRelation;
    use geopackage::core::related::RelationName;

    let (_dir, gpkg) = with_layer();
    gpkg.create_attributes_table(
        &TableSchemaBuilder::new("notes").column(ColumnSpec::new("note", ColumnType::Text(None))),
    )
    .unwrap();
    gpkg.add_relation(&NewRelation::new(
        "roads",
        "notes",
        RelationName::Media,
        "roads_notes",
    ))
    .unwrap();
    assert_eq!(gpkg.validate().unwrap(), Vec::new());

    gpkg.connection()
        .execute("DROP TABLE roads_notes", [])
        .unwrap();
    let findings = gpkg.validate().unwrap();
    assert!(findings.contains(&Finding::MissingMappingTable {
        mapping_table_name: "roads_notes".to_owned()
    }));
}

#[test]
fn findings_come_back_most_severe_first() {
    let (_dir, gpkg) = gpkg();
    // An advisory (unindexed layer) and an error (dangling contents row).
    gpkg.create_layer(
        &TableSchemaBuilder::new("roads")
            .geometry(GeometrySpec::new(GeometryType::LineString, 4326))
            .spatial_index(false),
    )
    .unwrap();
    gpkg.connection()
        .execute(
            "INSERT INTO gpkg_contents (table_name, data_type, identifier) \
             VALUES ('ghost', 'attributes', 'ghost')",
            [],
        )
        .unwrap();

    let findings = gpkg.validate().unwrap();
    assert!(findings.len() >= 2);
    let severities: Vec<Severity> = findings.iter().map(Finding::severity).collect();
    let mut sorted = severities.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(severities, sorted, "{findings:?}");
}

// --- the corpus ---------------------------------------------------------------
//
// Every committed fixture, with its findings pinned. A change in what this
// crate detects shows up here as a diff rather than passing quietly: known
// findings on known files are the expected output.

#[test]
fn every_committed_fixture_reports_what_it_is_expected_to() {
    let expected: &[(&str, &[&str])] = &[
        ("attributes_spread.gpkg", &[]),
        // A GDAL file whose catalogue name differs from the physical table
        // only in case, which is what the fixture exists to carry.
        ("case_mismatch.gpkg", &["TableNameCaseMismatch"]),
        // Written without indexes: the fixture is about relations.
        ("gdal_related.gpkg", &["NoSpatialIndex"]),
        ("gdal_curves.gpkg", &[]),
        (
            "gdal_multilayer_1_4.gpkg",
            &["NoSpatialIndex", "NoSpatialIndex", "NoSpatialIndex"],
        ),
        // A 1.2 file, so it has the pre-1.4 RTree trigger set.
        ("gdal_points_1_2.gpkg", &["LegacySpatialIndexTriggers"]),
        ("gdal_tiles.gpkg", &[]),
        // GP10 in the header, which is the point of this one.
        ("legacy_gp10.gpkg", &["LegacyApplicationId"]),
        ("qgis_lines.gpkg", &[]),
    ];

    for (name, want) in expected {
        if !fixtures_dir().join(name).exists() {
            // The QGIS fixture only regenerates where QGIS is installed.
            continue;
        }
        let (_dir, gpkg) = open_fixture(name);
        let mut got: Vec<String> = gpkg
            .validate()
            .unwrap()
            .iter()
            .map(|finding| {
                let debug = format!("{finding:?}");
                debug
                    .split_once(' ')
                    .map_or(debug.clone(), |(head, _)| head.to_owned())
                    .trim_end_matches('{')
                    .trim()
                    .to_owned()
            })
            .collect();
        got.sort();
        let mut want: Vec<String> = want.iter().map(|s| (*s).to_owned()).collect();
        want.sort();
        assert_eq!(got, want, "{name}");
    }
}

#[test]
fn every_finding_renders_as_a_sentence_naming_its_subject() {
    // One of each variant, so a new variant without a Display arm fails to
    // compile here rather than printing its debug form to a user.
    //
    // `SpatialIndexAudit` is `#[non_exhaustive]`, so the audit comes from a
    // real layer rather than a literal: one row inserted through the writer,
    // then the index emptied behind the triggers' back, which is the same
    // divergence `an_out_of_step_spatial_index_is_an_error` builds.
    let (_dir, gpkg) = with_layer();
    let layer = gpkg.layer("roads").unwrap();
    let mut writer = layer.writer().unwrap();
    writer
        .insert(
            None,
            &geo_types::Line::new(
                geo_types::Coord { x: 0.0, y: 0.0 },
                geo_types::Coord { x: 1.0, y: 1.0 },
            ),
            &[geopackage::ValueRef::Text("a")],
        )
        .unwrap();
    writer.commit().unwrap();
    gpkg.connection()
        .execute("DELETE FROM rtree_roads_geom", [])
        .unwrap();
    let audit = layer.audit_spatial_index().unwrap();
    assert_eq!(
        audit.missing, 1,
        "the emptied index should lose its one row"
    );

    let cases: Vec<(Finding, &str)> = vec![
        (
            Finding::LegacyApplicationId {
                version: geopackage::GpkgVersion::V1_0,
                application_id: 0x4750_3130,
            },
            "GP10",
        ),
        (
            Finding::MissingContentsTable {
                table_name: "roads".into(),
            },
            "roads",
        ),
        (
            Finding::TableNameCaseMismatch {
                declared: "Roads".into(),
                actual: "roads".into(),
            },
            "case",
        ),
        (
            Finding::RemovedExtension {
                extension_name: "gpkg_geom_CIRCULARSTRING".into(),
                table_name: Some("roads".into()),
            },
            "roads",
        ),
        (
            Finding::UnrecognisedExtension {
                extension_name: "acme_thing".into(),
                table_name: None,
                scope: geopackage::ExtensionScope::ReadWrite,
            },
            "acme_thing",
        ),
        (
            Finding::SpatialIndexOutOfStep {
                table_name: "roads".into(),
                audit,
            },
            "1 missing",
        ),
        (
            Finding::LegacySpatialIndexTriggers {
                table_name: "roads".into(),
            },
            "pre-1.4",
        ),
        (
            Finding::NoSpatialIndex {
                table_name: "roads".into(),
            },
            "no spatial index",
        ),
        (
            Finding::TilePyramidInconsistent {
                table_name: "basemap".into(),
                detail: "zoom 3 missing".into(),
            },
            "zoom 3 missing",
        ),
        (Finding::DanglingMetadataReference { md_id: 7 }, "7"),
        (
            Finding::MissingMappingTable {
                mapping_table_name: "map".into(),
            },
            "map",
        ),
        (
            Finding::NonConformantRelationName {
                relation_name: "sideways".into(),
            },
            "sideways",
        ),
    ];

    for (finding, expected_fragment) in cases {
        let rendered = finding.to_string();
        assert!(
            rendered.contains(expected_fragment),
            "{finding:?} rendered as {rendered:?}, which does not mention {expected_fragment:?}"
        );
        // A finding's own line includes neither its severity nor its repair:
        // those are separate accessors so a caller arranges them itself.
        assert!(!rendered.contains("Severity"), "{rendered:?}");
        assert!(!rendered.is_empty());
    }
}

#[test]
fn an_optional_table_name_reads_as_a_sentence_either_way() {
    let with = Finding::RemovedExtension {
        extension_name: "gpkg_geom_CIRCULARSTRING".into(),
        table_name: Some("roads".into()),
    };
    let without = Finding::RemovedExtension {
        extension_name: "gpkg_geom_CIRCULARSTRING".into(),
        table_name: None,
    };
    assert!(with.to_string().contains(r#"on "roads""#), "{with:?}");
    assert!(!without.to_string().contains(" on "), "{without:?}");
}

#[test]
fn severity_renders_as_a_lowercase_word() {
    assert_eq!(Severity::Advisory.to_string(), "advisory");
    assert_eq!(Severity::Warning.to_string(), "warning");
    assert_eq!(Severity::Error.to_string(), "error");
}
