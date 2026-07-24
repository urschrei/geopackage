//! SQL functions required by the RTree spatial index triggers.
//!
//! The spec's rtree triggers call `ST_IsEmpty`, `ST_MinX`, `ST_MaxX`,
//! `ST_MinY`, `ST_MaxY`, which SQLite does not provide. They are registered
//! on every connection this crate opens, so writes to indexed tables work on
//! any GeoPackage, including files created by other tools.
//!
//! Envelope values are taken from the GPB header when present (an O(1) read;
//! the rtree triggers call four of these per row). When the header carries no
//! envelope (as for GDAL-written points and other envelope-less blobs) the
//! functions fall back to a full traversal of the WKB body via
//! [`geopackage_core::geometry::GpbGeometry`], which handles every geometry
//! type the georust `wkb` crate can read (all byte orders, any Z/M variant).
//! `ST_IsEmpty` reports emptiness from the same wrapper (header empty flag,
//! the NaN empty-point convention, and zero-coordinate geometries).
//!
//! Limitation: a WKB body whose type the `wkb` crate cannot read, namely the
//! non-linear curve types (`CIRCULARSTRING`, `CURVEPOLYGON`, …) and the
//! abstract `CURVE`/`SURFACE`, produces a typed SQL error rather than an
//! envelope, so such a geometry cannot be inserted into an rtree-indexed
//! table. Curve-type envelope support is tracked as an open roadmap item and
//! needs curve support in `wkb` upstream.

use geopackage_core::geometry::{GeometryError, GpbGeometry};
use geopackage_core::gpb::{self, GpbError};
use rusqlite::Connection;
use rusqlite::functions::{Context, FunctionFlags};
use rusqlite::types::ValueRef;

/// Register the GeoPackage SQL functions on `conn`.
pub fn register(conn: &Connection) -> rusqlite::Result<()> {
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;

    conn.create_scalar_function("ST_IsEmpty", 1, flags, |ctx| {
        with_blob(ctx, |blob| {
            // The header empty flag is authoritative and cheap; only fall back
            // to a body traversal when it is not set.
            let (header, _) = gpb::parse_header(blob)?;
            if header.empty {
                return Ok(Some(true));
            }
            Ok(Some(GpbGeometry::parse(blob)?.is_empty()))
        })
    })?;

    // Each function selects one component of the `[min_x, max_x, min_y, max_y]`
    // bounds array by destructuring it, so there is no fallible indexing.
    type Select = fn([f64; 4]) -> f64;
    for (name, select) in [
        ("ST_MinX", (|[min_x, _, _, _]| min_x) as Select),
        ("ST_MaxX", |[_, max_x, _, _]| max_x),
        ("ST_MinY", |[_, _, min_y, _]| min_y),
        ("ST_MaxY", |[_, _, _, max_y]| max_y),
    ] {
        conn.create_scalar_function(name, 1, flags, move |ctx| {
            with_blob(ctx, |blob| {
                let (header, _) = gpb::parse_header(blob)?;
                if let Some((min_x, max_x, min_y, max_y)) = header.envelope.xy_bounds() {
                    return Ok(Some(select([min_x, max_x, min_y, max_y])));
                }
                // No header envelope: traverse the body. An empty geometry has
                // no spatial extent; report NaN (the rtree triggers guard these
                // calls with ST_IsEmpty, so a bound is never indexed for one).
                match GpbGeometry::parse(blob)?.xy_envelope() {
                    Some(bounds) => Ok(Some(select(bounds))),
                    None => Ok(Some(f64::NAN)),
                }
            })
        })?;
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum FnError {
    #[error("argument is not a BLOB")]
    NotABlob,
    #[error(transparent)]
    Gpb(#[from] GpbError),
    #[error(transparent)]
    Geometry(#[from] GeometryError),
}

fn with_blob<T: rusqlite::types::ToSql>(
    ctx: &Context<'_>,
    f: impl FnOnce(&[u8]) -> Result<Option<T>, FnError>,
) -> rusqlite::Result<Option<T>> {
    match ctx.get_raw(0) {
        ValueRef::Null => Ok(None),
        ValueRef::Blob(b) => f(b).map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e))),
        _ => Err(rusqlite::Error::UserFunctionError(Box::new(
            FnError::NotABlob,
        ))),
    }
}
