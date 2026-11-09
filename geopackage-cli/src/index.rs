//! `gpkg index` and `gpkg repair`: the two commands that write.
//!
//! Both open the file read-write and in lenient mode. The files worth repairing
//! are by definition ones with something wrong, and rejecting a legacy
//! `application_id` would put repair out of reach of the files that need it
//! most.
//!
//! The index work is chosen from the layer's [`SpatialIndexStatus`] rather
//! than done blindly, so the tool says what it does and skips a layer that
//! does not need it. `gpkg repair` also rewrites WKB type codes and
//! `DATETIME` text that are not in the form the spec gives them; each of
//! those repairs writes only the values that need it.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use geopackage::{ContentsDataType, GeoPackage, Layer, SpatialIndexStatus};

use crate::error::Result;

/// `gpkg index`: build an index on a layer that has none.
pub fn build(path: &Path, layer_name: &str, out: &mut impl Write) -> Result<ExitCode> {
    let gpkg = GeoPackage::open_lenient(path)?;
    let layer = gpkg.layer(layer_name)?;

    match layer.spatial_index_status()? {
        SpatialIndexStatus::Current => {
            writeln!(out, "{layer_name}: already indexed, nothing to do")?;
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
            writeln!(
                out,
                "{layer_name}: index built over {} rows",
                layer.count()?
            )?;
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

/// `gpkg repair`: repair every feature layer and attribute table that needs
/// it, or one named table.
pub fn repair(path: &Path, layer_name: Option<&str>, out: &mut impl Write) -> Result<ExitCode> {
    let gpkg = GeoPackage::open_lenient(path)?;

    let layers = match layer_name {
        Some(name) => vec![named_layer(&gpkg, name)?],
        None => {
            let mut layers = gpkg.layers()?;
            for entry in gpkg.contents()? {
                if entry.data_type == ContentsDataType::Attributes {
                    layers.push(gpkg.attributes(&entry.table_name)?);
                }
            }
            layers
        }
    };

    let mut repaired = 0usize;
    for layer in &layers {
        if repair_one(layer, out)? {
            repaired += 1;
        }
    }

    if repaired == 0 {
        writeln!(out, "nothing to repair")?;
    }
    Ok(ExitCode::SUCCESS)
}

/// Opens `name` as a feature layer, or as an attribute table when it is one.
fn named_layer<'g>(gpkg: &'g GeoPackage, name: &str) -> Result<Layer<'g>> {
    match gpkg.layer(name) {
        Err(geopackage::Error::WrongDataType { .. }) => Ok(gpkg.attributes(name)?),
        other => Ok(other?),
    }
}

/// Repair one layer, reporting whether anything was done.
fn repair_one(layer: &Layer<'_>, out: &mut impl Write) -> Result<bool> {
    let name = layer.table_name().to_owned();
    let has_geometry = layer.geometry_column().is_some();
    let mut changed = false;
    if has_geometry {
        let geometries = layer.repair_wkb_type_codes()?;
        if geometries > 0 {
            writeln!(
                out,
                "{name}: {geometries} geometries rewritten with ISO WKB type codes"
            )?;
            changed = true;
        }
    }
    let datetimes = layer.repair_datetimes()?;
    if datetimes > 0 {
        writeln!(
            out,
            "{name}: {datetimes} DATETIME values rewritten in the GeoPackage form"
        )?;
        changed = true;
    }
    // An attribute table has no geometry and so no index to repair.
    if has_geometry && repair_index(layer, &name, out)? {
        changed = true;
    }
    Ok(changed)
}

/// Repair one layer's spatial index, reporting whether anything was done.
fn repair_index(layer: &Layer<'_>, name: &str, out: &mut impl Write) -> Result<bool> {
    match layer.spatial_index_status()? {
        SpatialIndexStatus::Legacy | SpatialIndexStatus::Stale => {
            let status = layer.spatial_index_status()?;
            layer.repair_spatial_index()?;
            writeln!(
                out,
                "{name}: {status} index repaired to the 1.4 trigger set"
            )?;
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
