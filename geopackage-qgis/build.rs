//! Compiles the C++ shim against QGIS 4 and Qt 6, and links what it must.
//!
//! # What is linked
//!
//! The library is only ever loaded into a running QGIS, which has QGIS's and
//! Qt's libraries loaded already. So on Linux and macOS it links neither:
//! their symbols are resolved from the process when the library loads, and a
//! build needs QGIS's and Qt's headers only. That keeps a build from naming
//! one QGIS patch release (on Linux, `libqgis_core`'s soname carries the full
//! version) or a newer Qt than the QGIS it loads into, and on macOS it means
//! nothing inside QGIS.app is linked. Windows has no such late binding, so
//! there it links QGIS's and Qt's import libraries.
//!
//! SQLite is QGIS's too, on Linux and macOS: two copies in one process do not
//! see each other's locks. On Linux the library depends on `libsqlite3.so.0`,
//! which the loader resolves to QGIS's. On macOS it links
//! `@rpath/libsqlite3.dylib`, QGIS.app's name for it, through the stub that
//! `scripts/host_sqlite.py` makes; the build stops without that script's
//! settings, which would otherwise link Apple's SQLite.
//!
//! # Where they come from
//!
//! - QGIS headers: `QGIS_INCLUDE_DIR`, else `$QGIS_PREFIX/include/qgis` (or
//!   `$QGIS_PREFIX/include`, as conda-forge installs them on Windows).
//!   `QGIS_PREFIX` defaults to the active conda environment (its `Library`
//!   folder on Windows), which is how CI gets QGIS 4, and then to `/usr`.
//!   `scripts/qgis_headers.py` makes the directory from QGIS's source, for a
//!   QGIS installed without headers, such as QGIS.app.
//! - QGIS's import library (Windows): `QGIS_LIB_DIR`, else `$QGIS_PREFIX/lib`.
//! - Qt: `QT_DIR`, a Qt prefix in any of the usual layouts (`include/qt6`,
//!   `include`, or macOS frameworks under `lib`); else `QT_INCLUDE_DIR` (and
//!   on Windows `QT_LIB_DIR`); else `pkg-config`.
//!
//! Qt's headers must be no newer than the Qt the library will load into, or
//! its inline code may call functions that Qt lacks. QGIS 4.0 needs Qt 6.6 or
//! later, so 6.6's headers suit every QGIS 4.

use std::env;
use std::path::Path;
use std::process::Command;

/// `libsqlite3.so.0` as a dependency whether or not the linker would keep
/// it. Rust's `-as-needed` link modifier would say this, but is unstable.
const KEEP_SQLITE: &str = "-Wl,--push-state,--no-as-needed,-lsqlite3,--pop-state";

/// The Qt modules QGIS's core headers include (`qgsapplication.h` brings in
/// Widgets).
const QT: [&str; 4] = ["Core", "Gui", "Xml", "Widgets"];

