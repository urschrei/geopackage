//! The crate's error type: every failure the format primitives can report.

use crate::geometry::GeometryError;
use crate::gpb::GpbError;
use crate::tiles::TileError;

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
    /// A tile pyramid that does not satisfy the spec's consistency rules.
    #[error(transparent)]
    Tile(#[from] TileError),
    /// An SQL identifier that cannot be safely quoted.
    #[error("invalid SQL identifier: {0}")]
    InvalidIdentifier(String),
}
