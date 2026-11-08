//! RTree spatial index extension (`gpkg_rtree_index`, spec Annex F.3).
//!
//! GeoPackage 1.4 replaced the trigger set. `update1` became `update6` and
//! `update7` (for UPSERT compatibility), and `update3` became `update5`,
//! renamed so that the fixed variant is detectable by name. This module emits
//! the 1.4 set and classifies and repairs older generations.
//!
//! The triggers call `ST_IsEmpty`, `ST_MinX`, `ST_MaxX`, `ST_MinY` and
//! `ST_MaxY`, which are **not** built into SQLite. Every connection that writes
//! to an indexed table must register them. The `geopackage` crate does this on
//! open.

use crate::Error;
use crate::ident::quote;

/// Registered extension name for the RTree spatial index.
pub const EXTENSION_NAME: &str = "gpkg_rtree_index";
/// `gpkg_extensions.definition` value.
pub const EXTENSION_DEFINITION: &str = "http://www.geopackage.org/spec140/#extension_rtree";
/// `gpkg_extensions.scope` value.
pub const EXTENSION_SCOPE: &str = "write-only";

/// Returns the unquoted rtree virtual table name for a table/column pair.
pub fn rtree_table_name(table: &str, column: &str) -> String {
    format!("rtree_{table}_{column}")
}

/// Returns the `CREATE VIRTUAL TABLE` statement, in the exact form checked by
/// the OGC abstract test suite (double-quoted vtab name, verbatim column
/// list).
pub fn create_rtree_table_sql(table: &str, column: &str) -> Result<String, Error> {
    Ok(format!(
        "CREATE VIRTUAL TABLE {} USING rtree(id, minx, maxx, miny, maxy)",
        quote(&rtree_table_name(table, column))?
    ))
}

/// Returns the statement populating a freshly created index from existing
/// rows.
pub fn populate_rtree_sql(table: &str, column: &str, pk: &str) -> Result<String, Error> {
    let (rt, t, c, i) = quoted(table, column, pk)?;
    Ok(format!(
        "INSERT OR REPLACE INTO {rt} SELECT {i}, ST_MinX({c}), ST_MaxX({c}), ST_MinY({c}), ST_MaxY({c}) \
         FROM {t} WHERE {c} NOT NULL AND NOT ST_IsEmpty({c})"
    ))
}

