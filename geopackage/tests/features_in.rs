//! `Layer::features_in`: the RTree-accelerated path, the full-scan fallback,
//! and a property test proving they return identical rows.
//!
//! The property is a set equality against an independent oracle: the expected
//! fids are computed directly from the coordinates hegel generated
//! (`envelope_of` and `box_touches`), never from either query path, so a bug in
//! one path cannot hide. The geometries are points, line strings, polygons and
//! multi-points in XY, XYZ, XYM and XYZM, with and without a header envelope,
//! and their coordinates are either dense in a small range or from the whole
//! `f64` domain, NaN and the infinities included. Most are not `f32`-exact, so
//! they stress the RTree's `f32` bound rounding and `features_in`'s candidate
//! re-test against the true `f64` envelope. Hegel found the `f32` sub-normal
//! edge described at `draw_coord` (issue #12).

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geopackage::core::gpb::{Envelope, encode_header};
use geopackage::core::triggers;
use geopackage::{BoundingBox, GeoPackage};
use hegel::generators;

/// A GPB point blob with no header envelope (forcing traversal on read).
fn point_blob(x: f64, y: f64) -> Vec<u8> {
    let mut b = encode_header(4326, &Envelope::None, false, false);
    b.push(1);
    b.extend_from_slice(&1u32.to_le_bytes());
    b.extend_from_slice(&x.to_le_bytes());
    b.extend_from_slice(&y.to_le_bytes());
    b
}

/// The XY envelope `[min_x, max_x, min_y, max_y]` of a set of coordinate
/// pairs, by the rule `GpbGeometry::xy_envelope` documents: a pair with a
/// non-finite value does not contribute, and with no finite pair there is no
/// envelope.
fn envelope_of(points: &[(f64, f64)]) -> Option<[f64; 4]> {
    let finite: Vec<&(f64, f64)> = points
        .iter()
        .filter(|(x, y)| x.is_finite() && y.is_finite())
        .collect();
    if finite.is_empty() {
        return None;
    }
    let mut env = [
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ];
    for (x, y) in finite {
        env[0] = env[0].min(*x);
        env[1] = env[1].max(*x);
        env[2] = env[2].min(*y);
        env[3] = env[3].max(*y);
    }
    Some(env)
}

/// An arbitrary finite `f64` coordinate in `[-100, 100]`, un-quantised.
///
/// The full range, including the `f32` sub-normal band (`|x| < ~1.2e-38`)
/// that originally broke path equivalence (issue #12), is deliberately in
/// play: `features_in` widens its RTree query bounds one `f32` ULP outward
/// before binding, so the vtab candidate set is a conservative superset at
/// every magnitude and the `f64` re-filter restores exactness.
fn draw_coord(tc: &hegel::TestCase) -> f64 {
    tc.draw(
        generators::floats::<f64>()
            .min_value(-100.0)
            .max_value(100.0),
    )
}

/// Any `f64`: NaN, the infinities, subnormals and magnitudes beyond the `f32`
/// range included.
fn draw_any_coord(tc: &hegel::TestCase) -> f64 {
    tc.draw(generators::floats::<f64>())
}

/// The coordinate dimensions of a WKB body, with the ISO type-code offset and
/// the number of values per point.
#[derive(Debug, Clone, Copy)]
enum Dims {
    Xy,
    Xyz,
    Xym,
    Xyzm,
}

impl Dims {
    fn type_offset(self) -> u32 {
        match self {
            Self::Xy => 0,
            Self::Xyz => 1000,
            Self::Xym => 2000,
            Self::Xyzm => 3000,
        }
    }

    fn extra_values(self) -> usize {
        match self {
            Self::Xy => 0,
            Self::Xyz | Self::Xym => 1,
            Self::Xyzm => 2,
        }
    }
}

