//! Cross-implementation corpus verification.
//!
//! For every committed fixture under `tests/fixtures`, this suite opens the
//! file, enumerates its layers, iterates every feature fully, and checks our
//! read against a snapshot derived from GDAL's own read of the same bytes
//! (`ogrinfo -json -features`, see `scripts/generate_fixtures.py`). The
//! snapshot is the oracle: feature counts, the fid sequence, per-feature
//! geometry types and coordinates, and per-feature attribute values must all
//! agree. The snapshots are committed, so the suite runs with **no GDAL
//! installed**; one `#[ignore]`d test regenerates them live and diffs.
//!
//! ## Representation normalisations (honest, documented here at the site)
//!
//! GDAL and this crate model the same stored bytes differently; each difference
//! below is a representation gap, not a value disagreement, and is reconciled
//! rather than papered over. Where our read genuinely disagreed with GDAL's
//! beyond representation, that would be a bug to fix, not to normalise away.
//!
//! - **Dates**: GDAL's JSON prints `YYYY/MM/DD`; our canonical form is
//!   `YYYY-MM-DD`. Reconciled by replacing `/` with `-` and parsing to the same
//!   [`Date`].
//! - **Datetimes**: GDAL prints `YYYY/MM/DD HH:MM:SS[.fff]+00` (e.g.
//!   `2026/07/24 12:00:00+00`), omitting the fractional part when zero; our
//!   canonical form is `YYYY-MM-DDTHH:MM:SS.SSSZ`. Reconciled by mapping `/`→`-`
//!   and the date/time separator space→`T`, then parsing leniently to the same
//!   [`DateTime`] (a `+00` offset and a `Z` both denote UTC / offset zero, and a
//!   missing fraction parses as zero nanoseconds, so the two forms compare
//!   equal component-for-component).
//! - **Floats**: GDAL's JSON writer prints doubles at limited precision, so
//!   `Real` values are compared with a small relative epsilon rather than for
//!   exact bits. (The fixture values are chosen exactly representable, and the
//!   `FLOAT` column's values are additionally f32-exact, so in practice they
//!   match to the bit; the epsilon only guards the general case.)
//! - **Booleans**: a GPKG `BOOLEAN` column arrives from GDAL as a JSON boolean;
//!   we read it as [`Value::Boolean`].
//! - **Binary**: GDAL's `-json` omits non-NULL binary values entirely, so the
//!   generator recovers their bytes via GDAL's SQL `hex()` and stores them as a
//!   hex string in the snapshot (a NULL blob stays JSON null). Decoded here and
//!   compared to [`Value::Blob`] byte-for-byte.
//! - **Empty vs NULL geometry**: GDAL renders an *empty* geometry as its type
//!   with an empty coordinate array (not JSON null); we read it as a present but
//!   [`GpbGeometry`] whose `is_empty()` is true. A JSON-null geometry maps to
//!   our `Ok(None)`.
//! - **Z**: `GpbGeometry::to_geo` keeps only X and Y, so coordinate checks are
//!   XY only; the Z dimension of the XYZ fixture is confirmed through GDAL's
//!   `PointZ` layer type separately.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    clippy::string_slice,
    clippy::indexing_slicing,
    clippy::unreachable,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the panic-family patterns in these helpers are the intended failure mechanism"
)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use geopackage::core::datetime::{Date, DateTime};
use geopackage::{BoundingBox, Feature, GeoPackage, GpkgVersion, OpenWarning, ValueRef};
use serde_json::Value as Json;

// --- fixture / snapshot loading ---------------------------------------------

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

/// Copy a fixture into a temp dir before opening it, so a committed file is
/// never mutated and no `-wal`/`-shm` sidecar lands in the source tree.
fn copy_to_temp(name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join(name);
    std::fs::copy(fixtures_dir().join(name), &dst).unwrap();
    (dir, dst)
}

fn load_snapshot(stem: &str) -> Json {
    let path = fixtures_dir().join(format!("{stem}.expected.json"));
    let text = std::fs::read_to_string(&path).unwrap();
    serde_json::from_str(&text).unwrap()
}