/// Returns the complete GeoPackage 1.4 trigger set (insert, update2, update4,
/// update5, update6, update7, delete), verbatim from Annex F.3 modulo
/// identifier quoting.
pub fn create_triggers_sql(table: &str, column: &str, pk: &str) -> Result<Vec<String>, Error> {
    let (rt, t, c, i) = quoted(table, column, pk)?;
    let name = |suffix: &str| quote(&format!("{}_{suffix}", rtree_table_name(table, column)));
    Ok(vec![
        // Insertion of non-empty geometry
        format!(
            "CREATE TRIGGER {n} AFTER INSERT ON {t} \
             WHEN (NEW.{c} NOT NULL AND NOT ST_IsEmpty(NEW.{c})) \
             BEGIN \
             INSERT OR REPLACE INTO {rt} VALUES (NEW.{i}, \
             ST_MinX(NEW.{c}), ST_MaxX(NEW.{c}), ST_MinY(NEW.{c}), ST_MaxY(NEW.{c})); \
             END",
            n = name("insert")?
        ),
        // Update of geometry column to empty geometry, no row ID change
        format!(
            "CREATE TRIGGER {n} AFTER UPDATE OF {c} ON {t} \
             WHEN OLD.{i} = NEW.{i} AND (NEW.{c} ISNULL OR ST_IsEmpty(NEW.{c})) \
             BEGIN \
             DELETE FROM {rt} WHERE id = OLD.{i}; \
             END",
            n = name("update2")?
        ),
        // Update of any column, row ID change, empty geometry
        format!(
            "CREATE TRIGGER {n} AFTER UPDATE ON {t} \
             WHEN OLD.{i} != NEW.{i} AND (NEW.{c} ISNULL OR ST_IsEmpty(NEW.{c})) \
             BEGIN \
             DELETE FROM {rt} WHERE id IN (OLD.{i}, NEW.{i}); \
             END",
            n = name("update4")?
        ),
        // Update of any column, row ID change, non-empty geometry
        format!(
            "CREATE TRIGGER {n} AFTER UPDATE ON {t} \
             WHEN OLD.{i} != NEW.{i} AND (NEW.{c} NOTNULL AND NOT ST_IsEmpty(NEW.{c})) \
             BEGIN \
             DELETE FROM {rt} WHERE id = OLD.{i}; \
             INSERT OR REPLACE INTO {rt} VALUES (NEW.{i}, \
             ST_MinX(NEW.{c}), ST_MaxX(NEW.{c}), ST_MinY(NEW.{c}), ST_MaxY(NEW.{c})); \
             END",
            n = name("update5")?
        ),
        // Update of a non-empty geometry with another non-empty geometry
        format!(
            "CREATE TRIGGER {n} AFTER UPDATE OF {c} ON {t} \
             WHEN OLD.{i} = NEW.{i} AND (NEW.{c} NOTNULL AND NOT ST_IsEmpty(NEW.{c})) \
             AND (OLD.{c} NOTNULL AND NOT ST_IsEmpty(OLD.{c})) \
             BEGIN \
             UPDATE {rt} SET minx = ST_MinX(NEW.{c}), maxx = ST_MaxX(NEW.{c}), \
             miny = ST_MinY(NEW.{c}), maxy = ST_MaxY(NEW.{c}) WHERE id = NEW.{i}; \
             END",
            n = name("update6")?
        ),
        // Update of a null/empty geometry with a non-empty geometry
        format!(
            "CREATE TRIGGER {n} AFTER UPDATE OF {c} ON {t} \
             WHEN OLD.{i} = NEW.{i} AND (NEW.{c} NOTNULL AND NOT ST_IsEmpty(NEW.{c})) \
             AND (OLD.{c} ISNULL OR ST_IsEmpty(OLD.{c})) \
             BEGIN \
             INSERT INTO {rt} VALUES (NEW.{i}, \
             ST_MinX(NEW.{c}), ST_MaxX(NEW.{c}), ST_MinY(NEW.{c}), ST_MaxY(NEW.{c})); \
             END",
            n = name("update7")?
        ),
        // Row deleted
        format!(
            "CREATE TRIGGER {n} AFTER DELETE ON {t} \
             WHEN OLD.{c} NOT NULL \
             BEGIN \
             DELETE FROM {rt} WHERE id = OLD.{i}; \
             END",
            n = name("delete")?
        ),
    ])
}

/// Returns `DROP TRIGGER` statements for the deprecated pre-1.4 triggers
/// (`update1`, `update3`), for upgrading older GeoPackages.
pub fn drop_legacy_triggers_sql(table: &str, column: &str) -> Result<[String; 2], Error> {
    let base = rtree_table_name(table, column);
    Ok([
        format!(
            "DROP TRIGGER IF EXISTS {}",
            quote(&format!("{base}_update1"))?
        ),
        format!(
            "DROP TRIGGER IF EXISTS {}",
            quote(&format!("{base}_update3"))?
        ),
    ])
}

/// Which generation of rtree triggers is present for a table/column pair.
///
/// The generation says which trigger *names* are present, not that the set is
/// complete: a 1.4 set that has lost its `insert` trigger is still
/// [`TriggerGeneration::V1_4`]. [`missing_triggers`] answers the completeness
/// question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerGeneration {
    /// 1.4 triggers present (any of `update5`/`update6`/`update7`), and no
    /// legacy triggers.
    V1_4,
    /// A pre-1.4 set (`update1` and/or `update3` present, no 1.4 triggers).
    /// The buggy (≤1.2.0) and fixed (1.2.1–1.3.1) `update3` bodies share a
    /// name, so you must inspect the SQL body to tell them apart.
    PreV1_4,
    /// Both generations present. The set is inconsistent and should be
    /// repaired.
    Mixed,
    /// No rtree triggers found.
    None,
}

