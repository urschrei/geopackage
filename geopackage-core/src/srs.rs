//! Spatial reference system definitions: a vendored EPSG subset.
//!
//! Policy (design decision D3): no PROJ dependency and no coordinate
//! transformation, ever. This module ships WKT1 definitions for a small set
//! of EPSG codes that cover the bulk of real-world GeoPackage traffic, plus
//! synthesised definitions for all WGS 84 UTM zones (EPSG:32601-32660 north,
//! 32701-32760 south), which differ only in name, central meridian, false
//! northing, and authority code. A failed lookup is not the end of the road:
//! the container crate falls back to the EPSG registry in `epsg-utils` and
//! writes WKT2 through the `gpkg_crs_wkt_1_1` extension, and only a code in
//! neither place is a typed "supply the definition yourself" error. Nothing
//! here ever writes a silent `undefined` in place of a definition it has.
//!
//! Literal definitions live in the generated `epsg_wkt` table (private
//! submodule); regenerate with `scripts/generate_epsg_wkt.py`.

mod epsg_wkt;

use std::borrow::Cow;

/// A spatial reference system definition, ready to insert into
/// `gpkg_spatial_ref_sys`.
///
/// `definition` is WKT1 as used by the `gpkg_spatial_ref_sys.definition`
/// column. Codes with no WKT1 form, such as the geographic 3D CRS EPSG:4979,
/// are absent from this table by nature rather than by omission. The
/// container crate registers those through the `gpkg_crs_wkt_1_1` extension
/// column instead; see `GeoPackage::add_epsg_srs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrsDefinition {
    /// Human-readable SRS name.
    pub name: Cow<'static, str>,
    /// Case-insensitive name of the defining organization.
    pub organization: Cow<'static, str>,
    /// Numeric ID of the SRS as assigned by the organization.
    pub organization_coordsys_id: i32,
    /// WKT1 definition string.
    pub definition: Cow<'static, str>,
}

/// Look up the vendored definition for an EPSG code.
///
/// Returns `None` for codes outside the vendored subset; callers are then
/// expected to supply their own [`SrsDefinition`].
pub fn epsg_definition(code: i32) -> Option<SrsDefinition> {
    if let Some(&(code, name, wkt)) = epsg_wkt::EPSG_WKT1
        .binary_search_by_key(&code, |r| r.0)
        .ok()
        .and_then(|idx| epsg_wkt::EPSG_WKT1.get(idx))
    {
        return Some(SrsDefinition {
            name: Cow::Borrowed(name),
            organization: Cow::Borrowed("EPSG"),
            organization_coordsys_id: code,
            definition: Cow::Borrowed(wkt),
        });
    }
    utm_zone(code)
}

/// Synthesise the WKT1 definition for a WGS 84 UTM zone code.
fn utm_zone(code: i32) -> Option<SrsDefinition> {
    let (zone, north) = match code {
        32601..=32660 => (code - 32600, true),
        32701..=32760 => (code - 32700, false),
        _ => return None,
    };
    let central_meridian = zone * 6 - 183;
    let false_northing = if north { 0 } else { 10_000_000 };
    let hemisphere = if north { 'N' } else { 'S' };
    let name = format!("WGS 84 / UTM zone {zone}{hemisphere}");
    let definition = format!(
        r#"PROJCS["{name}",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563,AUTHORITY["EPSG","7030"]],AUTHORITY["EPSG","6326"]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],AUTHORITY["EPSG","4326"]],PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",{central_meridian}],PARAMETER["scale_factor",0.9996],PARAMETER["false_easting",500000],PARAMETER["false_northing",{false_northing}],UNIT["metre",1,AUTHORITY["EPSG","9001"]],AXIS["Easting",EAST],AXIS["Northing",NORTH],AUTHORITY["EPSG","{code}"]]"#
    );
    Some(SrsDefinition {
        name: Cow::Owned(name),
        organization: Cow::Borrowed("EPSG"),
        organization_coordsys_id: code,
        definition: Cow::Owned(definition),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_sorted_for_binary_search() {
        assert!(epsg_wkt::EPSG_WKT1.is_sorted_by_key(|r| r.0));
    }

    #[test]
    fn looks_up_literal_codes() {
        let osgb = epsg_definition(27700).unwrap();
        assert_eq!(osgb.name, "OSGB36 / British National Grid");
        assert_eq!(osgb.organization, "EPSG");
        assert_eq!(osgb.organization_coordsys_id, 27700);
        assert!(osgb.definition.starts_with("PROJCS["));
        assert!(osgb.definition.ends_with(r#"AUTHORITY["EPSG","27700"]]"#));

        let wgs84 = epsg_definition(4326).unwrap();
        assert!(wgs84.definition.starts_with("GEOGCS["));
    }

    #[test]
    fn unknown_codes_return_none() {
        for code in [0, -1, 1, 99999, 4979, 32600, 32661, 32700, 32761] {
            assert!(epsg_definition(code).is_none(), "EPSG:{code}");
        }
    }

    #[test]
    fn utm_synthesis_matches_gdal_reference() {
        for (code, reference) in epsg_wkt::UTM_TEST_REFERENCES {
            let def = epsg_definition(*code).unwrap();
            assert_eq!(def.definition, *reference, "EPSG:{code}");
        }
    }

    #[test]
    fn utm_zone_arithmetic() {
        let z1n = epsg_definition(32601).unwrap();
        assert_eq!(z1n.name, "WGS 84 / UTM zone 1N");
        assert!(
            z1n.definition
                .contains(r#"PARAMETER["central_meridian",-177]"#)
        );
        assert!(z1n.definition.contains(r#"PARAMETER["false_northing",0]"#));

        let z60s = epsg_definition(32760).unwrap();
        assert_eq!(z60s.name, "WGS 84 / UTM zone 60S");
        assert!(
            z60s.definition
                .contains(r#"PARAMETER["central_meridian",177]"#)
        );
        assert!(
            z60s.definition
                .contains(r#"PARAMETER["false_northing",10000000]"#)
        );
    }
}
