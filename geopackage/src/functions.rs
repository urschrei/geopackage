//! SQL functions required by the RTree spatial index triggers.
//!
//! The spec's rtree triggers call `ST_IsEmpty`, `ST_MinX`, `ST_MaxX`,
//! `ST_MinY`, `ST_MaxY`, which SQLite does not provide. They are registered
//! on every connection this crate opens, so writes to indexed tables work on
//! any GeoPackage, including files created by other tools.
//!
//! Envelope values are taken from the GPB header when present. When absent,
//! a fallback computes bounds from the WKB body — currently for point
//! geometries only (the common no-envelope case in the wild, e.g. GDAL
//! writes points without envelopes). Full WKB traversal lands in M1 via the
//! georust `wkb` crate.

use geopackage_core::gpb::{self, GpbError};
use rusqlite::Connection;
use rusqlite::functions::{Context, FunctionFlags};
use rusqlite::types::ValueRef;

/// Register the GeoPackage SQL functions on `conn`.
pub fn register(conn: &Connection) -> rusqlite::Result<()> {
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;

    conn.create_scalar_function("ST_IsEmpty", 1, flags, |ctx| {
        with_blob(ctx, |blob| {
            let (h, body) = parse(blob)?;
            if h.empty {
                return Ok(Some(true));
            }
            // Cheap check for the empty-point-as-NaN convention.
            match point_xy(&blob[body..]) {
                Ok(Some((x, y))) => Ok(Some(x.is_nan() || y.is_nan())),
                _ => Ok(Some(false)),
            }
        })
    })?;

    for (name, idx) in [
        ("ST_MinX", 0usize),
        ("ST_MaxX", 1),
        ("ST_MinY", 2),
        ("ST_MaxY", 3),
    ] {
        conn.create_scalar_function(name, 1, flags, move |ctx| {
            with_blob(ctx, |blob| {
                let (h, body) = parse(blob)?;
                if let Some(bounds) = h.envelope.xy_bounds() {
                    let v = [bounds.0, bounds.1, bounds.2, bounds.3][idx];
                    return Ok(Some(v));
                }
                match point_xy(&blob[body..])? {
                    Some((x, y)) => Ok(Some(if idx < 2 { x } else { y })),
                    None => Err(FnError::NoEnvelope),
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
    #[error("GPB has no envelope and non-point WKB envelope computation is not yet implemented")]
    NoEnvelope,
    #[error("truncated WKB body")]
    BadWkb,
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

fn parse(blob: &[u8]) -> Result<(gpb::GpbHeader, usize), FnError> {
    Ok(gpb::parse_header(blob)?)
}

/// If the WKB body is a Point (any of XY/Z/M/ZM, ISO codes), return its
/// coordinates; `Ok(None)` if it is some other geometry type.
fn point_xy(wkb: &[u8]) -> Result<Option<(f64, f64)>, FnError> {
    if wkb.len() < 5 {
        return Err(FnError::BadWkb);
    }
    let le = match wkb[0] {
        0 => false,
        1 => true,
        _ => return Err(FnError::BadWkb),
    };
    let ty = {
        let b: [u8; 4] = wkb[1..5].try_into().expect("len checked");
        if le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        }
    };
    if ty % 1000 != 1 || ty / 1000 > 3 {
        return Ok(None); // not a point
    }
    if wkb.len() < 5 + 16 {
        return Err(FnError::BadWkb);
    }
    let read = |off: usize| {
        let b: [u8; 8] = wkb[off..off + 8].try_into().expect("len checked");
        if le {
            f64::from_le_bytes(b)
        } else {
            f64::from_be_bytes(b)
        }
    };
    Ok(Some((read(5), read(13))))
}
