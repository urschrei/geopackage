//! The CLI's error type.
//!
//! `geopackage::Error` covers what the library does; a tool also writes files
//! and parses arguments, which it has no variants for and should not grow them
//! for. This wraps it rather than widening it.

/// Anything a subcommand can fail with.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The library rejected or could not do something.
    #[error("{0}")]
    Gpkg(#[from] geopackage::Error),
    /// Reading or writing a file outside the GeoPackage failed, such as the
    /// `--out` target of `tiles get`.
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
