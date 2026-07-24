//! GDAL interoperability and manual GeoPackage 1.4 conformance checks on files
//! this crate writes (M2 acceptance criteria 1 and 2).
//!
//! These tests need a local GDAL (`ogrinfo`/`ogr2ogr`) and so are `#[ignore]`d:
//! GDAL is available on the maintainer's machine but not in CI (see
//! `roadmap/08-testing-conformance.md`). Run them with:
//!
//! ```text
//! cargo test -p geopackage --test gdal_interop -- --ignored --nocapture
//! ```
//!
//! - [`ogrinfo_full_read_and_manual_1_4_checks`] writes a representative
//!   multi-layer file (indexed layers, a Z layer, an attributes table, every
//!   attribute type), reads it fully with `ogrinfo`, and asserts the manual 1.4
//!   checklist directly against the file: `user_version` 10400, the RTree
//!   trigger set is the 1.4 generation (`update5`/`update6`/`update7` present,
//!   no legacy `update1`/`update3`).
//! - [`gdal_roundtrip_wkb_and_values`] writes a file, copies it with `ogr2ogr`,
//!   reads the copy back with this crate, and byte-compares the geometry WKB
//!   bodies and every attribute value (criterion 2).

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the panic-family patterns in these helpers are the intended failure mechanism"
)]

use std::path::Path;
use std::process::Command;

use geo_types::{Geometry, LineString, Point, Polygon};
use geopackage::core::datetime::{Date, DateTime};
use geopackage::core::gpb::{Envelope, encode_header, parse_header};
use geopackage::core::types::{ColumnType, GeometryType, ZmFlag};
use geopackage::{ColumnSpec, Feature, GeoPackage, GeometrySpec, TableSchemaBuilder, Value};

// --- external-tool guards ---------------------------------------------------

fn tool_available(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// --- representative file -----------------------------------------------------

/// The full attribute-type column set (spec Table 1), in a fixed order. The
/// integer widths and both float widths all round-trip through this crate's
/// [`Value`] enum.
fn all_type_columns() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::new("name", ColumnType::Text(Some(64))),
        ColumnSpec::new("flag", ColumnType::Boolean),
        ColumnSpec::new("tiny", ColumnType::TinyInt),
        ColumnSpec::new("small", ColumnType::SmallInt),
        ColumnSpec::new("medium", ColumnType::MediumInt),
        ColumnSpec::new("big", ColumnType::Integer),
        ColumnSpec::new("single", ColumnType::Float),
        ColumnSpec::new("double", ColumnType::Double),
        ColumnSpec::new("when", ColumnType::DateTime),
        ColumnSpec::new("day", ColumnType::Date),
        ColumnSpec::new("payload", ColumnType::Blob(None)),
    ]
}

/// Values matching [`all_type_columns`] in order. All chosen to be exactly
/// representable so a round-trip is a byte/value identity, not an approximation.
fn all_type_values(seed: i64) -> Vec<Value> {
    vec![
        Value::Text(format!("feature-{seed}")),
        Value::Boolean(seed % 2 == 0),
        Value::Integer(seed % 100),
        Value::Integer(300 + seed),
        Value::Integer(70_000 + seed),
        Value::Integer(5_000_000_000 + seed),
        Value::Float(1.5),
        Value::Float(2.25),
        Value::DateTime(DateTime::parse_strict("2026-07-24T12:34:56.789Z").unwrap()),
        Value::Date(Date::parse("2026-07-24").unwrap()),
        Value::Blob(vec![0xDE, 0xAD, 0xBE, 0xEF]),
    ]
}

/// A little-endian ISO WKB `POINT Z` (type 1001) wrapped in a bare GPB header,
/// parsed into a `GpbGeometry` carrying a Z dimension.
fn z_point_blob(x: f64, y: f64, z: f64) -> Vec<u8> {
    let mut blob = encode_header(4326, &Envelope::None, false, false);
    blob.push(1);
    blob.extend_from_slice(&1001u32.to_le_bytes());
    blob.extend_from_slice(&x.to_le_bytes());
    blob.extend_from_slice(&y.to_le_bytes());
    blob.extend_from_slice(&z.to_le_bytes());
    blob
}