// --- comparison helpers ------------------------------------------------------

/// Relative-epsilon float comparison (see the module note on GDAL's printer).
fn float_eq(a: f64, b: f64) -> bool {
    if a == b {
        return true;
    }
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

fn hex_decode(s: &str) -> Vec<u8> {
    assert!(s.len().is_multiple_of(2), "odd-length hex {s:?}");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
        .collect()
}

fn has_warning(warnings: &[OpenWarning], tag: &str) -> bool {
    warnings.iter().any(|w| {
        matches!(
            (tag, w),
            (
                "LegacyApplicationId",
                OpenWarning::LegacyApplicationId { .. }
            ) | (
                "MissingGeometryColumns",
                OpenWarning::MissingGeometryColumns
            ) | (
                "TableNameCaseMismatch",
                OpenWarning::TableNameCaseMismatch { .. }
            )
        )
    })
}

/// Check one attribute value: GDAL's `(type, subtype, json)` against our read.
fn check_property(ftype: &str, subtype: Option<&str>, gdal: &Json, ours: ValueRef<'_>, ctx: &str) {
    if gdal.is_null() {
        assert_eq!(ours, ValueRef::Null, "{ctx}: expected NULL");
        return;
    }
    match ftype {
        "Integer" | "Integer64" if subtype == Some("Boolean") => {
            let b = gdal
                .as_bool()
                .unwrap_or_else(|| panic!("{ctx}: not a bool"));
            assert_eq!(ours, ValueRef::Boolean(b), "{ctx}: boolean");
        }
        "Integer" | "Integer64" => {
            let i = gdal.as_i64().unwrap_or_else(|| panic!("{ctx}: not an int"));
            assert_eq!(ours, ValueRef::Integer(i), "{ctx}: integer");
        }
        "Real" => {
            let f = gdal.as_f64().unwrap_or_else(|| panic!("{ctx}: not a real"));
            match ours {
                ValueRef::Float(o) => assert!(float_eq(o, f), "{ctx}: {o} != {f}"),
                other => panic!("{ctx}: expected Float, got {other:?}"),
            }
        }
        "String" => {
            let s = gdal
                .as_str()
                .unwrap_or_else(|| panic!("{ctx}: not a string"));
            assert_eq!(ours, ValueRef::Text(s), "{ctx}: text");
        }
        "Binary" => {
            let hex = gdal.as_str().unwrap_or_else(|| panic!("{ctx}: not hex"));
            assert_eq!(ours, ValueRef::Blob(&hex_decode(hex)), "{ctx}: blob");
        }
        "Date" => {
            // GDAL YYYY/MM/DD -> canonical YYYY-MM-DD (module note).
            let canon = gdal.as_str().unwrap().replace('/', "-");
            let expected = Date::parse(&canon).expect("GDAL date parses");
            assert_eq!(ours, ValueRef::Date(expected), "{ctx}: date");
        }
        "DateTime" => {
            // GDAL YYYY/MM/DD HH:MM:SS[.fff]+00 -> ISO, then parse leniently
            // (module note). `+00` and `Z` both mean offset zero.
            let canon = gdal.as_str().unwrap().replace('/', "-").replace(' ', "T");
            let expected = DateTime::parse_lenient(&canon).expect("GDAL datetime parses");
            assert_eq!(ours, ValueRef::DateTime(expected), "{ctx}: datetime");
        }
        other => panic!("{ctx}: unhandled GDAL field type {other}"),
    }
}

/// Flatten a GDAL GeoJSON `coordinates` value into a list of `[x, y]` pairs
/// (Z, if present, is dropped; see the module note).
fn flatten_gdal(v: &Json, out: &mut Vec<[f64; 2]>) {
    let Some(arr) = v.as_array() else { return };
    if arr.first().is_some_and(Json::is_number) {
        out.push([arr[0].as_f64().unwrap(), arr[1].as_f64().unwrap()]);
    } else {
        for e in arr {
            flatten_gdal(e, out);
        }
    }
}

/// Flatten a `geo_types` geometry into `[x, y]` pairs, in the same order GDAL's
/// GeoJSON walks them (both derive from the identical stored WKB, so the
/// sequences line up vertex-for-vertex).
fn flatten_geo(g: &geo_types::Geometry<f64>, out: &mut Vec<[f64; 2]>) {
    use geo_types::Geometry;
    fn line(ls: &geo_types::LineString<f64>, out: &mut Vec<[f64; 2]>) {
        out.extend(ls.0.iter().map(|c| [c.x, c.y]));
    }
    fn polygon(p: &geo_types::Polygon<f64>, out: &mut Vec<[f64; 2]>) {
        line(p.exterior(), out);
        for interior in p.interiors() {
            line(interior, out);
        }
    }
    match g {
        Geometry::Point(p) => out.push([p.x(), p.y()]),
        Geometry::Line(l) => out.extend([[l.start.x, l.start.y], [l.end.x, l.end.y]]),
        Geometry::LineString(ls) => line(ls, out),
        Geometry::Polygon(p) => polygon(p, out),
        Geometry::MultiPoint(mp) => out.extend(mp.0.iter().map(|p| [p.x(), p.y()])),
        Geometry::MultiLineString(mls) => mls.0.iter().for_each(|ls| line(ls, out)),
        Geometry::MultiPolygon(mp) => mp.0.iter().for_each(|p| polygon(p, out)),
        Geometry::GeometryCollection(gc) => gc.0.iter().for_each(|m| flatten_geo(m, out)),
        Geometry::Rect(r) => out.extend([[r.min().x, r.min().y], [r.max().x, r.max().y]]),
        Geometry::Triangle(t) => out.extend(t.to_array().map(|c| [c.x, c.y])),
    }
}

fn check_geometry(gdal: &Json, feature: &Feature, ctx: &str) {
    if gdal.is_null() {
        assert!(
            feature.geometry().unwrap().is_none(),
            "{ctx}: expected no geometry"
        );
        return;
    }
    let ours = feature
        .geometry()
        .unwrap()
        .unwrap_or_else(|| panic!("{ctx}: geometry missing"));

    let gdal_type = gdal["type"].as_str().unwrap().to_uppercase();
    assert_eq!(
        ours.geometry_type().as_str(),
        gdal_type,
        "{ctx}: geometry type"
    );

    let mut expected = Vec::new();
    flatten_gdal(&gdal["coordinates"], &mut expected);
    let mut got = Vec::new();
    if let Some(g) = ours.to_geo() {
        flatten_geo(&g, &mut got);
    }
    assert_eq!(got.len(), expected.len(), "{ctx}: coordinate count");
    for (i, (a, b)) in got.iter().zip(&expected).enumerate() {
        assert!(
            float_eq(a[0], b[0]) && float_eq(a[1], b[1]),
            "{ctx}: coordinate {i}: {a:?} != {b:?}"
        );
    }
    if expected.is_empty() {
        assert!(ours.is_empty(), "{ctx}: geometry should read as empty");
    }
}

fn check_layer(gpkg: &GeoPackage, stem: &str, ls: &Json) {
    let name = ls["name"].as_str().unwrap();
    let ctx = format!("{stem}/{name}");
    let is_feature = !ls["geometry_type"].is_null();

    let layer = if is_feature {
        gpkg.layer(name)
            .unwrap_or_else(|e| panic!("{ctx}: open feature layer: {e:?}"))
    } else {
        gpkg.attributes(name)
            .unwrap_or_else(|e| panic!("{ctx}: open attribute layer: {e:?}"))
    };

    // Cross-implementation index compatibility: our reader must agree with what
    // GDAL wrote (an RTree it built, not one of ours).
    assert_eq!(
        layer.has_spatial_index().unwrap(),
        ls["spatially_indexed"].as_bool().unwrap(),
        "{ctx}: spatial index presence"
    );

    // Iterate every feature; a per-row error here is a corpus failure.
    let features: Vec<Feature> = layer
        .features()
        .unwrap()
        .map(|r| r.unwrap_or_else(|e| panic!("{ctx}: feature read error: {e:?}")))
        .collect();

    let expected = ls["features"].as_array().unwrap();
    assert_eq!(
        features.len(),
        ls["feature_count"].as_u64().unwrap() as usize,
        "{ctx}: feature count vs GDAL featureCount"
    );
    assert_eq!(
        features.len(),
        expected.len(),
        "{ctx}: snapshot feature count"
    );

    let our_fids: Vec<i64> = features.iter().map(Feature::fid).collect();
    let exp_fids: Vec<i64> = expected
        .iter()
        .map(|f| f["fid"].as_i64().unwrap())
        .collect();
    assert_eq!(our_fids, exp_fids, "{ctx}: fid sequence");

    let fields = ls["fields"].as_array().unwrap();
    for (feature, ef) in features.iter().zip(expected) {
        let fctx = format!("{ctx}#{}", feature.fid());
        check_geometry(&ef["geometry"], feature, &fctx);
        for field in fields {
            let fname = field["name"].as_str().unwrap();
            let gdal = ef["properties"].get(fname).cloned().unwrap_or(Json::Null);
            let ours = feature
                .value(fname)
                .unwrap_or_else(|| panic!("{fctx}: missing value column {fname}"));
            check_property(
                field["type"].as_str().unwrap(),
                field["subtype"].as_str(),
                &gdal,
                ours,
                &format!("{fctx}.{fname}"),
            );
        }
    }
}

/// Open a fixture per its snapshot, check identity + warnings, then every layer.
fn check_fixture(stem: &str) {
    let snap = load_snapshot(stem);
    let fixture = snap["fixture"].as_str().unwrap();
    let (_dir, path) = copy_to_temp(fixture);

    let gpkg = match snap["open"].as_str().unwrap() {
        "strict" => GeoPackage::open_read_only(&path).unwrap(),
        "lenient" => GeoPackage::open_lenient(&path).unwrap(),
        other => panic!("{stem}: unknown open mode {other}"),
    };

    let app_id = snap["application_id"].as_u64().unwrap() as u32;
    let user_version = snap["user_version"].as_u64().unwrap() as u32;
    let expected = GpkgVersion::from_pragmas(app_id, user_version).expect("pragmas classify");
    assert_eq!(gpkg.version(), expected, "{stem}: version");

    for tag in snap["expect_warnings"].as_array().unwrap() {
        let tag = tag.as_str().unwrap();
        assert!(
            has_warning(gpkg.open_warnings(), tag),
            "{stem}: expected open warning {tag}, got {:?}",
            gpkg.open_warnings()
        );
    }

    for layer in snap["layers"].as_array().unwrap() {
        check_layer(&gpkg, stem, layer);
    }
}

// --- one test per fixture ----------------------------------------------------

#[test]
fn multilayer_1_4() {
    check_fixture("gdal_multilayer_1_4");
}

#[test]
fn points_1_2() {
    check_fixture("gdal_points_1_2");
}

#[test]
fn attributes_spread() {
    check_fixture("attributes_spread");
}

#[test]
fn legacy_gp10() {
    check_fixture("legacy_gp10");
}

#[test]
fn case_mismatch() {
    check_fixture("case_mismatch");
}

#[test]
fn qgis_lines() {
    // Written by QGIS (native:savefeatures) rather than ogr2ogr: a second
    // producer's container defaults, read against GDAL's view of the same
    // bytes. Generated only where QGIS is installed; the committed fixture
    // tests everywhere.
    check_fixture("qgis_lines");
}

// --- cross-implementation spatial-index query --------------------------------

/// A GDAL-built RTree must serve `features_in` correctly through our reader:
/// the indexed points layer reports `has_spatial_index()`, our query targets
/// GDAL's `rtree_points_geom` vtab, and the rows returned equal an independent
/// full-scan oracle, while the non-indexed lines layer reports no index yet
/// still answers `features_in` by full scan.
#[test]
fn gdal_rtree_features_in_is_correct() {
    let (_dir, path) = copy_to_temp("gdal_multilayer_1_4.gpkg");
    let gpkg = GeoPackage::open_read_only(&path).unwrap();

    let points = gpkg.layer("points").unwrap();
    assert!(
        points.has_spatial_index().unwrap(),
        "points is GDAL-indexed"
    );
    assert!(
        points
            .features_in_sql()
            .unwrap()
            .contains("rtree_points_geom"),
        "the query must target GDAL's RTree vtab"
    );

    // A box that selects a strict, non-empty subset of the three points.
    let bbox = BoundingBox::new(0.0, -5.0, 5.0, 5.0);

    // Independent oracle: full scan, filter on the point's own coordinates.
    let all: Vec<Feature> = points
        .features()
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let oracle: BTreeSet<i64> = all
        .iter()
        .filter(|f| {
            let geo = f.geometry().unwrap().unwrap().to_geo().unwrap();
            let geo_types::Geometry::Point(p) = geo else {
                unreachable!()
            };
            (bbox.min_x..=bbox.max_x).contains(&p.x()) && (bbox.min_y..=bbox.max_y).contains(&p.y())
        })
        .map(Feature::fid)
        .collect();
    assert!(
        !oracle.is_empty() && oracle.len() < all.len(),
        "box selects a subset"
    );

    let via_index: BTreeSet<i64> = points
        .features_in(bbox)
        .unwrap()
        .map(|r| r.unwrap().fid())
        .collect();
    assert_eq!(
        via_index, oracle,
        "RTree result equals the full-scan oracle"
    );

    // The non-indexed layer still answers by full scan.
    let lines = gpkg.layer("lines").unwrap();
    assert!(!lines.has_spatial_index().unwrap(), "lines is not indexed");
    let hit = lines.features_in(BoundingBox::new(-100.0, -100.0, 100.0, 100.0));
    assert_eq!(
        hit.unwrap().count(),
        2,
        "full-scan features_in returns both lines"
    );
}

// --- live regeneration guard (needs GDAL) ------------------------------------

fn gdal_available() -> bool {
    Command::new("ogr2ogr")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Regenerate the snapshots with the local GDAL and check they still match the
/// committed ones (ignoring the version-stamped `_provenance` block). Ignored
/// by default: run with `cargo test -- --ignored` on a machine with GDAL.
#[test]
#[ignore = "requires GDAL: regenerates snapshots and diffs against the committed ones"]
fn snapshots_match_live_gdal() {
    if !gdal_available() {
        eprintln!("skipping snapshots_match_live_gdal: GDAL (ogr2ogr) not found");
        return;
    }
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let script = repo_root.join("scripts").join("generate_fixtures.py");
    let out = tempfile::tempdir().unwrap();

    let status = Command::new("python3")
        .arg(&script)
        .env("GEOPACKAGE_FIXTURES_DIR", out.path())
        .status()
        .expect("run generate_fixtures.py");
    assert!(status.success(), "generator exited non-zero");

    let strip_provenance = |mut v: Json| -> Json {
        if let Some(obj) = v.as_object_mut() {
            obj.remove("_provenance");
        }
        v
    };

    for entry in std::fs::read_dir(fixtures_dir()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let name = path.file_name().unwrap().to_str().unwrap();
        let committed: Json =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let fresh_path = out.path().join(name);
        if !fresh_path.exists() {
            // The QGIS-written fixture regenerates only where QGIS is
            // installed; its committed snapshot still gates in the always-on
            // tests above.
            eprintln!("skipping {name}: this machine's generator run did not produce it");
            continue;
        }
        let fresh: Json =
            serde_json::from_str(&std::fs::read_to_string(&fresh_path).unwrap()).unwrap();
        assert_eq!(
            strip_provenance(committed),
            strip_provenance(fresh),
            "snapshot {name} drifted from live GDAL"
        );
    }
}
