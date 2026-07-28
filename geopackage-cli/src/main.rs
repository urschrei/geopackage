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

mod info;
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
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Info { file } => info::run(&file),
        Command::Validate { file, strict } => validate::run(&file, strict),
    };

    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("gpkg: {error}");
            ExitCode::FAILURE
        }
    }
}
