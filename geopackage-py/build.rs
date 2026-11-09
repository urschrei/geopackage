//! Link settings for a build that links the host's SQLite.
//!
//! The QGIS plugin's build links `@rpath/libsqlite3.dylib`, the name that
//! QGIS.app gives its SQLite (see `scripts/host_sqlite.py`). Where QGIS
//! loaded a library of that name, dyld uses it. Elsewhere dyld looks for the
//! file on the run paths: QGIS.app keeps its libraries in
//! `Contents/Frameworks`, next to the executable's directory, and a conda
//! environment keeps them in `lib`, next to `bin`.

use std::env;

/// `libsqlite3.so.0` as a dependency whether or not the linker would keep
/// it. Rust's `-as-needed` link modifier would say this, but is unstable.
const KEEP_SQLITE: &str = "-Wl,--push-state,--no-as-needed,-lsqlite3,--pop-state";

fn main() {
    if env::var_os("CARGO_FEATURE_BUNDLED").is_some() {
        return;
    }
    match env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("macos") => {
            for path in ["@executable_path/../Frameworks", "@executable_path/../lib"] {
                println!("cargo:rustc-cdylib-link-arg=-Wl,-rpath,{path}");
            }
        }
        // `libsqlite3.so.0` stays a dependency even where the linker would
        // drop it: in the manylinux_2_28 containers it did, and left the
        // SQLite symbols to whatever the process had loaded globally.
        Ok("linux") => println!("cargo:rustc-cdylib-link-arg={KEEP_SQLITE}"),
        _ => {}
    }
}
