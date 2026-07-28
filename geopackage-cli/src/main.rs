//! `gpkg`: a command-line tool over the `geopackage` crate.
//!
//! Every subcommand is a thin arrangement of library calls. Where the library
//! has no method for something, that is a gap in the library rather than
//! something for this crate to work around: being the first consumer outside
//! the workspace is part of what this tool is for (roadmap phase 8, which
//! precedes the API freeze deliberately).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod copy;
mod error;
mod index;
mod info;
mod tiles;
mod validate;

/// Read, check and convert OGC GeoPackage files.
#[derive(Parser)]
#[command(name = "gpkg", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Summarise a file: version, layers, schemas, spatial reference systems,
    /// index health and registered extensions.
    Info {
        /// The `.gpkg` file to read.
        file: PathBuf,
    },
    /// Report what is wrong with a file, and what would put it right.
    ///
    /// Exits non-zero when a finding is an error, meaning a reader can get a
    /// wrong answer from the file. Nothing is modified.
    Validate {
        /// The `.gpkg` file to check.
        file: PathBuf,
        /// Also exit non-zero for warnings, not just errors.
        #[arg(long)]
        strict: bool,
    },
    /// Build a spatial index on a layer that has none.
    Index {
        /// The `.gpkg` file to write to.
        file: PathBuf,
        /// The layer to index.
        layer: String,
    },
    /// Repair spatial indexes that a legacy or mixed trigger set maintains, or
    /// that were left desynchronised.
    ///
    /// A layer with no index is left alone: that is a choice rather than a
    /// defect, and `gpkg index` is how one is asked for.
    Repair {
        /// The `.gpkg` file to write to.
        file: PathBuf,
        /// Repair only this layer, rather than every layer that needs it.
        layer: Option<String>,
    },
    /// Copy the feature and attribute layers of one file into a new one.
    ///
    /// Tiles and the extension tables are not carried; whatever is left behind
    /// is named at the end.
    Copy {
        /// The `.gpkg` file to read.
        src: PathBuf,
        /// The `.gpkg` file to create. Must not already exist.
        dst: PathBuf,
    },
    /// Tile pyramids: what a file holds, and the bytes of one tile.
    Tiles {
        #[command(subcommand)]
        command: TileCommand,
    },
}

#[derive(Subcommand)]
enum TileCommand {
    /// Describe every tile pyramid in a file, or one named one.
    Info {
        /// The `.gpkg` file to read.
        file: PathBuf,
        /// Describe only this pyramid.
        pyramid: Option<String>,
    },
    /// Write one tile's stored bytes out, addressed by zoom, column and row.
    ///
    /// No image is decoded: the bytes come out exactly as the file holds them,
    /// whatever `--out` is named.
    Get {
        /// The `.gpkg` file to read.
        file: PathBuf,
        /// The pyramid's table name.
        pyramid: String,
        /// Zoom level.
        zoom: i64,
        /// Tile column, counting east from the extent's west edge.
        column: i64,
        /// Tile row, counting south from the extent's north edge (the WMTS and
        /// XYZ sense, not TMS).
        row: i64,
        /// Write to this path instead of standard output.
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Info { file } => info::run(&file),
        Command::Validate { file, strict } => validate::run(&file, strict),
        Command::Index { file, layer } => index::build(&file, &layer),
        Command::Repair { file, layer } => index::repair(&file, layer.as_deref()),
        Command::Copy { src, dst } => copy::run(&src, &dst),
        Command::Tiles { command } => match command {
            TileCommand::Info { file, pyramid } => tiles::info(&file, pyramid.as_deref()),
            TileCommand::Get {
                file,
                pyramid,
                zoom,
                column,
                row,
                out,
            } => tiles::get(
                &file,
                &pyramid,
                geopackage::core::tiles::TileCoord::new(zoom, column, row),
                out.as_deref(),
            ),
        },
    };

    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("gpkg: {error}");
            ExitCode::FAILURE
        }
    }
}
