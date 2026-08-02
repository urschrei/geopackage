//! `gpkg info`: what a file declares and what state it is in.
//!
//! Opened with [`GeoPackage::open_read_only_lenient`], so a file with something wrong
//! with it is reported rather than rejected: an inspection tool that will not
//! open the files worth inspecting is not much use. The warnings that leniency
//! collected are printed first, before anything they might explain.

use std::path::Path;
use std::process::ExitCode;

use geopackage::ContentsDataType;
use geopackage::GeoPackage;

use crate::error::Result;

pub fn run(path: &Path) -> Result<ExitCode> {
    let gpkg = GeoPackage::open_read_only_lenient(path)?;

    println!("{}", path.display());
    println!("  version: {}", gpkg.version());
    for warning in gpkg.open_warnings() {
        println!("  warning: {warning}");
    }

    print_layers(&gpkg)?;
    print_tile_pyramids(&gpkg)?;
    print_extensions(&gpkg)?;

    Ok(ExitCode::SUCCESS)
}

fn print_layers(gpkg: &GeoPackage) -> Result<()> {
    let layers = gpkg.layers()?;
    if layers.is_empty() {
        println!("\nno feature layers");
        return Ok(());
    }

    for layer in &layers {
        println!("\nlayer {:?} ({})", layer.table_name(), layer.kind());
        println!("  rows:     {}", layer.count()?);

        if let Some(geom) = layer.geometry_column() {
            println!(
                "  geometry: {} ({}, srs_id {})",
                geom.column_name, geom.geometry_type, geom.srs_id
            );
            if let Some(srs) = gpkg.srs(geom.srs_id)? {
                println!(
                    "  srs:      {} ({}:{})",
                    srs.name, srs.organization, srs.organization_coordsys_id
                );
            }

            // The status distinguishes a healthy index from a legacy trigger
            // set or one an interrupted build left desynchronised, and the last
            // two have a repair to name.
            let status = layer.spatial_index_status()?;
            print!("  index:    {status}");
            match layer.spatial_index_status()? {
                geopackage::SpatialIndexStatus::Legacy | geopackage::SpatialIndexStatus::Stale => {
                    println!("  (repair with `gpkg repair`)");
                }
                _ => println!(),
            }
        }

        println!("  columns:");
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
            println!("    {:<20} {declared}{pk}{not_null}", column.name);
        }
    }
    Ok(())
}

fn print_tile_pyramids(gpkg: &GeoPackage) -> Result<()> {
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
        println!("\ntiles {:?}", pyramid.table_name());
        match (zooms.first(), zooms.last()) {
            (Some(first), Some(last)) => println!("  zooms:    {first} to {last}"),
            _ => println!("  zooms:    none"),
        }
        if let Some(srs) = gpkg.srs(pyramid.matrix_set().srs_id)? {
            println!(
                "  srs:      {} ({}:{})",
                srs.name, srs.organization, srs.organization_coordsys_id
            );
        }
        for zoom in &zooms {
            if let Some(matrix) = pyramid.matrix(*zoom) {
                println!(
                    "    zoom {zoom:<3} {}x{} tiles of {}x{} px",
                    matrix.matrix_width,
                    matrix.matrix_height,
                    matrix.tile_width,
                    matrix.tile_height
                );
            }
        }
    }
    Ok(())
}

fn print_extensions(gpkg: &GeoPackage) -> Result<()> {
    let extensions = gpkg.extensions()?;
    if extensions.is_empty() {
        return Ok(());
    }

    println!("\nextensions:");
    for row in &extensions {
        let target = match (&row.table_name, &row.column_name) {
            (Some(table), Some(column)) => format!("{table}.{column}"),
            (Some(table), None) => table.clone(),
            // Requirement 61 forbids a column without a table, so the pair is
            // reported as written rather than guessed at.
            (None, _) => "(whole file)".to_owned(),
        };
        println!(
            "  {:<34} {:<22} {} [{}]",
            row.name,
            target,
            row.support(),
            row.scope
        );
    }
    Ok(())
}
