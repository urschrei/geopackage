//! `Layer::read_arrow_in`: the columnar counterpart of `features_in`.
//!
//! The governing property is that the two agree. The RTree stores `f32`
//! envelopes and is queried with outward-widened bounds, so its candidates are
//! a superset of the answer; if the columnar path skipped the exact re-test it
//! would return rows the row path does not, and no amount of matching row
//! counts on a tidy fixture would show it.

#![cfg(feature = "arrow")]
#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use arrow_array::cast::AsArray;
use geo_types::{Coord, LineString, Point};
use geopackage::arrow::ArrowReadOptions;
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{
    BoundingBox, ColumnSpec, GeoPackage, GeometrySpec, NewFeature, TableSchemaBuilder, Value,
};
use tempfile::TempDir;

/// A layer of `count` points on a diagonal, indexed unless `index` is false.
fn points(count: i32, index: bool) -> (TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("p.gpkg")).unwrap();
    gpkg.add_epsg_srs(4326).unwrap();
    gpkg.create_layer(
        &TableSchemaBuilder::new("pts")
            .column(ColumnSpec::new("name", ColumnType::Text(None)))
            .geometry(GeometrySpec::new(GeometryType::Point, 4326))
            .spatial_index(index),
    )
    .unwrap();
    gpkg.layer("pts")
        .unwrap()
        .write_all(
            (0..count)
                .map(|i| {
                    NewFeature::new(
                        Point::new(f64::from(i), f64::from(i)),
                        vec![Value::Text(format!("p{i}"))],
                    )
                })
                .collect::<Vec<_>>(),
            1000,
        )
        .unwrap();
    (dir, gpkg)
}

/// The `fid`s a columnar filtered read returns, in order.
fn arrow_fids(gpkg: &GeoPackage, bbox: BoundingBox, options: ArrowReadOptions) -> Vec<i64> {
    let layer = gpkg.layer("pts").unwrap();
    let mut fids = Vec::new();
    for batch in layer.read_arrow_in(bbox, options).unwrap() {
        let batch = batch.unwrap();
        let column = batch.column_by_name("fid").unwrap();
        for value in column.as_primitive::<arrow_array::types::Int64Type>() {
            fids.push(value.unwrap());
        }
    }
    fids
}

/// The `fid`s the row path returns, in order.
fn row_fids(gpkg: &GeoPackage, bbox: BoundingBox) -> Vec<i64> {
    gpkg.layer("pts")
        .unwrap()
        .features_in(bbox)
        .unwrap()
        .map(|feature| feature.unwrap().fid())
        .collect()
}

#[test]
fn agrees_with_features_in_over_an_indexed_layer() {
    let (_dir, gpkg) = points(500, true);
    for bbox in [
        BoundingBox::new(10.0, 10.0, 20.0, 20.0),
        BoundingBox::new(-5.0, -5.0, 0.5, 0.5),
        BoundingBox::new(0.0, 0.0, 499.0, 499.0),
        // Off the diagonal: the rtree returns nothing, and neither should we.
        BoundingBox::new(0.0, 400.0, 10.0, 410.0),
    ] {
        assert_eq!(
            arrow_fids(&gpkg, bbox, ArrowReadOptions::default()),
            row_fids(&gpkg, bbox),
            "{bbox:?}"
        );
    }
}

#[test]
fn agrees_with_features_in_with_no_index_at_all() {
    // The fallback path: a full scan carrying the same exact filter.
    let (_dir, gpkg) = points(300, false);
    let bbox = BoundingBox::new(10.0, 10.0, 40.0, 40.0);
    assert!(!gpkg.layer("pts").unwrap().has_spatial_index().unwrap());
    assert_eq!(
        arrow_fids(&gpkg, bbox, ArrowReadOptions::default()),
        row_fids(&gpkg, bbox)
    );
}

