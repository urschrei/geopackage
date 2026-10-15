//! Property tests over `gpkg_spatial_ref_sys`: registration through
//! `add_srs` and `add_epsg_srs` against a model of the table.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use std::collections::BTreeMap;
use std::path::Path;

use geopackage::core::srs::epsg_definition;
use geopackage::{Error, GeoPackage, Srs};
use hegel::generators;

/// EPSG codes: vendored WKT1 ones, a UTM zone, ones only the registry knows,
/// and ones nothing knows.
const CODES: [i32; 10] = [
    3857, 4258, 27700, 32630, 4979, 4937, 7843, 4326, 94326, 999_999,
];

/// The registry's WKT2 for an EPSG code, if it has one.
fn registry(code: i32) -> Option<String> {
    epsg_utils::epsg_to_wkt2(code).ok().map(str::to_owned)
}

/// The registry's WKT2 for a row that is an EPSG row with a positive code.
fn registry_for(row: &Srs) -> Option<String> {
    (row.organization.eq_ignore_ascii_case("EPSG") && row.organization_coordsys_id > 0)
        .then(|| registry(row.organization_coordsys_id))
        .flatten()
}

/// The name `add_epsg_srs` gives a code that only the registry knows: the
/// first quoted token of its WKT2.
fn registry_name(wkt2: &str, code: i32) -> String {
    wkt2.split_once('"')
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(name, _)| name)
        .filter(|name| !name.is_empty())
        .map_or_else(|| format!("EPSG:{code}"), ToOwned::to_owned)
}

/// The table as the documentation describes it.
struct Model {
    rows: BTreeMap<i32, Srs>,
    /// Whether the file has the `gpkg_crs_wkt_1_1` columns.
    extension: bool,
}

impl Model {
    fn new(gpkg: &GeoPackage) -> Self {
        Self {
            rows: gpkg
                .srs_list()
                .unwrap()
                .into_iter()
                .map(|srs| (srs.srs_id, srs))
                .collect(),
            extension: false,
        }
    }

    /// `add_srs`: an existing id keeps its row; a WKT2 definition or an
    /// epoch brings in the extension columns; once the file has them, each
    /// EPSG row without WKT2 has the registry's, whether it was present
    /// before or added after; `undefined` reads back as no WKT2.
    fn add(&mut self, srs: &Srs) -> bool {
        if self.rows.contains_key(&srs.srs_id) {
            return false;
        }
        if (srs.definition_wkt2.is_some() || srs.epoch.is_some()) && !self.extension {
            self.extension = true;
            for row in self.rows.values_mut() {
                if row.definition_wkt2.is_none() {
                    row.definition_wkt2 = registry_for(row);
                }
            }
        }
        let mut stored = srs.clone();
        if self.extension && stored.definition_wkt2.is_none() {
            stored.definition_wkt2 = registry_for(&stored);
        }
        stored.definition_wkt2 = stored.definition_wkt2.filter(|text| text != "undefined");
        self.rows.insert(srs.srs_id, stored);
        true
    }

    /// `add_epsg_srs`: the vendored WKT1 if there is one, otherwise the
    /// registry's WKT2 with `undefined` as the WKT1.
    fn add_epsg(&mut self, code: i32) -> Option<bool> {
        let srs = if let Some(definition) = epsg_definition(code) {
            Srs {
                name: definition.name.into_owned(),
                srs_id: code,
                organization: "EPSG".into(),
                organization_coordsys_id: code,
                definition: definition.definition.into_owned(),
                description: None,
                definition_wkt2: None,
                epoch: None,
            }
        } else {
            let wkt2 = registry(code)?;
            Srs {
                name: registry_name(&wkt2, code),
                srs_id: code,
                organization: "EPSG".into(),
                organization_coordsys_id: code,
                definition: "undefined".into(),
                description: None,
                definition_wkt2: Some(wkt2),
                epoch: None,
            }
        };
        Some(self.add(&srs))
    }
}

/// Draws an id: one the file may already have, a code, or any `i32`.
fn draw_id(tc: &hegel::TestCase) -> i32 {
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(2)) {
        0 => tc.draw(generators::sampled_from(vec![-1, 0, 4326, 90_001, 90_002])),
        1 => tc.draw(generators::sampled_from(CODES.to_vec())),
        _ => tc.draw(generators::integers::<i32>()),
    }
}

