//! Property tests over the extension catalogue: `gpkg_extensions` rows
//! written with SQL, as another tool would leave them, against the listing
//! methods and the check that refuses writes an unidentified extension
//! governs.

#![expect(
    clippy::unwrap_used,
    reason = "clippy's allow-*-in-tests covers #[test] fns but not the free helper fns in an integration-test crate; the unwraps in these helpers are the intended failure mechanism"
)]

use geopackage::core::coverage::CoverageDatatype;
use geopackage::core::extensions::{Extension, ExtensionScope, ExtensionSupport};
use geopackage::core::related::RelationName;
use geopackage::core::tiles::{TileMatrixSet, ZoomLadder};
use geopackage::core::types::{ColumnType, GeometryType};
use geopackage::{
    ColumnSpec, CoverageBuilder, Error, ExtensionRow, GeoPackage, GeometrySpec, NewRelation,
    OpenOptions, TableSchemaBuilder, TilePyramidBuilder,
};
use hegel::generators;

/// Creates a file with a feature layer `pts`, an attribute table `notes`, a
/// tile pyramid `tiles`, a coverage `cov` and a relationship from `pts` to
/// `notes` through the mapping table `map`. The coverage and the relationship
/// register extensions, which [`baseline`] reads.
fn fixture(path: &std::path::Path) {
    build(&GeoPackage::create(path).unwrap());
}

/// The fixture in SQLite's in-memory database, for the properties that do
/// not reopen the file.
fn in_memory() -> GeoPackage {
    let gpkg = GeoPackage::create(std::path::Path::new(":memory:")).unwrap();
    build(&gpkg);
    gpkg
}

fn build(gpkg: &GeoPackage) {
    gpkg.create_layer(
        &TableSchemaBuilder::new("pts")
            .column(ColumnSpec::new("name", ColumnType::Text(None)))
            .geometry(GeometrySpec::new(GeometryType::Point, 4326))
            .spatial_index(false),
    )
    .unwrap();
    gpkg.create_attributes_table(
        &TableSchemaBuilder::new("notes").column(ColumnSpec::new("body", ColumnType::Text(None))),
    )
    .unwrap();
    let set = TileMatrixSet::new(0, 0.0, 0.0, 1.0, 1.0);
    gpkg.create_tile_pyramid(
        &TilePyramidBuilder::new("tiles", set).matrices(set.ladder(ZoomLadder::new(0, 1)).unwrap()),
    )
    .unwrap();
    gpkg.create_coverage(
        &CoverageBuilder::new("cov", set, CoverageDatatype::Float)
            .matrices(set.ladder(ZoomLadder::new(0, 0)).unwrap()),
    )
    .unwrap();
    gpkg.add_relation(&NewRelation::new(
        "pts",
        "notes",
        RelationName::Media,
        "map",
    ))
    .unwrap();
}

