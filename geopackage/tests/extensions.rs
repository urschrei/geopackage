//! The `gpkg_extensions` catalogue: what a file declares, and what this crate
//! can do about it.
//!
//! The last test here is the inventory one: every extension row in every
//! committed fixture has to classify. It fails on a name this crate cannot
//! identify rather than skipping it, so a fixture that arrives carrying
//! something new is a build failure rather than a silent shrug.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::path::{Path, PathBuf};

use geopackage::core::tiles::{TileMatrix, TileMatrixSet};
use geopackage::core::types::GeometryType;
use geopackage::{
    Extension, ExtensionScope, ExtensionSupport, GeoPackage, GeometrySpec, TableSchemaBuilder,
    TilePyramidBuilder,
};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn gpkg() -> (tempfile::TempDir, GeoPackage) {
    let dir = tempfile::tempdir().unwrap();
    let gpkg = GeoPackage::create(dir.path().join("t.gpkg")).unwrap();
    (dir, gpkg)
}

/// Register a row directly, which is how an extension this crate does not
/// write arrives: from another implementation.
fn raw_register(gpkg: &GeoPackage, table: Option<&str>, column: Option<&str>, name: &str) {
    let conn = gpkg.connection();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS gpkg_extensions (\
           table_name TEXT, column_name TEXT, extension_name TEXT NOT NULL, \
           definition TEXT NOT NULL, scope TEXT NOT NULL, \
           CONSTRAINT ge_tce UNIQUE (table_name, column_name, extension_name));",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO gpkg_extensions \
         (table_name, column_name, extension_name, definition, scope) \
         VALUES (?1, ?2, ?3, 'http://example.invalid/', 'read-write')",
        rusqlite::params![table, column, name],
    )
    .unwrap();
}

#[test]
fn a_file_without_the_table_declares_no_extensions() {
    let (_dir, gpkg) = gpkg();
    // Requirement 59: no table means a GeoPackage rather than an Extended
    // GeoPackage, which is a fact about the file and not an error.
    assert_eq!(gpkg.extensions().unwrap(), Vec::new());
    assert_eq!(gpkg.table_extensions("anything").unwrap(), Vec::new());
}

#[test]
fn an_indexed_layer_declares_the_rtree_extension() {
    let (_dir, gpkg) = gpkg();
    gpkg.add_epsg_srs(4326).unwrap();
    gpkg.create_layer(
        &TableSchemaBuilder::new("pts").geometry(GeometrySpec::new(GeometryType::Point, 4326)),
    )
    .unwrap();

    let layer = gpkg.layer("pts").unwrap();
    let rows = layer.extensions().unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = &rows[0];
    assert_eq!(row.name, "gpkg_rtree_index");
    assert_eq!(row.table_name.as_deref(), Some("pts"));
    assert_eq!(row.column_name.as_deref(), Some("geom"));
    // Annex F.3 registers the index write-only: a reader that ignores the
    // index still reads the right rows, just more slowly.
    assert_eq!(row.scope, ExtensionScope::WriteOnly);
    assert!(!row.scope.affects_readers());
    assert!(row.scope.affects_writers());
    assert_eq!(row.extension(), Extension::RtreeIndex);
    assert_eq!(row.support(), ExtensionSupport::Implemented);

    // The same row, reached from the GeoPackage rather than from the layer.
    assert_eq!(gpkg.table_extensions("pts").unwrap(), rows);
    assert_eq!(gpkg.extensions().unwrap(), rows);
}

#[test]
fn a_pyramid_declares_its_tile_extensions() {
    let (_dir, gpkg) = gpkg();
    gpkg.add_epsg_srs(3857).unwrap();
    let matrix_set = TileMatrixSet::web_mercator_quad();
    let width = matrix_set.width();
    // Zoom 1 triples the grid rather than doubling it, so the pyramid needs
    // gpkg_zoom_other, which the builder registers on the opt-in.
    let matrices = [
        TileMatrix::new(0, 1, 1, 256, 256, width / 256.0, width / 256.0),
        TileMatrix::new(1, 3, 3, 256, 256, width / 768.0, width / 768.0),
    ];
    let pyramid = gpkg
        .create_tile_pyramid(
            &TilePyramidBuilder::new("basemap", matrix_set)
                .matrices(matrices)
                .allow_zoom_other(true),
        )
        .unwrap();

    let rows = pyramid.extensions().unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].extension(), Extension::ZoomOther);
    assert_eq!(rows[0].scope, ExtensionScope::ReadWrite);
    assert_eq!(rows[0].support(), ExtensionSupport::Implemented);
    assert_eq!(rows[0].column_name.as_deref(), Some("tile_data"));
}

