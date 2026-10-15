//! The scale probe: one very large or deeply nested instance per surface, put
//! through every operation once.
//!
//! The property tests draw small values, so per-element recursion and
//! accidental quadratic behaviour do not appear there. Each test here builds
//! one instance far larger than a generator does and checks every operation
//! on it against a count or a value known from its construction.

use std::path::Path;

use geo_types::{Coord, Geometry, GeometryCollection, LineString, Point};
use geopackage::core::geometry::{
    GeometryError, GpbGeometry, blob_envelope_and_empty, blob_is_empty, blob_xy_envelope,
    encode_gpb, encode_gpb_from_wkb,
};
use geopackage::core::gpb::parse_header;
use geopackage::core::tiles::{TileCoord, TileMatrixSet, ZoomLadder};
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{
    BoundingBox, BulkIndexOptions, ColumnSpec, GeoPackage, GeometrySpec, NewFeature,
    TableSchemaBuilder, TilePyramidBuilder, Value,
};

const FEATURES: i64 = 200_000;

/// A grid of `FEATURES` points, 1000 to a row, one unit apart.
fn point(fid: i64) -> Point<f64> {
    Point::new((fid % 1000) as f64, (fid / 1000) as f64)
}

/// A layer of 200,000 points, loaded through the bulk index build, put
/// through the count, a full read, a box query on each path, the audit, a
/// rebuild, a drop and a triggered build, a clone and `Debug` of every
/// feature.
#[test]
fn a_large_layer() {
    let gpkg = GeoPackage::create(Path::new(":memory:")).unwrap();
    let layer = gpkg
        .create_layer(
            &TableSchemaBuilder::new("points")
                .column(ColumnSpec::new("name", ColumnType::Text(None)))
                .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
        )
        .unwrap();
    let rows = (0..FEATURES).map(|fid| {
        NewFeature::new(point(fid), vec![Value::Text(format!("p{fid}"))]).with_fid(fid + 1)
    });
    let fids = layer
        .write_all_with(rows, 0, BulkIndexOptions::always_bulk())
        .unwrap();
    assert_eq!(fids.len() as i64, FEATURES);
    assert_eq!(layer.count().unwrap(), FEATURES as u64);

    let mut seen = 0_i64;
    let mut debug_len = 0_usize;
    for feature in layer.features().unwrap() {
        let feature = feature.unwrap();
        let copy = feature.clone();
        assert_eq!(copy.fid(), feature.fid());
        assert_eq!(copy.geometry_bytes(), feature.geometry_bytes());
        debug_len += format!("{feature:?}").len();
        seen += 1;
    }
    assert_eq!(seen, FEATURES);
    assert!(debug_len > 0);

    // Rows 100 to 199, columns 0 to 9: 1000 points, boundaries included.
    let bbox = BoundingBox::new(0.0, 100.0, 9.0, 199.0);
    let selected = |gpkg: &GeoPackage| {
        gpkg.layer("points")
            .unwrap()
            .features_in(bbox)
            .unwrap()
            .count()
    };
    assert_eq!(selected(&gpkg), 1_000);
    let everything = BoundingBox::new(-1.0, -1.0, 1e6, 1e6);
    assert_eq!(
        layer.features_in(everything).unwrap().count() as i64,
        FEATURES
    );

    assert!(layer.audit_spatial_index().unwrap().is_consistent());
    layer.rebuild_spatial_index().unwrap();
    assert!(layer.audit_spatial_index().unwrap().is_consistent());
    layer.drop_spatial_index().unwrap();
    assert_eq!(selected(&gpkg), 1_000, "the full scan");
    layer
        .create_spatial_index_with(BulkIndexOptions::never_bulk())
        .unwrap();
    assert!(layer.audit_spatial_index().unwrap().is_consistent());
    assert_eq!(selected(&gpkg), 1_000);
}

/// A line string of a million points through every reader of a blob, the
/// conversion back to `geo-types`, and a layer.
#[test]
fn a_large_geometry() {
    let coords: Vec<Coord<f64>> = (0..1_000_000)
        .map(|i| Coord {
            x: f64::from(i),
            y: -f64::from(i),
        })
        .collect();
    let line = LineString::new(coords);
    let geometry: Geometry<f64> = line.clone().into();
    let expected = Some([0.0, 999_999.0, -999_999.0, 0.0]);

    let (blob, envelope) = encode_gpb(&geometry, 4326).unwrap();
    assert_eq!(envelope, expected);
    let (_, offset) = parse_header(&blob).unwrap();
    assert_eq!(blob_xy_envelope(&blob).unwrap(), expected);
    assert!(!blob_is_empty(&blob).unwrap());
    assert_eq!(blob_envelope_and_empty(&blob).unwrap(), (expected, false));
    assert_eq!(
        encode_gpb_from_wkb(&blob[offset..], 4326).unwrap().blob,
        blob
    );
    let parsed = GpbGeometry::parse(&blob).unwrap();
    assert_eq!(parsed.xy_envelope(), expected);
    assert_eq!(parsed.to_geo(), Some(geometry.clone()));
    assert!(!format!("{parsed:?}").is_empty());

    let gpkg = GeoPackage::create(Path::new(":memory:")).unwrap();
    let layer = gpkg
        .create_layer(
            &TableSchemaBuilder::new("lines")
                .geometry(GeometrySpec::new(GeometryType::LineString, 4326)),
        )
        .unwrap();
    let mut writer = layer.writer().unwrap();
    writer.insert(Some(1), &line, &[]).unwrap();
    writer.commit().unwrap();
    let found: Vec<_> = layer
        .features_in(BoundingBox::new(
            999_998.5, -999_999.5, 999_999.5, -999_998.5,
        ))
        .unwrap()
        .collect();
    assert_eq!(found.len(), 1);
    let feature = found.into_iter().next().unwrap().unwrap();
    assert_eq!(feature.geometry_bytes(), Some(blob.as_slice()));
}

