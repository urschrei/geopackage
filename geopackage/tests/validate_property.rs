//! Property tests over `GeoPackage::validate`: what this crate writes
//! validates clean, and each kind of defect in an otherwise clean file is
//! reported as that defect.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::cmp::Reverse;
use std::path::Path;

use geo_types::{LineString, Point};
use geopackage::core::coverage::CoverageDatatype;
use geopackage::core::datetime::DateTime;
use geopackage::core::metadata::MetadataScope;
use geopackage::core::related::RelationName;
use geopackage::core::tiles::{TileCoord, TileMatrix, TileMatrixSet, ZoomLadder};
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{
    ColumnSpec, CoverageBuilder, Finding, GeoPackage, GeometrySpec, MetadataTarget, NewFeature,
    NewMetadata, NewRelation, TableSchemaBuilder, TilePyramidBuilder, Value,
};
use hegel::generators;

/// A PNG header of the given size: enough for the payload probe.
fn png(side: u32) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&13_u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&side.to_be_bytes());
    bytes.extend_from_slice(&side.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    bytes
}

/// A WebP header of the given size.
fn webp(side: u32) -> Vec<u8> {
    let mut bytes = Vec::from(*b"RIFF");
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(b"WEBPVP8X");
    bytes.extend_from_slice(&10_u32.to_le_bytes());
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    bytes.extend_from_slice(&(side - 1).to_le_bytes()[..3]);
    bytes.extend_from_slice(&(side - 1).to_le_bytes()[..3]);
    bytes
}

/// A coverage TIFF header of the given size and sample type.
fn tiff(side: u16, sample_format: u16, bits: u16) -> Vec<u8> {
    let entries: [(u16, u16); 7] = [
        (256, side),
        (257, side),
        (258, bits),
        (259, 5),
        (277, 1),
        (278, side),
        (339, sample_format),
    ];
    let mut bytes = b"II\x2a\x00".to_vec();
    bytes.extend_from_slice(&8u32.to_le_bytes());
    bytes.extend_from_slice(&7u16.to_le_bytes());
    for (tag, value) in entries {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&3u16.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
        bytes.extend_from_slice(&[0, 0]);
    }
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes
}

/// A little-endian `CIRCULARSTRING` body through three points.
fn circular_string() -> Vec<u8> {
    let mut body = vec![1];
    body.extend_from_slice(&8u32.to_le_bytes());
    body.extend_from_slice(&3u32.to_le_bytes());
    for value in [0.0_f64, 0.0, 1.0, 1.0, 2.0, 0.0] {
        body.extend_from_slice(&value.to_le_bytes());
    }
    body
}

/// What a drawn workload has created, for the later operations to use.
#[derive(Default)]
struct Created {
    /// Feature layers, and whether each has a spatial index.
    layers: Vec<(String, bool)>,
    /// Attribute tables.
    attributes: Vec<String>,
    /// Metadata record ids.
    records: Vec<i64>,
    /// Relationship mapping tables made so far, to name the next uniquely.
    relations: usize,
    /// Tables of every kind, to name the next uniquely.
    tables: usize,
}

impl Created {
    fn name(&mut self, prefix: &str) -> String {
        self.tables += 1;
        format!("{prefix}{}", self.tables)
    }
}