#[test]
fn whole_geopackage_rows_belong_to_no_table() {
    let (_dir, gpkg) = gpkg();
    // How the OGC 1.2 sample registers gpkg_metadata: NULL table_name, for an
    // extension that applies to the file rather than to any one table.
    raw_register(&gpkg, None, None, "gpkg_metadata");

    let all = gpkg.extensions().unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].table_name, None);
    assert_eq!(all[0].column_name, None);
    assert_eq!(all[0].extension(), Extension::Metadata);
    assert_eq!(all[0].support(), ExtensionSupport::Known);

    // A NULL table_name matches no table, including a table called "null".
    assert_eq!(gpkg.table_extensions("gpkg_metadata").unwrap(), Vec::new());
}

#[test]
fn table_names_match_case_insensitively() {
    let (_dir, gpkg) = gpkg();
    // The note under Requirement 60: sqlite_master and gpkg_extensions need
    // not agree on the case of a table name, and SQLite resolves either way.
    raw_register(
        &gpkg,
        Some("Points"),
        Some("geom"),
        "gpkg_geom_CIRCULARSTRING",
    );

    for spelling in ["Points", "points", "POINTS"] {
        let rows = gpkg.table_extensions(spelling).unwrap();
        assert_eq!(rows.len(), 1, "{spelling}");
        assert_eq!(
            rows[0].extension(),
            Extension::GeometryType(GeometryType::CircularString)
        );
        assert_eq!(rows[0].support(), ExtensionSupport::Implemented);
    }
}

#[test]
fn a_name_this_crate_cannot_identify_is_unrecognised() {
    let (_dir, gpkg) = gpkg();
    raw_register(&gpkg, Some("pts"), Some("geom"), "acme_secret_sauce");

    let rows = gpkg.table_extensions("pts").unwrap();
    assert_eq!(
        rows[0].extension(),
        Extension::Other("acme_secret_sauce".to_owned())
    );
    assert_eq!(rows[0].support(), ExtensionSupport::Unrecognised);
    // The row keeps the spelling the file used, whatever this crate makes of it.
    assert_eq!(rows[0].name, "acme_secret_sauce");
}

#[test]
fn the_2016_removals_are_identified_rather_than_unrecognised() {
    let (_dir, gpkg) = gpkg();
    raw_register(
        &gpkg,
        Some("pts"),
        Some("geom"),
        "gpkg_geometry_type_trigger",
    );
    raw_register(&gpkg, Some("pts"), Some("geom"), "gpkg_srs_id_trigger");

    let rows = gpkg.table_extensions("pts").unwrap();
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(
            row.support(),
            ExtensionSupport::Removed,
            "{}: removed from the standard on 2016-08-15, still readable",
            row.name
        );
        assert!(row.extension().is_removed());
    }
}

/// Build a layer with one row, then register an extension against it that this
/// crate cannot identify, as another implementation would have.
fn layer_with_unknown_extension(gpkg: &GeoPackage, extension_name: &str) {
    gpkg.add_epsg_srs(4326).unwrap();
    let layer = gpkg
        .create_layer(
            &TableSchemaBuilder::new("pts").geometry(GeometrySpec::new(GeometryType::Point, 4326)),
        )
        .unwrap();
    let mut writer = layer.writer().unwrap();
    writer
        .insert(None, &geo_types::Point::new(1.0, 2.0), &[])
        .unwrap();
    writer.commit().unwrap();
    raw_register(gpkg, Some("pts"), Some("geom"), extension_name);
}

