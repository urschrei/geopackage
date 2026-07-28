//! `gpkg tiles info` and `gpkg tiles get`, deferred from M4 for want of this
//! crate.
//!
//! **No image is decoded anywhere here.** `get` writes the stored bytes out
//! exactly as the file holds them, so `--out tile.png` produces whatever the
//! pyramid stored under that address, whether or not it is a PNG. This crate
//! reads a payload's header to name its format and pixel size and does nothing
//! else with it, which is the same position the library takes.

use std::path::Path;
use std::process::ExitCode;

use geopackage::core::tiles::{TileCoord, probe};
use geopackage::{GeoPackage, TilePyramid};

use crate::error::Result;

/// `gpkg tiles info`: every pyramid, or one named one.
pub fn info(path: &Path, pyramid_name: Option<&str>) -> Result<ExitCode> {
    let gpkg = GeoPackage::open_read_only_lenient(path)?;

    let pyramids = match pyramid_name {
        Some(name) => vec![gpkg.tiles(name)?],
        None => gpkg.tile_pyramids()?,
    };
    if pyramids.is_empty() {
        println!("{}: no tile pyramids", path.display());
        return Ok(ExitCode::SUCCESS);
    }

    for pyramid in &pyramids {
        print_pyramid(&gpkg, pyramid)?;
    }
    Ok(ExitCode::SUCCESS)
}

fn print_pyramid(gpkg: &GeoPackage, pyramid: &TilePyramid<'_>) -> Result<()> {
    let set = pyramid.matrix_set();
    println!("tiles {:?}", pyramid.table_name());
    println!("  tiles:    {}", pyramid.tile_count()?);
    if let Some(srs) = gpkg.srs(set.srs_id)? {
        println!(
            "  srs:      {} ({}:{})",
            srs.name, srs.organization, srs.organization_coordsys_id
        );
    }
    println!(
        "  extent:   {} {} {} {}",
        set.min_x, set.min_y, set.max_x, set.max_y
    );

    // A registered extension that blocks writes is worth stating here, since
    // it is why a `put` would be refused.
    if let Some(blocking) = pyramid.blocking_extension() {
        println!(
            "  blocked:  writes refused by extension {:?}",
            blocking.name
        );
    }

    println!("  zoom levels:");
    for matrix in pyramid.matrices() {
        println!(
            "    {:<3} {:>6} x {:<6} grid, {} x {} px, {} stored",
            matrix.zoom_level,
            matrix.matrix_width,
            matrix.matrix_height,
            matrix.tile_width,
            matrix.tile_height,
            pyramid.tile_count_at(matrix.zoom_level)?
        );
    }
    Ok(())
}

/// `gpkg tiles get`: one tile's stored bytes, to a file or to stdout.
pub fn get(
    path: &Path,
    pyramid_name: &str,
    coord: TileCoord,
    out: Option<&Path>,
) -> Result<ExitCode> {
    let gpkg = GeoPackage::open_read_only_lenient(path)?;
    let pyramid = gpkg.tiles(pyramid_name)?;

    let Some(bytes) = pyramid.get_tile(coord)? else {
        eprintln!(
            "gpkg: no tile at {}/{}/{} in {pyramid_name}",
            coord.zoom_level, coord.column, coord.row
        );
        return Ok(ExitCode::FAILURE);
    };

    match out {
        Some(out) => {
            std::fs::write(out, &bytes)?;
            // Say what was written rather than only that something was, since
            // the extension on `--out` is the caller's guess and the payload
            // is whatever the file stored.
            let described = match probe(&bytes) {
                Ok(payload) => format!("{:?} {}x{}", payload.format, payload.width, payload.height),
                Err(_) => "unrecognised payload".to_owned(),
            };
            println!(
                "wrote {} bytes to {} ({described})",
                bytes.len(),
                out.display()
            );
        }
        None => {
            use std::io::Write;
            std::io::stdout().write_all(&bytes)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}