#[test]
fn the_exact_refilter_removes_what_the_index_only_narrows() {
    // The index stores `f32` envelopes, rounded outward, and its bounds are
    // widened further still, so it can hand back a row the answer does not
    // contain. This is a case where it demonstrably does: the point sits at
    // x = 0.05, which has no exact `f32`, and the box stops just short of it.
    //
    // Integer coordinates would not do: they round exactly, the index never
    // over-returns, and the test would pass with the re-filter deleted.
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("f.gpkg")).unwrap();
    gpkg.add_epsg_srs(4326).unwrap();
    gpkg.create_layer(
        &TableSchemaBuilder::new("pts")
            .column(ColumnSpec::new("name", ColumnType::Text(None)))
            .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
    )
    .unwrap();
    gpkg.layer("pts")
        .unwrap()
        .write_all(
            (0..200)
                .map(|i| {
                    let x = f64::from(i).mul_add(0.1, 0.05);
                    NewFeature::new(Point::new(x, x), vec![Value::Text("p".into())])
                })
                .collect::<Vec<_>>(),
            1000,
        )
        .unwrap();

    let bbox = BoundingBox::new(
        0.030_000_000_000_000_002,
        0.030_000_000_000_000_002,
        0.049_999_999,
        0.049_999_999,
    );

    // The premise, asserted rather than assumed: the index really does offer a
    // candidate here. Without this the test could quietly stop exercising the
    // re-filter if the widening or the index encoding ever changed.
    let candidates: i64 = gpkg
        .connection()
        .query_row(
            "SELECT count(*) FROM rtree_pts_geom \
             WHERE minx <= ?1 AND maxx >= ?2 AND miny <= ?3 AND maxy >= ?4",
            [bbox.max_x, bbox.min_x, bbox.max_y, bbox.min_y],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(candidates, 1, "the index should over-return here");

    // And the answer is empty, because the point is outside the box.
    let rows = row_fids(&gpkg, bbox);
    assert!(rows.is_empty(), "{rows:?}");
    assert_eq!(
        arrow_fids(&gpkg, bbox, ArrowReadOptions::default()),
        rows,
        "the columnar path must drop the candidate the index only narrowed"
    );
}

#[test]
fn a_page_whose_candidates_are_all_filtered_out_is_not_the_end() {
    // The pagination hazard, and it needs the unindexed path to reach. With an
    // index the spatial subquery narrows before `LIMIT`, so a page is nearly
    // all matches; without one every row is a candidate and the exact filter
    // does all the work, so the early pages are dropped in full.
    //
    // If a page that yielded no rows were taken for the end of the layer, every
    // match after it would silently vanish. That is what this pins.
    let (_dir, gpkg) = points(400, false);
    assert!(!gpkg.layer("pts").unwrap().has_spatial_index().unwrap());

    // Matches only the far end, so roughly a hundred pages of four rows each
    // are read and discarded before the first match appears.
    let bbox = BoundingBox::new(395.0, 395.0, 399.0, 399.0);
    let expected = row_fids(&gpkg, bbox);
    assert_eq!(expected.len(), 5, "sanity: {expected:?}");

    assert_eq!(
        arrow_fids(&gpkg, bbox, ArrowReadOptions::with_batch_size(4)),
        expected,
        "pages of candidates that all fail the filter must not end the read"
    );
}

#[test]
fn batches_respect_the_batch_size_without_losing_rows() {
    let (_dir, gpkg) = points(300, true);
    let bbox = BoundingBox::new(0.0, 0.0, 299.0, 299.0);
    let layer = gpkg.layer("pts").unwrap();

    let mut total = 0usize;
    for batch in layer
        .read_arrow_in(bbox, ArrowReadOptions::with_batch_size(32))
        .unwrap()
    {
        let batch = batch.unwrap();
        assert!(batch.num_rows() <= 32, "{} rows", batch.num_rows());
        total += batch.num_rows();
    }
    assert_eq!(total, row_fids(&gpkg, bbox).len());
}