/// Applies one drawn write through the crate's API.
fn apply(tc: &hegel::TestCase, gpkg: &GeoPackage, created: &mut Created) {
    let op = tc.draw(generators::integers::<u8>().min_value(0).max_value(8));
    tc.event(format!("operation {op}"));
    match op {
        // A feature layer of a drawn geometry type, indexed or not, with
        // drawn features.
        0 => {
            let name = created.name("layer");
            let ty = tc.draw(generators::sampled_from(vec![
                GeometryType::Point,
                GeometryType::LineString,
                GeometryType::Geometry,
            ]));
            let indexed = tc.draw(generators::booleans());
            let layer = gpkg
                .create_layer(
                    &TableSchemaBuilder::new(&name)
                        .column(ColumnSpec::new("label", ColumnType::Text(None)))
                        .geometry(GeometrySpec::new(ty, 4326))
                        .spatial_index(indexed),
                )
                .unwrap();
            let count = tc.draw(generators::integers::<usize>().min_value(0).max_value(4));
            let mut writer = layer.writer().unwrap();
            for i in 0..count {
                let x = f64::from(u32::try_from(i).unwrap());
                let label = geopackage::ValueRef::Text("x");
                let inserted = match ty {
                    GeometryType::Point => writer.insert(None, &Point::new(x, x), &[label]),
                    GeometryType::LineString => writer.insert(
                        None,
                        &LineString::from(vec![(0.0, 0.0), (x, 1.0)]),
                        &[label],
                    ),
                    _ => writer.insert_wkb(None, &circular_string(), &[label]),
                };
                inserted.unwrap();
            }
            writer.commit().unwrap();
            created.layers.push((name, indexed));
        }
        // An attribute table with rows.
        1 => {
            let name = created.name("attrs");
            let layer = gpkg
                .create_attributes_table(
                    &TableSchemaBuilder::new(&name)
                        .column(ColumnSpec::new("body", ColumnType::Text(None))),
                )
                .unwrap();
            layer
                .write_all(
                    (0..3).map(|_| {
                        NewFeature::<Point<f64>>::attributes(vec![Value::Text("b".into())])
                    }),
                    0,
                )
                .unwrap();
            created.attributes.push(name);
        }
        // A tile pyramid, a ladder or not, with PNG and WebP tiles.
        2 => {
            let name = created.name("tiles");
            let set = TileMatrixSet::new(0, 0.0, 0.0, 256.0, 256.0);
            let ladder = tc.draw(generators::booleans());
            let matrices = if ladder {
                set.ladder(ZoomLadder::new(0, 1).tile_size(16, 16)).unwrap()
            } else {
                vec![
                    TileMatrix::new(0, 1, 1, 16, 16, 16.0, 16.0),
                    TileMatrix::new(1, 3, 3, 16, 16, 256.0 / 48.0, 256.0 / 48.0),
                ]
            };
            let pyramid = gpkg
                .create_tile_pyramid(
                    &TilePyramidBuilder::new(&name, set)
                        .matrices(matrices)
                        .allow_zoom_other(!ladder),
                )
                .unwrap();
            let payload = if tc.draw(generators::booleans()) {
                webp(16)
            } else {
                png(16)
            };
            pyramid.put_tile(TileCoord::new(0, 0, 0), &payload).unwrap();
        }
        // A coverage of either datatype, with a tile.
        3 => {
            let name = created.name("cov");
            let float = tc.draw(generators::booleans());
            let datatype = if float {
                CoverageDatatype::Float
            } else {
                CoverageDatatype::Integer
            };
            let set = TileMatrixSet::new(0, 0.0, 0.0, 16.0, 16.0);
            let coverage = gpkg
                .create_coverage(
                    &CoverageBuilder::new(&name, set, datatype)
                        .matrix(TileMatrix::new(0, 1, 1, 16, 16, 1.0, 1.0)),
                )
                .unwrap();
            let payload = if float {
                tiff(16, 3, 32)
            } else {
                tiff(16, 1, 16)
            };
            coverage
                .put_tile(TileCoord::new(0, 0, 0), &payload)
                .unwrap();
        }
        // A metadata record, attached to the file or to a created table, its
        // column or its first row, with a parent or not.
        4 => {
            let id = gpkg
                .add_metadata(&NewMetadata::new(MetadataScope::Dataset, "uri", "doc"))
                .unwrap();
            let target = match created.attributes.first() {
                Some(table) if tc.draw(generators::booleans()) => MetadataTarget::Cell {
                    table_name: table.clone(),
                    column_name: "body".into(),
                    row_id: 1,
                },
                _ => MetadataTarget::GeoPackage,
            };
            let parent = created
                .records
                .last()
                .copied()
                .filter(|_| tc.draw(generators::booleans()));
            gpkg.add_metadata_reference(
                id,
                &target,
                DateTime::from_micros_since_epoch(0).unwrap(),
                parent,
            )
            .unwrap();
            created.records.push(id);
        }
        // A relationship between two created tables, with mappings.
        5 => {
            let tables: Vec<String> = created
                .layers
                .iter()
                .map(|(name, _)| name.clone())
                .chain(created.attributes.iter().cloned())
                .collect();
            if let (Some(base), Some(related)) = (tables.first(), tables.last()) {
                created.relations += 1;
                let mapping = format!("map{}", created.relations);
                gpkg.add_relation(&NewRelation::new(
                    base,
                    related,
                    RelationName::Attributes,
                    &mapping,
                ))
                .unwrap();
                let relation = gpkg.relations().unwrap().pop().unwrap();
                gpkg.add_mapping(&relation, 1, 1).unwrap();
            }
        }
        // Drop, create or rebuild a layer's spatial index.
        6 => {
            if let Some((name, indexed)) = created.layers.first_mut() {
                let layer = gpkg.layer(name).unwrap();
                if *indexed && tc.draw(generators::booleans()) {
                    layer.drop_spatial_index().unwrap();
                    *indexed = false;
                } else if *indexed {
                    layer.rebuild_spatial_index().unwrap();
                } else {
                    layer.create_spatial_index().unwrap();
                    *indexed = true;
                }
            }
        }
        // Register more spatial reference systems.
        7 => {
            gpkg.add_epsg_srs(tc.draw(generators::sampled_from(vec![3857, 4258, 27700])))
                .unwrap();
        }
        // Delete a feature through the writer.
        _ => {
            if let Some((name, _)) = created.layers.last() {
                let layer = gpkg.layer(name).unwrap();
                let mut writer = layer.writer().unwrap();
                writer.delete(1).unwrap();
                writer.commit().unwrap();
            }
        }
    }
}

