//! A C ABI for the [`geopackage`] crate.
//!
//! This crate exists to be linked against from C, not to be used from Rust: a
//! Rust caller should use `geopackage` directly, which is safe. Everything here
//! is `unsafe extern "C"`.
//!
//! # The unsafe carve-out
//!
//! The workspace sets `unsafe_code = "forbid"` and every other member inherits
//! it. This crate is the single exception (roadmap decision D12): a C ABI
//! cannot be written without `unsafe`, so the policy quarantines it in one
//! reviewable place rather than weakening it everywhere. In exchange this crate
//! turns on `undocumented_unsafe_blocks`, `multiple_unsafe_ops_per_block` and
//! `missing_safety_doc` as denies, so every `unsafe` block carries a written
//! justification, does one thing, and every `unsafe fn` states its contract.
//!
//! The interesting unsafe is not the pointer marshalling, which is routine, but
//! the lifetime erasure in [`handle`]. Read that module first.
//!
//! # Conventions
//!
//! - **Strings** are NUL-terminated UTF-8 in both directions. A string this
//!   library returns is owned by the caller and released with
//!   [`gpkg_string_free`]; a string the caller passes in is borrowed for the
//!   duration of the call only.
//! - **Errors** go through an optional `gpkg_error_t *` out-parameter. NULL
//!   means the caller does not want the detail. A filled-in error owns a
//!   message that must be released with [`gpkg_error_clear`].
//! - **Failure** is NULL for functions returning a pointer, and a non-`OK`
//!   [`Status`] for functions returning one.
//! - **Handles** are opaque. Each has exactly one destructor, and using a
//!   handle after its destructor is undefined behaviour, as it is in C
//!   generally.
//!
//! # Threading
//!
//! **One handle per thread.** `geopackage::GeoPackage` is `Send` but not
//! `Sync`, because `rusqlite::Connection` is, so a handle may be created on one
//! thread and used on another, but never used from two at once. Nothing here is
//! internally locked: a caller wanting concurrent access should open the file
//! once per thread, which is also what gives SQLite its own per-connection
//! state. Reads across separate connections are safe and are how the library's
//! own threaded Arrow reader works.

pub mod container;
pub mod error;
pub mod handle;
pub mod layer;
pub mod stream;
pub mod util;

pub use container::{
    gpkg_close, gpkg_create, gpkg_open, gpkg_open_read_only, gpkg_open_warning,
    gpkg_open_warning_count, gpkg_t, gpkg_version,
};
pub use error::{Status, gpkg_error_clear, gpkg_error_t};
pub use layer::{
    gpkg_attributes_open, gpkg_layer_count, gpkg_layer_free, gpkg_layer_name, gpkg_layer_name_at,
    gpkg_layer_names_count, gpkg_layer_open, gpkg_layer_t,
};
pub use stream::{gpkg_layer_read_arrow, gpkg_layer_read_arrow_in};
pub use util::gpkg_string_free;
