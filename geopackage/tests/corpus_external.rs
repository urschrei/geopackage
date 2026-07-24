//! External corpus soak (ignored; needs `scripts/fetch_corpus.sh` first).
//!
//! Unlike the committed-fixture comparison in `corpus.rs`, this sweep runs over
//! whatever larger, third-party GeoPackages happen to be present under `corpus/`
//! (git-ignored; populate it with `scripts/fetch_corpus.sh`). For each file it
//! opens leniently, enumerates every `features`/`attributes` layer, iterates all
//! features and tallies read errors -- a broad "does our reader survive real
//! files written by other tools, at other spec versions" check rather than a
//! value-for-value comparison. It is `#[ignore]`d and the downloads are never
//! committed, so the default test run is unaffected.
//!
//! Run: `scripts/fetch_corpus.sh` then
//! `cargo test -p geopackage --test corpus_external -- --ignored --nocapture`.

use std::path::PathBuf;

use geopackage::{ContentsDataType, ConversionOptions, GeoPackage};

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
}

fn sweep(path: &std::path::Path) -> Tally {
    let gpkg = GeoPackage::open_lenient(path)
        .unwrap_or_else(|e| panic!("open_lenient({}): {e:?}", path.display()));

    let mut tally = Tally::default();
    let contents = gpkg.contents().unwrap();
    for entry in contents {
        let layer = match entry.data_type {
            ContentsDataType::Features => gpkg.layer(&entry.table_name),
            ContentsDataType::Attributes => gpkg.attributes(&entry.table_name),
            // Tiles and extension data types are out of scope for the read path.
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
    for path in &files {
        let t = sweep(path);
        total_features += t.features;
        println!(
            "{:40} layers={:<3} features={:<7} row_errors={:<4} geometry_errors={}",
            path.file_name().unwrap().to_string_lossy(),
            t.layers,
            t.features,
            t.row_errors,
            t.geometry_errors,
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
}
