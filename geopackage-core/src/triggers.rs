//! RTree spatial index extension (`gpkg_rtree_index`, spec Annex F.3).
//!
//! GeoPackage 1.4 replaced the trigger set: `update1` → `update6` + `update7`
//! (UPSERT compatibility) and `update3` → `update5` (rename, so the fixed
//! variant is detectable by name). This module emits the 1.4 set and
//! classifies/repairs older generations.
//!
//! The triggers call `ST_IsEmpty`, `ST_MinX`, `ST_MaxX`, `ST_MinY`, `ST_MaxY`,
//! which are **not** built into SQLite — every connection that writes to an
//! indexed table must register them (the `geopackage` crate does this on open).

use crate::Error;
use crate::ident::quote;

/// Registered extension name for the RTree spatial index.
pub const EXTENSION_NAME: &str = "gpkg_rtree_index";
/// `gpkg_extensions.definition` value.
pub const EXTENSION_DEFINITION: &str = "http://www.geopackage.org/spec140/#extension_rtree";
/// `gpkg_extensions.scope` value.
pub const EXTENSION_SCOPE: &str = "write-only";

/// The unquoted rtree virtual table name for a table/column pair.
pub fn rtree_table_name(table: &str, column: &str) -> String {
    format!("rtree_{table}_{column}")
}

/// `CREATE VIRTUAL TABLE` statement, in the exact form checked by the OGC
/// abstract test suite (double-quoted vtab name, verbatim column list).
pub fn create_rtree_table_sql(table: &str, column: &str) -> Result<String, Error> {
    Ok(format!(
        "CREATE VIRTUAL TABLE {} USING rtree(id, minx, maxx, miny, maxy)",
        quote(&rtree_table_name(table, column))?
    ))
}

/// Statement populating a freshly created index from existing rows.
pub fn populate_rtree_sql(table: &str, column: &str, pk: &str) -> Result<String, Error> {
    let (rt, t, c, i) = quoted(table, column, pk)?;
    Ok(format!(
        "INSERT OR REPLACE INTO {rt} SELECT {i}, ST_MinX({c}), ST_MaxX({c}), ST_MinY({c}), ST_MaxY({c}) \
         FROM {t} WHERE {c} NOT NULL AND NOT ST_IsEmpty({c})"
    ))
}

/// The complete GeoPackage 1.4 trigger set (insert, update2, update4,
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

/// `DROP TRIGGER` statements for the deprecated pre-1.4 triggers
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

/// Which generation of rtree triggers a table/column pair carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerGeneration {
    /// The 1.4 set (`update5`/`update6`/`update7` present, no legacy triggers).
    V1_4,
    /// A pre-1.4 set (`update1` and/or `update3` present, no 1.4 triggers).
    /// Note: the buggy (≤1.2.0) and fixed (1.2.1–1.3.1) `update3` bodies
    /// share a name; distinguishing them requires inspecting the SQL body.
    PreV1_4,
    /// Both generations present — inconsistent; should be repaired.
    Mixed,
    /// No rtree triggers found.
    None,
}

/// Classify trigger generation from the trigger names present for
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
}
