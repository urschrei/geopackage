//! No-IO core of the [OGC GeoPackage](https://www.geopackage.org/spec140/) format.
//!
//! This crate contains the parts of the GeoPackage 1.4 specification that can be
//! expressed without a database connection:
//!
//! - [`gpb`]: the GeoPackage Binary (GPB) geometry blob header codec
//! - [`geometry`]: the parsed geometry wrapper ([`GpbGeometry`]), and the GPB
//!   encoders, from a geometry object or from bytes that are already ISO WKB
//! - [`types`]: column and geometry type vocabulary (spec Table 1, Annex G)
//! - [`datetime`]: `DATE`/`DATETIME` text form parsing (strict and lenient),
//!   calendar validation, and epoch conversion
//! - [`ddl`]: normative `CREATE TABLE` SQL and required `gpkg_spatial_ref_sys` seed rows
//! - [`srs`]: vendored EPSG WKT1 subset for `gpkg_spatial_ref_sys` seeding
//! - [`triggers`]: RTree spatial index virtual table and trigger SQL (version-aware)
//! - [`version`]: `application_id` / `user_version` handling
//! - [`ident`]: SQL identifier quoting
//!
//! It is deliberately dependency-light so that other implementations (e.g. `geozero`)
//! can share it. Database I/O lives in the `geopackage` crate, and so does
//! everything that needs a file to act on: a code outside the vendored [`srs`]
//! subset is resolved against the EPSG registry there, not here.
//!
//! SQL text is reproduced verbatim from the spec's normative annexes
//! (Annex C "Table Definition SQL", Annex F.3 "R-tree Spatial Indexes").
//!
//! # Cargo features
//!
//! - **`geo-types`** (on by default): adds [`GpbGeometry::to_geo`], converting
//!   a parsed geometry to an owned `geo-types` value. Decline it with
//!   `default-features = false`; the [`geo_traits`] implementation, which is
//!   how coordinates are read without materialising anything, does not depend
//!   on it.
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
// workspace lints table (root Cargo.toml). This crate never uses `unsafe`; the
// planned `geopackage-ffi` crate (M3) is the sole intended exception, and will
// opt out of the workspace lints rather than relax them here.

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
