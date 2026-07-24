//! No-IO core of the [OGC GeoPackage](https://www.geopackage.org/spec140/) format.
//!
//! This crate contains the parts of the GeoPackage 1.4 specification that can be
//! expressed without a database connection:
//!
//! - [`gpb`]: the GeoPackage Binary (GPB) geometry blob header codec
//! - [`geometry`]: the parsed geometry wrapper ([`GpbGeometry`])
//! - [`types`]: column and geometry type vocabulary (spec Table 1, Annex G)
//! - [`datetime`]: `DATE`/`DATETIME` text form parsing (strict and lenient)
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
//!
//! # Reading untrusted geometries
//!
//! [`GpbGeometry`] parses WKB bodies with the `wkb` crate, whose 0.9.2 reader
//! pre-allocates from element counts read out of the blob without bounding them
//! against the buffer: a malformed geometry declaring a `0xFFFFFFFF`-member
//! collection drives a multi-gigabyte allocation. The fix belongs upstream in
//! [georust/wkb](https://github.com/georust/wkb); until it lands and this crate
//! bumps its dependency, do not parse geometries from untrusted sources.

// `unsafe_code = "forbid"` and `missing_docs = "warn"` come from the
// workspace lints table (root Cargo.toml); see roadmap decision D12 for the
// unsafe policy and its single planned exception (`geopackage-ffi`, M3).

pub mod datetime;
pub mod ddl;
pub mod geometry;
pub mod gpb;
pub mod ident;
pub mod srs;
pub mod triggers;
pub mod types;
pub mod version;

pub use geometry::{GeometryError, GpbGeometry};
pub use gpb::{Envelope, GpbError, GpbHeader};
pub use srs::SrsDefinition;
pub use types::{ColumnType, GeometryType, ZmFlag};
pub use version::GpkgVersion;

/// Errors produced by this crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Invalid GeoPackage Binary blob.
    #[error(transparent)]
    Gpb(#[from] GpbError),
    /// A GeoPackage geometry that could not be parsed or read.
    #[error(transparent)]
    Geometry(#[from] GeometryError),
    /// An SQL identifier that cannot be safely quoted.
    #[error("invalid SQL identifier: {0}")]
    InvalidIdentifier(String),
}
