//! Layer handles ([`GeoPackage::layer`]/`attributes`/`layers`), the streaming
//! feature read path, and the `select` WHERE-clause passthrough.

use geopackage::core::gpb::{Envelope, encode_header};
use geopackage::{ContentsDataType, Error, GeoPackage, LayerKind, Value};
use geopackage::{ConversionOptions, core::datetime::DateTime};

/// A GPB point blob with an XY envelope (little-endian WKB).
fn gpb_point(x: f64, y: f64) -> Vec<u8> {
    let mut blob = encode_header(4326, &Envelope::Xy([x, x, y, y]), false, false);
    blob.push(1);
    blob.extend_from_slice(&1u32.to_le_bytes());
    blob.extend_from_slice(&x.to_le_bytes());
    blob.extend_from_slice(&y.to_le_bytes());
    blob
}

/// A GeoPackage with a feature table `roads` (fid, geom, name, lanes, built),
/// registered in `gpkg_contents` and `gpkg_geometry_columns`.
fn roads_gpkg() -> (tempfile::TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    gpkg.connection()
        .execute_batch(
            "CREATE TABLE roads (\
               fid INTEGER PRIMARY KEY, geom POINT, name TEXT, lanes INTEGER, built DATETIME);\
             INSERT INTO gpkg_contents (table_name, data_type, identifier, srs_id) \
               VALUES ('roads', 'features', 'Roads', 4326);\
             CREATE TABLE gpkg_geometry_columns (\
               table_name TEXT NOT NULL, column_name TEXT NOT NULL, \
               geometry_type_name TEXT NOT NULL, srs_id INTEGER NOT NULL, \
               z TINYINT NOT NULL, m TINYINT NOT NULL);\
             INSERT INTO gpkg_geometry_columns VALUES ('roads', 'geom', 'POINT', 4326, 0, 0);",
        )
        .unwrap();
    for (fid, x, y, name, lanes) in [
        (1i64, 0.0f64, 0.0f64, "a", 2i64),
        (2, 10.0, 20.0, "b", 4),
        (3, -5.0, 7.0, "c", 1),
    ] {
        gpkg.connection()
            .execute(
                "INSERT INTO roads (fid, geom, name, lanes) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![fid, gpb_point(x, y), name, lanes],
            )
            .unwrap();
    }
    (dir, gpkg)
}

#[test]
fn enumerate_and_open_feature_layer() {
    let (_dir, gpkg) = roads_gpkg();

    let layers = gpkg.layers().unwrap();
    assert_eq!(layers.len(), 1);
    assert_eq!(layers[0].table_name(), "roads");
    assert_eq!(layers[0].kind(), LayerKind::Feature);
    assert_eq!(
        layers[0].geometry_column().unwrap().column_name,
        "geom".to_string()
    );
    assert_eq!(layers[0].primary_key_column(), Some("fid"));

    let layer = gpkg.layer("roads").unwrap();
    assert_eq!(layer.schema().columns.len(), 5);
}

#[test]
fn layer_lookup_errors_are_typed() {
    let (_dir, gpkg) = roads_gpkg();

    match gpkg.layer("ghost") {
        Err(Error::NoSuchLayer { table_name }) => assert_eq!(table_name, "ghost"),
        other => panic!("expected NoSuchLayer, got {other:?}"),
    }

    // 'roads' is a feature layer; asking for it as an attribute table is a
    // typed data-type error.
    match gpkg.attributes("roads") {
        Err(Error::WrongDataType {
            table_name,
            expected,
            found,
        }) => {
            assert_eq!(table_name, "roads");
            assert_eq!(expected, "attributes");
            assert_eq!(found, "features");
        }
        other => panic!("expected WrongDataType, got {other:?}"),
    }
}

#[test]
fn attribute_layer_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("a.gpkg")).unwrap();
    gpkg.connection()
        .execute_batch(
            "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT);\
             INSERT INTO gpkg_contents (table_name, data_type, srs_id) \
               VALUES ('notes', 'attributes', 0);\
             INSERT INTO notes (id, body) VALUES (1, 'hello'), (2, 'world');",
        )
        .unwrap();

    // Attribute layers are not feature layers, so `layers()` skips them.
    assert!(gpkg.layers().unwrap().is_empty());

    let layer = gpkg.attributes("notes").unwrap();
    assert_eq!(layer.kind(), LayerKind::Attributes);
    assert!(layer.geometry_column().is_none());

    let features: Vec<_> = layer.features().unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(features.len(), 2);
    assert!(features[0].geometry().unwrap().is_none());
    assert_eq!(
        features[0].value("body"),
        Some(&Value::Text("hello".into()))
    );

    // A feature accessor on an attribute table is a typed error.
    match gpkg.layer("notes") {
        Err(Error::WrongDataType { found, .. }) => assert_eq!(found, "attributes"),
        other => panic!("expected WrongDataType, got {other:?}"),
    }
    // Contents still reports it as an attributes row.
    assert_eq!(
        gpkg.contents().unwrap()[0].data_type,
        ContentsDataType::Attributes
    );
}

