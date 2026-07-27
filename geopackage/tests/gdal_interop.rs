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
use geopackage::core::TileFormat;
use geopackage::core::datetime::{Date, DateTime};
use geopackage::core::gpb::{Envelope, encode_header, parse_header};
use geopackage::core::tiles::{TileCoord, TileMatrixSet, ZoomLadder};
use geopackage::core::types::{ColumnType, GeometryType, ZmFlag};
use geopackage::{
    ColumnSpec, Feature, GeoPackage, GeometrySpec, TableSchemaBuilder, TilePyramidBuilder, Value,
    ValueRef,
};

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
            let values = all_type_values(i);
            let binds: Vec<ValueRef<'_>> = values.iter().map(ValueRef::from).collect();
            w.insert(None, &Point::new(i as f64 - 2.0, i as f64 + 1.0), &binds)
                .unwrap();
        }
        w.commit().unwrap();
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
            let name = format!("beacon-{i}");
            w.insert(None, &g, &[ValueRef::Text(&name)]).unwrap();
        }
        w.commit().unwrap();
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
            let name = format!("R{i}");
            w.insert(None, &line, &[ValueRef::Text(&name)]).unwrap();
        }
        w.commit().unwrap();
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
                &[ValueRef::Text(&format!("Z{i}"))],
            )
            .unwrap();
        }
        w.commit().unwrap();
    }

    // Non-spatial attributes table with every attribute type.
    {
        let builder = all_type_columns()
            .into_iter()
            .fold(TableSchemaBuilder::new("notes"), TableSchemaBuilder::column);
        let layer = gpkg.create_attributes_table(&builder).unwrap();
        let mut w = layer.writer().unwrap();
        for i in 0..3i64 {
            let values = all_type_values(i);
            let binds: Vec<ValueRef<'_>> = values.iter().map(ValueRef::from).collect();
            w.insert_row(None, &binds).unwrap();
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
        let name = format!("shape-{seed}");
        w.insert(
            None,
            &geom,
            &[
                ValueRef::Text(&name),
                ValueRef::Integer(seed * 10),
                ValueRef::Float(0.5 + f64::from(u8::try_from(i).unwrap())),
                ValueRef::Boolean(seed % 2 == 0),
            ],
        )
        .unwrap();
    }
    w.commit().unwrap();
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
        let Some(ValueRef::Text(name)) = feature.value("name") else {
            panic!("every shape has a text name");
        };
        let name = name.to_owned();
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

// --- tiles (M4) -------------------------------------------------------------

/// A greyscale PGM, the one raster format that can be written here without an
/// encoder: `gdal_translate` turns it into a tile pyramid.
fn write_pgm(path: &Path, side: usize) {
    let mut bytes = format!("P5\n{side} {side}\n255\n").into_bytes();
    for y in 0..side {
        for x in 0..side {
            bytes.push(((x * 3 + y * 5) % 256) as u8);
        }
    }
    std::fs::write(path, bytes).unwrap();
}

/// The committed pyramid fixture's single PNG tile, which is a real image
/// GDAL encoded rather than the bare headers the unit tests use.
fn fixture_tile() -> Vec<u8> {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gdal_tiles.gpkg");
    let gpkg = GeoPackage::open_read_only(fixture).unwrap();
    gpkg.tiles("tiles")
        .unwrap()
        .get_tile(TileCoord::new(0, 0, 0))
        .unwrap()
        .expect("the fixture holds one tile")
}

#[test]
#[ignore = "requires GDAL (gdalinfo); reads a tile pyramid we wrote"]
fn gdal_reads_a_pyramid_we_wrote() {
    if !tool_available("gdalinfo") {
        eprintln!("skipping: gdalinfo not found");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ours.gpkg");
    let tile = fixture_tile();
    {
        let gpkg = GeoPackage::create(&path).unwrap();
        gpkg.add_epsg_srs(3857).unwrap();
        let matrix_set = TileMatrixSet::web_mercator_quad();
        let matrices = matrix_set.ladder(ZoomLadder::new(0, 1)).unwrap();
        let pyramid = gpkg
            .create_tile_pyramid(&TilePyramidBuilder::new("basemap", matrix_set).matrices(matrices))
            .unwrap();
        // Zoom 1 is a two by two grid; fill it, so GDAL sees a full level.
        let tiles: Vec<(TileCoord, &[u8])> = (0..2)
            .flat_map(|row| (0..2).map(move |column| TileCoord::new(1, column, row)))
            .map(|coord| (coord, tile.as_slice()))
            .collect();
        pyramid.write_all(tiles, 0).unwrap();
        gpkg.close().unwrap();
    }

    let out = Command::new("gdalinfo")
        .arg("-json")
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "gdalinfo failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let info: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(info["driverShortName"], "GPKG");
    // Two tiles across at 256 pixels each, at the pyramid's highest zoom level.
    assert_eq!(info["size"], serde_json::json!([512, 512]));
    let wkt = info["coordinateSystem"]["wkt"].as_str().unwrap_or_default();
    assert!(
        wkt.contains("3857") || wkt.contains("Pseudo-Mercator"),
        "GDAL did not resolve the pyramid's CRS: {wkt}"
    );
    eprintln!("gdalinfo read our pyramid: 512x512 over the web mercator quad");
}

#[test]
#[ignore = "requires GDAL (gdal_translate); reads a multi-level pyramid GDAL wrote"]
fn we_read_a_pyramid_gdal_wrote() {
    if !tool_available("gdal_translate") {
        eprintln!("skipping: gdal_translate not found");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.pgm");
    let path = dir.path().join("gdal.gpkg");
    write_pgm(&source, 256);
    let half_span = "20037508.34";
    let out = Command::new("gdal_translate")
        .args(["-q", "-of", "GPKG", "-a_srs", "EPSG:3857", "-a_ullr"])
        .args([
            format!("-{half_span}"),
            half_span.to_owned(),
            half_span.to_owned(),
            format!("-{half_span}"),
        ])
        .args(["-co", "TILING_SCHEME=GoogleMapsCompatible"])
        .args(["-co", "ZOOM_LEVEL=2", "-co", "TILE_FORMAT=PNG"])
        .args(["-co", "RASTER_TABLE=tiles"])
        .arg(&source)
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "gdal_translate failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let gpkg = GeoPackage::open_read_only(&path).unwrap();
    let pyramid = gpkg.tiles("tiles").unwrap();
    // GDAL's own ladder satisfies the rules we check our own writes against.
    pyramid.validate().unwrap();
    assert_eq!(pyramid.zoom_levels(), vec![0, 1, 2]);
    assert!(
        pyramid.matrix_set().is_web_mercator_quad(),
        "the GoogleMapsCompatible scheme is the web mercator quad"
    );

    // Only the highest level is populated, which is legal: a pyramid may be
    // sparse. Every tile there is a PNG of the size its zoom level declares.
    assert_eq!(pyramid.tile_count().unwrap(), 16);
    assert_eq!(pyramid.tile_count_at(0).unwrap(), 0);
    let matrix = *pyramid.matrix(2).unwrap();
    let mut cursor = pyramid.cursor_at(2).unwrap();
    let mut stream = cursor.tiles().unwrap();
    let mut seen = 0;
    while let Some(tile) = stream.next().unwrap() {
        let payload = tile.probe().unwrap();
        assert_eq!(payload.format, TileFormat::Png);
        matrix.check_payload(&payload).unwrap();
        assert!(matrix.contains(tile.coord().column, tile.coord().row));
        seen += 1;
    }
    assert_eq!(seen, 16);
    eprintln!("read GDAL's GoogleMapsCompatible pyramid: 3 levels, 16 tiles at zoom 2");
}

// --- (f) gpkg_crs_wkt: WKT2 definitions both ways ---------------------------

#[test]
#[ignore = "requires GDAL (ogrinfo, ogr2ogr); WKT2 definitions written here and written by GDAL"]
fn crs_wkt_extension_round_trips_with_gdal() {
    if !tool_available("ogrinfo") || !tool_available("ogr2ogr") {
        eprintln!("skipping: ogrinfo or ogr2ogr not found");
        return;
    }
    let dir = tempfile::tempdir().unwrap();

    // (1) A file we write carrying a code with no WKT1 form: the definition
    // lives in definition_12_063, and GDAL has to find it there.
    let ours = dir.path().join("ours.gpkg");
    {
        let gpkg = GeoPackage::create(&ours).unwrap();
        gpkg.add_epsg_srs(4979).unwrap();
        let layer = gpkg
            .create_layer(&TableSchemaBuilder::new("pts").geometry(GeometrySpec::new(
                geopackage::core::types::GeometryType::Point,
                4979,
            )))
            .unwrap();
        let mut writer = layer.writer().unwrap();
        writer.insert(None, &Point::new(-6.26, 53.35), &[]).unwrap();
        writer.commit().unwrap();
        gpkg.close().unwrap();
    }
    let out = Command::new("ogrinfo")
        .args(["-al", "-so"])
        .arg(&ours)
        .output()
        .expect("run ogrinfo");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("4979"),
        "GDAL did not resolve the CRS from definition_12_063:\n{stdout}"
    );

    // (2) The other direction: GDAL writes the extension itself when a CRS has
    // no WKT1 form, adding both columns and renaming its own `gpkg_crs_wkt`
    // row to `gpkg_crs_wkt_1_1`. Reading that file back has to surface the
    // definition, which means reading a column GDAL added rather than one we
    // did.
    let theirs = dir.path().join("theirs.gpkg");
    let out = Command::new("ogr2ogr")
        .args(["-f", "GPKG"])
        .arg(&theirs)
        .arg(&ours)
        .args(["-t_srs", "EPSG:4979"])
        .output()
        .expect("run ogr2ogr");
    assert!(
        out.status.success(),
        "ogr2ogr failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let gpkg = GeoPackage::open_read_only(&theirs).unwrap();
    let with_wkt2: Vec<_> = gpkg
        .srs_list()
        .unwrap()
        .into_iter()
        .filter(|srs| srs.definition_wkt2.is_some())
        .collect();
    assert!(
        !with_wkt2.is_empty(),
        "a GDAL file carrying the extension read back with no WKT2 definition"
    );
    for row in &gpkg.extensions().unwrap() {
        assert_ne!(
            row.support(),
            geopackage::ExtensionSupport::Unrecognised,
            "GDAL wrote {} and we cannot name it",
            row.name
        );
    }
    eprintln!(
        "read {} WKT2 definition(s) from a GDAL-written file",
        with_wkt2.len()
    );
}

// --- (g) gpkg_schema: constraints as GDAL field domains ---------------------

#[test]
#[ignore = "requires GDAL (ogrinfo, ogr2ogr); gpkg_schema constraints as field domains"]
fn column_constraints_round_trip_as_gdal_field_domains() {
    if !tool_available("ogrinfo") || !tool_available("ogr2ogr") {
        eprintln!("skipping: ogrinfo or ogr2ogr not found");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ours = dir.path().join("domains.gpkg");
    {
        let gpkg = GeoPackage::create(&ours).unwrap();
        gpkg.create_layer(
            &TableSchemaBuilder::new("sites")
                .column(ColumnSpec::new("code", ColumnType::Text(None)))
                .column(ColumnSpec::new("year", ColumnType::Integer))
                .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
        )
        .unwrap();
        gpkg.add_column_constraint(&geopackage::ColumnConstraint {
            name: "years".to_owned(),
            kind: geopackage::ConstraintKind::Range {
                min: 1900.0,
                min_is_inclusive: true,
                max: 2000.0,
                max_is_inclusive: true,
            },
            description: Some("in range".to_owned()),
        })
        .unwrap();
        gpkg.add_column_constraint(&geopackage::ColumnConstraint {
            name: "codes".to_owned(),
            kind: geopackage::ConstraintKind::Enum(vec!["IE".to_owned(), "GB".to_owned()]),
            description: None,
        })
        .unwrap();
        for (column, constraint) in [("year", "years"), ("code", "codes")] {
            gpkg.set_data_column(
                "sites",
                &geopackage::DataColumn {
                    column_name: column.to_owned(),
                    name: None,
                    title: None,
                    description: None,
                    mime_type: None,
                    constraint_name: Some(constraint.to_owned()),
                },
            )
            .unwrap();
        }
        let layer = gpkg.layer("sites").unwrap();
        let mut writer = layer.writer().unwrap();
        writer
            .insert(
                None,
                &Point::new(-6.26, 53.35),
                &[ValueRef::Text("IE"), ValueRef::Integer(1950)],
            )
            .unwrap();
        writer.commit().unwrap();
        gpkg.close().unwrap();
    }

    // GDAL reads gpkg_data_column_constraints as field domains.
    let out = Command::new("ogrinfo")
        .args(["-al", "-so"])
        .arg(&ours)
        .output()
        .expect("run ogrinfo");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for domain in ["years", "codes"] {
        assert!(
            stdout.contains(domain),
            "ogrinfo did not report the {domain} domain:\n{stdout}"
        );
    }
    let described = Command::new("ogrinfo")
        .args(["-fielddomain", "years"])
        .arg(&ours)
        .output()
        .expect("run ogrinfo -fielddomain");
    let described = String::from_utf8_lossy(&described.stdout);
    assert!(
        described.contains("1900") && described.contains("2000"),
        "ogrinfo did not describe the range:\n{described}"
    );

    // And writes them back: a copy has to carry the same constraints, which
    // means GDAL read ours and wrote its own from them.
    let theirs = dir.path().join("copy.gpkg");
    let out = Command::new("ogr2ogr")
        .args(["-f", "GPKG"])
        .arg(&theirs)
        .arg(&ours)
        .output()
        .expect("run ogr2ogr");
    assert!(
        out.status.success(),
        "ogr2ogr failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let copy = GeoPackage::open_read_only(&theirs).unwrap();
    assert_eq!(
        copy.column_constraint("years").unwrap().map(|c| c.kind),
        Some(geopackage::ConstraintKind::Range {
            min: 1900.0,
            min_is_inclusive: true,
            max: 2000.0,
            max_is_inclusive: true,
        })
    );
    let members = copy.column_constraint("codes").unwrap().unwrap();
    // As a set: GDAL hands the members back in its own order, and the spec
    // calls an enum a set, so the order carries no meaning.
    match members.kind {
        geopackage::ConstraintKind::Enum(mut values) => {
            values.sort();
            assert_eq!(values, ["GB", "IE"]);
        }
        other => panic!("expected an enum, got {other:?}"),
    }
    // The descriptions survive the copy too, which is how a reader finds the
    // constraint from the column.
    let described = copy.data_columns("sites").unwrap();
    let years = described
        .iter()
        .find(|d| d.column_name == "year")
        .expect("the year column is still described");
    assert_eq!(years.constraint_name.as_deref(), Some("years"));
}