#[test]
fn values_and_geometry_come_through_the_filtered_read() {
    let (_dir, gpkg) = points(50, true);
    let layer = gpkg.layer("pts").unwrap();
    let bbox = BoundingBox::new(10.0, 10.0, 12.0, 12.0);

    let batches: Vec<_> = layer
        .read_arrow_in(bbox, ArrowReadOptions::default())
        .unwrap()
        .map(std::result::Result::unwrap)
        .collect();
    let rows: usize = batches.iter().map(arrow_array::RecordBatch::num_rows).sum();
    assert_eq!(rows, 3);

    let batch = batches.first().unwrap();
    let names = batch.column_by_name("name").unwrap();
    let names = names.as_string::<i32>();
    assert_eq!(names.value(0), "p10");
    // The geometry column is a GeoArrow WKB binary column, as in an unfiltered
    // read; a filtered read is the same columns, fewer rows.
    assert!(batch.column_by_name("geom").is_some());
}

#[test]
fn a_layer_with_no_geometry_is_refused_rather_than_scanned() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("a.gpkg")).unwrap();
    gpkg.create_attributes_table(
        &TableSchemaBuilder::new("notes").column(ColumnSpec::new("note", ColumnType::Text(None))),
    )
    .unwrap();

    let layer = gpkg.attributes("notes").unwrap();
    assert!(
        layer
            .read_arrow_in(
                BoundingBox::new(0.0, 0.0, 1.0, 1.0),
                ArrowReadOptions::default()
            )
            .is_err()
    );
}

#[test]
fn agrees_with_features_in_over_lines_whose_envelopes_overlap() {
    // Points are a special case: their envelope is a degenerate box. Lines
    // exercise an envelope wider than the geometry, where the index and the
    // exact test disagree more often.
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("l.gpkg")).unwrap();
    gpkg.add_epsg_srs(4326).unwrap();
    gpkg.create_layer(
        &TableSchemaBuilder::new("pts")
            .column(ColumnSpec::new("name", ColumnType::Text(None)))
            .geometry(GeometrySpec::new(GeometryType::LineString, 4326)),
    )
    .unwrap();
    gpkg.layer("pts")
        .unwrap()
        .write_all(
            (0..200)
                .map(|i| {
                    let x = f64::from(i);
                    NewFeature::new(
                        LineString::new(vec![
                            Coord { x, y: 0.0 },
                            Coord {
                                x: x + 0.5,
                                y: 100.0,
                            },
                        ]),
                        vec![Value::Text("l".into())],
                    )
                })
                .collect::<Vec<_>>(),
            1000,
        )
        .unwrap();

    for bbox in [
        BoundingBox::new(50.0, 0.0, 60.0, 10.0),
        BoundingBox::new(0.0, 99.0, 200.0, 101.0),
        BoundingBox::new(20.6, 40.0, 20.7, 60.0),
    ] {
        assert_eq!(
            arrow_fids(&gpkg, bbox, ArrowReadOptions::default()),
            row_fids(&gpkg, bbox),
            "{bbox:?}"
        );
    }
}

