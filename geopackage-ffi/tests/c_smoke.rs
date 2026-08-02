//! Compile `examples/smoke.c` against the built static library and run it.
//!
//! This is the test that proves the header and the ABI are usable from C rather
//! than merely generated. It catches what a Rust-only test cannot: a header
//! that does not compile, a symbol that is not exported, a signature that does
//! not match, and a static link that is missing a platform library.
//!
//! Skipped when the static library has not been built (`cargo nextest` builds
//! the test binaries but not necessarily the `staticlib` artifact) and on
//! Windows, where the link line differs enough to want its own handling.

use std::path::{Path, PathBuf};
use std::process::Command;

/// When set, a precondition this test would otherwise skip on is a failure
/// instead. CI sets it, so a gate cannot go quiet because a tool or an artefact
/// was missing; locally it is unset, so a plain `cargo test` still works
/// without cbindgen installed.
const REQUIRE: &str = "GPKG_FFI_REQUIRE_C_TESTS";

fn required() -> bool {
    std::env::var_os(REQUIRE).is_some()
}

/// Skip, or fail if the environment says these tests are mandatory.
fn skip(reason: &str) {
    assert!(
        !required(),
        "{REQUIRE} is set, so this may not be skipped: {reason}"
    );
    eprintln!("skipped: {reason}");
}

/// The libraries a static link of this crate needs, per platform.
///
/// Obtained from `cargo rustc -p geopackage-ffi --crate-type staticlib --
/// --print native-static-libs`. They are listed rather than queried so the test
/// does not shell out to cargo recursively; if a dependency changes them, this
/// test fails at the link step and the list wants updating.
fn native_libs() -> Vec<&'static str> {
    if cfg!(target_os = "macos") {
        vec![
            "-framework",
            "IOKit",
            "-framework",
            "CoreFoundation",
            "-liconv",
        ]
    } else if cfg!(target_os = "linux") {
        vec!["-lm", "-ldl", "-lpthread"]
    } else {
        Vec::new()
    }
}

/// Where cargo put the build artefacts, walking up from the test binary.
fn target_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // .../target/<profile>/deps/<test binary>
    Some(exe.parent()?.parent()?.to_path_buf())
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Compile one of `examples/*.c` against the static library, returning the
/// binary, or `None` when the environment cannot support it.
fn build_c_example(dir: &Path, name: &str) -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        // Genuinely not applicable rather than a missing precondition, so this
        // one skips even under `GPKG_FFI_REQUIRE_C_TESTS`.
        eprintln!("skipped: the Windows link line wants its own handling");
        return None;
    }
    let target = target_dir()?;
    let static_lib = target.join("libgeopackage_ffi.a");
    if !static_lib.exists() {
        skip(&format!(
            "{} has not been built; run `cargo build -p geopackage-ffi` first",
            static_lib.display()
        ));
        return None;
    }

    let binary = dir.join(name);
    let source = manifest_dir().join(format!("examples/{name}.c"));
    let include = manifest_dir().join("include");

    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    let compiled = Command::new(&cc)
        .arg("-I")
        .arg(&include)
        // The header is public API; it should compile clean under a strict
        // warning set, not merely parse.
        .args(["-Wall", "-Wextra", "-Werror", "-std=c11"])
        .arg(&source)
        .arg(&static_lib)
        .args(native_libs())
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("failed to run the C compiler");
    assert!(
        compiled.status.success(),
        "compiling {name}.c failed:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    Some(binary)
}

fn fixture(name: &str) -> PathBuf {
    manifest_dir()
        .parent()
        .expect("the crate directory has a parent")
        .join("geopackage/tests/fixtures")
        .join(name)
}

#[test]
fn a_c_program_links_against_the_library_and_reads_a_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let Some(binary) = build_c_example(dir.path(), "smoke") else {
        return;
    };

    let run = Command::new(&binary)
        .arg(fixture("gdal_multilayer_1_4.gpkg"))
        .output()
        .expect("failed to run the compiled C program");
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        run.status.success(),
        "smoke.c failed:\nstdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&run.stderr)
    );

    assert!(stdout.contains("version: 1.4"), "{stdout}");
    assert!(stdout.contains("layers: 5"), "{stdout}");
    assert!(stdout.contains("layer points: 3 rows"), "{stdout}");
    // The C program pulls every layer through the Arrow C Data Interface and
    // checks the streamed row count against the scalar one itself, so these
    // lines appearing at all means the data plane worked from C.
    assert!(stdout.contains("stream schema: 4 columns"), "{stdout}");
    assert!(stdout.contains("streamed 3 rows"), "{stdout}");
    // The C program asserts the close refusal itself and exits non-zero if it
    // does not happen, so reaching "ok" means the handle rule held from C too.
    assert!(stdout.contains("ok"), "{stdout}");
}