/// Classifies the trigger generation from the trigger names present for
/// `table`/`column` (e.g. from `sqlite_master`).
pub fn classify_triggers<'a>(
    names: impl IntoIterator<Item = &'a str>,
    table: &str,
    column: &str,
) -> TriggerGeneration {
    let base = rtree_table_name(table, column);
    let (mut legacy, mut v14) = (false, false);
    for n in names {
        let Some(suffix) = n.strip_prefix(&base).and_then(|s| s.strip_prefix('_')) else {
            continue;
        };
        match suffix {
            "update1" | "update3" => legacy = true,
            "update5" | "update6" | "update7" => v14 = true,
            _ => {}
        }
    }
    match (legacy, v14) {
        (true, true) => TriggerGeneration::Mixed,
        (true, false) => TriggerGeneration::PreV1_4,
        (false, true) => TriggerGeneration::V1_4,
        (false, false) => TriggerGeneration::None,
    }
}

/// The seven jobs an RTree trigger set does, each with the trigger names that
/// do it: the 1.4 name first, then the pre-1.4 name that did the same job.
///
/// Pre-1.4 `update1` handled every same-id update to a non-empty geometry,
/// which 1.4 split into `update6` (non-empty to non-empty) and `update7`
/// (empty to non-empty). `update3`, the id-changing update, became `update5`.
/// `insert`, `update2`, `update4` and `delete` kept their names.
const TRIGGER_ROLES: [&[&str]; 7] = [
    &["insert"],
    &["update2"],
    &["update4"],
    &["update5", "update3"],
    &["update6", "update1"],
    &["update7", "update1"],
    &["delete"],
];

/// Returns the 1.4 suffixes (`insert`, `update2`, ... `delete`) of the
/// triggers whose job no trigger in `names` does, for `table`/`column`. An
/// empty result means the set is complete.
///
/// A complete set keeps the index in step with every write; an incomplete one
/// lets some writes past, so the index silently drifts from the table. The
/// check is by role rather than exact name, so a complete pre-1.4 set
/// (`insert`, `update1` to `update4`, `delete`) and a complete mixed set are
/// both complete. Like [`classify_triggers`], it reads names only: a trigger
/// with the right name and the wrong body passes, and
/// `Layer::audit_spatial_index` in the `geopackage` crate is what finds the
/// drift such a trigger causes.
pub fn missing_triggers<'a>(
    names: impl IntoIterator<Item = &'a str>,
    table: &str,
    column: &str,
) -> Vec<&'static str> {
    let base = rtree_table_name(table, column);
    let present: Vec<&str> = names
        .into_iter()
        .filter_map(|n| n.strip_prefix(&base).and_then(|s| s.strip_prefix('_')))
        .collect();
    TRIGGER_ROLES
        .iter()
        .filter(|role| !role.iter().any(|name| present.contains(name)))
        .filter_map(|role| role.first().copied())
        .collect()
}

