//! `gpkg validate`: what this crate can see wrong with a file.
//!
//! Read-only and lenient, for the same reason as `gpkg info`: a file with
//! nothing wrong with it does not need validating, so refusing to open the
//! interesting ones would defeat the command.
//!
//! Nothing is modified. Where a finding has a repair, the advice names the
//! library method that performs it, and running it stays the caller's decision.

use std::path::Path;
use std::process::ExitCode;

use geopackage::{GeoPackage, Result, Severity};

pub fn run(path: &Path, strict: bool) -> Result<ExitCode> {
    let gpkg = GeoPackage::open_read_only_lenient(path)?;
    let findings = gpkg.validate()?;

    println!("{}", path.display());
    if findings.is_empty() {
        // Said explicitly, because a silent exit reads as "did not run". The
        // qualification matters: this is what we can see, not conformance.
        println!("  no findings (this crate's checks; the OGC ETS is the authority)");
        return Ok(ExitCode::SUCCESS);
    }

    // `validate` returns findings most severe first, so they print in the order
    // a reader should care about them.
    for finding in &findings {
        println!("  {}: {finding}", finding.severity());
        if let Some(repair) = finding.repair() {
            println!("    repair: {repair}");
        }
    }

    let errors = count(&findings, Severity::Error);
    let warnings = count(&findings, Severity::Warning);
    let advisories = count(&findings, Severity::Advisory);
    println!(
        "\n  {}, {}, {}",
        plural(errors, "error", "errors"),
        plural(warnings, "warning", "warnings"),
        plural(advisories, "advisory", "advisories")
    );

    // An error means a reader can get a wrong answer, which is worth a failing
    // exit status so a script or a CI job notices. Warnings and advisories are
    // not, unless the caller asks for `--strict`.
    let failed = if strict {
        errors + warnings > 0
    } else {
        errors > 0
    };
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn count(findings: &[geopackage::Finding], severity: Severity) -> usize {
    findings
        .iter()
        .filter(|finding| finding.severity() == severity)
        .count()
}

/// `1 error`, `0 errors`. The plural is given rather than derived, because
/// "advisory" does not pluralise by adding an `s`.
fn plural(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("{count} {one}")
    } else {
        format!("{count} {many}")
    }
}
