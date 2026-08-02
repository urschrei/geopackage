//! The book's Rust code blocks, compiled as doctests.
//!
//! Each item includes one chapter of `docs/book` as its documentation, so
//! `cargo test --doc` compiles every Rust block in it with this crate's real
//! dependencies. The blocks are marked `no_run` in the book (they reference
//! files a reader has and CI does not), so they are compiled, never executed.
//! This module exists only under `cfg(doctest)`: it is absent from ordinary
//! builds and from the packaged crate.

#[doc = include_str!("../../docs/book/src/tutorial/first-geopackage.md")]
pub struct TutorialFirstGeopackage;

#[doc = include_str!("../../docs/book/src/how-to/add-spatial-index.md")]
pub struct HowToAddSpatialIndex;

#[doc = include_str!("../../docs/book/src/how-to/repair-spatial-indexes.md")]
pub struct HowToRepairSpatialIndexes;

#[doc = include_str!("../../docs/book/src/how-to/copy-features.md")]
pub struct HowToCopyFeatures;

#[doc = include_str!("../../docs/book/src/how-to/validate.md")]
pub struct HowToValidate;

#[cfg(feature = "arrow")]
#[doc = include_str!("../../docs/book/src/how-to/read-arrow.md")]
pub struct HowToReadArrow;
