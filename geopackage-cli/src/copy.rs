//! `gpkg copy`: read a GeoPackage through this crate and write it back out.
//!
//! # What is copied
//!
//! Feature and attribute layers: their schemas, their spatial reference
//! systems, their rows, and a spatial index wherever the source had one.
//!
//! **Not** tiles, and not the extension tables (`gpkg_metadata`, `gpkg_schema`,
//! the Related Tables mapping tables). Those are out of scope for this
//! command. A source containing anything not copied is reported at the end
//! rather than passed over in silence, so the output is never mistaken for
//! the whole file.
//!
//! # How geometry crosses
//!
//! By its WKB bytes, through `FeatureWriter::insert_wkb`, which copies the
//! body into the new blob rather than re-serialising it. Reading the geometry
//! into a `geo-types` value and writing that back would lose the non-linear
//! curve types entirely, since `geo-traits` cannot represent an arc, and would
//! put every coordinate through a needless parse. The body is located with
//! `gpb::body_offset`, which reads the header only, so a curve passes through
//! a copy without anything ever parsing it.

use std::path::Path;
use std::process::ExitCode;

use geopackage::core::gpb;
use geopackage::core::types::ColumnType;
use geopackage::{
    ColumnSpec, ContentsDataType, GeoPackage, GeometrySpec, Layer, TableSchemaBuilder,
};

use crate::error::Result;

/// Rows per write transaction. Large enough that the per-transaction cost
/// disappears, small enough that a failure does not roll back the whole layer.
const BATCH: usize = 10_000;

pub fn run(src_path: &Path, dst_path: &Path) -> Result<ExitCode> {
    if dst_path.exists() {
        eprintln!("gpkg: {} already exists", dst_path.display());
        return Ok(ExitCode::FAILURE);
    }

    let src = GeoPackage::open_read_only_lenient(src_path)?;
    let dst = GeoPackage::create(dst_path)?;

    for warning in src.open_warnings() {
        println!("source warning: {warning}");
    }

    let layers = src.layers()?;
    for layer in &layers {
        copy_layer(&src, &dst, layer)?;
    }

    report_what_was_left(&src, &dst, &layers)?;
    dst.close()?;
    Ok(ExitCode::SUCCESS)
}