/// Writes one point's values: X, Y, then any Z and M, drawn from `coord`.
fn write_point(
    tc: &hegel::TestCase,
    body: &mut Vec<u8>,
    dims: Dims,
    coord: fn(&hegel::TestCase) -> f64,
    points: &mut Vec<(f64, f64)>,
) {
    let (x, y) = (coord(tc), coord(tc));
    points.push((x, y));
    body.extend_from_slice(&x.to_le_bytes());
    body.extend_from_slice(&y.to_le_bytes());
    for _ in 0..dims.extra_values() {
        body.extend_from_slice(&draw_any_coord(tc).to_le_bytes());
    }
}

/// Draws a little-endian ISO WKB body: a point, line string, polygon (one
/// ring) or multi-point in drawn dimensions, with up to four points where the
/// type takes a list. Returns the body and its XY coordinate pairs.
fn draw_wkb(
    tc: &hegel::TestCase,
    coord: fn(&hegel::TestCase) -> f64,
) -> (Vec<u8>, Vec<(f64, f64)>) {
    let dims = tc.draw(generators::sampled_from(vec![
        Dims::Xy,
        Dims::Xyz,
        Dims::Xym,
        Dims::Xyzm,
    ]));
    let kind = tc.draw(generators::integers::<u32>().min_value(1).max_value(4));
    let mut body = vec![1];
    body.extend_from_slice(&(kind + dims.type_offset()).to_le_bytes());
    let mut points = Vec::new();
    let count = || tc.draw(generators::integers::<u32>().min_value(0).max_value(4));
    match kind {
        1 => write_point(tc, &mut body, dims, coord, &mut points),
        2 => {
            let n = count();
            body.extend_from_slice(&n.to_le_bytes());
            for _ in 0..n {
                write_point(tc, &mut body, dims, coord, &mut points);
            }
        }
        3 => {
            body.extend_from_slice(&1u32.to_le_bytes());
            let n = count();
            body.extend_from_slice(&n.to_le_bytes());
            for _ in 0..n {
                write_point(tc, &mut body, dims, coord, &mut points);
            }
        }
        _ => {
            let n = count();
            body.extend_from_slice(&n.to_le_bytes());
            for _ in 0..n {
                body.push(1);
                body.extend_from_slice(&(1 + dims.type_offset()).to_le_bytes());
                write_point(tc, &mut body, dims, coord, &mut points);
            }
        }
    }
    (body, points)
}

/// A drawn geometry blob and its true XY envelope, or `None` when it has no
/// finite coordinate pair. The header has the XY envelope, or no envelope,
/// which makes the reader walk the body; each is drawn half the time.
fn draw_geom(tc: &hegel::TestCase) -> (Vec<u8>, Option<[f64; 4]>) {
    let coord: fn(&hegel::TestCase) -> f64 = if tc.draw(generators::weighted_booleans(0.8)) {
        draw_coord
    } else {
        draw_any_coord
    };
    let (body, points) = draw_wkb(tc, coord);
    let env = envelope_of(&points);
    let header = match env {
        Some(env) if tc.draw(generators::booleans()) => Envelope::Xy(env),
        _ => Envelope::None,
    };
    // A body with no finite pair is empty whether or not the header says so,
    // so the empty flag is drawn for it.
    let empty = env.is_none() && tc.draw(generators::booleans());
    let mut blob = encode_header(4326, &header, empty, false);
    blob.extend_from_slice(&body);
    (blob, env)
}

/// Draws a box edge: usually near the dense coordinate range, sometimes an
/// edge of a stored envelope, and sometimes any `f64`.
fn draw_box_edge(tc: &hegel::TestCase, edges: &[f64]) -> f64 {
    if tc.draw(generators::weighted_booleans(0.1)) {
        return draw_any_coord(tc);
    }
    if !edges.is_empty() && tc.draw(generators::booleans()) {
        return tc.draw(generators::sampled_from(edges.to_vec()));
    }
    tc.draw(
        generators::floats::<f64>()
            .min_value(-110.0)
            .max_value(110.0),
    )
}

