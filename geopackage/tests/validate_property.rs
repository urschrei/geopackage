//! Property tests over `GeoPackage::validate`: what this crate writes
//! validates clean, and each kind of defect in an otherwise clean file is
//! reported as that defect.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

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