/// Draws a row with any text, an organisation that is or is not EPSG in
/// any case, and any id, code, WKT2 and finite epoch.
fn draw_srs(tc: &hegel::TestCase) -> Srs {
    let organization = match tc.draw(generators::integers::<u8>().min_value(0).max_value(2)) {
        0 => tc
            .draw(generators::sampled_from(vec!["EPSG", "epsg", "Epsg"]))
            .to_owned(),
        1 => "NONE".to_owned(),
        _ => tc.draw(generators::text().max_size(8)),
    };
    let organization_coordsys_id = if tc.draw(generators::booleans()) {
        tc.draw(generators::sampled_from(CODES.to_vec()))
    } else {
        tc.draw(generators::integers::<i32>())
    };
    let definition_wkt2 = match tc.draw(generators::integers::<u8>().min_value(0).max_value(3)) {
        0 => Some("undefined".to_owned()),
        1 => Some(tc.draw(generators::text().max_size(16))),
        _ => None,
    };
    let epoch = tc.draw(generators::booleans()).then(|| {
        tc.draw(
            generators::floats::<f64>()
                .allow_nan(false)
                .allow_infinity(false),
        )
    });
    Srs {
        name: tc.draw(generators::text().max_size(16)),
        srs_id: draw_id(tc),
        organization,
        organization_coordsys_id,
        definition: tc.draw(generators::text().max_size(16)),
        description: tc
            .draw(generators::booleans())
            .then(|| tc.draw(generators::text().max_size(16))),
        definition_wkt2,
        epoch,
    }
}

/// Any sequence of `add_srs` and `add_epsg_srs` calls leaves the table the
/// documentation describes, as read by `srs` and by `srs_list`.
#[hegel::test]
fn registration_matches_the_model(tc: hegel::TestCase) {
    let gpkg = GeoPackage::create(Path::new(":memory:")).unwrap();
    let mut model = Model::new(&gpkg);
    let steps = tc.draw(generators::integers::<usize>().min_value(1).max_value(8));
    for _ in 0..steps {
        if tc.draw(generators::booleans()) {
            let srs = draw_srs(&tc);
            tc.event(format!(
                "add_srs, wkt2 {}, epoch {}",
                srs.definition_wkt2.is_some(),
                srs.epoch.is_some()
            ));
            assert_eq!(gpkg.add_srs(&srs).unwrap(), model.add(&srs), "{srs:?}");
        } else {
            let code = tc.draw(generators::sampled_from(CODES.to_vec()));
            match (gpkg.add_epsg_srs(code), model.add_epsg(code)) {
                (Ok(inserted), Some(expected)) => assert_eq!(inserted, expected, "{code}"),
                (Err(Error::UnknownEpsgCode { code: reported }), None) => {
                    assert_eq!(reported, code);
                }
                (result, expected) => panic!("EPSG:{code}: {result:?}, expected {expected:?}"),
            }
        }
        let expected: Vec<Srs> = model.rows.values().cloned().collect();
        assert_eq!(gpkg.srs_list().unwrap(), expected);
        for row in &expected {
            assert_eq!(gpkg.srs(row.srs_id).unwrap().as_ref(), Some(row));
        }
    }
}

/// The rows a set of `add_epsg_srs` calls leaves do not depend on the order
/// of the calls.
#[hegel::test]
fn epsg_registration_does_not_depend_on_order(tc: hegel::TestCase) {
    let codes = tc.draw(
        generators::vecs(generators::sampled_from(CODES.to_vec()))
            .min_size(1)
            .max_size(5),
    );
    let mut sorted = codes.clone();
    sorted.sort_unstable();
    let rows = |order: &[i32]| {
        let gpkg = GeoPackage::create(Path::new(":memory:")).unwrap();
        for &code in order {
            // An unknown code is an error and changes nothing.
            match gpkg.add_epsg_srs(code) {
                Ok(_) | Err(Error::UnknownEpsgCode { .. }) => {}
                Err(error) => panic!("EPSG:{code}: {error}"),
            }
        }
        gpkg.srs_list().unwrap()
    };
    assert_eq!(rows(&codes), rows(&sorted), "{codes:?}");
}

/// A row with a NaN epoch is `NanEpoch`, and the table and the extension
/// registrations are as they were.
#[hegel::test]
fn a_nan_epoch_is_refused_and_changes_nothing(tc: hegel::TestCase) {
    let gpkg = GeoPackage::create(Path::new(":memory:")).unwrap();
    if tc.draw(generators::booleans()) {
        gpkg.add_epsg_srs(4979).unwrap();
    }
    let before = (gpkg.srs_list().unwrap(), gpkg.extensions().unwrap());
    let mut srs = draw_srs(&tc);
    srs.epoch = Some(f64::NAN);
    assert!(
        matches!(gpkg.add_srs(&srs), Err(Error::NanEpoch { srs_id }) if srs_id == srs.srs_id),
        "{srs:?}"
    );
    assert_eq!(
        (gpkg.srs_list().unwrap(), gpkg.extensions().unwrap()),
        before
    );
}