/// Draws a query box. Its two edges on each axis are put in order nine times
/// in ten, so that a minimum above its maximum also occurs. Sometimes the box
/// is the degenerate box on a stored envelope's minimum corner (the
/// inclusive-boundary and `f32`-rounding case).
fn draw_bbox(tc: &hegel::TestCase, envelopes: &[[f64; 4]]) -> BoundingBox {
    if !envelopes.is_empty() && tc.draw(generators::weighted_booleans(0.25)) {
        let env = tc.draw(generators::sampled_from(envelopes.to_vec()));
        return BoundingBox::new(env[0], env[2], env[0], env[2]);
    }
    let xs: Vec<f64> = envelopes.iter().flat_map(|e| [e[0], e[1]]).collect();
    let ys: Vec<f64> = envelopes.iter().flat_map(|e| [e[2], e[3]]).collect();
    let axis = |edges: &[f64]| {
        let (a, b) = (draw_box_edge(tc, edges), draw_box_edge(tc, edges));
        if b < a && tc.draw(generators::weighted_booleans(0.9)) {
            (b, a)
        } else {
            (a, b)
        }
    };
    let (min_x, max_x) = axis(&xs);
    let (min_y, max_y) = axis(&ys);
    BoundingBox::new(min_x, min_y, max_x, max_y)
}

/// The independent oracle: does envelope `env` share a point with `bbox`, as
/// the set of points `(x, y)` with `min_x <= x <= max_x` and
/// `min_y <= y <= max_y`? Inclusive at the boundary; a box with a NaN edge or
/// a minimum above its maximum contains no point.
fn box_touches(env: [f64; 4], bbox: BoundingBox) -> bool {
    bbox.min_x <= bbox.max_x
        && bbox.min_y <= bbox.max_y
        && env[0] <= bbox.max_x
        && env[1] >= bbox.min_x
        && env[2] <= bbox.max_y
        && env[3] >= bbox.min_y
}

/// Build the two layers: `pts` with a 1.4 RTree index, `plain` without one.
fn build_layers(gpkg: &GeoPackage) {
    let conn = gpkg.connection();
    conn.execute_batch(
        "CREATE TABLE pts (fid INTEGER PRIMARY KEY, geom GEOMETRY);\
         CREATE TABLE plain (fid INTEGER PRIMARY KEY, geom GEOMETRY);\
         CREATE TABLE gpkg_geometry_columns (\
           table_name TEXT NOT NULL, column_name TEXT NOT NULL, \
           geometry_type_name TEXT NOT NULL, srs_id INTEGER NOT NULL, \
           z TINYINT NOT NULL, m TINYINT NOT NULL);\
         INSERT INTO gpkg_contents (table_name, data_type, srs_id) VALUES ('pts', 'features', 4326);\
         INSERT INTO gpkg_contents (table_name, data_type, srs_id) VALUES ('plain', 'features', 4326);\
         INSERT INTO gpkg_geometry_columns VALUES ('pts', 'geom', 'GEOMETRY', 4326, 0, 0);\
         INSERT INTO gpkg_geometry_columns VALUES ('plain', 'geom', 'GEOMETRY', 4326, 0, 0);",
    )
    .unwrap();
    conn.execute_batch(&triggers::create_rtree_table_sql("pts", "geom").unwrap())
        .unwrap();
    for sql in triggers::create_triggers_sql("pts", "geom", "fid").unwrap() {
        conn.execute_batch(&sql).unwrap();
    }
}

fn fids(mut it: geopackage::Features) -> Vec<i64> {
    let mut out: Vec<i64> = std::iter::from_fn(|| it.next())
        .map(|r| r.unwrap().fid())
        .collect();
    out.sort_unstable();
    out
}

