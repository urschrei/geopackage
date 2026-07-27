//! External corpus soak (ignored; needs `scripts/fetch_corpus.sh` first).
//!
//! Unlike the committed-fixture comparison in `corpus.rs`, this sweep runs over
//! whatever larger, third-party GeoPackages happen to be present under `corpus/`
//! (git-ignored; populate it with `scripts/fetch_corpus.sh`). For each file it
//! opens leniently, enumerates every `features`/`attributes` layer, iterates all
//! features, walks every tile of every pyramid probing its payload, and tallies
//! read errors -- a broad "does our reader survive real files written by other
//! tools, at other spec versions" check rather than a value-for-value
//! comparison. It is `#[ignore]`d and the downloads are never committed, so the
//! default test run is unaffected.
//!
//! Run: `scripts/fetch_corpus.sh` then
//! `cargo test -p geopackage --test corpus_external -- --ignored --nocapture`.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the panic-family patterns in these helpers are the intended failure mechanism"
)]

use std::path::PathBuf;

use geopackage::{ContentsDataType, ConversionOptions, Extension, GeoPackage};

fn corpus_dir() -> PathBuf {
    match std::env::var_os("GEOPACKAGE_CORPUS_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("corpus"),
    }
}

fn gpkg_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("gpkg"))
        .collect();
    files.sort();
    files
}

/// Per-file tally.
#[derive(Default)]
struct Tally {
    layers: usize,
    features: usize,
    row_errors: usize,
    geometry_errors: usize,
    pyramids: usize,
    tiles: usize,
    tile_errors: usize,
    /// Extension names this crate could not identify.
    ///
    /// Unlike the error counts, this one is asserted empty by the caller: the
    /// corpus is pinned by sha256, so a name turning up here is a real file
    /// declaring something we have never seen, which is worth knowing about
    /// rather than tallying.
    unclassified: Vec<String>,
}

fn sweep(path: &std::path::Path) -> Tally {
    let gpkg = GeoPackage::open_lenient(path)
        .unwrap_or_else(|e| panic!("open_lenient({}): {e:?}", path.display()));

    let mut tally = Tally::default();
    for row in gpkg.extensions().unwrap() {
        if matches!(row.extension(), Extension::Other(_)) {
            tally.unclassified.push(row.name);
        }
    }
    let contents = gpkg.contents().unwrap();
    for entry in contents {
        if entry.data_type == ContentsDataType::Tiles {
            sweep_pyramid(&gpkg, &entry.table_name, &mut tally);
            continue;
        }
        let layer = match entry.data_type {
            ContentsDataType::Features => gpkg.layer(&entry.table_name),
            ContentsDataType::Attributes => gpkg.attributes(&entry.table_name),
            // Extension data types are out of scope for the read path.
            _ => continue,
        };
        let Ok(layer) = layer else { continue };
        let layer = layer.with_conversion_options(ConversionOptions::lenient());
        tally.layers += 1;

        let features = match layer.features() {
            Ok(features) => features,
            Err(_) => {
                tally.row_errors += 1;
                continue;
            }
        };
        for result in features {
            tally.features += 1;
            match result {
                Ok(feature) => {
                    // Geometry is parsed lazily; force it to surface curve-type
                    // and malformed-blob errors (counted, never fatal).
                    if feature.geometry().is_err() {
                        tally.geometry_errors += 1;
                    }
                }
                Err(_) => tally.row_errors += 1,
            }
        }
    }
    tally
}

/// Walk one tile pyramid: every tile, with its payload probed against the size
/// its zoom level declares.
///
/// The payload is what a file from elsewhere is least constrained in, so a
/// header that cannot be read, or that disagrees with `gpkg_tile_matrix`, is
/// counted rather than fatal: this sweep asks whether real files can be read at
/// all, and reports what is odd about them.
fn sweep_pyramid(gpkg: &GeoPackage, table_name: &str, tally: &mut Tally) {
    let Ok(pyramid) = gpkg.tiles(table_name) else {
        tally.tile_errors += 1;
        return;
    };
    tally.pyramids += 1;
    if pyramid.validate().is_err() {
        tally.tile_errors += 1;
    }
    let Ok(mut cursor) = pyramid.cursor() else {
        tally.tile_errors += 1;
        return;
    };
    let Ok(mut stream) = cursor.tiles() else {
        tally.tile_errors += 1;
        return;
    };
    loop {
        match stream.next() {
            Ok(Some(tile)) => {
                tally.tiles += 1;
                let matrix = pyramid.matrix(tile.coord().zoom_level);
                match (tile.probe(), matrix) {
                    (Ok(payload), Some(matrix)) => {
                        if matrix.check_payload(&payload).is_err() {
                            tally.tile_errors += 1;
                        }
                    }
                    // A zoom level with tiles but no gpkg_tile_matrix row
                    // breaks Requirement 44; an unreadable payload is its own
                    // fault. Both are counted the same way.
                    _ => tally.tile_errors += 1,
                }
            }
            Ok(None) => break,
            Err(_) => {
                tally.tile_errors += 1;
                break;
            }
        }
    }
}

#[test]
#[ignore = "needs scripts/fetch_corpus.sh to populate corpus/ with external files"]
fn sweep_external_corpus() {
    let dir = corpus_dir();
    let files = gpkg_files(&dir);
    if files.is_empty() {
        eprintln!(
            "no .gpkg files under {} -- run scripts/fetch_corpus.sh first",
            dir.display()
        );
        return;
    }

    let mut total_features = 0usize;
    let mut unclassified: Vec<(String, String)> = Vec::new();
    for path in &files {
        let t = sweep(path);
        total_features += t.features;
        let file_name = path.file_name().unwrap().to_string_lossy().into_owned();
        println!(
            "{:40} layers={:<3} features={:<7} row_errors={:<4} geometry_errors={:<4} \
             pyramids={:<3} tiles={:<6} tile_errors={:<4} unclassified_extensions={}",
            file_name,
            t.layers,
            t.features,
            t.row_errors,
            t.geometry_errors,
            t.pyramids,
            t.tiles,
            t.tile_errors,
            t.unclassified.len(),
        );
        unclassified.extend(
            t.unclassified
                .into_iter()
                .map(|name| (file_name.clone(), name)),
        );
    }
    println!(
        "swept {} file(s), {} feature(s) total",
        files.len(),
        total_features
    );
    // Every file opened and enumerated without panicking; that is the bar. Error
    // counts are reported, not asserted: some published samples carry curve-type
    // geometries the wkb reader cannot yet parse (tracked in the M1 roadmap).
    assert!(!files.is_empty());
    // Extension names are the exception, and are asserted: a real file
    // declaring a name this crate cannot identify is what the catalogue exists
    // to surface, and the fix is to name it rather than to widen this test.
    assert_eq!(
        unclassified,
        Vec::new(),
        "unclassified extension names, as (file, extension_name)"
    );
}
