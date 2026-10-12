//! The parts of the [OGC GeoPackage](https://www.geopackage.org/spec140/)
//! format that don't need a database.
//!
//! This crate contains the geometry codec, table definitions and validation
//! rules of GeoPackage 1.4, with no SQLite dependency. Most users want the
//! `geopackage` crate, which builds the SQLite container on top of this one.
//! Use this crate directly for format code without SQLite: in a fuzz target,
//! for example, or another GeoPackage implementation.
//!
//! # Modules
//!
//! - **[`gpb`]**: The GeoPackage Binary (GPB) geometry header codec
//! - **[`geometry`]**: A parsed geometry view ([`GpbGeometry`]), and GPB
//!   encoders for geometry objects and existing ISO WKB
//! - **[`curve`]**: Envelopes read directly from ISO WKB, including exact
//!   extents for circular arcs
//! - **[`types`]**: Column and geometry types (spec Table 1, Annex G)
//! - **[`datetime`]**: `DATE` and `DATETIME` parsing, strict or lenient, with
//!   calendar validation and epoch conversion
//! - **[`ddl`]**: `CREATE TABLE` statements and the required
//!   `gpkg_spatial_ref_sys` rows
//! - **[`srs`]**: A subset of EPSG WKT1 definitions for
//!   `gpkg_spatial_ref_sys`
//! - **[`triggers`]**: RTree spatial index tables and triggers, by spec
//!   version
//! - **[`tiles`]**: Tile pyramid table definitions and the tile matrix model
//! - **[`coverage`]**: Payload rules for tiled gridded coverages, checked from
//!   the TIFF or PNG header
//! - **[`schema`]**: Column descriptions and value constraints from the
//!   `gpkg_schema` extension (Annex F.9)
//! - **[`metadata`]**: Metadata records and references from the
//!   `gpkg_metadata` extension (Annex F.8)
//! - **[`related`]**: Relationships from the Related Tables Extension
//!   (OGC 18-000)
//! - **[`version`]**: `application_id` and `user_version` handling
//! - **[`ident`]**: SQL identifier quoting
//!
//! SQL is copied verbatim from the spec's Annex C ("Table Definition SQL") and
//! Annex F.3 ("R-tree Spatial Indexes").
//!
//! # Example
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
//! - **`geo-types`** (default): Enables [`GpbGeometry::to_geo`], which converts
//!   a parsed geometry to a `geo-types` geometry. The [`geo_traits`]
//!   implementation doesn't need it
//!
//! # Untrusted input
//!
//! [`GpbGeometry`] parses WKB with the `wkb` crate. Its 0.9.2 reader allocates
//! based on element counts read from the blob without checking them against
//! the blob's length, so a malformed geometry can trigger a multi-gigabyte
//! allocation. Until this is fixed upstream in
//! [georust/wkb](https://github.com/georust/wkb), don't parse geometries from
//! untrusted sources.

// `unsafe_code = "forbid"` and `missing_docs = "warn"` come from the
// workspace lints table (root Cargo.toml). This crate never uses `unsafe`; the
// `geopackage-ffi` crate is the sole exception, and opts out of the workspace
// lints rather than relaxing them here.

mod error;

pub mod coverage;
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