/// Any sequence of this crate's writes leaves a file that validates clean,
/// apart from one `NoSpatialIndex` advisory for each feature layer left
/// without an index.
#[hegel::test]
fn what_this_crate_writes_validates_clean(tc: hegel::TestCase) {
    let gpkg = GeoPackage::create(Path::new(":memory:")).unwrap();
    let mut created = Created::default();
    let steps = tc.draw(generators::integers::<usize>().min_value(0).max_value(10));
    for _ in 0..steps {
        apply(&tc, &gpkg, &mut created);
    }
    let mut expected: Vec<Finding> = created
        .layers
        .iter()
        .filter(|(_, indexed)| !indexed)
        .map(|(name, _)| Finding::NoSpatialIndex {
            table_name: name.clone(),
        })
        .collect();
    let mut findings = gpkg.validate().unwrap();
    let key = |f: &Finding| format!("{f:?}");
    expected.sort_by_key(key);
    findings.sort_by_key(key);
    assert_eq!(findings, expected);
}

/// Builds a file that validates clean, with one object for each defect the
/// completeness property makes, so that each defect has its own target.
fn build_clean() -> GeoPackage {
    let gpkg = GeoPackage::create(Path::new(":memory:")).unwrap();
    for name in ["pts", "noidx", "legacy", "stale", "lost", "casepts"] {
        let layer = gpkg
            .create_layer(
                &TableSchemaBuilder::new(name)
                    .geometry(GeometrySpec::new(GeometryType::Point, 4326))
                    .spatial_index(true),
            )
            .unwrap();
        let mut writer = layer.writer().unwrap();
        for x in [0.0, 1.0, 2.0] {
            writer.insert(None, &Point::new(x, x), &[]).unwrap();
        }
        writer.commit().unwrap();
    }
    for name in ["notes", "gone", "case"] {
        gpkg.create_attributes_table(
            &TableSchemaBuilder::new(name).column(ColumnSpec::new("body", ColumnType::Text(None))),
        )
        .unwrap();
    }
    let set = TileMatrixSet::new(0, 0.0, 0.0, 256.0, 256.0);
    let pyramid = gpkg
        .create_tile_pyramid(
            &TilePyramidBuilder::new("tiles", set)
                .matrices(set.ladder(ZoomLadder::new(0, 1).tile_size(16, 16)).unwrap()),
        )
        .unwrap();
    pyramid.put_tile(TileCoord::new(0, 0, 0), &png(16)).unwrap();
    gpkg.create_tile_pyramid(
        &TilePyramidBuilder::new("tiles_lost", set)
            .matrices(set.ladder(ZoomLadder::new(0, 0).tile_size(16, 16)).unwrap()),
    )
    .unwrap();
    for name in COVERAGES {
        let datatype = if name == "cov_scale" {
            CoverageDatatype::Float
        } else {
            CoverageDatatype::Integer
        };
        let coverage = gpkg
            .create_coverage(
                &CoverageBuilder::new(name, TileMatrixSet::new(0, 0.0, 0.0, 16.0, 16.0), datatype)
                    .matrix(TileMatrix::new(0, 1, 1, 16, 16, 1.0, 1.0)),
            )
            .unwrap();
        let payload = if datatype == CoverageDatatype::Float {
            tiff(16, 3, 32)
        } else {
            tiff(16, 1, 16)
        };
        coverage
            .put_tile(TileCoord::new(0, 0, 0), &payload)
            .unwrap();
    }
    let parent = gpkg
        .add_metadata(&NewMetadata::new(MetadataScope::Dataset, "uri", "parent"))
        .unwrap();
    let child = gpkg
        .add_metadata(&NewMetadata::new(MetadataScope::Dataset, "uri", "child"))
        .unwrap();
    assert_eq!((parent, child), (PARENT, CHILD));
    gpkg.add_metadata_reference(
        CHILD,
        &MetadataTarget::GeoPackage,
        DateTime::from_micros_since_epoch(0).unwrap(),
        Some(PARENT),
    )
    .unwrap();
    for mapping in ["map1", "map2"] {
        gpkg.add_relation(&NewRelation::new(
            "notes",
            "notes",
            RelationName::Attributes,
            mapping,
        ))
        .unwrap();
    }
    gpkg
}