#[test]
fn scattered_candidates_agree_across_segment_boundaries() {
    // The rtree is scanned once and its sorted candidates split into pages
    // of at most `batch_size` ids. A page whose ids fill at least half its key
    // span is read as a range; a sparser one by its explicit ids. This places
    // five matching rows every hundred, so the candidate ids form islands,
    // and the batch sizes make pages that sit inside one island (dense), span
    // several (sparse), and interleave with batches every way.
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("s.gpkg")).unwrap();
    gpkg.add_epsg_srs(4326).unwrap();
    gpkg.create_layer(
        &TableSchemaBuilder::new("pts")
            .column(ColumnSpec::new("name", ColumnType::Text(None)))
            .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
    )
    .unwrap();
    gpkg.layer("pts")
        .unwrap()
        .write_all(
            (0..500)
                .map(|i: i32| {
                    // Five in-box rows per hundred; the rest far outside.
                    let inside = i % 100 < 5;
                    let x = if inside { 1.0 } else { 500.0 + f64::from(i) };
                    NewFeature::new(Point::new(x, x), vec![Value::Text(format!("p{i}"))])
                })
                .collect::<Vec<_>>(),
            1000,
        )
        .unwrap();
    let bbox = BoundingBox::new(0.0, 0.0, 2.0, 2.0);

    // The row path yields rtree order; the columnar path orders by key. The
    // sets must agree, and the columnar result must be the sorted one.
    let mut expected = row_fids(&gpkg, bbox);
    expected.sort_unstable();
    assert_eq!(expected.len(), 25, "the case must select five islands");

    for batch_size in [1, 3, 64, 100_000] {
        let options = ArrowReadOptions::with_batch_size(batch_size);
        assert_eq!(
            arrow_fids(&gpkg, bbox, options),
            expected,
            "batch size {batch_size}"
        );
    }
}

#[test]
fn rows_overfetched_inside_a_folded_gap_are_dropped() {
    // A page whose candidates fill at least half its key span is read as a
    // key range, so the fetch reads the non-matching rows between them; only
    // the exact re-test keeps them out of the batches. Alternating in-box and
    // out-of-box rows make every second row such an over-fetch: 100
    // candidates over 199 keys is dense enough for a range.
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("g.gpkg")).unwrap();
    gpkg.add_epsg_srs(4326).unwrap();
    gpkg.create_layer(
        &TableSchemaBuilder::new("pts")
            .column(ColumnSpec::new("name", ColumnType::Text(None)))
            .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
    )
    .unwrap();
    gpkg.layer("pts")
        .unwrap()
        .write_all(
            (0..200)
                .map(|i: i32| {
                    let x = if i % 2 == 0 { 1.0 } else { 900.0 };
                    NewFeature::new(Point::new(x, x), vec![Value::Text(format!("p{i}"))])
                })
                .collect::<Vec<_>>(),
            1000,
        )
        .unwrap();
    let bbox = BoundingBox::new(0.0, 0.0, 2.0, 2.0);

    // As above: the row path yields rtree order, the columnar path key order.
    let mut expected = row_fids(&gpkg, bbox);
    expected.sort_unstable();
    assert_eq!(expected.len(), 100);
    assert_eq!(
        arrow_fids(&gpkg, bbox, ArrowReadOptions::default()),
        expected
    );
}

/// A layer of `count` points at pseudo-random positions in a 100 by 100
/// square, from a fixed xorshift sequence, so that neighbours in space are
/// strangers in key order: the case that made the read issue a query per
/// candidate when it walked runs of consecutive keys.
fn scattered(count: i32) -> (TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("s.gpkg")).unwrap();
    gpkg.add_epsg_srs(4326).unwrap();
    gpkg.create_layer(
        &TableSchemaBuilder::new("pts")
            .column(ColumnSpec::new("name", ColumnType::Text(None)))
            .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
    )
    .unwrap();
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    let mut unit = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f64 / (1u64 << 53) as f64
    };
    gpkg.layer("pts")
        .unwrap()
        .write_all(
            (0..count)
                .map(|i| {
                    // Every tenth row clusters at the origin, so a box there
                    // has dense runs of keys beside the scattered ones, and a
                    // read plans range pages and id pages together.
                    let (x, y) = if i % 10 == 0 {
                        (f64::from(i % 50) / 10.0, 0.5)
                    } else {
                        (unit() * 100.0, unit() * 100.0)
                    };
                    NewFeature::new(Point::new(x, y), vec![Value::Text(format!("p{i}"))])
                })
                .collect::<Vec<_>>(),
            1000,
        )
        .unwrap();
    (dir, gpkg)
}

