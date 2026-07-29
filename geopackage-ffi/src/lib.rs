//! A C ABI for the [`geopackage`] crate: reading and writing OGC GeoPackage
//! files from C.
//!
//! This crate exists to be linked against from C, not to be used from Rust: a
//! Rust caller should use [`geopackage`] directly, which is safe and has a
//! wider surface. Everything exported here is `unsafe extern "C"`.
//!
//! The header is `include/geopackage.h`. It is generated from this crate with
//! cbindgen and checked in, so the comments below are the comments a C consumer
//! reads beside each declaration. `cargo cinstall -p geopackage-ffi` installs
//! that header, a versioned library and a pkg-config file named `geopackage`.
//!
//! # Opening a file
//!
//! Every call that can fail takes an optional `gpkg_error_t *`. On failure it
//! is filled in with a code and an owned message, which
//! [`gpkg_error_clear`] releases; clearing also resets the code, so one error
//! variable serves a whole program.
//!
//! ```c
//! #include <stdio.h>
//! #include "geopackage.h"
//!
//! int main(void) {
//!     gpkg_error_t error = {GPKG_STATUS_OK, NULL};
//!
//!     gpkg_t *gpkg = gpkg_open_read_only("places.gpkg", &error);
//!     if (!gpkg) {
//!         fprintf(stderr, "open failed (%d): %s\n", (int)error.code,
//!                 error.message ? error.message : "(no message)");
//!         gpkg_error_clear(&error);
//!         return 1;
//!     }
//!
//!     char *version = gpkg_version(gpkg, &error);
//!     if (version) {
//!         printf("GeoPackage %s\n", version);
//!         gpkg_string_free(version);
//!     }
//!
//!     if (gpkg_close(gpkg, &error) != GPKG_STATUS_OK) {
//!         fprintf(stderr, "close failed: %s\n",
//!                 error.message ? error.message : "(no message)");
//!         gpkg_error_clear(&error);
//!         return 1;
//!     }
//!     return 0;
//! }
//! ```
//!
//! # Conventions
//!
//! - **Strings** are NUL-terminated UTF-8 in both directions. A string this
//!   library returns is owned by the caller and released with
//!   [`gpkg_string_free`]; a string the caller passes in is borrowed for the
//!   duration of the call only.
//! - **Errors** go through an optional `gpkg_error_t *` out-parameter. NULL
//!   means the caller wants only the return value. A filled-in error owns its
//!   message, so clear it before reusing the same variable: a second failure
//!   overwrites the pointer, and the first message is then unreachable.
//! - **Failure** is NULL for a function returning a pointer, and a status other
//!   than `GPKG_STATUS_OK` for a function returning [`Status`]. The codes are a
//!   small set of categories to branch on, and the message carries the detail.
//! - **Handles** are opaque, and each has exactly one destructor: `gpkg_t` from
//!   [`gpkg_open`], [`gpkg_open_read_only`] or [`gpkg_create`] is destroyed by
//!   [`gpkg_close`]; `gpkg_layer_t` from [`gpkg_layer_open`] or
//!   [`gpkg_attributes_open`] by [`gpkg_layer_free`]; `gpkg_tiles_t` from
//!   [`gpkg_tiles_open`] by [`gpkg_tiles_free`]. Using a handle after its
//!   destructor is undefined behaviour.
//!
//! # Handle lifetime
//!
//! A layer handle, a tiles handle and an Arrow stream each borrow the container
//! they were taken from. A container with any of them outstanding cannot be
//! closed: [`gpkg_close`] returns `GPKG_STATUS_HANDLE_IN_USE`, changes nothing,
//! and leaves the container open and usable. Free the children first.
//!
//! ```c
//! gpkg_error_t error = {GPKG_STATUS_OK, NULL};
//! gpkg_t *gpkg = gpkg_open_read_only("places.gpkg", &error);
//! gpkg_layer_t *layer = gpkg_layer_open(gpkg, "points", &error);
//!
//! struct ArrowArrayStream stream;
//! memset(&stream, 0, sizeof(stream));
//! gpkg_layer_read_arrow(layer, &stream, &error);
//!
//! // Refused while either child is alive, and nothing is torn down.
//! if (gpkg_close(gpkg, &error) == GPKG_STATUS_HANDLE_IN_USE) {
//!     gpkg_error_clear(&error);
//! }
//!
//! stream.release(&stream);
//! gpkg_layer_free(layer);
//! gpkg_close(gpkg, &error);  // now it succeeds
//! ```
//!
//! The refusal is the runtime form of the rule the Rust API states in its
//! types, where `close` takes `self` and a live `Layer` borrows it, so the
//! compiler rejects the same program instead. [`handle`] describes how the
//! borrow is erased and what keeps the erasure sound.
//!
//! # What the ABI covers
//!
//! - [`container`]: opening, closing, the version a file declares, the warnings
//!   a lenient open collected, and the transaction calls [`gpkg_begin`],
//!   [`gpkg_commit`], [`gpkg_rollback`] and [`gpkg_in_transaction`].
//! - [`layer`]: opening a feature or attribute layer by name, enumerating a
//!   file's feature layers, and counting rows.
//! - [`schema`]: what a layer declares. Columns and their types, the geometry
//!   column and its spatial reference system, the extent, and the spatial
//!   index.
//! - [`stream`]: the data plane. Layers are read and written as Arrow record
//!   batches through the C Data Interface, and a layer can be created from an
//!   Arrow schema without its columns being described in C at all.
//! - [`tiles`]: tile pyramids, addressed by zoom level, column and row.
//!   Payloads cross the boundary as the bytes the file holds; nothing is
//!   decoded.
//! - [`error`] and [`util`]: the status codes, the error out-parameter, and
//!   [`gpkg_string_free`]. The other deallocator, [`gpkg_bytes_free`], sits
//!   with [`tiles`], which is the only thing that hands out a buffer.
//!
//! Feature data moves only as Arrow. There is no row-at-a-time feature API on
//! this side, so a C consumer reads a layer by pulling batches and writes one
//! by pushing them. Validation, metadata and the related-tables extension are
//! not exposed at all: they stay behind the Rust API, and `gpkg validate`
//! covers the first from the command line.
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
pub mod schema;
pub mod stream;
pub mod tiles;
pub mod util;