/// The coverages of the clean file, one for each coverage defect.
const COVERAGES: [&str; 8] = [
    "cov_uninterpretable",
    "cov_datatype",
    "cov_scale",
    "cov_profile",
    "cov_mismatch",
    "cov_missing",
    "cov_dangling",
    "cov_lost",
];

/// The ids of the two metadata records of the clean file.
const PARENT: i64 = 1;
const CHILD: i64 = 2;

/// What identifies a finding for this property: its variant, its table, and
/// the record id of a dangling metadata reference.
type Key = (String, Option<String>, Option<i64>);

fn key(finding: &Finding) -> Key {
    let debug = format!("{finding:?}");
    let variant = debug.split([' ', '(']).next().unwrap().to_owned();
    let md_id = match finding {
        Finding::DanglingMetadataReference { md_id } => Some(*md_id),
        _ => None,
    };
    (variant, finding.table_name().map(str::to_owned), md_id)
}

fn expect(variant: &str, table: Option<&str>) -> Key {
    (variant.to_owned(), table.map(str::to_owned), None)
}

/// Runs a statement of a defect.
fn sql(gpkg: &GeoPackage, statement: &str) {
    gpkg.connection().execute_batch(statement).unwrap();
}

/// Applies the drawn defect number `defect` to the clean file, and returns
/// the finding it should cause.
fn corrupt(tc: &hegel::TestCase, gpkg: &GeoPackage, defect: usize) -> Key {
    match defect {
        // First, because the crate refuses this write once a later defect
        // registers an extension it does not know.
        0 => {
            gpkg.layer("noidx").unwrap().drop_spatial_index().unwrap();
            expect("NoSpatialIndex", Some("noidx"))
        }
        1 => {
            sql(gpkg, "DROP TABLE gone");
            expect("MissingContentsTable", Some("gone"))
        }
        2 => {
            sql(gpkg, "DROP TABLE lost");
            expect("MissingContentsTable", Some("lost"))
        }
        3 => {
            sql(
                gpkg,
                "UPDATE gpkg_contents SET table_name = 'CASE' WHERE table_name = 'case'",
            );
            expect("TableNameCaseMismatch", Some("CASE"))
        }
        4 => {
            sql(
                gpkg,
                "INSERT INTO gpkg_extensions VALUES \
                 (NULL, NULL, 'gpkg_srs_id_trigger', 'def', 'read-write')",
            );
            expect("RemovedExtension", None)
        }
        5 => {
            sql(
                gpkg,
                "INSERT INTO gpkg_extensions VALUES \
                 (NULL, NULL, 'acme_widgets', 'def', 'write-only')",
            );
            expect("UnrecognisedExtension", None)
        }
        6 => {
            sql(gpkg, "DELETE FROM rtree_pts_geom WHERE id = 2");
            expect("SpatialIndexOutOfStep", Some("pts"))
        }
        7 => {
            let id = tc.draw(generators::sampled_from(vec![0x4750_3130_u32, 0x4750_3131]));
            sql(gpkg, &format!("PRAGMA application_id = {id}"));
            expect("LegacyApplicationId", None)
        }
        8 => {
            // A pre-1.4 set, or a mixed set when the 1.4 triggers stay.
            sql(
                gpkg,
                "CREATE TRIGGER rtree_legacy_geom_update1 AFTER UPDATE ON legacy \
                 BEGIN SELECT 1; END",
            );
            if tc.draw(generators::booleans()) {
                sql(
                    gpkg,
                    "DROP TRIGGER rtree_legacy_geom_update5; \
                     DROP TRIGGER rtree_legacy_geom_update6; \
                     DROP TRIGGER rtree_legacy_geom_update7",
                );
            }
            expect("LegacySpatialIndexTriggers", Some("legacy"))
        }
        9 => {
            sql(
                gpkg,
                "UPDATE gpkg_tile_matrix SET pixel_x_size = pixel_x_size * 3 \
                 WHERE table_name = 'tiles' AND zoom_level = 1",
            );
            expect("TilePyramidInconsistent", Some("tiles"))
        }
        10 => {
            sql(
                gpkg,
                "DELETE FROM gpkg_2d_gridded_coverage_ancillary \
                 WHERE tile_matrix_set_name = 'cov_uninterpretable'",
            );
            expect("CoverageUninterpretable", Some("cov_uninterpretable"))
        }
        11 => {
            sql(
                gpkg,
                "UPDATE gpkg_2d_gridded_coverage_ancillary SET datatype = 'double' \
                 WHERE tile_matrix_set_name = 'cov_datatype'",
            );
            expect("CoverageDatatypeUnknown", Some("cov_datatype"))
        }
        12 => {
            let column = tc.draw(generators::sampled_from(vec!["scale", "offset"]));
            let (table, name_column) = tc.draw(generators::sampled_from(vec![
                ("gpkg_2d_gridded_coverage_ancillary", "tile_matrix_set_name"),
                ("gpkg_2d_gridded_tile_ancillary", "tpudt_name"),
            ]));
            sql(
                gpkg,
                &format!("UPDATE {table} SET {column} = 2 WHERE {name_column} = 'cov_scale'"),
            );
            expect("CoverageScaleNotDefault", Some("cov_scale"))
        }
        13 => {
            sql(gpkg, "UPDATE cov_profile SET tile_data = x'00010203'");
            expect("CoveragePayloadNonConformant", Some("cov_profile"))
        }
        14 => {
            gpkg.connection()
                .execute("UPDATE cov_mismatch SET tile_data = ?1", [tiff(16, 3, 32)])
                .unwrap();
            expect("CoveragePayloadDatatypeMismatch", Some("cov_mismatch"))
        }
        15 => {
            sql(
                gpkg,
                "DELETE FROM gpkg_2d_gridded_tile_ancillary WHERE tpudt_name = 'cov_missing'",
            );
            expect("MissingTileAncillary", Some("cov_missing"))
        }
        16 => {
            sql(gpkg, "DELETE FROM cov_dangling");
            expect("DanglingTileAncillary", Some("cov_dangling"))
        }
        17 => {
            sql(
                gpkg,
                &format!("DELETE FROM gpkg_metadata WHERE id = {PARENT}"),
            );
            ("DanglingMetadataReference".to_owned(), None, Some(PARENT))
        }
        18 => {
            sql(
                gpkg,
                &format!("DELETE FROM gpkg_metadata WHERE id = {CHILD}"),
            );
            ("DanglingMetadataReference".to_owned(), None, Some(CHILD))
        }
        19 => {
            sql(gpkg, "DROP TABLE map1");
            expect("MissingMappingTable", Some("map1"))
        }
        20 => {
            sql(gpkg, "DROP TABLE tiles_lost");
            expect("MissingContentsTable", Some("tiles_lost"))
        }
        21 => {
            sql(gpkg, "DROP TABLE cov_lost");
            expect("MissingContentsTable", Some("cov_lost"))
        }
        22 => {
            sql(
                gpkg,
                "UPDATE gpkg_contents SET table_name = 'CasePts' WHERE table_name = 'casepts'",
            );
            expect("TableNameCaseMismatch", Some("CasePts"))
        }
        23 => {
            // The virtual table without its triggers, or the reverse.
            if tc.draw(generators::booleans()) {
                sql(gpkg, "DROP TABLE rtree_stale_geom");
            } else {
                let triggers: Vec<String> = gpkg
                    .connection()
                    .prepare(
                        "SELECT name FROM sqlite_master \
                         WHERE type = 'trigger' AND name LIKE 'rtree_stale_geom_%'",
                    )
                    .unwrap()
                    .query_map([], |row| row.get(0))
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap();
                assert!(!triggers.is_empty());
                for trigger in triggers {
                    sql(gpkg, &format!("DROP TRIGGER {trigger}"));
                }
            }
            expect("StaleSpatialIndex", Some("stale"))
        }
        _ => {
            sql(
                gpkg,
                "UPDATE gpkgext_relations SET relation_name = 'bogus' \
                 WHERE mapping_table_name = 'map2'",
            );
            expect("NonConformantRelationName", None)
        }
    }
}

