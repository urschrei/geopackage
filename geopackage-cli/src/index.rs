//! `gpkg index` and `gpkg repair`: the two commands that write.
//!
//! Both open read-write and lenient. Lenient because the files worth repairing
//! are by definition ones something is wrong with, and rejecting a legacy
//! `application_id` would put the repair out of reach of the files that need
//! it most.
//!
//! What each does is chosen from the layer's [`SpatialIndexStatus`] rather than
//! attempted blindly, so the tool says what it is about to do and skips a layer
//! that does not need it.

use std::path::Path;
use std::process::ExitCode;

use geopackage::{GeoPackage, Layer, SpatialIndexStatus};

use crate::error::Result;

/// `gpkg index`: build an index on a layer that has none.
pub fn build(path: &Path, layer_name: &str) -> Result<ExitCode> {
    let gpkg = GeoPackage::open_lenient(path)?;
    let layer = gpkg.layer(layer_name)?;

    match layer.spatial_index_status()? {
        SpatialIndexStatus::Current => {
            println!("{layer_name}: already indexed, nothing to do");
            Ok(ExitCode::SUCCESS)
        }
        // Present but wrong. Building over it is not the right verb, and
        // silently repairing instead would be doing something other than what
        // was asked.
        status @ (SpatialIndexStatus::Legacy | SpatialIndexStatus::Stale) => {
            eprintln!("gpkg: {layer_name} already has an index ({status}); run `gpkg repair`");
            Ok(ExitCode::FAILURE)
        }
        SpatialIndexStatus::Absent => {
            layer.create_spatial_index()?;
            println!("{layer_name}: index built over {} rows", layer.count()?);
            Ok(ExitCode::SUCCESS)
        }
        // `SpatialIndexStatus` is `#[non_exhaustive]`: a status added later
        // should stop here rather than be guessed at.
        other => {
            eprintln!("gpkg: {layer_name} has an index state this version does not know ({other})");
            Ok(ExitCode::FAILURE)
        }
    }
}

/// `gpkg repair`: put right the index of every layer that needs it, or of one
/// named layer.
pub fn repair(path: &Path, layer_name: Option<&str>) -> Result<ExitCode> {
    let gpkg = GeoPackage::open_lenient(path)?;

    let layers = match layer_name {
        Some(name) => vec![gpkg.layer(name)?],
        None => gpkg.layers()?,
    };

    let mut repaired = 0usize;
    for layer in &layers {
        if repair_one(layer)? {
            repaired += 1;
        }
    }

    if repaired == 0 {
        println!("nothing to repair");
    }
    Ok(ExitCode::SUCCESS)
}

/// Repair one layer, reporting whether anything was done.
fn repair_one(layer: &Layer<'_>) -> Result<bool> {
    // An attribute layer has no geometry and so no index to repair.
    if layer.geometry_column().is_none() {
        return Ok(false);
    }

    let name = layer.table_name().to_owned();
    match layer.spatial_index_status()? {
        SpatialIndexStatus::Legacy | SpatialIndexStatus::Stale => {
            let status = layer.spatial_index_status()?;
            layer.repair_spatial_index()?;
            println!("{name}: {status} index repaired to the 1.4 trigger set");
            Ok(true)
        }
        // Left alone deliberately. An absent index is a choice, not a defect
        // (`validate` calls it an advisory), so repairing one into existence
        // would be building something nobody asked for; `gpkg index` is how
        // that is asked for.
        SpatialIndexStatus::Absent | SpatialIndexStatus::Current => Ok(false),
        other => {
            eprintln!("gpkg: {name} has an index state this version does not know ({other})");
            Ok(false)
        }
    }
}