/// The catalogue rows the fixture registered, read with SQL rather than
/// through the methods under test.
fn baseline(gpkg: &GeoPackage) -> Vec<ExtensionRow> {
    gpkg.connection()
        .prepare("SELECT table_name, column_name, extension_name, definition, scope FROM gpkg_extensions")
        .unwrap()
        .query_map([], |r| {
            Ok(ExtensionRow {
                table_name: r.get(0)?,
                column_name: r.get(1)?,
                name: r.get(2)?,
                definition: r.get(3)?,
                scope: ExtensionScope::parse(&r.get::<_, String>(4)?),
            })
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// Draws an extension name: one this crate identifies, an alias, a removed
/// one, or one it does not identify.
fn draw_name(tc: &hegel::TestCase) -> String {
    match tc.draw(generators::integers::<u8>().min_value(0).max_value(2)) {
        0 => tc
            .draw(generators::sampled_from(vec![
                "gpkg_rtree_index",
                "gpkg_webp",
                "related_tables",
                "gpkg_geom_CIRCULARSTRING",
                "gdal_aspatial",
                "gpkg_srs_id_trigger",
            ]))
            .to_owned(),
        1 => tc
            .draw(generators::sampled_from(vec![
                "acme_magic",
                "gpkg_geom_POINT",
                "GPKG_WEBP",
            ]))
            .to_owned(),
        _ => tc.draw(generators::text().min_size(1).max_size(10)),
    }
}

/// Draws the catalogue rows to insert, unique on table, column and name as
/// the `ge_tce` constraint requires. Returns them as `ExtensionRow`s.
fn draw_rows(tc: &hegel::TestCase, existing: &[ExtensionRow]) -> Vec<ExtensionRow> {
    let count = tc.draw(generators::integers::<usize>().min_value(0).max_value(8));
    let mut rows: Vec<ExtensionRow> = Vec::new();
    for _ in 0..count {
        let table_name = tc
            .draw(generators::sampled_from(vec![
                None,
                Some("pts"),
                Some("PTS"),
                Some("notes"),
                Some("tiles"),
                Some("cov"),
                Some("map"),
                Some("newmap"),
                Some("fresh"),
                Some("absent"),
            ]))
            .map(str::to_owned);
        let column_name = tc
            .draw(generators::sampled_from(vec![
                None,
                Some("geom"),
                Some("tile_data"),
            ]))
            .map(str::to_owned);
        let scope = match tc.draw(generators::integers::<u8>().min_value(0).max_value(2)) {
            0 => "read-write".to_owned(),
            1 => "write-only".to_owned(),
            _ => tc.draw(generators::text().max_size(6)),
        };
        let row = ExtensionRow {
            table_name,
            column_name,
            name: draw_name(tc),
            definition: tc.draw(generators::text().max_size(8)),
            scope: ExtensionScope::parse(&scope),
        };
        // SQLite's UNIQUE treats NULLs as distinct, so only rows whose three
        // columns are all set can collide.
        let collides = row.table_name.is_some()
            && row.column_name.is_some()
            && rows.iter().chain(existing).any(|r| {
                r.table_name == row.table_name
                    && r.column_name == row.column_name
                    && r.name == row.name
            });
        if !collides {
            rows.push(row);
        }
    }
    rows
}

/// Inserts the rows with SQL, into the table the fixture created.
fn insert(gpkg: &GeoPackage, rows: &[ExtensionRow]) {
    let conn = gpkg.connection();
    for row in rows {
        conn.execute(
            "INSERT INTO gpkg_extensions VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                row.table_name,
                row.column_name,
                row.name,
                row.definition,
                row.scope.as_str()
            ],
        )
        .unwrap();
    }
}

/// `extensions` returns every row exactly, ordered by extension name, table
/// and column, with a NULL before any value and text compared byte by byte.
#[hegel::test]
fn extensions_lists_every_row_in_order(tc: hegel::TestCase) {
    let gpkg = in_memory();
    let existing = baseline(&gpkg);
    let drawn = draw_rows(&tc, &existing);
    insert(&gpkg, &drawn);
    let mut rows = [existing, drawn].concat();
    rows.sort_by(|a, b| {
        (&a.name, &a.table_name, &a.column_name).cmp(&(&b.name, &b.table_name, &b.column_name))
    });
    let listed = gpkg.extensions().unwrap();
    // Rows equal on all three sort keys have no order between them.
    let key = |r: &ExtensionRow| (r.name.clone(), r.table_name.clone(), r.column_name.clone());
    assert_eq!(
        listed.iter().map(key).collect::<Vec<_>>(),
        rows.iter().map(key).collect::<Vec<_>>()
    );
    let mut sorted_listed = listed.clone();
    let full = |r: &ExtensionRow| {
        (
            r.name.clone(),
            r.table_name.clone(),
            r.column_name.clone(),
            r.definition.clone(),
            r.scope.as_str().to_owned(),
        )
    };
    sorted_listed.sort_by_key(full);
    rows.sort_by_key(full);
    assert_eq!(sorted_listed, rows);
}

/// The table names the catalogue checks are asked about, in drawn case.
fn draw_table(tc: &hegel::TestCase) -> String {
    let name = tc.draw(generators::sampled_from(vec![
        "pts", "notes", "tiles", "cov", "map", "fresh",
    ]));
    name.chars()
        .map(|c| {
            if tc.draw(generators::weighted_booleans(0.2)) {
                c.to_ascii_uppercase()
            } else {
                c
            }
        })
        .collect()
}

/// `table_extensions` returns the rows naming a table, with the name
/// compared ignoring ASCII case, ordered by extension name then column.
#[hegel::test]
fn table_extensions_matches_names_ignoring_case(tc: hegel::TestCase) {
    let gpkg = in_memory();
    let existing = baseline(&gpkg);
    let drawn = draw_rows(&tc, &existing);
    insert(&gpkg, &drawn);
    let rows = [existing, drawn].concat();
    let table = draw_table(&tc);
    let mut expected: Vec<(String, Option<String>)> = rows
        .iter()
        .filter(|r| {
            r.table_name
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case(&table))
        })
        .map(|r| (r.name.clone(), r.column_name.clone()))
        .collect();
    expected.sort();
    let got: Vec<(String, Option<String>)> = gpkg
        .table_extensions(&table)
        .unwrap()
        .into_iter()
        .map(|r| (r.name, r.column_name))
        .collect();
    assert_eq!(got, expected, "{table}");
}

/// The rows that block a write to `table`: unidentified, and naming the
/// table ignoring case or applying to the whole GeoPackage.
fn blockers<'a>(rows: &'a [ExtensionRow], table: &str) -> Vec<&'a ExtensionRow> {
    rows.iter()
        .filter(|r| {
            r.table_name
                .as_deref()
                .is_none_or(|t| t.eq_ignore_ascii_case(table))
                && Extension::from_name(&r.name).support() == ExtensionSupport::Unrecognised
        })
        .collect()
}

