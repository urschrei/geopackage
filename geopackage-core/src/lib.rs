//! No-IO core of the [OGC GeoPackage](https://www.geopackage.org/spec140/) format.
//!
//! This crate contains the parts of the GeoPackage 1.4 specification that can be
//! expressed without a database connection:
//!
//! - [`gpb`]: the GeoPackage Binary (GPB) geometry blob header codec
//! - [`ddl`]: normative `CREATE TABLE` SQL and required `gpkg_spatial_ref_sys` seed rows
//! - [`srs`]: vendored EPSG WKT1 subset for `gpkg_spatial_ref_sys` seeding
//! - [`triggers`]: RTree spatial index virtual table and trigger SQL (version-aware)
//! - [`version`]: `application_id` / `user_version` handling
//! - [`ident`]: SQL identifier quoting
//!
//! It is deliberately dependency-light so that other implementations (e.g. `geozero`)
//! can share it. Database I/O lives in the `geopackage` crate.
//!
//! SQL text is reproduced verbatim from the spec's normative annexes
//! (Annex C "Table Definition SQL", Annex F.3 "R-tree Spatial Indexes").

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod ddl;
pub mod gpb;
pub mod ident;
pub mod srs;
pub mod triggers;
pub mod version;

pub use gpb::{Envelope, GpbError, GpbHeader};
pub use srs::SrsDefinition;
pub use version::GpkgVersion;

/// Errors produced by this crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Invalid GeoPackage Binary blob.
    #[error(transparent)]
    Gpb(#[from] GpbError),
    /// An SQL identifier that cannot be safely quoted.
    #[error("invalid SQL identifier: {0}")]
    InvalidIdentifier(String),
}