fn eqp(gpkg: &GeoPackage, sql: &str, n_params: usize) -> Vec<String> {
    let conn = gpkg.connection();
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let params: Vec<rusqlite::types::Value> = (0..n_params)
        .map(|_| rusqlite::types::Value::Real(0.0))
        .collect();
    stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
        r.get::<_, String>(3)
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

#[test]
fn rtree_path_uses_vtab_full_scan_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    build_layers(&gpkg);

    let indexed = gpkg.layer("pts").unwrap();
    assert!(indexed.has_spatial_index().unwrap());
    let indexed_sql = indexed.features_in_sql().unwrap();
    assert!(
        indexed_sql.contains("rtree_pts_geom"),
        "the RTree query joins the virtual table: {indexed_sql}"
    );
    let plan = eqp(&gpkg, &indexed_sql, 4);
    assert!(
        plan.iter().any(|d| d.contains("VIRTUAL TABLE")),
        "RTree query must use the virtual table, plan was: {plan:?}"
    );
    assert!(
        plan.iter().any(|d| d.contains("INTEGER PRIMARY KEY")),
        "RTree candidates are joined back by fid, plan was: {plan:?}"
    );

    let plain = gpkg.layer("plain").unwrap();
    assert!(!plain.has_spatial_index().unwrap());
    let plain_sql = plain.features_in_sql().unwrap();
    assert!(!plain_sql.contains("rtree"));
    assert!(plain_sql.contains("ST_EnvIntersects"));
    let plain_plan = eqp(&gpkg, &plain_sql, 4);
    assert!(!plain_plan.iter().any(|d| d.contains("VIRTUAL TABLE")));
    assert!(plain_plan.iter().any(|d| d.contains("SCAN plain")));
}

#[hegel::test]
fn features_in_matches_full_scan_filter(tc: hegel::TestCase) {
    // In memory, and the setup inserts in one transaction. A case writes up to
    // 60 rows through the RTree triggers; autocommitted against a file that is
    // 60 journal cycles, and on a Windows runner it was enough for hegel to
    // report the *generator* as too slow (issue #44), since the writes happen
    // inside the case. Same reasoning as `in_memory_with_points` in
    // `spatial_index.rs`, and neither the file nor the per-row commits are what
    // this property is about: the example-based tests in this file still use
    // real files.
    let gpkg = GeoPackage::create(std::path::Path::new(":memory:")).unwrap();
    build_layers(&gpkg);
    let conn = gpkg.connection();

    // The same geometries into both tables (indexed `pts`, plain `plain`),
    // recording each true `f64` envelope for the independent oracle.
    let n = tc.draw(generators::integers::<usize>().min_value(0).max_value(30));
    let mut envelopes: Vec<(i64, Option<[f64; 4]>)> = Vec::with_capacity(n);
    let tx = conn.unchecked_transaction().unwrap();
    for fid in 1..=(n as i64) {
        let (blob, env) = draw_geom(&tc);
        for table in ["pts", "plain"] {
            tx.execute(
                &format!("INSERT INTO {table} (fid, geom) VALUES (?1, ?2)"),
                rusqlite::params![fid, blob],
            )
            .unwrap();
        }
        envelopes.push((fid, env));
    }
    tx.commit().unwrap();

    let indexed = gpkg.layer("pts").unwrap();
    let plain = gpkg.layer("plain").unwrap();
    let stored: Vec<[f64; 4]> = envelopes.iter().filter_map(|(_, env)| *env).collect();
    let bbox = draw_bbox(&tc, &stored);

    let reference: Vec<i64> = envelopes
        .iter()
        .filter(|(_, env)| env.is_some_and(|env| box_touches(env, bbox)))
        .map(|(fid, _)| *fid)
        .collect();
    tc.event(if reference.is_empty() {
        "the box selects no feature"
    } else {
        "the box selects a feature"
    });

    let via_rtree = fids(indexed.features_in(bbox).unwrap());
    let via_scan = fids(plain.features_in(bbox).unwrap());

    assert_eq!(via_rtree, reference, "RTree path diverged (bbox={bbox:?})");
    assert_eq!(
        via_scan, reference,
        "full-scan path diverged (bbox={bbox:?})"
    );
}