fn pkg_config(args: &[&str]) -> Vec<String> {
    let output = Command::new("pkg-config")
        .args(args)
        .output()
        .expect("pkg-config is needed to find Qt 6, or set QT_DIR");
    assert!(output.status.success(), "pkg-config {args:?} failed");
    String::from_utf8(output.stdout)
        .expect("pkg-config output is UTF-8")
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// Adds a QGIS or Qt include directory as a system one, so warnings in those
/// headers (QGIS 4's still name deprecated Qt 5 types) don't bury the shim's.
fn system_include(build: &mut cc::Build, msvc: bool, dir: &str) {
    if msvc {
        build.flag(format!("/external:I{dir}")).flag("/external:W0");
    } else {
        build.flag("-isystem").flag(dir);
    }
}

fn var(name: &str) -> Option<String> {
    println!("cargo:rerun-if-env-changed={name}");
    env::var(name).ok().filter(|v| !v.is_empty())
}

/// Qt's include directories, any framework directory, and its library
/// directory, from a Qt prefix in whichever layout it has.
fn qt_from_prefix(dir: &str) -> (Vec<String>, Option<String>, String) {
    let lib = format!("{dir}/lib");
    // macOS frameworks, under `lib` in a Qt install or directly in a
    // directory such as QGIS.app/Contents/Frameworks: `<QtCore/qstring.h>`
    // through -F, `<QString>` through each framework's Headers.
    for frameworks in [lib.as_str(), dir] {
        if Path::new(&format!("{frameworks}/QtCore.framework/Headers")).is_dir() {
            let includes = QT
                .iter()
                .map(|m| format!("{frameworks}/Qt{m}.framework/Headers"))
                .collect();
            return (includes, Some(frameworks.to_owned()), frameworks.to_owned());
        }
    }
    let root = [format!("{dir}/include/qt6"), format!("{dir}/include")]
        .into_iter()
        .find(|d| Path::new(&format!("{d}/QtCore")).is_dir())
        .unwrap_or_else(|| panic!("no Qt headers under {dir}/include/qt6 or {dir}/include"));
    let mut includes = vec![root.clone()];
    includes.extend(QT.iter().map(|m| format!("{root}/Qt{m}")));
    (includes, None, lib)
}

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let windows = target_os == "windows";
    let msvc = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");

    let prefix = var("QGIS_PREFIX")
        .or_else(|| var("CONDA_PREFIX").map(|p| if windows { format!("{p}/Library") } else { p }))
        .unwrap_or_else(|| "/usr".to_owned());
    // `include/qgis` as QGIS installs itself; `include` as conda-forge's
    // Windows package does.
    let qgis_include = var("QGIS_INCLUDE_DIR")
        .or_else(|| {
            [
                format!("{prefix}/include/qgis"),
                format!("{prefix}/include"),
            ]
            .into_iter()
            .find(|d| Path::new(&format!("{d}/qgsconfig.h")).is_file())
        })
        .unwrap_or_else(|| format!("{prefix}/include/qgis"));
    assert!(
        Path::new(&format!("{qgis_include}/qgsconfig.h")).is_file(),
        "no QGIS headers in {qgis_include}: set QGIS_INCLUDE_DIR or QGIS_PREFIX"
    );

    let mut build = cxx_build::bridge("src/lib.rs");
    // QGIS 4 is C++20 throughout.
    build.file("src/provider.cpp").std("c++20");
    if msvc {
        // Qt 6 requires these of MSVC; QGIS's headers use M_PI and std::min.
        build
            .flag("/Zc:__cplusplus")
            .flag("/permissive-")
            .flag("/utf-8")
            .define("_USE_MATH_DEFINES", None)
            .define("NOMINMAX", None);
    } else {
        build.flag_if_supported("-Wno-unused-parameter");
    }
    system_include(&mut build, msvc, &qgis_include);
    for module in QT {
        build.define(&format!("QT_{}_LIB", module.to_ascii_uppercase()), None);
    }

    // Qt's headers, and on Windows where its import libraries are.
    let mut qt_lib = var("QT_LIB_DIR");
    if let Some(dir) = var("QT_DIR") {
        let (includes, frameworks, lib) = qt_from_prefix(&dir);
        for include in &includes {
            system_include(&mut build, msvc, include);
        }
        if let Some(frameworks) = frameworks {
            build.flag(format!("-F{frameworks}"));
        }
        qt_lib = qt_lib.or(Some(lib));
    } else if let Some(dir) = var("QT_INCLUDE_DIR") {
        system_include(&mut build, msvc, &dir);
        for module in QT {
            system_include(&mut build, msvc, &format!("{dir}/Qt{module}"));
        }
    } else {
        let mut args = vec!["--cflags"];
        let modules: Vec<String> = QT.iter().map(|m| format!("Qt6{m}")).collect();
        args.extend(modules.iter().map(String::as_str));
        for flag in pkg_config(&args) {
            if let Some(dir) = flag.strip_prefix("-I") {
                system_include(&mut build, msvc, dir);
            } else if let Some(define) = flag.strip_prefix("-D") {
                build.define(define, None);
            } else if flag.starts_with("-F") {
                build.flag(&flag);
            }
        }
    }
    build.compile("geopackage_qgis_shim");

    match target_os.as_str() {
        "windows" => {
            let qgis_lib = var("QGIS_LIB_DIR").unwrap_or_else(|| format!("{prefix}/lib"));
            println!("cargo:rustc-link-search=native={qgis_lib}");
            println!("cargo:rustc-link-lib=dylib=qgis_core");
            let qt_lib = qt_lib.unwrap_or_else(|| format!("{prefix}/lib"));
            println!("cargo:rustc-link-search=native={qt_lib}");
            for module in QT {
                println!("cargo:rustc-link-lib=dylib=Qt6{module}");
            }
        }
        // ELF shared libraries may leave symbols undefined by default; the
        // loader binds them to the QGIS process's.
        "macos" => {
            println!("cargo:rustc-cdylib-link-arg=-Wl,-undefined,dynamic_lookup");
            assert!(
                var("SQLITE3_LIB_DIR").is_some() && var("SQLITE3_NO_PKG_CONFIG").is_some(),
                "set the environment that scripts/host_sqlite.py prints, so that the \
                 library links QGIS's SQLite"
            );
            // Where dyld looks for `@rpath/libsqlite3.dylib` if QGIS has not
            // loaded it under that name: QGIS.app's `Contents/Frameworks`, and
            // a conda environment's `lib`.
            for path in ["@executable_path/../Frameworks", "@executable_path/../lib"] {
                println!("cargo:rustc-cdylib-link-arg=-Wl,-rpath,{path}");
            }
        }
        // `libsqlite3.so.0` stays a dependency even where the linker would
        // drop it: in the manylinux_2_28 containers it did, and left the
        // SQLite symbols to whatever the process had loaded globally.
        "linux" => println!("cargo:rustc-cdylib-link-arg={KEEP_SQLITE}"),
        _ => {}
    }
    println!("cargo:rerun-if-changed=src/provider.cpp");
    println!("cargo:rerun-if-changed=src/provider.h");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo:rerun-if-env-changed=SQLITE3_LIB_DIR");
    println!("cargo:rerun-if-env-changed=SQLITE3_NO_PKG_CONFIG");
}