/// Wraps `geometry` in `levels` single-member collections.
fn wrap(mut geometry: Geometry<f64>, levels: usize) -> Geometry<f64> {
    for _ in 0..levels {
        geometry = Geometry::GeometryCollection(GeometryCollection::new_from(vec![geometry]));
    }
    geometry
}

/// A collection at the deepest nesting the crate writes, with a member at
/// every level, round-trips; one nested ten thousand levels deep is rejected
/// without exhausting the stack.
#[test]
fn deep_nesting() {
    let mut deepest: Geometry<f64> = Point::new(0.0, 0.0).into();
    for level in 1..=32 {
        deepest = Geometry::GeometryCollection(GeometryCollection::new_from(vec![
            deepest,
            Point::new(f64::from(level), -f64::from(level)).into(),
        ]));
    }
    let (blob, envelope) = encode_gpb(&deepest, 4326).unwrap();
    assert_eq!(envelope, Some([0.0, 32.0, -32.0, 0.0]));
    assert!(!blob_is_empty(&blob).unwrap());
    let back = GpbGeometry::parse(&blob).unwrap().to_geo().unwrap();
    assert_eq!(back, deepest);
    assert_eq!(format!("{back:?}"), format!("{deepest:?}"));

    let too_deep = wrap(Point::new(0.0, 0.0).into(), 10_000);
    assert!(matches!(
        encode_gpb(&too_deep, 4326),
        Err(GeometryError::NestingTooDeep)
    ));
    // `geo-types` drops a collection by recursing into its members, which
    // overflows the test thread's stack at this depth. That is outside this
    // crate, so the value is taken apart one level at a time instead.
    let mut level = too_deep;
    while let Geometry::GeometryCollection(mut collection) = level {
        level = collection
            .0
            .pop()
            .unwrap_or_else(|| Point::new(0.0, 0.0).into());
    }
}

/// A pyramid of 87,381 tiles over eight zoom levels, written in batches and
/// read back through the counts, each scan and a box query.
#[test]
fn a_large_pyramid() {
    let gpkg = GeoPackage::create(Path::new(":memory:")).unwrap();
    let set = TileMatrixSet::new(0, 0.0, 0.0, 256.0, 256.0);
    let matrices = set.ladder(ZoomLadder::new(0, 8).tile_size(1, 1)).unwrap();
    let pyramid = gpkg
        .create_tile_pyramid(&TilePyramidBuilder::new("basemap", set).matrices(matrices.clone()))
        .unwrap();
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    png.extend_from_slice(&13_u32.to_be_bytes());
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&1_u32.to_be_bytes());
    png.extend_from_slice(&1_u32.to_be_bytes());
    png.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    let tiles = matrices.iter().flat_map(|matrix| {
        let png = png.clone();
        (0..matrix.matrix_height).flat_map(move |row| {
            let png = png.clone();
            (0..matrix.matrix_width)
                .map(move |column| (TileCoord::new(matrix.zoom_level, column, row), png.clone()))
        })
    });
    let total: i64 = matrices
        .iter()
        .map(|m| m.matrix_width * m.matrix_height)
        .sum();
    assert_eq!(pyramid.write_all(tiles, 10_000).unwrap() as i64, total);
    assert_eq!(pyramid.tile_count().unwrap(), total);

    let count = |cursor: &mut geopackage::TileCursor<'_>| {
        let mut stream = cursor.tiles().unwrap();
        let mut n = 0_i64;
        while stream.next().unwrap().is_some() {
            n += 1;
        }
        n
    };
    assert_eq!(count(&mut pyramid.cursor().unwrap()), total);
    for matrix in &matrices {
        let level = matrix.matrix_width * matrix.matrix_height;
        assert_eq!(pyramid.tile_count_at(matrix.zoom_level).unwrap(), level);
        assert_eq!(
            count(&mut pyramid.cursor_at(matrix.zoom_level).unwrap()),
            level
        );
    }
    // At zoom 8 each tile is one unit square. The box's edges lie on tile
    // edges, so it touches the tiles on both sides of each: 12 by 12.
    let bbox = BoundingBox::new(10.0, 10.0, 20.0, 20.0);
    assert_eq!(count(&mut pyramid.cursor_in(8, bbox).unwrap()), 144);
}