pub use container::{
    gpkg_begin, gpkg_close, gpkg_commit, gpkg_create, gpkg_in_transaction, gpkg_open,
    gpkg_open_read_only, gpkg_open_warning, gpkg_open_warning_count, gpkg_rollback, gpkg_t,
    gpkg_version,
};
pub use error::{Status, gpkg_error_clear, gpkg_error_t};
pub use layer::{
    gpkg_attributes_open, gpkg_layer_count, gpkg_layer_free, gpkg_layer_name, gpkg_layer_name_at,
    gpkg_layer_names_count, gpkg_layer_open, gpkg_layer_t,
};
pub use schema::{
    gpkg_layer_column_count, gpkg_layer_column_is_primary_key, gpkg_layer_column_name,
    gpkg_layer_column_type, gpkg_layer_create_spatial_index, gpkg_layer_drop_spatial_index,
    gpkg_layer_extent, gpkg_layer_geometry_column, gpkg_layer_geometry_type,
    gpkg_layer_has_spatial_index, gpkg_layer_kind, gpkg_layer_repair_spatial_index,
    gpkg_layer_spatial_index_status, gpkg_layer_srs_id,
};
pub use stream::{
    gpkg_add_epsg_srs, gpkg_create_layer_from_arrow_schema, gpkg_layer_read_arrow,
    gpkg_layer_read_arrow_in, gpkg_layer_write_arrow,
};
pub use tiles::{
    gpkg_bytes_free, gpkg_tiles_count, gpkg_tiles_count_at, gpkg_tiles_delete, gpkg_tiles_extent,
    gpkg_tiles_free, gpkg_tiles_get, gpkg_tiles_get_into, gpkg_tiles_has, gpkg_tiles_matrix_at,
    gpkg_tiles_name, gpkg_tiles_open, gpkg_tiles_put, gpkg_tiles_t, gpkg_tiles_zoom_level_count,
};
pub use util::gpkg_string_free;