fn copy_layer(src: &GeoPackage, dst: &GeoPackage, layer: &Layer<'_>) -> Result<()> {
    let name = layer.table_name();

    // Every column except the geometry and the primary key, in the source's
    // order: those two are declared by `GeometrySpec` and by the builder's own
    // key rather than as ordinary columns, and `Feature::values` skips them.
    let geometry_name = layer.geometry_column().map(|geom| &geom.column_name);
    let pk_name = layer.primary_key_column();
    let mut builder = TableSchemaBuilder::new(name);
    // The key's name is part of the schema: a source calling it `id` should not
    // come out calling it `fid`, which is only this crate's default.
    if let Some(pk) = pk_name {
        builder = builder.primary_key(pk);
    }
    let mut untyped = Vec::new();
    for column in &layer.schema().columns {
        if Some(&column.name) == geometry_name || Some(column.name.as_str()) == pk_name {
            continue;
        }
        // A column whose declared type is outside the spec vocabulary parses to
        // `None`. Rather than drop it or guess, carry it as a BLOB, which
        // SQLite's affinity rules treat as the typeless case, and say so.
        let column_type = match &column.column_type {
            Some(column_type) => column_type.clone(),
            None => {
                untyped.push(column.name.clone());
                ColumnType::Blob(None)
            }
        };
        let mut spec = ColumnSpec::new(column.name.clone(), column_type);
        if column.not_null {
            spec = spec.not_null();
        }
        if let Some(default) = &column.default_value {
            spec = spec.default_value(default.clone());
        }
        builder = builder.column(spec);
    }
    if !untyped.is_empty() {
        println!(
            "{name}: {} column(s) had a type outside the spec vocabulary, carried as BLOB: {}",
            untyped.len(),
            untyped.join(", ")
        );
    }

    let indexed = match layer.geometry_column() {
        Some(geom) => {
            // The definition has to exist in the destination before a layer can
            // name it. `add_epsg_srs` is a no-op for one already present.
            if let Some(srs) = src.srs(geom.srs_id)? {
                dst.add_srs(&srs)?;
            }
            let indexed = layer.has_spatial_index()?;
            // The z and m flags have to be carried, not defaulted. Leaving them
            // at the default rejects the first geometry that has a z, because
            // the destination column declares the dimension Prohibited while
            // the body coming through insert_wkb still has one.
            builder = builder
                .geometry(
                    GeometrySpec::new(geom.geometry_type, geom.srs_id)
                        .column_name(geom.column_name.clone())
                        .z(geom.z)
                        .m(geom.m),
                )
                .spatial_index(indexed);
            dst.create_layer(&builder)?;
            indexed
        }
        None => {
            dst.create_attributes_table(&builder)?;
            false
        }
    };

    let target = if layer.geometry_column().is_some() {
        dst.layer(name)?
    } else {
        dst.attributes(name)?
    };

    let mut copied = 0u64;
    let mut writer = target.writer()?;
    let mut cursor = layer.cursor()?;
    let mut rows = cursor.features()?;
    let mut in_batch = 0usize;
    for feature in rows.by_ref() {
        let feature = feature?;
        let values: Vec<_> = feature.values().collect();

        match feature.geometry_bytes() {
            // The WKB body, located from the header alone, so a non-linear
            // geometry crosses without being parsed.
            Some(blob) => {
                let offset =
                    gpb::body_offset(blob).map_err(|e| geopackage::Error::Core(e.into()))?;
                let body = blob.get(offset..).unwrap_or_default();
                writer.insert_wkb(Some(feature.fid()), body, &values)?;
            }
            None => {
                writer.insert_row(Some(feature.fid()), &values)?;
            }
        }

        copied += 1;
        in_batch += 1;
        if in_batch >= BATCH {
            writer.commit()?;
            writer = target.writer()?;
            in_batch = 0;
        }
    }
    writer.commit()?;

    let index_note = if indexed { ", indexed" } else { "" };
    println!("{name}: {copied} rows{index_note}");
    Ok(())
}

/// Names anything in the source this command did not copy, so a copy is
/// never quietly mistaken for the whole file.
fn report_what_was_left(src: &GeoPackage, dst: &GeoPackage, copied: &[Layer<'_>]) -> Result<()> {
    let mut left = Vec::new();

    let tile_tables = src
        .contents()?
        .into_iter()
        .filter(|entry| entry.data_type == ContentsDataType::Tiles)
        .count();
    if tile_tables > 0 {
        left.push(format!("{tile_tables} tile pyramid(s)"));
    }

    // Only extensions the destination did not end up with. Writing the layers
    // registers some of them by itself: a curve layer registers its own
    // `gpkg_geom_<TYPE>`, and an indexed one registers `gpkg_rtree_index`, so
    // listing every source row here would report as lost several things that
    // are present in the copy.
    let arrived: std::collections::BTreeSet<String> =
        dst.extensions()?.into_iter().map(|row| row.name).collect();
    let mut missing: Vec<String> = src
        .extensions()?
        .into_iter()
        .map(|row| row.name)
        .filter(|name| !arrived.contains(name))
        .collect();
    missing.sort();
    missing.dedup();
    if !missing.is_empty() {
        left.push(format!("extensions: {}", missing.join(", ")));
    }

    if left.is_empty() {
        return Ok(());
    }
    println!(
        "\nnot copied ({} layers were): {}",
        copied.len(),
        left.join("; ")
    );
    println!("`gpkg copy` copies feature and attribute layers only.");
    Ok(())
}
