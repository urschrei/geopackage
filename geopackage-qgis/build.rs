//! Compiles the C++ shim against QGIS 4 and Qt 6, and links them.
//!
//! QGIS is found under `QGIS_PREFIX`: `$QGIS_PREFIX/include/qgis` and
//! `$QGIS_PREFIX/lib`. It defaults to an activated conda environment's
//! `CONDA_PREFIX`, which is how CI installs QGIS 4 (from conda-forge), and
//! otherwise to `/usr`.
//!
//! Qt is found with `pkg-config`, since QGIS's headers include Qt's. Setting
//! `QT_INCLUDE_DIR` (the directory holding `QtCore/`) and `QT_LIB_DIR`
//! instead names them directly, for a Qt that is unpacked rather than
//! installed, whose `.pc` files point elsewhere.

use std::env;
use std::process::Command;

/// The Qt modules QGIS's core headers include (`qgsapplication.h` brings in
/// Widgets).
const QT: [&str; 4] = ["Core", "Gui", "Xml", "Widgets"];

fn pkg_config(args: &[&str]) -> Vec<String> {
    let output = Command::new("pkg-config")
        .args(args)
        .output()
        .expect("pkg-config is needed to find Qt 6, or set QT_INCLUDE_DIR and QT_LIB_DIR");
    assert!(output.status.success(), "pkg-config {args:?} failed");
    String::from_utf8(output.stdout)
        .expect("pkg-config output is UTF-8")
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// Adds a QGIS or Qt include directory as a system one, so warnings in those
/// headers (QGIS 4's still name deprecated Qt 5 types) don't bury the shim's.
fn system_include(build: &mut cc::Build, dir: &str) {
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        build.flag(format!("/external:I{dir}")).flag("/external:W0");
    } else {
        build.flag("-isystem").flag(dir);
    }
}

fn main() {
    let prefix = env::var("QGIS_PREFIX")
        .or_else(|_| env::var("CONDA_PREFIX"))
        .unwrap_or_else(|_| "/usr".to_owned());
    let modules: Vec<String> = QT.iter().map(|m| format!("Qt6{m}")).collect();

    let mut build = cxx_build::bridge("src/lib.rs");
    build
        .file("src/provider.cpp")
        .std("c++17")
        .flag_if_supported("-Wno-unused-parameter");
    system_include(&mut build, &format!("{prefix}/include/qgis"));
    for module in QT {
        build.define(&format!("QT_{}_LIB", module.to_ascii_uppercase()), None);
    }

    let qt_include = env::var("QT_INCLUDE_DIR").ok();
    if let Some(dir) = &qt_include {
        system_include(&mut build, dir);
        for module in QT {
            system_include(&mut build, &format!("{dir}/Qt{module}"));
        }
    } else {
        let mut args = vec!["--cflags"];
        args.extend(modules.iter().map(String::as_str));
        for flag in pkg_config(&args) {
            if let Some(dir) = flag.strip_prefix("-I") {
                system_include(&mut build, dir);
            } else if let Some(define) = flag.strip_prefix("-D") {
                build.define(define, None);
            }
        }
    }
    build.compile("geopackage_qgis_shim");

    println!("cargo:rustc-link-search=native={prefix}/lib");
    println!("cargo:rustc-link-lib=dylib=qgis_core");
    if let Ok(dir) = env::var("QT_LIB_DIR") {
        println!("cargo:rustc-link-search=native={dir}");
        for module in &modules {
            println!("cargo:rustc-link-lib=dylib={module}");
        }
    } else {
        let mut args = vec!["--libs"];
        args.extend(modules.iter().map(String::as_str));
        for flag in pkg_config(&args) {
            if let Some(lib) = flag.strip_prefix("-l") {
                println!("cargo:rustc-link-lib=dylib={lib}");
            } else if let Some(dir) = flag.strip_prefix("-L") {
                println!("cargo:rustc-link-search=native={dir}");
            }
        }
    }
    println!("cargo:rerun-if-changed=src/provider.cpp");
    println!("cargo:rerun-if-changed=src/provider.h");
    for var in [
        "QGIS_PREFIX",
        "CONDA_PREFIX",
        "QT_INCLUDE_DIR",
        "QT_LIB_DIR",
        "PKG_CONFIG_PATH",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
}
