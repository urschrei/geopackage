//! `gpkg copy`: read a GeoPackage through this crate and write it back out.
//!
//! # What is copied
//!
//! Feature and attribute layers: their schemas, their spatial reference
//! systems, their rows, and a spatial index wherever the source had one.
//!
//! **Not** tiles, and not the extension tables (`gpkg_metadata`, `gpkg_schema`,
//! the Related Tables mapping tables). Those are out of scope for this command.
//! If the source contains anything that is not copied, the command reports it
//! at the end, so you don't mistake the output for the whole file.
//!
//! # How geometry is copied
//!
//! Geometry is copied as WKB bytes through `FeatureWriter::insert_wkb`, which
//! copies the body into the new blob rather than re-serialising it. Reading the
//! geometry into a `geo-types` value and writing that back would lose the
//! non-linear curve types entirely, since `geo-traits` cannot represent an arc,
//! and would parse every coordinate for no reason. `gpb::body_offset` locates
//! the body by reading the header only, so a curve passes through a copy
//! without ever being parsed.
//!
//! # Threads
//!
//! Each layer is read on a second thread, through a second read-only
//! connection to the source, and written on the calling thread. The reader
//! sends owned rows to the writer in chunks over a bounded channel, so the two
//! halves overlap and memory use stays bounded. The writer commits every
//! `BATCH` rows, independently of the chunk size.
//!
//! # Durability
//!
//! The output is opened with `synchronous = OFF`, as GDAL does for a new
//! GeoPackage. If the copy stops before it completes, the output is incomplete
//! whatever the setting, so a sync at each commit gives no protection. The
//! command does not sync the file when it completes, as `cp` and `ogr2ogr` do
//! not: the operating system writes it to disk later. A power loss in that
//! interval can leave the output damaged. Run `sync` after the command if the
//! output must be on disk when it returns.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

use geopackage::core::gpb;
use geopackage::core::types::ColumnType;
use geopackage::{
    ColumnSpec, ContentsDataType, GeoPackage, GeometrySpec, Layer, OpenOptions, Synchronous,
    TableSchemaBuilder, Value, ValueRef,
};

use crate::error::Result;

/// Rows per write transaction. This is large enough to make the
/// per-transaction cost negligible, and small enough that a failure does not
/// roll back the whole layer.
const BATCH: usize = 10_000;

/// The reader sends a chunk when it holds this many rows.
const CHUNK_ROWS: usize = 1_000;

/// The reader also sends a chunk when its geometry bodies reach this many
/// bytes, so that a layer of large geometries does not fill memory.
const CHUNK_BYTES: usize = 8 << 20;

/// Chunks that can wait in the channel before the reader blocks.
const CHANNEL_DEPTH: usize = 8;

/// One source row, owned, so that it can go to the writer thread.
struct Row {
    fid: i64,
    /// The WKB body, without the GeoPackage header. `None` for a row with no
    /// geometry, and for every row of an attribute table.
    body: Option<Vec<u8>>,
    values: Vec<Value>,
}

pub fn run(src_path: &Path, dst_path: &Path, out: &mut impl Write) -> Result<ExitCode> {
    if dst_path.exists() {
        eprintln!("gpkg: {} already exists", dst_path.display());
        return Ok(ExitCode::FAILURE);
    }

    let src = GeoPackage::open_read_only_lenient(src_path)?;
    let dst = OpenOptions::new()
        .synchronous(Synchronous::Off)
        .create(dst_path)?;

    for warning in src.open_warnings() {
        writeln!(out, "source warning: {warning}")?;
    }

    let layers = src.layers()?;
    for layer in &layers {
        copy_layer(&src, src_path, &dst, layer, out)?;
    }

    report_what_was_left(&src, &dst, &layers, out)?;
    dst.close()?;
    Ok(ExitCode::SUCCESS)
}

fn copy_layer(
    src: &GeoPackage,
    src_path: &Path,
    dst: &GeoPackage,
    layer: &Layer<'_>,
    out: &mut impl Write,
) -> Result<()> {
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
        writeln!(
            out,
            "{name}: {} column(s) had a type outside the spec vocabulary, carried as BLOB: {}",
            untyped.len(),
            untyped.join(", ")
        )?;
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

    let is_feature = layer.geometry_column().is_some();
    let copied = std::thread::scope(|scope| -> Result<u64> {
        let (tx, rx) = sync_channel(CHANNEL_DEPTH);
        let reader = scope.spawn(move || read_rows(src_path, name, is_feature, &tx));
        // `write_rows` takes `rx` and drops it when it returns. A reader that is
        // blocked on a full channel then gets an error and stops.
        let written = write_rows(&target, rx);
        match reader.join() {
            Ok(read) => read?,
            Err(panic) => std::panic::resume_unwind(panic),
        }
        written
    })?;

    let index_note = if indexed { ", indexed" } else { "" };
    writeln!(out, "{name}: {copied} rows{index_note}")?;
    Ok(())
}

