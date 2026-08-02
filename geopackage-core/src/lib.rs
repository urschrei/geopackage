//! No-IO core of the [OGC GeoPackage](https://www.geopackage.org/spec140/) format.
//!
//! This crate holds the parts of the GeoPackage 1.4 specification that can be
//! expressed without a database connection. Most users want the `geopackage`
//! crate, which adds the SQLite container on top of this one. Use this crate
//! directly when you need the format code without SQLite: a fuzz target, or
//! another GeoPackage implementation sharing the codec and DDL.
//!
//! - [`gpb`]: the GeoPackage Binary (GPB) geometry blob header codec
//! - [`geometry`]: the parsed geometry view ([`GpbGeometry`]), and the GPB
//!   encoders, from a geometry object or from bytes that are already ISO WKB
//! - [`types`]: column and geometry type vocabulary (spec Table 1, Annex G)
//! - [`datetime`]: `DATE`/`DATETIME` text form parsing (strict and lenient),
//!   calendar validation, and epoch conversion
//! - [`ddl`]: normative `CREATE TABLE` SQL and required `gpkg_spatial_ref_sys` seed rows
//! - [`srs`]: vendored EPSG WKT1 subset for `gpkg_spatial_ref_sys` seeding
//! - [`tiles`]: tile pyramid user table SQL and the tile matrix model
//! - [`triggers`]: RTree spatial index virtual table and trigger SQL (version-aware)
//! - [`version`]: `application_id` / `user_version` handling
//! - [`ident`]: SQL identifier quoting
//! - [`schema`]: the `gpkg_schema` extension's column descriptions and
//!   value constraints (Annex F.9)
//! - [`metadata`]: the `gpkg_metadata` extension's records and the references
//!   attaching them to a file's contents (Annex F.8)
//! - [`related`]: the Related Tables Extension's relationships (OGC 18-000)
//! - [`curve`]: envelopes read straight from ISO WKB, non-linear types
//!   included, with exact circular-arc extents
//!
//! SQL text is reproduced verbatim from the spec's normative annexes
//! (Annex C "Table Definition SQL", Annex F.3 "R-tree Spatial Indexes").
//!
//! Encode a geometry as a GPB blob and parse it back:
//!
//! ```
//! # #[cfg(feature = "geo-types")]
//! # fn main() -> Result<(), geopackage_core::Error> {
//! use geopackage_core::GpbGeometry;
//! use geopackage_core::geometry::encode_gpb;
//!
//! let point = geo_types::Point::new(-6.26, 53.35);
//! let (blob, envelope) = encode_gpb(&point, 4326)?;
//! assert_eq!(envelope, Some([-6.26, -6.26, 53.35, 53.35]));
//!
//! let parsed = GpbGeometry::parse(&blob)?;
//! assert_eq!(parsed.header().srs_id, 4326);
//! assert_eq!(parsed.xy_envelope(), envelope);
//! # Ok(()) }
//! # #[cfg(not(feature = "geo-types"))]
//! # fn main() {}
//! ```
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

mod error;

pub mod curve;
pub mod datetime;
pub mod ddl;
pub mod extensions;
pub mod geometry;
pub mod gpb;
pub mod ident;
pub mod metadata;
pub mod related;
pub mod schema;
pub mod srs;
pub mod tiles;
pub mod triggers;
pub mod types;
pub mod version;

pub use error::Error;
pub use geometry::{GeometryError, GpbGeometry};
pub use gpb::{Envelope, GpbError, GpbHeader};
pub use srs::SrsDefinition;
pub use tiles::{TileCoord, TileError, TileFormat, TileMatrix, TileMatrixSet};
pub use types::{ColumnType, GeometryType, GeometryTypeSet, ZmFlag};
pub use version::GpkgVersion;