#[test]
fn agrees_with_features_in_over_scattered_points() {
    let (_dir, gpkg) = scattered(3000);
    let boxes = [
        BoundingBox::new(40.0, 40.0, 41.0, 41.0),
        BoundingBox::new(10.0, 10.0, 30.0, 30.0),
        BoundingBox::new(0.0, 0.0, 5.0, 1.0),
        BoundingBox::new(-1.0, -1.0, 60.0, 60.0),
        BoundingBox::new(-10.0, -10.0, 200.0, 200.0),
        BoundingBox::new(500.0, 500.0, 600.0, 600.0),
    ];
    for bbox in boxes {
        let mut expected = row_fids(&gpkg, bbox);
        expected.sort_unstable();
        for options in [
            ArrowReadOptions::with_batch_size(1),
            ArrowReadOptions::with_batch_size(7),
            ArrowReadOptions::with_batch_size(64),
            ArrowReadOptions::default(),
            // A ceiling a single geometry exceeds: every page is cut short
            // after one row, so a page resumes inside its own id list.
            ArrowReadOptions::with_batch_size(64).with_max_batch_bytes(1),
        ] {
            assert_eq!(
                arrow_fids(&gpkg, bbox, options),
                expected,
                "{bbox:?} {options:?}"
            );
        }
    }
}

#[test]
fn a_where_clause_composes_with_a_scattered_box() {
    let (_dir, gpkg) = scattered(2000);
    let layer = gpkg.layer("pts").unwrap();
    let bbox = BoundingBox::new(20.0, 20.0, 70.0, 70.0);
    let mut expected: Vec<i64> = row_fids(&gpkg, bbox)
        .into_iter()
        .filter(|fid| fid % 3 == 0)
        .collect();
    expected.sort_unstable();
    for batch_size in [5, 100, 65_536] {
        let mut fids = Vec::new();
        for batch in layer
            .read_arrow_in_where(
                bbox,
                "fid % ?1 = 0",
                &[geopackage::ValueRef::Integer(3)],
                ArrowReadOptions::with_batch_size(batch_size),
            )
            .unwrap()
        {
            let batch = batch.unwrap();
            fids.extend(
                batch
                    .column_by_name("fid")
                    .unwrap()
                    .as_primitive::<arrow_array::types::Int64Type>()
                    .iter()
                    .map(Option::unwrap),
            );
        }
        assert_eq!(fids, expected, "batch size {batch_size}");
    }
}

/// The `fid`s of a read, in order.
fn fids_of(batches: geopackage::arrow::ArrowBatches<'_>) -> Vec<i64> {
    let mut fids = Vec::new();
    for batch in batches {
        let batch = batch.unwrap();
        fids.extend(
            batch
                .column_by_name("fid")
                .unwrap()
                .as_primitive::<arrow_array::types::Int64Type>()
                .iter()
                .map(Option::unwrap),
        );
    }
    fids
}

#[test]
fn a_box_holding_the_whole_layer_reads_every_row_on_any_thread_count() {
    // Such a box reads the layer unfiltered, which can use threads; the answer
    // must still be `features_in`'s, in key order.
    let (_dir, gpkg) = points(5000, true);
    for bbox in [
        // Exactly the index's bounds, which still filters; then one `f32`
        // step outside them, the tightest box the unfiltered read takes.
        BoundingBox::new(0.0, 0.0, 4999.0, 4999.0),
        BoundingBox::new(
            f64::from(0.0f32.next_down()),
            f64::from(0.0f32.next_down()),
            f64::from(4999.0f32.next_up()),
            f64::from(4999.0f32.next_up()),
        ),
        BoundingBox::new(-1e9, -1e9, 1e9, 1e9),
    ] {
        let mut expected = row_fids(&gpkg, bbox);
        expected.sort_unstable();
        assert_eq!(expected.len(), 5000);
        for options in [
            ArrowReadOptions::default(),
            ArrowReadOptions::with_batch_size(7),
            ArrowReadOptions::default().with_threads(4),
            ArrowReadOptions::with_batch_size(100).with_threads(3),
        ] {
            assert_eq!(
                arrow_fids(&gpkg, bbox, options),
                expected,
                "{bbox:?} {options:?}"
            );
        }
    }
}