/// Write a representative GeoPackage: four feature layers (2D point, Z point,
/// linestring in EPSG:3857, polygon) each with a spatial index, plus a
/// non-spatial attributes table, all carrying every attribute type.
fn build_representative(path: &Path) {
    let gpkg = GeoPackage::create(path).unwrap();
    assert!(gpkg.add_epsg_srs(3857).unwrap(), "3857 seeded");

    // 2D point layer, all attribute types, indexed.
    {
        let builder = all_type_columns()
            .into_iter()
            .fold(
                TableSchemaBuilder::new("cities"),
                TableSchemaBuilder::column,
            )
            .geometry(GeometrySpec::new(GeometryType::Point, 4326));
        let layer = gpkg.create_layer(&builder).unwrap();
        let mut w = layer.writer().unwrap();
        for i in 0..5i64 {
            w.insert(
                None,
                &Point::new(i as f64 - 2.0, i as f64 + 1.0),
                &all_type_values(i),
            )
            .unwrap();
        }
        w.commit().unwrap();
        layer.create_spatial_index().unwrap();
    }

    // Z point layer (z mandatory), indexed.
    {
        let builder = TableSchemaBuilder::new("beacons")
            .column(ColumnSpec::new("name", ColumnType::Text(None)))
            .geometry(GeometrySpec::new(GeometryType::Point, 4326).z(ZmFlag::Mandatory));
        let layer = gpkg.create_layer(&builder).unwrap();
        let mut w = layer.writer().unwrap();
        for i in 0..4i64 {
            let blob = z_point_blob(i as f64, i as f64 * 2.0, 100.0 + i as f64);
            let g = geopackage::core::GpbGeometry::parse(&blob).unwrap();
            w.insert(None, &g, &[Value::Text(format!("beacon-{i}"))])
                .unwrap();
        }
        w.commit().unwrap();
        layer.create_spatial_index().unwrap();
    }

    // Linestring layer in EPSG:3857, indexed.
    {
        let builder = TableSchemaBuilder::new("roads")
            .column(ColumnSpec::new("ref", ColumnType::Text(None)))
            .geometry(GeometrySpec::new(GeometryType::LineString, 3857));
        let layer = gpkg.create_layer(&builder).unwrap();
        let mut w = layer.writer().unwrap();
        for i in 0..3i64 {
            let f = i as f64 * 1000.0;
            let line = LineString::from(vec![(f, f), (f + 500.0, f + 250.0), (f + 900.0, f)]);
            w.insert(None, &line, &[Value::Text(format!("R{i}"))])
                .unwrap();
        }
        w.commit().unwrap();
        layer.create_spatial_index().unwrap();
    }

    // Polygon layer, indexed.
    {
        let builder = TableSchemaBuilder::new("zones")
            .column(ColumnSpec::new("label", ColumnType::Text(None)))
            .geometry(GeometrySpec::new(GeometryType::Polygon, 4326));
        let layer = gpkg.create_layer(&builder).unwrap();
        let mut w = layer.writer().unwrap();
        for i in 0..3i64 {
            let b = i as f64;
            let ring = LineString::from(vec![
                (b, b),
                (b + 1.0, b),
                (b + 1.0, b + 1.0),
                (b, b + 1.0),
                (b, b),
            ]);
            w.insert(
                None,
                &Polygon::new(ring, Vec::new()),
                &[Value::Text(format!("Z{i}"))],
            )
            .unwrap();
        }
        w.commit().unwrap();
        layer.create_spatial_index().unwrap();
    }

    // Non-spatial attributes table with every attribute type.
    {
        let builder = all_type_columns()
            .into_iter()
            .fold(TableSchemaBuilder::new("notes"), TableSchemaBuilder::column);
        let layer = gpkg.create_attributes_table(&builder).unwrap();
        let mut w = layer.writer().unwrap();
        for i in 0..3i64 {
            w.insert_row(None, &all_type_values(i)).unwrap();
        }
        w.commit().unwrap();
    }

    gpkg.close().unwrap();
}