fn quoted(table: &str, column: &str, pk: &str) -> Result<(String, String, String, String), Error> {
    Ok((
        quote(&rtree_table_name(table, column))?,
        quote(table)?,
        quote(column)?,
        quote(pk)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vtab_sql_matches_ats_form() {
        assert_eq!(
            create_rtree_table_sql("roads", "geom").unwrap(),
            "CREATE VIRTUAL TABLE \"rtree_roads_geom\" USING rtree(id, minx, maxx, miny, maxy)"
        );
    }

    #[test]
    fn v1_4_set_has_no_legacy_triggers() {
        let sql = create_triggers_sql("roads", "geom", "fid").unwrap();
        assert_eq!(sql.len(), 7);
        let all = sql.join("\n");
        for required in [
            "_insert", "_update2", "_update4", "_update5", "_update6", "_update7", "_delete",
        ] {
            assert!(all.contains(required), "missing {required}");
        }
        assert!(!all.contains("_update1\""));
        assert!(!all.contains("_update3\""));
    }

    #[test]
    fn classification() {
        let t = |names: &[&str]| classify_triggers(names.iter().copied(), "roads", "geom");
        assert_eq!(
            t(&[
                "rtree_roads_geom_insert",
                "rtree_roads_geom_update5",
                "rtree_roads_geom_update6"
            ]),
            TriggerGeneration::V1_4
        );
        assert_eq!(
            t(&[
                "rtree_roads_geom_insert",
                "rtree_roads_geom_update1",
                "rtree_roads_geom_update3"
            ]),
            TriggerGeneration::PreV1_4
        );
        assert_eq!(
            t(&["rtree_roads_geom_update1", "rtree_roads_geom_update6"]),
            TriggerGeneration::Mixed
        );
        assert_eq!(t(&["rtree_other_geom_update6"]), TriggerGeneration::None);
    }

    const V1_4: [&str; 7] = [
        "insert", "update2", "update4", "update5", "update6", "update7", "delete",
    ];

    fn names(suffixes: &[&str]) -> Vec<String> {
        suffixes
            .iter()
            .map(|s| format!("rtree_roads_geom_{s}"))
            .collect()
    }

    fn missing(suffixes: &[&str]) -> Vec<&'static str> {
        let names = names(suffixes);
        missing_triggers(names.iter().map(String::as_str), "roads", "geom")
    }

    /// Every subset of the 1.4 set: only the whole set is complete, and each
    /// subset is missing exactly the triggers it leaves out.
    #[test]
    fn every_partial_1_4_set_is_incomplete() {
        for mask in 0u32..(1 << V1_4.len()) {
            let (present, absent): (Vec<_>, Vec<_>) = V1_4
                .iter()
                .enumerate()
                .partition(|(i, _)| mask & (1 << i) != 0);
            let present: Vec<&str> = present.into_iter().map(|(_, s)| *s).collect();
            let absent: Vec<&str> = absent.into_iter().map(|(_, s)| *s).collect();
            assert_eq!(missing(&present), absent, "present: {present:?}");
        }
    }

    #[test]
    fn complete_legacy_and_mixed_sets_are_complete() {
        let pre_1_4 = [
            "insert", "update1", "update2", "update3", "update4", "delete",
        ];
        assert_eq!(missing(&pre_1_4), Vec::<&str>::new());
        assert_eq!(
            classify_triggers(names(&pre_1_4).iter().map(String::as_str), "roads", "geom"),
            TriggerGeneration::PreV1_4
        );
        // A 1.4 set with a stray legacy trigger, and a half-upgraded set.
        let mut with_stray = V1_4.to_vec();
        with_stray.push("update1");
        assert_eq!(missing(&with_stray), Vec::<&str>::new());
        let half = [
            "insert", "update2", "update3", "update4", "update6", "update7", "delete",
        ];
        assert_eq!(missing(&half), Vec::<&str>::new());
    }

    #[test]
    fn incomplete_legacy_sets_are_incomplete() {
        // No update1: same-id updates of a non-empty geometry go unindexed.
        let no_update1 = ["insert", "update2", "update3", "update4", "delete"];
        assert_eq!(missing(&no_update1), ["update6", "update7"]);
        // update6 alone does not stand in for update1's other job.
        let no_update7 = [
            "insert", "update2", "update3", "update4", "update6", "delete",
        ];
        assert_eq!(missing(&no_update7), ["update7"]);
    }

    #[test]
    fn other_tables_and_unknown_suffixes_do_not_count() {
        let mut other: Vec<String> = V1_4
            .iter()
            .map(|s| format!("rtree_other_geom_{s}"))
            .collect();
        other.push("rtree_roads_geom_foo".to_owned());
        other.push("rtree_roads_geometry_insert".to_owned());
        assert_eq!(
            missing_triggers(other.iter().map(String::as_str), "roads", "geom"),
            V1_4.to_vec()
        );
    }
}