/// Reads every row of the layer `name` through a new connection to the source
/// and sends the rows to `tx` in chunks.
///
/// Returns `Ok` without reading further if the writer has stopped: the writer
/// reports its own error.
fn read_rows(
    src_path: &Path,
    name: &str,
    is_feature: bool,
    tx: &SyncSender<Vec<Row>>,
) -> Result<()> {
    let src = GeoPackage::open_read_only_lenient(src_path)?;
    let layer = if is_feature {
        src.layer(name)?
    } else {
        src.attributes(name)?
    };
    let mut chunk = Vec::with_capacity(CHUNK_ROWS);
    let mut chunk_bytes = 0;
    let mut cursor = layer.cursor()?;
    let mut rows = cursor.features()?;
    for feature in rows.by_ref() {
        let feature = feature?;
        // The WKB body, located from the header alone, so a non-linear
        // geometry crosses without being parsed.
        let body = match feature.geometry_bytes() {
            Some(blob) => {
                let offset =
                    gpb::body_offset(blob).map_err(|e| geopackage::Error::Core(e.into()))?;
                Some(blob.get(offset..).unwrap_or_default().to_vec())
            }
            None => None,
        };
        chunk_bytes += body.as_ref().map_or(0, Vec::len);
        chunk.push(Row {
            fid: feature.fid(),
            body,
            values: feature.values().map(|value| value.to_value()).collect(),
        });
        if chunk.len() >= CHUNK_ROWS || chunk_bytes >= CHUNK_BYTES {
            let full = std::mem::replace(&mut chunk, Vec::with_capacity(CHUNK_ROWS));
            chunk_bytes = 0;
            if tx.send(full).is_err() {
                return Ok(());
            }
        }
    }
    if !chunk.is_empty() && tx.send(chunk).is_err() {
        return Ok(());
    }
    Ok(())
}

/// Writes the rows that arrive on `rx` into `target`, and commits every
/// `BATCH` rows. Returns the number of rows written.
fn write_rows(target: &Layer<'_>, rx: Receiver<Vec<Row>>) -> Result<u64> {
    let mut copied = 0u64;
    let mut in_batch = 0usize;
    let mut writer = target.writer()?;
    for chunk in rx {
        for row in &chunk {
            let values: Vec<_> = row.values.iter().map(ValueRef::from).collect();
            match &row.body {
                Some(body) => writer.insert_wkb(Some(row.fid), body, &values)?,
                None => writer.insert_row(Some(row.fid), &values)?,
            };
            copied += 1;
            in_batch += 1;
            if in_batch >= BATCH {
                writer.commit()?;
                writer = target.writer()?;
                in_batch = 0;
            }
        }
    }
    writer.commit()?;
    Ok(copied)
}

/// Names anything in the source that this command did not copy, so that a
/// partial copy is never mistaken for the whole file.
fn report_what_was_left(
    src: &GeoPackage,
    dst: &GeoPackage,
    copied: &[Layer<'_>],
    out: &mut impl Write,
) -> Result<()> {
    let mut left = Vec::new();

    let contents = src.contents()?;
    let tile_tables = contents
        .iter()
        .filter(|entry| entry.data_type == ContentsDataType::Tiles)
        .count();
    if tile_tables > 0 {
        left.push(format!("{tile_tables} tile pyramid(s)"));
    }
    // Counted separately, not in the pyramid line: a coverage is a separate
    // data type. Otherwise the report below shows it only as a missing
    // extension, which looks like lost metadata, not lost data.
    let coverages = contents
        .iter()
        .filter(|entry| entry.data_type == ContentsDataType::Coverage)
        .count();
    if coverages > 0 {
        left.push(format!("{coverages} tiled gridded coverage(s)"));
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
    writeln!(
        out,
        "\nnot copied ({} layers were): {}",
        copied.len(),
        left.join("; ")
    )?;
    writeln!(out, "`gpkg copy` copies feature and attribute layers only.")?;
    Ok(())
}
