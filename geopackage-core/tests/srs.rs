//! Property tests over the vendored EPSG definitions.

use geopackage_core::srs::epsg_definition;
use hegel::generators;

/// Codes common in GeoPackage files, most of them in the vendored table.
const COMMON: [i32; 12] = [
    2056, 2154, 2157, 2193, 3006, 3035, 3857, 4258, 4269, 4326, 4979, 27700,
];

/// Draws a code: a common one, one at or next to a UTM range bound, one inside
/// a UTM range, or any `i32`.
fn draw_code(tc: &hegel::TestCase) -> i32 {
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(3)) {
        0 => tc.draw(generators::sampled_from(COMMON.to_vec())),
        1 => {
            let bound = tc.draw(generators::sampled_from(vec![32601, 32660, 32701, 32760]));
            bound + tc.draw(generators::integers::<i32>().min_value(-1).max_value(1))
        }
        2 => {
            let base = tc.draw(generators::sampled_from(vec![32600, 32700]));
            base + tc.draw(generators::integers::<i32>().min_value(1).max_value(60))
        }
        _ => tc.draw(generators::integers::<i32>()),
    }
}

/// Whether the brackets of `wkt` outside quoted text nest and close, and
/// nothing follows the last one.
fn brackets_balance(wkt: &str) -> bool {
    let mut depth = 0_usize;
    let mut quoted = false;
    let mut closed = false;
    for c in wkt.chars() {
        if closed {
            return false;
        }
        match c {
            '"' => quoted = !quoted,
            '[' if !quoted => depth += 1,
            ']' if !quoted => {
                let Some(next) = depth.checked_sub(1) else {
                    return false;
                };
                depth = next;
                closed = depth == 0;
            }
            _ => {}
        }
    }
    closed && !quoted
}

/// A definition, when there is one, is an EPSG row for the code asked for:
/// its organisation is `EPSG`, its id is the code, its WKT1 names the code as
/// its authority and the SRS by its name, and its brackets balance.
#[hegel::test]
fn a_definition_describes_the_code_asked_for(tc: hegel::TestCase) {
    let code = draw_code(&tc);
    let Some(definition) = epsg_definition(code) else {
        tc.event("no definition");
        return;
    };
    assert_eq!(definition.organization, "EPSG");
    assert_eq!(definition.organization_coordsys_id, code);
    let wkt = &definition.definition;
    assert!(
        wkt.ends_with(&format!(r#"AUTHORITY["EPSG","{code}"]]"#)),
        "{wkt}"
    );
    let body = ["PROJCS[", "GEOGCS["]
        .iter()
        .find_map(|keyword| wkt.strip_prefix(keyword))
        .expect("a projected or geographic CRS");
    let name = body
        .strip_prefix('"')
        .and_then(|rest| rest.split_once('"'))
        .map(|(name, _)| name);
    assert_eq!(name, Some(&*definition.name), "{wkt}");
    assert!(brackets_balance(wkt), "{wkt}");
}

/// Every code in the two WGS 84 UTM ranges has a definition whose central
/// meridian is the middle of its zone, and whose false northing is
/// 10,000,000 m in the south and 0 in the north.
#[hegel::test]
fn every_utm_zone_has_its_definition(tc: hegel::TestCase) {
    let zone = tc.draw(generators::integers::<i32>().min_value(1).max_value(60));
    let north = tc.draw(generators::booleans());
    let code = if north { 32600 + zone } else { 32700 + zone };
    let definition = epsg_definition(code).expect("a UTM zone");
    let hemisphere = if north { 'N' } else { 'S' };
    assert_eq!(
        definition.name,
        format!("WGS 84 / UTM zone {zone}{hemisphere}")
    );
    let wkt = &definition.definition;
    let meridian = zone * 6 - 183;
    assert!(
        wkt.contains(&format!(r#"PARAMETER["central_meridian",{meridian}]"#)),
        "{wkt}"
    );
    let northing = if north { 0 } else { 10_000_000 };
    assert!(
        wkt.contains(&format!(r#"PARAMETER["false_northing",{northing}]"#)),
        "{wkt}"
    );
}