// --- (c) ogrinfo full read + manual 1.4 checks ------------------------------

#[test]
#[ignore = "requires GDAL (ogrinfo); reads a file we write and checks 1.4 conformance"]
fn ogrinfo_full_read_and_manual_1_4_checks() {
    if !tool_available("ogrinfo") {
        eprintln!("skipping: ogrinfo not found");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("representative.gpkg");
    build_representative(&path);

    // ogrinfo full read of every layer and every feature. A clean exit with no
    // "ERROR" on stderr/stdout is the "opens correctly in ogrinfo" check.
    let out = Command::new("ogrinfo")
        .arg("-al")
        .arg(&path)
        .output()
        .expect("run ogrinfo -al");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "ogrinfo -al failed: {stderr}\n{stdout}"
    );
    assert!(
        !stderr.to_ascii_uppercase().contains("ERROR"),
        "ogrinfo reported errors: {stderr}"
    );
    for layer in ["cities", "beacons", "roads", "zones", "notes"] {
        assert!(
            stdout.contains(layer),
            "ogrinfo output missing layer {layer}:\n{stdout}"
        );
    }
    // The Z layer is reported as a 3D/Point25D geometry.
    assert!(
        stdout.contains("Point25D") || stdout.contains("Point Z") || stdout.contains("3D Point"),
        "beacons not reported as a Z point layer:\n{stdout}"
    );

    // Manual GeoPackage 1.4 checklist, read straight from the file.
    let gpkg = GeoPackage::open(&path).unwrap();
    let conn = gpkg.connection();

    let user_version: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(user_version, 10400, "user_version is 1.4 (0x2870 = 10400)");

    let triggers: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'trigger' ORDER BY name")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert!(
        !triggers.is_empty(),
        "indexed layers install RTree triggers"
    );
    let has = |suffix: &str| triggers.iter().any(|t| t.ends_with(suffix));
    // The 1.4 set is present …
    assert!(has("_update5"), "update5 trigger present: {triggers:?}");
    assert!(has("_update6"), "update6 trigger present: {triggers:?}");
    assert!(has("_update7"), "update7 trigger present: {triggers:?}");
    // … and the pre-1.4 triggers are absent.
    assert!(!has("_update1"), "no legacy update1 trigger: {triggers:?}");
    assert!(!has("_update3"), "no legacy update3 trigger: {triggers:?}");

    eprintln!(
        "ogrinfo + manual 1.4 checks passed: user_version=10400, {} triggers, 1.4 generation",
        triggers.len()
    );
}

/// Emit the representative file to `$GPKG_REPRESENTATIVE_OUT` so the external
/// validator scripts (`scripts/run_ets_gpkg12.sh`, `scripts/run_pdok_validator.sh`)
/// can run against a file this crate wrote. No external tool needed; ignored so
/// it never runs in CI.
#[test]
#[ignore = "writes the representative file to $GPKG_REPRESENTATIVE_OUT for the external validators"]
fn emit_representative_file() {
    let out = std::env::var("GPKG_REPRESENTATIVE_OUT")
        .unwrap_or_else(|_| "representative.gpkg".to_owned());
    let path = std::path::PathBuf::from(&out);
    build_representative(&path);
    eprintln!("wrote representative GeoPackage: {}", path.display());
}

// --- (d) GDAL round-trip: WKB body + value byte-compare ----------------------