#[test]
fn features_in_requires_a_geometry_column() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("a.gpkg")).unwrap();
    gpkg.connection()
        .execute_batch(
            "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT);\
             INSERT INTO gpkg_contents (table_name, data_type, srs_id) \
               VALUES ('notes', 'attributes', 0);",
        )
        .unwrap();
    let layer = gpkg.attributes("notes").unwrap();
    match layer.features_in(BoundingBox::new(0.0, 0.0, 1.0, 1.0)) {
        Err(geopackage::Error::NoGeometryColumn { table_name }) => assert_eq!(table_name, "notes"),
        other => panic!("expected NoGeometryColumn, got {:?}", other.map(|_| ())),
    }
}

#[test]
fn out_of_box_value_errors_do_not_surface() {
    // A row whose DATETIME text is malformed but whose geometry lies outside
    // the query box must be skipped before value conversion on both paths; the
    // same bad value inside the box must surface as an Err.
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    let conn = gpkg.connection();
    for table in ["dt_idx", "dt_plain"] {
        conn.execute_batch(&format!(
            "CREATE TABLE {table} (fid INTEGER PRIMARY KEY, geom GEOMETRY, seen DATETIME);\
             INSERT INTO gpkg_contents (table_name, data_type, srs_id) \
               VALUES ('{table}', 'features', 4326);"
        ))
        .unwrap();
    }
    conn.execute_batch(
        "CREATE TABLE gpkg_geometry_columns (\
           table_name TEXT NOT NULL, column_name TEXT NOT NULL, \
           geometry_type_name TEXT NOT NULL, srs_id INTEGER NOT NULL, \
           z TINYINT NOT NULL, m TINYINT NOT NULL);\
         INSERT INTO gpkg_geometry_columns VALUES ('dt_idx', 'geom', 'GEOMETRY', 4326, 0, 0);\
         INSERT INTO gpkg_geometry_columns VALUES ('dt_plain', 'geom', 'GEOMETRY', 4326, 0, 0);",
    )
    .unwrap();
    conn.execute_batch(&triggers::create_rtree_table_sql("dt_idx", "geom").unwrap())
        .unwrap();
    for sql in triggers::create_triggers_sql("dt_idx", "geom", "fid").unwrap() {
        conn.execute_batch(&sql).unwrap();
    }
    for table in ["dt_idx", "dt_plain"] {
        conn.execute(
            &format!("INSERT INTO {table} VALUES (1, ?1, '2026-07-24T12:00:00.000Z')"),
            [point_blob(0.0, 0.0)],
        )
        .unwrap();
        conn.execute(
            &format!("INSERT INTO {table} VALUES (2, ?1, 'not a datetime')"),
            [point_blob(100.0, 100.0)],
        )
        .unwrap();
    }

    let near_origin = BoundingBox::new(-1.0, -1.0, 1.0, 1.0);
    let far_corner = BoundingBox::new(99.0, 99.0, 101.0, 101.0);
    for name in ["dt_idx", "dt_plain"] {
        let layer = gpkg.layer(name).unwrap();
        assert_eq!(
            layer.has_spatial_index().unwrap(),
            name == "dt_idx",
            "{name}"
        );

        let results: Vec<_> = layer.features_in(near_origin).unwrap().collect();
        let fids: Vec<i64> = results
            .iter()
            .map(|r| {
                r.as_ref()
                    .expect("bad row outside the box must not surface")
                    .fid()
            })
            .collect();
        assert_eq!(fids, vec![1], "{name}");

        let errors = layer
            .features_in(far_corner)
            .unwrap()
            .filter(|r| r.is_err())
            .count();
        assert_eq!(errors, 1, "{name}: bad row inside the box must surface");
    }
}
