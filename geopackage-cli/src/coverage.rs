//! `gpkg coverage`: the contents of the tiled gridded coverages in a file.
//!
//! A coverage is a tile pyramid with payloads that are measurements. This crate
//! reads the payloads but does not decode them. The command prints the grid,
//! the ancillary metadata that states what the samples mean, and the values
//! that the header of each payload declares.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use geopackage::core::tiles::TileCoord;
use geopackage::{Coverage, GeoPackage};

use crate::error::Result;

/// `gpkg coverage info`: every coverage in a file, or one named coverage.
pub fn info(path: &Path, coverage_name: Option<&str>, out: &mut impl Write) -> Result<ExitCode> {
    let gpkg = GeoPackage::open_read_only_lenient(path)?;

    let coverages = match coverage_name {
        Some(name) => vec![gpkg.coverage(name)?],
        None => gpkg.coverages()?,
    };
    if coverages.is_empty() {
        writeln!(out, "{}: no tiled gridded coverages", path.display())?;
        return Ok(ExitCode::SUCCESS);
    }

    for coverage in &coverages {
        print_coverage(&gpkg, coverage, out)?;
    }
    Ok(ExitCode::SUCCESS)
}

fn print_coverage(gpkg: &GeoPackage, coverage: &Coverage<'_>, out: &mut impl Write) -> Result<()> {
    let set = coverage.matrix_set();
    let ancillary = coverage.ancillary();
    writeln!(out, "coverage {:?}", coverage.table_name())?;
    writeln!(out, "  tiles:    {}", coverage.tile_count()?)?;
    writeln!(
        out,
        "  samples:  {}{}",
        ancillary.datatype,
        match ancillary.uom.as_deref() {
            Some(uom) => format!(" in {uom}"),
            None => String::new(),
        }
    )?;
    if let Some(field_name) = &ancillary.field_name {
        writeln!(out, "  field:    {field_name}")?;
    }
    // The three numbers that a reader needs to convert a sample to a value,
    // and the number that marks a sample as no value.
    writeln!(
        out,
        "  scale:    {} offset: {}",
        ancillary.scale, ancillary.offset
    )?;
    match ancillary.data_null {
        Some(null) => writeln!(out, "  null:     {null}")?,
        None => writeln!(out, "  null:     none declared")?,
    }
    if let Some(encoding) = &ancillary.grid_cell_encoding {
        writeln!(out, "  cells:    {encoding}")?;
    }
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

    writeln!(out, "  zoom levels:")?;
    for matrix in coverage.matrices() {
        writeln!(
            out,
            "    {:<3} {:>6} x {:<6} grid, {} x {} px, {} stored",
            matrix.zoom_level,
            matrix.matrix_width,
            matrix.matrix_height,
            matrix.tile_width,
            matrix.tile_height,
            coverage.tile_count_at(matrix.zoom_level)?
        )?;
    }

    // A description of one payload. The encoding is a property of the file,
    // not of the extension, and a reader needs to know the encoding before it
    // reads tiles.
    if let Some(matrix) = coverage.matrices().first() {
        let mut cursor = coverage.cursor_at(matrix.zoom_level)?;
        let mut stream = cursor.tiles()?;
        if let Some(tile) = stream.next()? {
            let described = match coverage.check_payload(tile.data()) {
                Ok(payload) => format!(
                    "{} {:?} {}x{}",
                    payload.mime_type(),
                    payload.sample_type(),
                    payload.width(),
                    payload.height()
                ),
                Err(error) => format!("unusable: {error}"),
            };
            writeln!(out, "  payloads: {described} (first tile)")?;
        }
    }
    Ok(())
}

/// `gpkg coverage get`: one tile's stored bytes, to a file or to stdout.
pub fn get(
    path: &Path,
    coverage_name: &str,
    coord: TileCoord,
    out_path: Option<&Path>,
    out: &mut impl Write,
) -> Result<ExitCode> {
    let gpkg = GeoPackage::open_read_only_lenient(path)?;
    let coverage = gpkg.coverage(coverage_name)?;

    let Some(bytes) = coverage.get_tile(coord)? else {
        eprintln!(
            "gpkg: no tile at {}/{}/{} in {coverage_name}",
            coord.zoom_level, coord.column, coord.row
        );
        return Ok(ExitCode::FAILURE);
    };

    match out_path {
        Some(out_path) => {
            std::fs::write(out_path, &bytes)?;
            let described = match coverage.check_payload(&bytes) {
                Ok(payload) => format!(
                    "{} {:?} {}x{}",
                    payload.mime_type(),
                    payload.sample_type(),
                    payload.width(),
                    payload.height()
                ),
                Err(error) => format!("unusable payload: {error}"),
            };
            writeln!(
                out,
                "wrote {} bytes to {} ({described})",
                bytes.len(),
                out_path.display()
            )?;
            // The statistics are the only numbers about the contents of a tile
            // that this crate can report, because it does not decode samples.
            if let Some(stats) = coverage.tile_ancillary(coord)?
                && let (Some(min), Some(max)) = (stats.min, stats.max)
            {
                writeln!(
                    out,
                    "  recorded range {} to {} (values {} to {})",
                    min,
                    max,
                    coverage.value(&stats, min),
                    coverage.value(&stats, max)
                )?;
            }
        }
        None => {
            out.write_all(&bytes)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}