/// One layer written with this crate, exercising point/line/polygon geometries
/// and the robustly-round-tripping attribute types.
fn build_roundtrip_source(path: &Path) {
    let gpkg = GeoPackage::create(path).unwrap();
    let builder = TableSchemaBuilder::new("shapes")
        .column(ColumnSpec::new("name", ColumnType::Text(None)))
        .column(ColumnSpec::new("count", ColumnType::Integer))
        .column(ColumnSpec::new("ratio", ColumnType::Double))
        .column(ColumnSpec::new("flag", ColumnType::Boolean))
        .geometry(GeometrySpec::new(GeometryType::Geometry, 4326));
    let layer = gpkg.create_layer(&builder).unwrap();
    let mut w = layer.writer().unwrap();
    let geoms: Vec<Geometry<f64>> = vec![
        Geometry::Point(Point::new(1.0, 2.0)),
        Geometry::LineString(LineString::from(vec![(0.0, 0.0), (3.0, 4.0), (6.0, 0.0)])),
        Geometry::Polygon(Polygon::new(
            LineString::from(vec![
                (0.0, 0.0),
                (2.0, 0.0),
                (2.0, 2.0),
                (0.0, 2.0),
                (0.0, 0.0),
            ]),
            Vec::new(),
        )),
    ];
    for (i, geom) in geoms.into_iter().enumerate() {
        let seed = i as i64;
        w.insert(
            None,
            &geom,
            &[
                Value::Text(format!("shape-{seed}")),
                Value::Integer(seed * 10),
                Value::Float(0.5 + f64::from(u8::try_from(i).unwrap())),
                Value::Boolean(seed % 2 == 0),
            ],
        )
        .unwrap();
    }
    w.commit().unwrap();
    layer.create_spatial_index().unwrap();
    gpkg.close().unwrap();
}

/// The WKB body of a feature's geometry: the GPB blob with its header stripped.
fn wkb_body(feature: &Feature) -> Vec<u8> {
    let blob = feature.geometry_bytes().expect("geometry present");
    let (_, offset) = parse_header(blob).expect("parse GPB header");
    blob[offset..].to_vec()
}

/// Features of a layer, keyed by their `name` attribute (a stable identity
/// across the round-trip, since `ogr2ogr` may not preserve fid).
fn features_by_name(gpkg: &GeoPackage, table: &str) -> std::collections::BTreeMap<String, Feature> {
    let layer = gpkg.layer(table).unwrap();
    let mut map = std::collections::BTreeMap::new();
    for feature in layer.features().unwrap() {
        let feature = feature.unwrap();
        let Some(Value::Text(name)) = feature.value("name").cloned() else {
            panic!("every shape has a text name");
        };
        map.insert(name, feature);
    }
    map
}

#[test]
#[ignore = "requires GDAL (ogr2ogr); write -> ogr2ogr copy -> read back, byte-compare"]
fn gdal_roundtrip_wkb_and_values() {
    if !tool_available("ogr2ogr") {
        eprintln!("skipping: ogr2ogr not found");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("source.gpkg");
    let dst = dir.path().join("dest.gpkg");
    build_roundtrip_source(&src);

    let status = Command::new("ogr2ogr")
        .arg("-f")
        .arg("GPKG")
        .arg(&dst)
        .arg(&src)
        .status()
        .expect("run ogr2ogr");
    assert!(status.success(), "ogr2ogr copy failed");

    let src_gpkg = GeoPackage::open(&src).unwrap();
    let dst_gpkg = GeoPackage::open(&dst).unwrap();

    let src_features = features_by_name(&src_gpkg, "shapes");
    let dst_features = features_by_name(&dst_gpkg, "shapes");
    assert_eq!(
        src_features.len(),
        dst_features.len(),
        "same feature count after round-trip"
    );
    assert_eq!(src_features.len(), 3, "three shapes written");

    for (name, src_feature) in &src_features {
        let dst_feature = dst_features
            .get(name)
            .unwrap_or_else(|| panic!("dest missing shape {name}"));

        // Geometry: byte-identical WKB body (the GPB header envelope may differ,
        // the body must not).
        assert_eq!(
            wkb_body(src_feature),
            wkb_body(dst_feature),
            "WKB body mismatch for {name}"
        );

        // Every non-geometry value matches by name.
        for column in ["name", "count", "ratio", "flag"] {
            assert_eq!(
                src_feature.value(column),
                dst_feature.value(column),
                "value mismatch for {name}.{column}"
            );
        }
    }

    eprintln!(
        "GDAL round-trip passed: {} shapes, WKB bodies and all values byte-identical",
        src_features.len()
    );
}