#[test]
fn an_unidentified_extension_blocks_writes_to_the_table_it_covers() {
    let (_dir, gpkg) = gpkg();
    layer_with_unknown_extension(&gpkg, "acme_secret_sauce");
    let layer = gpkg.layer("pts").unwrap();

    let blocked = |result: geopackage::Result<()>| match result {
        Err(geopackage::Error::UnsupportedExtension {
            table_name,
            extension_name,
            scope,
        }) => {
            assert_eq!(table_name, "pts");
            assert_eq!(extension_name, "acme_secret_sauce");
            assert_eq!(scope, "read-write");
        }
        other => panic!("expected UnsupportedExtension, got {other:?}"),
    };

    blocked(layer.writer().map(|_| ()));
    blocked(
        layer
            .write_all(
                [geopackage::NewFeature::new(
                    geo_types::Point::new(0.0, 0.0),
                    Vec::new(),
                )],
                0,
            )
            .map(|_| ()),
    );
    blocked(layer.create_spatial_index());
    blocked(layer.drop_spatial_index());
    blocked(layer.repair_spatial_index());

    // Reading is not refused: Requirement 64 makes this a writer's problem,
    // and a reader that turns the file away helps nobody.
    assert_eq!(layer.features().unwrap().count(), 1);
    assert_eq!(
        gpkg.blocking_extension("pts").unwrap().unwrap().name,
        "acme_secret_sauce"
    );
}

#[test]
fn an_extension_we_can_name_does_not_block_writes() {
    let (_dir, gpkg) = gpkg();
    // gpkg_metadata is not implemented here, but it is identified, and what it
    // adds sits beside the feature data rather than inside it.
    layer_with_unknown_extension(&gpkg, "gpkg_metadata");
    let layer = gpkg.layer("pts").unwrap();

    assert_eq!(gpkg.blocking_extension("pts").unwrap(), None);
    let mut writer = layer.writer().unwrap();
    writer
        .insert(None, &geo_types::Point::new(3.0, 4.0), &[])
        .unwrap();
    writer.commit().unwrap();
    assert_eq!(layer.features().unwrap().count(), 2);
}

#[test]
fn a_whole_geopackage_extension_blocks_writes_to_every_table() {
    let (_dir, gpkg) = gpkg();
    gpkg.add_epsg_srs(4326).unwrap();
    // Requirement 60's third case: a NULL table_name, for an extension that
    // applies to the file. It covers tables that do not exist yet.
    raw_register(&gpkg, None, None, "acme_whole_file");

    let created = gpkg.create_layer(
        &TableSchemaBuilder::new("later").geometry(GeometrySpec::new(GeometryType::Point, 4326)),
    );
    assert!(
        matches!(created, Err(geopackage::Error::UnsupportedExtension { .. })),
        "{created:?}"
    );
}

#[test]
fn an_unidentified_extension_blocks_tile_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.gpkg");
    {
        let gpkg = GeoPackage::create(&path).unwrap();
        gpkg.add_epsg_srs(3857).unwrap();
        let matrix_set = TileMatrixSet::web_mercator_quad();
        let matrices = matrix_set
            .ladder(geopackage::core::tiles::ZoomLadder::new(0, 0))
            .unwrap();
        gpkg.create_tile_pyramid(
            &TilePyramidBuilder::new("basemap", matrix_set).matrices(matrices),
        )
        .unwrap();
        raw_register(&gpkg, Some("basemap"), Some("tile_data"), "acme_tile_codec");
        gpkg.close().unwrap();
    }

    // The pyramid handle reads the block once, so a per-tile write pays a
    // branch rather than a catalogue query.
    let gpkg = GeoPackage::open(&path).unwrap();
    let pyramid = gpkg.tiles("basemap").unwrap();
    assert_eq!(
        pyramid.blocking_extension().map(|row| row.name.as_str()),
        Some("acme_tile_codec")
    );
    let put = pyramid.put_tile(geopackage::core::tiles::TileCoord::new(0, 0, 0), &png());
    assert!(
        matches!(put, Err(geopackage::Error::UnsupportedExtension { .. })),
        "{put:?}"
    );
    pyramid.writer().unwrap_err();
    // Reading the pyramid is untouched.
    assert_eq!(pyramid.tile_count().unwrap(), 0);
}

/// A 256x256 PNG header, which is all the payload probe reads.
fn png() -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&13_u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&256_u32.to_be_bytes());
    bytes.extend_from_slice(&256_u32.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    bytes
}