#[test]
fn a_c_program_copies_a_layer_between_files_through_arrow() {
    // The full data plane from C, in both directions, and with no
    // schema-description API: the destination layer is created from the source
    // stream's own Arrow schema.
    let dir = tempfile::tempdir().expect("tempdir");
    let Some(binary) = build_c_example(dir.path(), "roundtrip") else {
        return;
    };
    let destination = dir.path().join("copied.gpkg");

    let run = Command::new(&binary)
        .arg(fixture("gdal_multilayer_1_4.gpkg"))
        .arg("points")
        .arg(&destination)
        .output()
        .expect("failed to run the compiled C program");
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        run.status.success(),
        "roundtrip.c failed:\nstdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    // The C program checks the written count against the source and re-reads
    // the destination itself, so "ok" means both agreed.
    assert!(stdout.contains("copied 3 rows"), "{stdout}");
    assert!(stdout.contains("ok"), "{stdout}");

    // And what landed is a GeoPackage this crate is happy with, checked from
    // Rust rather than taking the C program's word for it.
    let written = geopackage::GeoPackage::open_read_only(&destination)
        .expect("the C-written file should open");
    assert_eq!(
        written.validate().expect("validate"),
        Vec::new(),
        "a file written entirely from C should have nothing to report"
    );
    assert_eq!(
        written
            .layer("points")
            .expect("the copied layer")
            .count()
            .expect("count"),
        3
    );
}

#[test]
fn the_committed_header_matches_what_cbindgen_produces() {
    // The API-stability gate: an ABI change has to show up as a diff in the
    // committed header rather than reaching a consumer unannounced.
    let Ok(cbindgen) = which_cbindgen() else {
        skip("cbindgen is not installed");
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let generated = dir.path().join("geopackage.h");
    let config = manifest_dir().join("cbindgen.toml");

    let output = Command::new(cbindgen)
        .arg("--config")
        .arg(&config)
        .arg("--crate")
        .arg("geopackage-ffi")
        .arg("--output")
        .arg(&generated)
        .current_dir(manifest_dir().parent().expect("workspace root"))
        .output()
        .expect("failed to run cbindgen");
    assert!(
        output.status.success(),
        "cbindgen failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Compared with line endings normalised. Git may check the committed
    // header out with CRLF on Windows while cbindgen always writes LF, and a
    // header that differs only in line endings is the same header.
    let normalise = |text: String| text.replace("\r\n", "\n").trim_end().to_owned();
    let fresh = normalise(std::fs::read_to_string(&generated).expect("read generated header"));
    let committed = normalise(
        std::fs::read_to_string(manifest_dir().join("include/geopackage.h"))
            .expect("read committed header"),
    );
    assert_eq!(
        fresh, committed,
        "geopackage-ffi/include/geopackage.h is out of date. Regenerate it:\n  \
         cbindgen --config geopackage-ffi/cbindgen.toml --crate geopackage-ffi \
         --output geopackage-ffi/include/geopackage.h"
    );
}

fn which_cbindgen() -> Result<PathBuf, ()> {
    // `cargo install` puts it here; fall back to PATH.
    if let Some(home) = std::env::var_os("CARGO_HOME") {
        let candidate = Path::new(&home).join("bin/cbindgen");
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let candidate = Path::new(&home).join(".cargo/bin/cbindgen");
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    let found = Command::new("cbindgen").arg("--version").output();
    match found {
        Ok(output) if output.status.success() => Ok(PathBuf::from("cbindgen")),
        _ => Err(()),
    }
}

#[test]
fn a_c_program_inspects_a_file_it_knows_nothing_about() {
    // The fail-fast pattern from C: warnings, enumeration, the extensions
    // catalogue with support levels, and validation, all before any write.
    let dir = tempfile::tempdir().expect("tempdir");
    let Some(binary) = build_c_example(dir.path(), "inspect") else {
        return;
    };

    // A feature file: five layers, four extension rows, advisory findings.
    let run = Command::new(&binary)
        .arg(fixture("gdal_multilayer_1_4.gpkg"))
        .output()
        .expect("failed to run the compiled C program");
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        run.status.success(),
        "inspect.c failed:\nstdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(stdout.contains("layers: 5"), "{stdout}");
    assert!(stdout.contains("extensions: 4"), "{stdout}");
    assert!(
        stdout.contains("gpkg_rtree_index on points: implemented"),
        "{stdout}"
    );
    assert!(!stdout.contains("would decline writes"), "{stdout}");
    // The fixture has unindexed layers, so validation reports advisories.
    assert!(stdout.contains("[advisory]"), "{stdout}");
    assert!(stdout.contains("ok"), "{stdout}");

    // A tile file: the pyramid enumeration side of the same program.
    let run = Command::new(&binary)
        .arg(fixture("gdal_tiles.gpkg"))
        .output()
        .expect("failed to run the compiled C program");
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        run.status.success(),
        "inspect.c failed:\nstdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(stdout.contains("pyramids: 1"), "{stdout}");
    assert!(stdout.contains("ok"), "{stdout}");
}
