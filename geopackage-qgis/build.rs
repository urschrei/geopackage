//! Compiles the C++ shim against QGIS and Qt, and links them.
//!
//! The QGIS headers are found from `QGIS_PREFIX` (default `/usr`), as
//! `$QGIS_PREFIX/include/qgis` and `$QGIS_PREFIX/lib`. Qt is found with
//! `pkg-config`, since QGIS's headers include Qt's.

use std::env;
use std::process::Command;

fn pkg_config(args: &[&str]) -> Vec<String> {
    let output = Command::new("pkg-config")
        .args(args)
        .output()
        .expect("pkg-config is needed to find Qt");
    assert!(output.status.success(), "pkg-config {args:?} failed");
    String::from_utf8(output.stdout)
        .expect("pkg-config output is UTF-8")
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

fn main() {
    let prefix = env::var("QGIS_PREFIX").unwrap_or_else(|_| "/usr".to_owned());
    let qt = ["Qt5Core", "Qt5Gui", "Qt5Xml", "Qt5Widgets"];

    let mut build = cxx_build::bridge("src/lib.rs");
    build
        .file("src/provider.cpp")
        .std("c++17")
        .include(format!("{prefix}/include/qgis"))
        .flag_if_supported("-Wno-deprecated-declarations")
        .flag_if_supported("-Wno-unused-parameter");
    let mut cflags = vec!["--cflags"];
    cflags.extend(qt);
    for flag in pkg_config(&cflags) {
        if let Some(dir) = flag.strip_prefix("-I") {
            build.include(dir);
        } else if let Some(define) = flag.strip_prefix("-D") {
            build.define(define, None);
        }
    }
    build.compile("geopackage_qgis_shim");

    println!("cargo:rustc-link-search=native={prefix}/lib");
    println!("cargo:rustc-link-lib=dylib=qgis_core");
    let mut libs = vec!["--libs"];
    libs.extend(qt);
    for flag in pkg_config(&libs) {
        if let Some(lib) = flag.strip_prefix("-l") {
            println!("cargo:rustc-link-lib=dylib={lib}");
        } else if let Some(dir) = flag.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={dir}");
        }
    }
    println!("cargo:rerun-if-changed=src/provider.cpp");
    println!("cargo:rerun-if-changed=src/provider.h");
    println!("cargo:rerun-if-env-changed=QGIS_PREFIX");
}
