//! `gpkg info`: what a file declares and what state it is in.
//!
//! The command opens the file with [`GeoPackage::open_read_only_lenient`], so
//! it reports on a file with problems instead of rejecting it. An
//! inspection tool that refused to open the files most worth inspecting would
//! not be much use. The warnings collected while opening the file are printed
//! first, before anything they might explain.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use geopackage::ContentsDataType;
use geopackage::GeoPackage;

use crate::error::Result;

pub fn run(path: &Path, out: &mut impl Write) -> Result<ExitCode> {
    let gpkg = GeoPackage::open_read_only_lenient(path)?;

    writeln!(out, "{}", path.display())?;
    writeln!(out, "  version: {}", gpkg.version())?;
    for warning in gpkg.open_warnings() {
        writeln!(out, "  warning: {warning}")?;
    }

    print_layers(&gpkg, out)?;
    print_tile_pyramids(&gpkg, out)?;
    print_coverages(&gpkg, out)?;
    print_extensions(&gpkg, out)?;

    Ok(ExitCode::SUCCESS)
}

fn print_layers(gpkg: &GeoPackage, out: &mut impl Write) -> Result<()> {
    let layers = gpkg.layers()?;
    if layers.is_empty() {
        writeln!(out, "\nno feature layers")?;
        return Ok(());
    }

    for layer in &layers {
        writeln!(out, "\nlayer {:?} ({})", layer.table_name(), layer.kind())?;
        writeln!(out, "  rows:     {}", layer.count()?)?;

        if let Some(geom) = layer.geometry_column() {
            writeln!(
                out,
                "  geometry: {} ({}, srs_id {})",
                geom.column_name, geom.geometry_type, geom.srs_id
            )?;
            if let Some(srs) = gpkg.srs(geom.srs_id)? {
                writeln!(
                    out,
                    "  srs:      {} ({}:{})",
                    srs.name, srs.organization, srs.organization_coordsys_id
                )?;
            }

            // The status distinguishes a healthy index from a legacy trigger
            // set or one an interrupted build left desynchronised, and the last
            // two have a repair to name.
            let status = layer.spatial_index_status()?;
            write!(out, "  index:    {status}")?;
            match layer.spatial_index_status()? {
                geopackage::SpatialIndexStatus::Legacy | geopackage::SpatialIndexStatus::Stale => {
                    writeln!(out, "  (repair with `gpkg repair`)")?;
                }
                _ => writeln!(out)?,
            }
        }

        writeln!(out, "  columns:")?;
        for column in &layer.schema().columns {
            let pk = if column.is_primary_key() {
                " PRIMARY KEY"
            } else {
                ""
            };
            let not_null = if column.not_null { " NOT NULL" } else { "" };
            let declared = if column.declared_type.is_empty() {
                "(untyped)"
            } else {
                &column.declared_type
            };
            writeln!(out, "    {:<20} {declared}{pk}{not_null}", column.name)?;
        }
    }
    Ok(())
}

fn print_tile_pyramids(gpkg: &GeoPackage, out: &mut impl Write) -> Result<()> {
    // Listed from `gpkg_contents` rather than from the pyramid handles, so a
    // catalogue row whose table is missing still shows up here instead of
    // vanishing. `validate` is what says it is wrong.
    let declared: Vec<String> = gpkg
        .contents()?
        .into_iter()
        .filter(|entry| entry.data_type == ContentsDataType::Tiles)
        .map(|entry| entry.table_name)
        .collect();
    if declared.is_empty() {
        return Ok(());
    }

    for pyramid in gpkg.tile_pyramids()? {
        let zooms = pyramid.zoom_levels();
        writeln!(out, "\ntiles {:?}", pyramid.table_name())?;
        match (zooms.first(), zooms.last()) {
            (Some(first), Some(last)) => writeln!(out, "  zooms:    {first} to {last}")?,
            _ => writeln!(out, "  zooms:    none")?,
        }
        if let Some(srs) = gpkg.srs(pyramid.matrix_set().srs_id)? {
            writeln!(
                out,
                "  srs:      {} ({}:{})",
                srs.name, srs.organization, srs.organization_coordsys_id
            )?;
        }
        for zoom in &zooms {
            if let Some(matrix) = pyramid.matrix(*zoom) {
                writeln!(
                    out,
                    "    zoom {zoom:<3} {}x{} tiles of {}x{} px",
                    matrix.matrix_width,
                    matrix.matrix_height,
                    matrix.tile_width,
                    matrix.tile_height
                )?;
            }
        }
    }
    Ok(())
}

/// Prints the coverages, separately from the pyramids, because a coverage is a
/// different kind of content. `gpkg coverage info` gives the details.
fn print_coverages(gpkg: &GeoPackage, out: &mut impl Write) -> Result<()> {
    let declared: Vec<String> = gpkg
        .contents()?
        .into_iter()
        .filter(|entry| entry.data_type == ContentsDataType::Coverage)
        .map(|entry| entry.table_name)
        .collect();
    if declared.is_empty() {
        return Ok(());
    }

    for name in declared {
        // Uses the catalogue names, not `coverages()`, so that the list
        // includes a coverage that this crate cannot open. `validate` gives
        // the reason.
        match gpkg.coverage(&name) {
            Ok(coverage) => {
                let ancillary = coverage.ancillary();
                writeln!(out, "\ncoverage {name:?}")?;
                writeln!(
                    out,
                    "  samples:  {}{}",
                    ancillary.datatype,
                    match ancillary.uom.as_deref() {
                        Some(uom) => format!(" in {uom}"),
                        None => String::new(),
                    }
                )?;
                let zooms = coverage.zoom_levels();
                match (zooms.first(), zooms.last()) {
                    (Some(first), Some(last)) => writeln!(out, "  zooms:    {first} to {last}")?,
                    _ => writeln!(out, "  zooms:    none")?,
                }
                if let Some(srs) = gpkg.srs(coverage.matrix_set().srs_id)? {
                    writeln!(
                        out,
                        "  srs:      {} ({}:{})",
                        srs.name, srs.organization, srs.organization_coordsys_id
                    )?;
                }
            }
            Err(error) => {
                writeln!(out, "\ncoverage {name:?}")?;
                writeln!(out, "  unreadable: {error}")?;
            }
        }
    }
    Ok(())
}

fn print_extensions(gpkg: &GeoPackage, out: &mut impl Write) -> Result<()> {
    let extensions = gpkg.extensions()?;
    if extensions.is_empty() {
        return Ok(());
    }

    writeln!(out, "\nextensions:")?;
    for row in &extensions {
        let target = match (&row.table_name, &row.column_name) {
            (Some(table), Some(column)) => format!("{table}.{column}"),
            (Some(table), None) => table.clone(),
            // Requirement 61 forbids a column without a table, so the pair is
            // reported as written rather than guessed at.
            (None, _) => "(whole file)".to_owned(),
        };
        writeln!(
            out,
            "  {:<34} {:<22} {} [{}]",
            row.name,
            target,
            row.support(),
            row.scope
        )?;
    }
    Ok(())
}
