//! `gpkg coverage`: the contents of the tiled gridded coverages in a file.
//!
//! A coverage is a tile pyramid with payloads that are measurements. This crate
//! reads the payloads but does not decode them. The command prints the grid,
//! the ancillary metadata that states what the samples mean, and the values
//! that the header of each payload declares.

use std::path::Path;
use std::process::ExitCode;

use geopackage::core::tiles::TileCoord;
use geopackage::{Coverage, GeoPackage};

use crate::error::Result;

/// `gpkg coverage info`: every coverage in a file, or one named coverage.
pub fn info(path: &Path, coverage_name: Option<&str>) -> Result<ExitCode> {
    let gpkg = GeoPackage::open_read_only_lenient(path)?;

    let coverages = match coverage_name {
        Some(name) => vec![gpkg.coverage(name)?],
        None => gpkg.coverages()?,
    };
    if coverages.is_empty() {
        println!("{}: no tiled gridded coverages", path.display());
        return Ok(ExitCode::SUCCESS);
    }

    for coverage in &coverages {
        print_coverage(&gpkg, coverage)?;
    }
    Ok(ExitCode::SUCCESS)
}

fn print_coverage(gpkg: &GeoPackage, coverage: &Coverage<'_>) -> Result<()> {
    let set = coverage.matrix_set();
    let ancillary = coverage.ancillary();
    println!("coverage {:?}", coverage.table_name());
    println!("  tiles:    {}", coverage.tile_count()?);
    println!(
        "  samples:  {}{}",
        ancillary.datatype,
        match ancillary.uom.as_deref() {
            Some(uom) => format!(" in {uom}"),
            None => String::new(),
        }
    );
    if let Some(field_name) = &ancillary.field_name {
        println!("  field:    {field_name}");
    }
    // The three numbers that a reader needs to convert a sample to a value,
    // and the number that marks a sample as no value.
    println!(
        "  scale:    {} offset: {}",
        ancillary.scale, ancillary.offset
    );
    match ancillary.data_null {
        Some(null) => println!("  null:     {null}"),
        None => println!("  null:     none declared"),
    }
    if let Some(encoding) = &ancillary.grid_cell_encoding {
        println!("  cells:    {encoding}");
    }
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

    println!("  zoom levels:");
    for matrix in coverage.matrices() {
        println!(
            "    {:<3} {:>6} x {:<6} grid, {} x {} px, {} stored",
            matrix.zoom_level,
            matrix.matrix_width,
            matrix.matrix_height,
            matrix.tile_width,
            matrix.tile_height,
            coverage.tile_count_at(matrix.zoom_level)?
        );
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
            println!("  payloads: {described} (first tile)");
        }
    }
    Ok(())
}

/// `gpkg coverage get`: one tile's stored bytes, to a file or to stdout.
pub fn get(
    path: &Path,
    coverage_name: &str,
    coord: TileCoord,
    out: Option<&Path>,
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

    match out {
        Some(out) => {
            std::fs::write(out, &bytes)?;
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
            println!(
                "wrote {} bytes to {} ({described})",
                bytes.len(),
                out.display()
            );
            // The statistics are the only numbers about the contents of a tile
            // that this crate can report, because it does not decode samples.
            if let Some(stats) = coverage.tile_ancillary(coord)?
                && let (Some(min), Some(max)) = (stats.min, stats.max)
            {
                println!(
                    "  recorded range {} to {} (values {} to {})",
                    min,
                    max,
                    coverage.value(&stats, min),
                    coverage.value(&stats, max)
                );
            }
        }
        None => {
            use std::io::Write;
            std::io::stdout().write_all(&bytes)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}
