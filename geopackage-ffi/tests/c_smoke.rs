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

#[test]
fn a_c_program_links_against_the_library_and_reads_a_file() {
    if cfg!(target_os = "windows") {
        eprintln!("skipped: the Windows link line wants its own handling");
        return;
    }

    let Some(target) = target_dir() else {
        eprintln!("skipped: could not locate the target directory");
        return;
    };
    let static_lib = target.join("libgeopackage_ffi.a");
    if !static_lib.exists() {
        eprintln!(
            "skipped: {} has not been built; run `cargo build -p geopackage-ffi` first",
            static_lib.display()
        );
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let binary = dir.path().join("smoke");
    let source = manifest_dir().join("examples/smoke.c");
    let include = manifest_dir().join("include");

    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    let mut compile = Command::new(&cc);
    compile
        .arg("-I")
        .arg(&include)
        // The header is public API; it should compile clean under a strict
        // warning set, not merely parse.
        .args(["-Wall", "-Wextra", "-Werror", "-std=c11"])
        .arg(&source)
        .arg(&static_lib)
        .args(native_libs())
        .arg("-o")
        .arg(&binary);

    let compiled = compile.output().expect("failed to run the C compiler");
    assert!(
        compiled.status.success(),
        "compiling smoke.c failed:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    let fixture = manifest_dir()
        .parent()
        .expect("the crate directory has a parent")
        .join("geopackage/tests/fixtures/gdal_multilayer_1_4.gpkg");

    let run = Command::new(&binary)
        .arg(&fixture)
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
fn the_committed_header_matches_what_cbindgen_produces() {
    // The API-stability gate: an ABI change has to show up as a diff in the
    // committed header rather than reaching a consumer unannounced.
    let Ok(cbindgen) = which_cbindgen() else {
        eprintln!("skipped: cbindgen is not installed");
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

    let fresh = std::fs::read_to_string(&generated).expect("read generated header");
    let committed = std::fs::read_to_string(manifest_dir().join("include/geopackage.h"))
        .expect("read committed header");
    assert_eq!(
        fresh.trim_end(),
        committed.trim_end(),
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
