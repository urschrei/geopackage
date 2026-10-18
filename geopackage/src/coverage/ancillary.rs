//! Checks on the values of an ancillary row before it is written.

use geopackage_core::coverage::CoverageDatatype;

use crate::{Error, Result};

/// Requirement 11: "When the datatype of the corresponding
/// `gpkg_2d_gridded_coverage_ancillary` row is _float_, the `scale` and
/// `offset` values _SHALL_ be set to the defaults."
#[expect(
    clippy::float_cmp,
    reason = "the requirement says \"set to the defaults\", which is exactly 1.0 and 0.0; a tolerance would accept values it does not"
)]
pub(crate) fn check_float_defaults(
    datatype: CoverageDatatype,
    scale: f64,
    offset: f64,
    table_name: &str,
) -> Result<()> {
    if datatype == CoverageDatatype::Float && (scale != 1.0 || offset != 0.0) {
        return Err(Error::FloatCoverageScaled {
            table_name: table_name.to_owned(),
            scale,
            offset,
        });
    }
    Ok(())
}

/// Rejects a NaN in an ancillary row, which SQLite would store as NULL.
/// `values` pairs each column, qualified by its table, with its value.
pub(crate) fn check_not_nan(
    table_name: &str,
    values: &[(&'static str, Option<f64>)],
) -> Result<()> {
    for &(column, value) in values {
        if value.is_some_and(f64::is_nan) {
            return Err(Error::NanAncillaryValue {
                table_name: table_name.to_owned(),
                column,
            });
        }
    }
    Ok(())
}