#[test]
fn features_stream_values_and_geometry() {
    let (_dir, gpkg) = roads_gpkg();
    let layer = gpkg.layer("roads").unwrap();

    let features: Vec<_> = layer.features().unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(features.len(), 3);

    let first = &features[0];
    assert_eq!(first.fid(), 1);
    // By name.
    assert_eq!(first.value("name"), Some(&Value::Text("a".into())));
    assert_eq!(first.value("lanes"), Some(&Value::Integer(2)));
    assert_eq!(first.value("fid"), Some(&Value::Integer(1)));
    // By index: fid, name, lanes, built (geometry excluded from values).
    assert_eq!(first.get(0), Some(&Value::Integer(1)));
    assert_eq!(first.get(1), Some(&Value::Text("a".into())));
    assert_eq!(first.columns(), &["fid", "name", "lanes", "built"]);
    assert!(first.value("no_such").is_none());

    // Geometry parses lazily from the owned blob.
    let geom = first.geometry().unwrap().unwrap();
    assert_eq!(
        geom.to_geo().unwrap(),
        geo_types::Geometry::Point(geo_types::Point::new(0.0, 0.0))
    );

    // A row whose geometry cell is NULL reads as no geometry.
    gpkg.connection()
        .execute(
            "INSERT INTO roads (fid, geom, name) VALUES (9, NULL, 'z')",
            [],
        )
        .unwrap();
    let features: Vec<_> = layer.features().unwrap().collect::<Result<_, _>>().unwrap();
    let null_geom = features.iter().find(|f| f.fid() == 9).unwrap();
    assert!(null_geom.geometry().unwrap().is_none());
    assert!(null_geom.geometry_bytes().is_none());
}

#[test]
fn per_row_error_does_not_stop_iteration() {
    let (_dir, gpkg) = roads_gpkg();
    // Store TEXT in the INTEGER-declared `lanes` column (SQLite permits it).
    gpkg.connection()
        .execute(
            "INSERT INTO roads (fid, geom, name, lanes) VALUES (4, ?1, 'd', 'not-a-number')",
            [gpb_point(1.0, 1.0)],
        )
        .unwrap();
    let layer = gpkg.layer("roads").unwrap();
    let results: Vec<_> = layer.features().unwrap().collect();
    assert_eq!(results.len(), 4);
    let errors = results.iter().filter(|r| r.is_err()).count();
    assert_eq!(errors, 1, "the bad row errors, the others still yield");
}

#[test]
fn conversion_options_apply_to_features() {
    let (_dir, gpkg) = roads_gpkg();
    // A second-precision datetime: rejected under strict, accepted under lenient.
    gpkg.connection()
        .execute(
            "INSERT INTO roads (fid, geom, name, built) VALUES (5, ?1, 'e', '2026-07-24T12:34:56Z')",
            [gpb_point(2.0, 2.0)],
        )
        .unwrap();

    let strict = gpkg.layer("roads").unwrap();
    let row = strict
        .select("fid = ?1", &[Value::Integer(5)])
        .unwrap()
        .next()
        .unwrap();
    assert!(matches!(row, Err(Error::InvalidDateTimeValue { .. })));

    let lenient = gpkg
        .layer("roads")
        .unwrap()
        .with_conversion_options(ConversionOptions::lenient());
    let row = lenient
        .select("fid = ?1", &[Value::Integer(5)])
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(
        row.value("built"),
        Some(&Value::DateTime(
            DateTime::parse_lenient("2026-07-24T12:34:56Z").unwrap()
        ))
    );
}

#[test]
fn select_passthrough_binds_values() {
    let (_dir, gpkg) = roads_gpkg();
    let layer = gpkg.layer("roads").unwrap();

    let hits: Vec<_> = layer
        .select("lanes >= ?1", &[Value::Integer(2)])
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut names: Vec<&str> = hits
        .iter()
        .map(|f| match f.value("name").unwrap() {
            Value::Text(s) => s.as_str(),
            _ => unreachable!(),
        })
        .collect();
    names.sort_unstable();
    assert_eq!(names, vec!["a", "b"]);

    // A text parameter binds too.
    let by_name: Vec<_> = layer
        .select("name = ?1", &[Value::Text("c".into())])
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(by_name.len(), 1);
    assert_eq!(by_name[0].fid(), 3);
}