/// `blocking_extension` returns an unidentified row covering the table, the
/// first by extension name and column, or none; with
/// `allow_unsupported_extension_writes` it returns none.
///
/// The option is set when the file is opened, so each case writes and
/// reopens a file. That costs far more per case on Windows CI (issue #44),
/// so this property runs 200 cases rather than the suite's 2000.
#[hegel::test(test_cases = 200)]
fn blocking_extension_finds_the_first_unidentified_row(tc: hegel::TestCase) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.gpkg");
    fixture(&path);
    let rows = {
        let gpkg = GeoPackage::open(&path).unwrap();
        let existing = baseline(&gpkg);
        let drawn = draw_rows(&tc, &existing);
        insert(&gpkg, &drawn);
        [existing, drawn].concat()
    };
    let allow = tc.draw(generators::booleans());
    let gpkg = OpenOptions::new()
        .allow_unsupported_extension_writes(allow)
        .open(&path)
        .unwrap();
    let table = draw_table(&tc);
    let expected = if allow {
        None
    } else {
        blockers(&rows, &table)
            .into_iter()
            .map(|r| (r.name.clone(), r.column_name.clone()))
            .min()
    };
    let got = gpkg
        .blocking_extension(&table)
        .unwrap()
        .map(|r| (r.name, r.column_name));
    assert_eq!(got, expected, "{table} allow={allow}");
}

/// A write to a table, and the creation of a new one, fails with
/// `UnsupportedExtension` exactly when an unidentified row covers it.
#[hegel::test]
fn writes_are_refused_exactly_where_an_extension_blocks_them(tc: hegel::TestCase) {
    let gpkg = in_memory();
    let existing = baseline(&gpkg);
    let drawn = draw_rows(&tc, &existing);
    insert(&gpkg, &drawn);
    let rows = [existing, drawn].concat();
    let table = tc.draw(generators::sampled_from(vec![
        "pts", "notes", "tiles", "cov", "map", "fresh", "newmap",
    ]));
    let refused = |result: geopackage::Result<()>| match result {
        Ok(()) => false,
        Err(Error::UnsupportedExtension { .. }) => true,
        Err(other) => panic!("{other:?}"),
    };
    let was_refused = match table {
        "pts" => refused(gpkg.layer("pts").unwrap().writer().map(drop)),
        "notes" => refused(gpkg.attributes("notes").unwrap().writer().map(drop)),
        "tiles" => refused(gpkg.tiles("tiles").unwrap().writer().map(drop)),
        "cov" => refused(gpkg.coverage("cov").unwrap().writer().map(drop)),
        "map" => {
            let relation = gpkg.relations().unwrap().remove(0);
            refused(gpkg.add_mapping(&relation, 1, 1))
        }
        "newmap" => refused(
            gpkg.add_relation(&NewRelation::new(
                "pts",
                "notes",
                RelationName::Media,
                "newmap",
            ))
            .map(drop),
        ),
        _ => refused(
            gpkg.create_attributes_table(
                &TableSchemaBuilder::new("fresh")
                    .column(ColumnSpec::new("body", ColumnType::Text(None))),
            )
            .map(drop),
        ),
    };
    assert_eq!(was_refused, !blockers(&rows, table).is_empty(), "{table}");
}
