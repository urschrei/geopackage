//! `Layer::features_in`: the RTree-accelerated path, the full-scan fallback,
//! and a seeded property test proving they return identical rows.
//!
//! The generator is a hand-rolled deterministic SplitMix64 rather than
//! `proptest`: the property here is a set equality with an independent oracle
//! (envelopes computed directly from the coordinates we generated), which does
//! not benefit much from shrinking, and a zero-dependency seeded generator
//! keeps the dependency tree honest and CI reproducible. Failing seeds print,
//! so a regression is replayable. Decision recorded in
//! `roadmap/03-m1-read-path.md`.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geopackage::core::gpb::{Envelope, encode_header};
use geopackage::core::triggers;
use geopackage::{BoundingBox, GeoPackage};

/// A deterministic SplitMix64 PRNG (public-domain algorithm).
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A full-mantissa `f64` in `[lo, hi)` — almost never exactly representable
    /// in `f32`, which is the point (it stresses the RTree's f32 rounding).
    fn f64_in(&mut self, lo: f64, hi: f64) -> f64 {
        let u = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        lo + u * (hi - lo)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// A GPB point blob with no header envelope (forcing traversal on read).
fn point_blob(x: f64, y: f64) -> Vec<u8> {
    let mut b = encode_header(4326, &Envelope::None, false, false);
    b.push(1);
    b.extend_from_slice(&1u32.to_le_bytes());
    b.extend_from_slice(&x.to_le_bytes());
    b.extend_from_slice(&y.to_le_bytes());
    b
}

/// A GPB linestring blob with no header envelope.
fn linestring_blob(pts: &[(f64, f64)]) -> Vec<u8> {
    let mut b = encode_header(4326, &Envelope::None, false, false);
    b.push(1);
    b.extend_from_slice(&2u32.to_le_bytes());
    b.extend_from_slice(&(pts.len() as u32).to_le_bytes());
    for (x, y) in pts {
        b.extend_from_slice(&x.to_le_bytes());
        b.extend_from_slice(&y.to_le_bytes());
    }
    b
}

/// A random geometry blob and its true `f64` envelope `[min_x, max_x, min_y, max_y]`.
fn random_geom(rng: &mut Rng) -> (Vec<u8>, [f64; 4]) {
    let coord = |rng: &mut Rng| rng.f64_in(-100.0, 100.0);
    if rng.below(2) == 0 {
        let (x, y) = (coord(rng), coord(rng));
        (point_blob(x, y), [x, x, y, y])
    } else {
        let n = 2 + rng.below(3) as usize;
        let pts: Vec<(f64, f64)> = (0..n).map(|_| (coord(rng), coord(rng))).collect();
        let mut env = [
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ];
        for (x, y) in &pts {
            env[0] = env[0].min(*x);
            env[1] = env[1].max(*x);
            env[2] = env[2].min(*y);
            env[3] = env[3].max(*y);
        }
        (linestring_blob(&pts), env)
    }
}

/// The independent oracle: does envelope `env` intersect `bbox`, inclusive?
fn env_intersects(env: [f64; 4], bbox: BoundingBox) -> bool {
    env[0] <= bbox.max_x && env[1] >= bbox.min_x && env[2] <= bbox.max_y && env[3] >= bbox.min_y
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
    let plain_plan = eqp(&gpkg, &plain_sql, 0);
    assert!(!plain_plan.iter().any(|d| d.contains("VIRTUAL TABLE")));
    assert!(plain_plan.iter().any(|d| d.contains("SCAN plain")));
}

#[test]
fn features_in_matches_full_scan_filter_property() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    build_layers(&gpkg);
    let conn = gpkg.connection();

    let mut total_hits = 0usize;
    for seed in 0..40u64 {
        let mut rng = Rng(seed.wrapping_mul(0x0123_4567_89AB_CDEF).wrapping_add(1));
        conn.execute_batch("DELETE FROM pts; DELETE FROM plain;")
            .unwrap();

        // Insert the same geometries into both tables, recording true envelopes.
        let n = 15 + rng.below(15) as usize;
        let mut envelopes: Vec<(i64, [f64; 4])> = Vec::with_capacity(n);
        for fid in 1..=(n as i64) {
            let (blob, env) = random_geom(&mut rng);
            for table in ["pts", "plain"] {
                conn.execute(
                    &format!("INSERT INTO {table} (fid, geom) VALUES (?1, ?2)"),
                    rusqlite::params![fid, blob],
                )
                .unwrap();
            }
            envelopes.push((fid, env));
        }

        let indexed = gpkg.layer("pts").unwrap();
        let plain = gpkg.layer("plain").unwrap();

        for q in 0..12u64 {
            // A mix of random boxes and boxes whose edges sit exactly on a
            // stored coordinate (the inclusive-boundary + f32-rounding case).
            let bbox = if q < 8 || envelopes.is_empty() {
                let min_x = rng.f64_in(-110.0, 110.0);
                let min_y = rng.f64_in(-110.0, 110.0);
                BoundingBox::new(
                    min_x,
                    min_y,
                    min_x + rng.f64_in(0.0, 60.0),
                    min_y + rng.f64_in(0.0, 60.0),
                )
            } else {
                let (_, env) = envelopes[rng.below(envelopes.len() as u64) as usize];
                // A degenerate box exactly on the feature's min corner.
                BoundingBox::new(env[0], env[2], env[0], env[2])
            };

            let reference: Vec<i64> = {
                let mut r: Vec<i64> = envelopes
                    .iter()
                    .filter(|(_, env)| env_intersects(*env, bbox))
                    .map(|(fid, _)| *fid)
                    .collect();
                r.sort_unstable();
                r
            };
            total_hits += reference.len();

            let via_rtree = fids(indexed.features_in(bbox).unwrap());
            let via_scan = fids(plain.features_in(bbox).unwrap());

            assert_eq!(
                via_rtree, reference,
                "RTree path diverged (seed={seed}, query={q}, bbox={bbox:?})"
            );
            assert_eq!(
                via_scan, reference,
                "full-scan path diverged (seed={seed}, query={q}, bbox={bbox:?})"
            );
        }
    }
    assert!(total_hits > 0, "the property test never matched anything");
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