#[test]
fn the_refusal_can_be_overridden() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.gpkg");
    {
        let gpkg = GeoPackage::create(&path).unwrap();
        layer_with_unknown_extension(&gpkg, "acme_secret_sauce");
        gpkg.close().unwrap();
    }

    // The check is this crate's, not the format's: a caller who knows the
    // extension is harmless says so, and writes.
    let gpkg = geopackage::OpenOptions::new()
        .allow_unsupported_extension_writes(true)
        .open(&path)
        .unwrap();
    assert_eq!(gpkg.blocking_extension("pts").unwrap(), None);
    let layer = gpkg.layer("pts").unwrap();
    let mut writer = layer.writer().unwrap();
    writer
        .insert(None, &geo_types::Point::new(5.0, 6.0), &[])
        .unwrap();
    writer.commit().unwrap();
    assert_eq!(layer.features().unwrap().count(), 2);
}

#[test]
fn open_lenient_warns_about_what_it_cannot_identify() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.gpkg");
    {
        let gpkg = GeoPackage::create(&path).unwrap();
        layer_with_unknown_extension(&gpkg, "acme_secret_sauce");
        // An extension we can name is not warned about, whether or not it is
        // implemented.
        raw_register(&gpkg, None, None, "gpkg_schema");
        gpkg.close().unwrap();
    }

    let gpkg = GeoPackage::open_lenient(&path).unwrap();
    let warnings: Vec<_> = gpkg
        .open_warnings()
        .iter()
        .filter(|w| matches!(w, geopackage::OpenWarning::UnsupportedExtension { .. }))
        .collect();
    assert_eq!(warnings.len(), 1, "{:?}", gpkg.open_warnings());
    match warnings[0] {
        geopackage::OpenWarning::UnsupportedExtension {
            extension_name,
            table_name,
            scope,
        } => {
            assert_eq!(extension_name, "acme_secret_sauce");
            assert_eq!(table_name.as_deref(), Some("pts"));
            assert_eq!(*scope, ExtensionScope::ReadWrite);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn every_extension_in_the_committed_fixtures_is_classified() {
    let mut seen: Vec<(String, ExtensionSupport)> = Vec::new();
    let mut files = 0;
    for entry in std::fs::read_dir(fixtures_dir()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("gpkg") {
            continue;
        }
        files += 1;
        // Lenient, because the corpus deliberately includes legacy files: a
        // GP10 application_id is a warning here, not a reason to skip the
        // file's extension rows.
        let gpkg = GeoPackage::open_lenient(&path).unwrap();
        for row in gpkg.extensions().unwrap() {
            assert!(
                !matches!(row.extension(), Extension::Other(_)),
                "{}: unclassified extension {:?}. Add it to \
                 geopackage_core::extensions::Extension rather than widening this test.",
                path.display(),
                row.name
            );
            assert_ne!(
                row.support(),
                ExtensionSupport::Unrecognised,
                "{}: {} classified but unsupported",
                path.display(),
                row.name
            );
            let entry = (row.name.clone(), row.support());
            if !seen.contains(&entry) {
                seen.push(entry);
            }
        }
    }
    assert!(files > 0, "no fixtures found under {:?}", fixtures_dir());
    seen.sort_by(|a, b| a.0.cmp(&b.0));
    // The inventory itself, so a fixture gaining or losing an extension shows
    // up as a diff here rather than passing quietly.
    assert_eq!(
        seen,
        vec![
            (
                "gpkg_geom_CIRCULARSTRING".to_owned(),
                ExtensionSupport::Implemented
            ),
            (
                "gpkg_geom_COMPOUNDCURVE".to_owned(),
                ExtensionSupport::Implemented
            ),
            (
                "gpkg_geom_CURVEPOLYGON".to_owned(),
                ExtensionSupport::Implemented
            ),
            (
                "gpkg_geom_MULTICURVE".to_owned(),
                ExtensionSupport::Implemented
            ),
            (
                "gpkg_geom_MULTISURFACE".to_owned(),
                ExtensionSupport::Implemented
            ),
            ("gpkg_metadata".to_owned(), ExtensionSupport::Known),
            ("gpkg_rtree_index".to_owned(), ExtensionSupport::Implemented),
        ]
    );
}