/// The number of defects `corrupt` knows.
const DEFECTS: usize = 25;

/// Each defect, made alone or together with others on separate objects of a
/// clean file, is reported as exactly its own finding, and the findings come
/// back most severe first.
///
/// Each case builds a file of about 25 tables, so this property runs 500
/// cases rather than the suite's 2000. Each defect is drawn in about 30 % of
/// cases.
#[hegel::test(test_cases = 500)]
fn validate_reports_each_defect(tc: hegel::TestCase) {
    let gpkg = build_clean();
    assert_eq!(gpkg.validate().unwrap(), Vec::new());
    // The defects break references and CHECK constraints on purpose, as a
    // file from another writer can.
    sql(
        &gpkg,
        "PRAGMA foreign_keys = OFF; PRAGMA ignore_check_constraints = ON",
    );
    let mut expected = Vec::new();
    for defect in 0..DEFECTS {
        if tc.draw(generators::weighted_booleans(0.3)) {
            tc.event(format!("defect {defect}"));
            expected.push(corrupt(&tc, &gpkg, defect));
        }
    }
    let findings = gpkg.validate().unwrap();
    assert!(
        findings.is_sorted_by_key(|finding| Reverse(finding.severity())),
        "{findings:?}"
    );
    let mut found: Vec<Key> = findings.iter().map(key).collect();
    found.sort();
    expected.sort();
    assert_eq!(found, expected, "{findings:#?}");
}
