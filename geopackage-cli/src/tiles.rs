//! `gpkg tiles info` and `gpkg tiles get`.
//!
//! **These commands decode no images.** `get` writes out the bytes exactly as
//! the file stores them, so `--out tile.png` produces whatever the pyramid
//! stored under that address, whether or not it is a PNG. This crate reads a
//! payload's header to name its format and pixel size, and does nothing else
//! with it. The library takes the same approach.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use geopackage::core::coverage::coverage_tiff;
use geopackage::core::tiles::{TileCoord, probe};
use geopackage::{GeoPackage, TilePyramid};

use crate::error::Result;

/// `gpkg tiles info`: every pyramid, or one named one.
pub fn info(path: &Path, pyramid_name: Option<&str>, out: &mut impl Write) -> Result<ExitCode> {
    let gpkg = GeoPackage::open_read_only_lenient(path)?;

    let pyramids = match pyramid_name {
        Some(name) => vec![gpkg.tiles(name)?],
        None => gpkg.tile_pyramids()?,
    };
    if pyramids.is_empty() {
        writeln!(out, "{}: no tile pyramids", path.display())?;
        return Ok(ExitCode::SUCCESS);
    }

    for pyramid in &pyramids {
        print_pyramid(&gpkg, pyramid, out)?;
    }
    Ok(ExitCode::SUCCESS)
}

fn print_pyramid(gpkg: &GeoPackage, pyramid: &TilePyramid<'_>, out: &mut impl Write) -> Result<()> {
    let set = pyramid.matrix_set();
    writeln!(out, "tiles {:?}", pyramid.table_name())?;
    writeln!(out, "  tiles:    {}", pyramid.tile_count()?)?;
    if let Some(srs) = gpkg.srs(set.srs_id)? {
        writeln!(
            out,
            "  srs:      {} ({}:{})",
            srs.name, srs.organization, srs.organization_coordsys_id
        )?;
    }
    writeln!(
        out,
        "  extent:   {} {} {} {}",
        set.min_x, set.min_y, set.max_x, set.max_y
    )?;

    // A registered extension that blocks writes is worth stating here, since
    // it is why a `put` would be refused.
    if let Some(blocking) = pyramid.blocking_extension() {
        writeln!(
            out,
            "  blocked:  writes refused by extension {:?}",
            blocking.name
        )?;
    }

    writeln!(out, "  zoom levels:")?;
    for matrix in pyramid.matrices() {
        writeln!(
            out,
            "    {:<3} {:>6} x {:<6} grid, {} x {} px, {} stored",
            matrix.zoom_level,
            matrix.matrix_width,
            matrix.matrix_height,
            matrix.tile_width,
            matrix.tile_height,
            pyramid.tile_count_at(matrix.zoom_level)?
        )?;
    }
    Ok(())
}

/// `gpkg tiles get`: one tile's stored bytes, to a file or to stdout.
pub fn get(
    path: &Path,
    pyramid_name: &str,
    coord: TileCoord,
    out_path: Option<&Path>,
    out: &mut impl Write,
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

    match out_path {
        Some(out_path) => {
            std::fs::write(out_path, &bytes)?;
            // Say what was written rather than only that something was, since
            // the extension on `--out` is the caller's guess and the payload
            // is whatever the file stored.
            let described = match probe(&bytes) {
                Ok(payload) => {
                    // A TIFF in a tile table is part of the coverage
                    // extension, and its header gives more than its size. A
                    // reader needs the sample type and the compression before
                    // it can use the bytes.
                    let profile = match coverage_tiff(&bytes) {
                        Ok(coverage) => {
                            format!(", {:?}, {:?}", coverage.sample_type, coverage.compression)
                        }
                        Err(_) => String::new(),
                    };
                    format!(
                        "{:?} {}x{}{profile}",
                        payload.format, payload.width, payload.height
                    )
                }
                Err(_) => "unrecognised payload".to_owned(),
            };
            writeln!(
                out,
                "wrote {} bytes to {} ({described})",
                bytes.len(),
                out_path.display()
            )?;
        }
        None => {
            out.write_all(&bytes)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}