#[test]
fn a_box_just_short_of_the_whole_layer_still_filters() {
    let (_dir, gpkg) = points(500, true);
    let bbox = BoundingBox::new(0.0, 0.0, 498.5, 499.0);
    let expected = row_fids(&gpkg, bbox);
    assert_eq!(expected.len(), 499);
    for options in [
        ArrowReadOptions::default(),
        ArrowReadOptions::default().with_threads(4),
    ] {
        assert_eq!(arrow_fids(&gpkg, bbox, options), expected, "{options:?}");
    }
}

#[test]
fn a_whole_layer_box_composes_with_a_where_clause() {
    let (_dir, gpkg) = scattered(3000);
    let layer = gpkg.layer("pts").unwrap();
    let bbox = BoundingBox::new(-10.0, -10.0, 200.0, 200.0);
    let mut expected: Vec<i64> = row_fids(&gpkg, bbox)
        .into_iter()
        .filter(|fid| fid % 3 == 0)
        .collect();
    expected.sort_unstable();
    assert_eq!(expected.len(), 1000);
    for options in [
        ArrowReadOptions::with_batch_size(5),
        ArrowReadOptions::default().with_threads(4),
    ] {
        let fids = fids_of(
            layer
                .read_arrow_in_where(
                    bbox,
                    "fid % ?1 = 0",
                    &[geopackage::ValueRef::Integer(3)],
                    options,
                )
                .unwrap(),
        );
        assert_eq!(fids, expected, "{options:?}");
    }
}

/// A GPB blob for an empty point: the empty flag, NaN coordinates, no
/// envelope. The RTree triggers leave such a row out of the index.
fn gpb_empty_point(srs_id: i32) -> Vec<u8> {
    use geopackage::core::gpb::{Envelope, encode_header};
    let mut blob = encode_header(srs_id, &Envelope::None, true, false);
    blob.push(1);
    blob.extend_from_slice(&1u32.to_le_bytes());
    blob.extend_from_slice(&f64::NAN.to_le_bytes());
    blob.extend_from_slice(&f64::NAN.to_le_bytes());
    blob
}

#[test]
fn rows_the_index_leaves_out_are_not_read_by_a_whole_layer_box() {
    // A NULL or empty geometry has no index entry and intersects no box. The
    // box contains the index's bounds, so only the row counts can tell that
    // an unfiltered read would return these rows too.
    for geometry in [None, Some(gpb_empty_point(4326))] {
        let (_dir, gpkg) = points(200, true);
        gpkg.connection()
            .execute(
                "INSERT INTO pts (fid, name, geom) VALUES (1000, 'odd', ?1)",
                [&geometry],
            )
            .unwrap();
        let bbox = BoundingBox::new(-1e9, -1e9, 1e9, 1e9);
        let expected = row_fids(&gpkg, bbox);
        assert_eq!(expected, (1..=200).collect::<Vec<_>>());
        for options in [
            ArrowReadOptions::default(),
            ArrowReadOptions::default().with_threads(4),
        ] {
            assert_eq!(
                arrow_fids(&gpkg, bbox, options),
                expected,
                "{geometry:?} {options:?}"
            );
        }
    }
}

#[test]
fn an_empty_indexed_layer_reads_nothing_through_any_box() {
    let (_dir, gpkg) = points(0, true);
    let bbox = BoundingBox::new(-1e9, -1e9, 1e9, 1e9);
    assert!(arrow_fids(&gpkg, bbox, ArrowReadOptions::default()).is_empty());
}
