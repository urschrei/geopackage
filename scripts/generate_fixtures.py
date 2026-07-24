#!/usr/bin/env python3
"""Generate the committed GeoPackage fixture corpus and its expected-output snapshots.

The corpus lives in ``geopackage/tests/fixtures``: a set of small ``.gpkg`` files
plus, next to each, a ``<name>.expected.json`` snapshot derived from GDAL's own
read of that file (``ogrinfo -json -features``). The Rust corpus tests
(``geopackage/tests/corpus.rs``) open every fixture, iterate every feature and
check our read against the snapshot, so the snapshot is the cross-implementation
oracle: what GDAL saw when it read the same bytes.

Only the Python standard library plus the external command-line tools ``ogr2ogr``
and ``ogrinfo`` (GDAL) are used; there is no project virtualenv. QGIS's
``qgis_process`` is used for one fixture when available (``QGIS_PROCESS`` env
var, PATH, or a macOS ``/Applications/QGIS*.app`` bundle) and skipped with a
warning otherwise. ``sqlite3`` is
used through Python's bundled module, both to build the raw fixtures that GDAL
cannot express directly (exact ``TINYINT``/``DOUBLE`` column types, legacy
``application_id``, a case-mismatched catalogue) and to shrink each container to
a 512-byte page size (GDAL's default 4 KiB pages waste space on these tiny
tables).

Determinism: every input geometry and attribute value is a fixed literal; the
``gpkg_contents.last_change`` and ``gpkg_metadata_reference.timestamp`` values
are pinned (GDAL writes "now" into both); and the SQLite header's change
counter is forced to a constant after the final VACUUM. Regenerating with the
same GDAL/QGIS therefore reproduces byte-identical containers and snapshots. Fields that vary with the
GDAL/PROJ build -- the ``coordinateSystem`` WKT/PROJJSON block, layer ``extent``,
driver name strings, dataset ``metadata`` -- are deliberately *not* copied into
the snapshot; only the GDAL version string is recorded, as provenance the Rust
tests ignore.

GDAL representation quirks the snapshot captures faithfully (the Rust side
documents each normalisation at the comparison site):

* Booleans (GPKG ``BOOLEAN``) arrive as JSON ``true``/``false``.
* Dates print as ``YYYY/MM/DD`` and datetimes as ``YYYY/MM/DD HH:MM:SS[.fff]+00``.
* Non-NULL binary values are omitted from ``-json`` entirely; their bytes are
  recovered through ``ogrinfo -json -sql "SELECT hex(col) ..."`` and folded back
  into the snapshot as a hex string (a NULL blob stays JSON ``null``).

Run from anywhere; paths are resolved relative to the repository root. Requires
GDAL on ``PATH`` (the committed fixtures let the tests run without it).
"""

from __future__ import annotations

import glob
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
# Output directory. Overridable so the live-regeneration corpus test
# (geopackage/tests/corpus.rs) can regenerate into a scratch directory and diff,
# without touching the committed fixtures.
FIXTURES = Path(
    os.environ.get(
        "GEOPACKAGE_FIXTURES_DIR",
        REPO_ROOT / "geopackage" / "tests" / "fixtures",
    )
)

# SQLite header pragmas (see geopackage-core/src/version.rs).
APPLICATION_ID_GPKG = 0x4750_4B47
APPLICATION_ID_GP10 = 0x4750_3130

# Pinned so regeneration does not depend on the wall clock.
FIXED_LAST_CHANGE = "2020-01-01T00:00:00.000Z"

# Smallest SQLite page size; the fixtures are a handful of near-empty tables, so
# 512-byte pages beat GDAL's 4 KiB default by roughly 8x.
PAGE_SIZE = 512

# Size budgets. A GeoPackage with a spatial index carries seven verbose RTree
# triggers whose SQL text dominates the schema page, so a few tens of KiB is the
# practical floor for an indexed feature file; these budgets guard against
# accidental bloat, not against the inherent schema cost.
PER_FILE_BUDGET = 64 * 1024
TOTAL_BUDGET = 256 * 1024


def run(args: list[str], **kwargs) -> subprocess.CompletedProcess:
    """Run a command, capturing output and raising on a non-zero exit."""
    return subprocess.run(args, check=True, capture_output=True, text=True, **kwargs)


def require_tools() -> None:
    for tool in ("ogr2ogr", "ogrinfo"):
        if shutil.which(tool) is None:
            sys.exit(
                f"error: {tool} not found on PATH; GDAL is required to "
                "regenerate the fixtures (committed fixtures let the tests "
                "run without it)"
            )


def gdal_version() -> str:
    return run(["ogrinfo", "--version"]).stdout.strip()


def find_qgis_process() -> str | None:
    """Locate ``qgis_process`` for the QGIS-written fixture.

    Order: the ``QGIS_PROCESS`` environment variable, then PATH, then macOS
    application bundles (``/Applications/QGIS*.app``). Returns ``None`` when
    QGIS is not installed; the QGIS fixture is then skipped with a warning
    rather than failing the whole generation run.
    """
    env = os.environ.get("QGIS_PROCESS")
    if env:
        return env if Path(env).exists() else None
    on_path = shutil.which("qgis_process")
    if on_path:
        return on_path
    bundles = sorted(glob.glob("/Applications/QGIS*.app/Contents/MacOS/qgis_process"))
    return bundles[-1] if bundles else None


def qgis_version(qgis_process: str) -> str:
    out = run([qgis_process, "--version"]).stdout
    for line in out.splitlines():
        if line.startswith("QGIS "):
            return line.strip()
    return "QGIS (unknown version)"


def ogr2ogr(*args: str) -> None:
    run(["ogr2ogr", *args])


# --- container helpers -------------------------------------------------------


def connect(path: Path) -> sqlite3.Connection:
    con = sqlite3.connect(path)
    con.execute("PRAGMA foreign_keys = OFF")
    return con


def pin_last_change(path: Path) -> None:
    """Pin every wall-clock timestamp GDAL writes into the container.

    ``gpkg_contents.last_change`` always exists; GDAL also stamps "now" into
    ``gpkg_metadata_reference.timestamp`` when it creates metadata tables,
    which would otherwise make regenerated containers differ byte-wise from
    run to run.
    """
    con = connect(path)
    try:
        con.execute("UPDATE gpkg_contents SET last_change = ?", (FIXED_LAST_CHANGE,))
        has_metadata_reference = con.execute(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' "
            "AND name = 'gpkg_metadata_reference'"
        ).fetchone()
        if has_metadata_reference:
            con.execute(
                "UPDATE gpkg_metadata_reference SET timestamp = ?",
                (FIXED_LAST_CHANGE,),
            )
        con.commit()
    finally:
        con.close()


def shrink(path: Path) -> None:
    """Rewrite the container at a 512-byte page size and drop any WAL sidecar."""
    con = connect(path)
    try:
        con.execute("PRAGMA journal_mode = DELETE")
        con.execute(f"PRAGMA page_size = {PAGE_SIZE}")
        con.execute("VACUUM")
        con.commit()
    finally:
        con.close()
    for suffix in ("-wal", "-shm", "-journal"):
        sidecar = path.with_name(path.name + suffix)
        if sidecar.exists():
            sidecar.unlink()


def pin_change_counter(path: Path) -> None:
    """Pin the SQLite header's file change counter.

    Bytes 24-27 (change counter) and 92-95 (its version-valid-for copy) hold
    the cumulative count of write transactions the file has seen, which varies
    with GDAL's internal statement batching from run to run. With every
    content timestamp already pinned, these two header words are the last
    byte-level difference between regenerations, so they are forced to a
    constant. Safe for a fully checkpointed, rollback-journal database.
    """
    counter = (1).to_bytes(4, "big")
    with path.open("r+b") as fh:
        fh.seek(24)
        fh.write(counter)
        fh.seek(92)
        fh.write(counter)


def finalise(path: Path) -> None:
    pin_last_change(path)
    shrink(path)
    pin_change_counter(path)


# --- fixture builders --------------------------------------------------------

# Fixed input geometries and attributes. Coordinates are chosen to be exactly
# representable in f64 (and, for the FLOAT column, in f32) so GDAL's JSON printer
# reproduces them without rounding.

POINTS_GEOJSON = {
    "type": "FeatureCollection",
    "features": [
        {
            "type": "Feature",
            "properties": {"name": "alpha", "pop": 100},
            "geometry": {"type": "Point", "coordinates": [1.0, 2.0]},
        },
        {
            "type": "Feature",
            # Non-ASCII text: e-acute plus a snowman, to exercise UTF-8.
            "properties": {"name": "béta ☃", "pop": 200},
            "geometry": {"type": "Point", "coordinates": [3.5, -4.25]},
        },
        {
            "type": "Feature",
            "properties": {"name": "gamma", "pop": 300},
            "geometry": {"type": "Point", "coordinates": [10.0, 20.0]},
        },
    ],
}

LINES_GEOJSON = {
    "type": "FeatureCollection",
    "features": [
        {
            "type": "Feature",
            "properties": {"rd": "main"},
            "geometry": {
                "type": "LineString",
                "coordinates": [[0.0, 0.0], [10.0, -3.0], [4.0, 8.0]],
            },
        },
        {
            "type": "Feature",
            "properties": {"rd": "side"},
            "geometry": {
                "type": "LineString",
                "coordinates": [[-2.0, 7.0], [9.0, -1.0]],
            },
        },
    ],
}

POLYGONS_GEOJSON = {
    "type": "FeatureCollection",
    "features": [
        {
            "type": "Feature",
            "properties": {"nm": "square"},
            "geometry": {
                "type": "Polygon",
                "coordinates": [
                    [[0.0, 0.0], [6.0, 0.0], [6.0, 5.0], [0.0, 5.0], [0.0, 0.0]]
                ],
            },
        },
        {
            "type": "Feature",
            "properties": {"nm": "triangle"},
            "geometry": {
                "type": "Polygon",
                "coordinates": [[[1.0, 1.0], [4.0, 1.0], [4.0, 3.0], [1.0, 1.0]]],
            },
        },
    ],
}

POINTS3D_GEOJSON = {
    "type": "FeatureCollection",
    "features": [
        {
            "type": "Feature",
            "properties": {"h": "low"},
            "geometry": {"type": "Point", "coordinates": [1.0, 2.0, 3.0]},
        },
        {
            "type": "Feature",
            "properties": {"h": "high"},
            "geometry": {"type": "Point", "coordinates": [4.0, 5.0, 6.0]},
        },
    ],
}

# An empty geometry alongside a normal one, in a single-geometry-type layer. CSV
# WKT is the route: GeoJSON cannot spell "LINESTRING EMPTY". GDAL writes it with
# the GPB empty flag set and a zero-element WKB body.
EMPTYLINE_CSV = 'id,geom\n1,"LINESTRING (0 0, 3 4)"\n2,"LINESTRING EMPTY"\n'
EMPTYLINE_CSVT = "Integer,WKT\n"


def write(path: Path, text: str) -> None:
    path.write_text(text, encoding="utf-8")


def write_geojson(path: Path, obj: dict) -> None:
    write(path, json.dumps(obj))


def build_multilayer_1_4(tmp: Path) -> Path:
    """Five GDAL-written layers in one 1.4 container.

    ``points`` are indexed (GDAL's default) and carry no GPB envelope (GDAL's
    default for points -- exercises our traversal fallback); ``lines`` are
    non-indexed and ``polygons`` indexed, both with GPB envelopes; ``points3d``
    holds XYZ points; ``emptyline`` holds one normal and one empty linestring.
    """
    out = FIXTURES / "gdal_multilayer_1_4.gpkg"
    out.unlink(missing_ok=True)

    points = tmp / "points.geojson"
    write_geojson(points, POINTS_GEOJSON)
    ogr2ogr(
        "-f",
        "GPKG",
        "-nln",
        "points",
        "-a_srs",
        "EPSG:4326",
        "-dsco",
        "VERSION=1.4",
        "-dsco",
        "ADD_GPKG_OGR_CONTENTS=NO",
        "-dsco",
        "METADATA_TABLES=NO",
        str(out),
        str(points),
    )

    lines = tmp / "lines.geojson"
    write_geojson(lines, LINES_GEOJSON)
    ogr2ogr(
        "-f",
        "GPKG",
        "-update",
        "-append",
        "-nln",
        "lines",
        "-a_srs",
        "EPSG:4326",
        "-lco",
        "SPATIAL_INDEX=NO",
        str(out),
        str(lines),
    )

    polygons = tmp / "polygons.geojson"
    write_geojson(polygons, POLYGONS_GEOJSON)
    ogr2ogr(
        "-f",
        "GPKG",
        "-update",
        "-append",
        "-nln",
        "polygons",
        "-a_srs",
        "EPSG:4326",
        str(out),
        str(polygons),
    )

    points3d = tmp / "points3d.geojson"
    write_geojson(points3d, POINTS3D_GEOJSON)
    ogr2ogr(
        "-f",
        "GPKG",
        "-update",
        "-append",
        "-nln",
        "points3d",
        "-a_srs",
        "EPSG:4326",
        "-lco",
        "SPATIAL_INDEX=NO",
        str(out),
        str(points3d),
    )

    emptyline = tmp / "emptyline.csv"
    write(emptyline, EMPTYLINE_CSV)
    write(tmp / "emptyline.csvt", EMPTYLINE_CSVT)
    ogr2ogr(
        "-f",
        "GPKG",
        "-update",
        "-append",
        "-nln",
        "emptyline",
        "-a_srs",
        "EPSG:4326",
        "-lco",
        "SPATIAL_INDEX=NO",
        "-oo",
        "GEOM_POSSIBLE_NAMES=geom",
        "-oo",
        "KEEP_GEOM_COLUMNS=NO",
        str(out),
        str(emptyline),
    )

    finalise(out)
    return out


def build_points_1_2(tmp: Path) -> Path:
    """The same points, written as a GeoPackage 1.2 container (user_version 10200)."""
    out = FIXTURES / "gdal_points_1_2.gpkg"
    out.unlink(missing_ok=True)
    points = tmp / "points12.geojson"
    write_geojson(points, POINTS_GEOJSON)
    ogr2ogr(
        "-f",
        "GPKG",
        "-nln",
        "points",
        "-a_srs",
        "EPSG:4326",
        "-dsco",
        "VERSION=1.2",
        "-dsco",
        "ADD_GPKG_OGR_CONTENTS=NO",
        "-dsco",
        "METADATA_TABLES=NO",
        str(out),
        str(points),
    )
    finalise(out)
    return out


def build_attributes_spread(_tmp: Path) -> Path:
    """A non-spatial (``attributes``) table with the full column-type spread.

    Built with raw SQL: GDAL cannot emit ``TINYINT`` or ``DOUBLE`` column types
    (it writes ``MEDIUMINT``/``REAL``), so to exercise every declared type the
    table is created by hand. GDAL still reads it -- the empty
    ``gpkg_geometry_columns`` table is required for GDAL to enumerate the
    aspatial layer. Row 1 holds representative non-NULL values (FLOAT values are
    f32-exact so GDAL's Float32 read is lossless), row 2 the opposite signs, row
    3 is entirely NULL.
    """
    out = FIXTURES / "attributes_spread.gpkg"
    out.unlink(missing_ok=True)
    con = sqlite3.connect(out)
    try:
        con.execute(f"PRAGMA application_id = {APPLICATION_ID_GPKG}")
        con.execute("PRAGMA user_version = 10400")
        con.executescript(
            """
            CREATE TABLE gpkg_spatial_ref_sys (
              srs_name TEXT NOT NULL,
              srs_id INTEGER NOT NULL PRIMARY KEY,
              organization TEXT NOT NULL,
              organization_coordsys_id INTEGER NOT NULL,
              definition TEXT NOT NULL,
              description TEXT);
            CREATE TABLE gpkg_contents (
              table_name TEXT NOT NULL PRIMARY KEY,
              data_type TEXT NOT NULL,
              identifier TEXT UNIQUE,
              description TEXT DEFAULT '',
              last_change DATETIME NOT NULL
                DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
              min_x DOUBLE, min_y DOUBLE, max_x DOUBLE, max_y DOUBLE,
              srs_id INTEGER,
              CONSTRAINT fk_gc_r_srs_id FOREIGN KEY (srs_id)
                REFERENCES gpkg_spatial_ref_sys(srs_id));
            CREATE TABLE gpkg_geometry_columns (
              table_name TEXT NOT NULL,
              column_name TEXT NOT NULL,
              geometry_type_name TEXT NOT NULL,
              srs_id INTEGER NOT NULL,
              z TINYINT NOT NULL,
              m TINYINT NOT NULL,
              CONSTRAINT pk_geom_cols PRIMARY KEY (table_name, column_name));
            CREATE TABLE spread (
              fid INTEGER PRIMARY KEY,
              b BOOLEAN, ti TINYINT, si SMALLINT, mi MEDIUMINT, i INTEGER,
              fl FLOAT, db DOUBLE, txt TEXT, bl BLOB, dt DATE, dtm DATETIME);
            INSERT INTO gpkg_spatial_ref_sys VALUES
              ('Undefined cartesian SRS', -1, 'NONE', -1, 'undefined', NULL),
              ('Undefined geographic SRS', 0, 'NONE', 0, 'undefined', NULL),
              ('WGS 84 geodetic', 4326, 'EPSG', 4326,
               'GEOGCS["WGS 84"]', NULL);
            """
        )
        con.execute(
            "INSERT INTO gpkg_contents "
            "(table_name, data_type, identifier, last_change, srs_id) "
            "VALUES ('spread', 'attributes', 'spread', ?, 0)",
            (FIXED_LAST_CHANGE,),
        )
        con.execute(
            "INSERT INTO spread VALUES (1,1,7,300,70000,5000000000,1.5,2.5,?,?,?,?)",
            ("café ☃", b"\xde\xad\xbe\xef", "2026-07-24", "2026-07-24T12:00:00.000Z"),
        )
        con.execute(
            "INSERT INTO spread VALUES (2,0,-8,-300,-1,-2,-0.5,-0.25,?,?,?,?)",
            ("plain ascii", b"\x00\x01\x02", "2000-02-29", "1999-12-31T23:59:59.999Z"),
        )
        con.execute(
            "INSERT INTO spread VALUES "
            "(3,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL)"
        )
        con.commit()
    finally:
        con.close()
    shrink(out)
    return out


def build_legacy_gp10(tmp: Path) -> Path:
    """A valid 1.4 file with its application_id flipped to the legacy "GP10".

    Only the four header bytes change; the schema and features are untouched, so
    the snapshot (taken after the flip) still describes them. Exercises
    ``open_lenient``'s ``LegacyApplicationId`` warning.
    """
    out = FIXTURES / "legacy_gp10.gpkg"
    out.unlink(missing_ok=True)
    points = tmp / "gp10.geojson"
    write_geojson(points, POINTS_GEOJSON)
    ogr2ogr(
        "-f",
        "GPKG",
        "-nln",
        "points",
        "-a_srs",
        "EPSG:4326",
        "-dsco",
        "VERSION=1.4",
        "-dsco",
        "ADD_GPKG_OGR_CONTENTS=NO",
        "-dsco",
        "METADATA_TABLES=NO",
        str(out),
        str(points),
    )
    finalise(out)
    con = connect(out)
    try:
        con.execute(f"PRAGMA application_id = {APPLICATION_ID_GP10}")
        con.commit()
    finally:
        con.close()
    return out


def build_case_mismatch(tmp: Path) -> Path:
    """Physical table ``roads`` but a catalogue that spells it ``Roads``.

    SQLite resolves the table case-insensitively, but a string join between the
    catalogue tables would not. Exercises ``open_lenient``'s
    ``TableNameCaseMismatch`` warning and case-insensitive layer enumeration.
    """
    out = FIXTURES / "case_mismatch.gpkg"
    out.unlink(missing_ok=True)
    lines = tmp / "case.geojson"
    write_geojson(lines, LINES_GEOJSON)
    ogr2ogr(
        "-f",
        "GPKG",
        "-nln",
        "roads",
        "-a_srs",
        "EPSG:4326",
        "-dsco",
        "VERSION=1.4",
        "-dsco",
        "ADD_GPKG_OGR_CONTENTS=NO",
        "-dsco",
        "METADATA_TABLES=NO",
        str(out),
        str(lines),
    )
    con = connect(out)
    try:
        con.execute(
            "UPDATE gpkg_contents SET table_name = 'Roads' WHERE table_name = 'roads'"
        )
        con.execute(
            "UPDATE gpkg_geometry_columns SET table_name = 'Roads' "
            "WHERE table_name = 'roads'"
        )
        con.commit()
    finally:
        con.close()
    finalise(out)
    return out


# --- snapshot extraction -----------------------------------------------------


QGIS_LINES_GEOJSON = {
    "type": "FeatureCollection",
    "features": [
        {
            "type": "Feature",
            "properties": {"name": "High Street", "lanes": 2, "length_m": 431.25},
            "geometry": {
                "type": "LineString",
                "coordinates": [[-6.26, 53.34], [-6.25, 53.345], [-6.245, 53.35]],
            },
        },
        {
            "type": "Feature",
            "properties": {"name": "Quay", "lanes": 1, "length_m": 88.5},
            "geometry": {
                "type": "LineString",
                "coordinates": [[-6.28, 53.346], [-6.27, 53.347]],
            },
        },
    ],
}


def build_qgis_lines(tmp: Path, qgis_process: str) -> Path:
    """A GeoPackage written by QGIS itself (``native:savefeatures``).

    The other fixtures are ogr2ogr-written; this one exercises a second
    producer. QGIS drives GDAL's GPKG driver through its own vector-file-writer
    defaults, so the container it emits (fid handling, index creation) is
    QGIS's, not ogr2ogr's.
    """
    out = FIXTURES / "qgis_lines.gpkg"
    out.unlink(missing_ok=True)
    src = tmp / "qgis_lines.geojson"
    write_geojson(src, QGIS_LINES_GEOJSON)
    run(
        [
            qgis_process,
            "run",
            "native:savefeatures",
            "--",
            f"INPUT={src}",
            f"OUTPUT={out}",
            "LAYER_NAME=qgis_lines",
        ]
    )
    finalise(out)
    return out


def sqlite_query(path: Path, sql: str, params: tuple = ()) -> list[tuple]:
    con = sqlite3.connect(path)
    try:
        return list(con.execute(sql, params))
    finally:
        con.close()


def physical_table(path: Path, layer_name: str) -> str:
    """The physical SQLite table backing a layer (differs in case for the
    case-mismatch fixture)."""
    rows = sqlite_query(
        path,
        "SELECT name FROM sqlite_master WHERE type = 'table' "
        "AND name = ? COLLATE NOCASE",
        (layer_name,),
    )
    return rows[0][0] if rows else layer_name


def geometry_column(path: Path, table: str) -> str | None:
    rows = sqlite_query(
        path,
        "SELECT column_name FROM gpkg_geometry_columns "
        "WHERE table_name = ? COLLATE NOCASE",
        (table,),
    )
    return rows[0][0] if rows else None


def has_rtree(path: Path, table: str, geom_col: str) -> bool:
    rows = sqlite_query(
        path,
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?",
        (f"rtree_{table}_{geom_col}",),
    )
    return bool(rows)


def blob_hex_by_order(path: Path, table: str, column: str, fid_col: str) -> list:
    """GDAL's read of a binary column, as hex strings ordered by fid.

    ``-json`` omits non-NULL binary values, so recover them through GDAL's SQL
    ``hex()``. A NULL blob comes back as an empty string, normalised to ``None``.
    """
    order = fid_col if fid_col else "rowid"
    sql = f'SELECT hex("{column}") AS h FROM "{table}" ORDER BY {order}'
    doc = json.loads(
        run(["ogrinfo", "-json", "-features", "-sql", sql, str(path)]).stdout
    )
    out = []
    for feature in doc["layers"][0]["features"]:
        h = feature["properties"].get("h")
        out.append(h if h else None)
    return out


def layer_snapshot(path: Path, name: str) -> dict:
    doc = json.loads(run(["ogrinfo", "-json", "-features", str(path), name]).stdout)
    layer = doc["layers"][0]
    fid_col = layer.get("fidColumnName")
    geom_fields = layer.get("geometryFields") or []
    geometry_type = geom_fields[0]["type"] if geom_fields else None

    fields = [
        {
            "name": f["name"],
            "type": f["type"],
            "subtype": f.get("subType"),
        }
        for f in layer.get("fields", [])
    ]

    table = physical_table(path, name)
    geom_col = geometry_column(path, table)
    spatially_indexed = bool(geom_col) and has_rtree(path, table, geom_col)

    features = sorted(layer.get("features", []), key=lambda f: f["fid"])

    # Fold recovered blob bytes back into each feature's properties.
    for field in fields:
        if field["type"] == "Binary":
            hexes = blob_hex_by_order(path, table, field["name"], fid_col)
            for feature, h in zip(features, hexes):
                feature["properties"][field["name"]] = h

    out_features = [
        {
            "fid": f["fid"],
            "geometry": f.get("geometry"),
            "properties": f.get("properties", {}),
        }
        for f in features
    ]

    return {
        "name": name,
        "fid_column": fid_col,
        "geometry_type": geometry_type,
        "feature_count": layer.get("featureCount"),
        "spatially_indexed": spatially_indexed,
        "fields": fields,
        "features": out_features,
    }


def layer_names(path: Path) -> list[str]:
    doc = json.loads(run(["ogrinfo", "-json", str(path)]).stdout)
    return [layer["name"] for layer in doc["layers"]]


def write_snapshot(
    path: Path,
    *,
    open_mode: str,
    expect_warnings: list[str],
    version: str,
    writer: str | None = None,
) -> None:
    # Read the header pragmas directly.
    con = sqlite3.connect(path)
    try:
        app_id = con.execute("PRAGMA application_id").fetchone()[0]
        user_version = con.execute("PRAGMA user_version").fetchone()[0]
    finally:
        con.close()

    snapshot = {
        "_provenance": {
            "generator": "scripts/generate_fixtures.py",
            "gdal_version": version,
            "writer": writer or "ogr2ogr / raw sqlite3",
            "note": (
                "Derived from ogrinfo -json -features. GDAL/PROJ-version-"
                "dependent fields (coordinateSystem, extent, driver names, "
                "metadata) are not recorded; normalisations are documented in "
                "geopackage/tests/corpus.rs."
            ),
        },
        "fixture": path.name,
        "application_id": app_id,
        "user_version": user_version,
        "open": open_mode,
        "expect_warnings": expect_warnings,
        "layers": [layer_snapshot(path, name) for name in layer_names(path)],
    }

    dest = FIXTURES / (path.stem + ".expected.json")
    with dest.open("w", encoding="utf-8") as fh:
        json.dump(snapshot, fh, indent=2, ensure_ascii=False)
        fh.write("\n")


# --- driver ------------------------------------------------------------------


def main() -> None:
    require_tools()
    version = gdal_version()
    FIXTURES.mkdir(parents=True, exist_ok=True)

    with tempfile.TemporaryDirectory() as tmpdir:
        tmp = Path(tmpdir)
        plan = [
            (build_multilayer_1_4(tmp), "strict", [], None),
            (build_points_1_2(tmp), "strict", [], None),
            (build_attributes_spread(tmp), "strict", [], None),
            (build_legacy_gp10(tmp), "lenient", ["LegacyApplicationId"], None),
            (build_case_mismatch(tmp), "lenient", ["TableNameCaseMismatch"], None),
        ]
        qgis = find_qgis_process()
        if qgis:
            plan.append((build_qgis_lines(tmp, qgis), "strict", [], qgis_version(qgis)))
        else:
            print(
                "warning: qgis_process not found (set QGIS_PROCESS to override); "
                "skipping the QGIS-written fixture qgis_lines.gpkg",
                file=sys.stderr,
            )

    total = 0
    for path, open_mode, expect_warnings, writer in plan:
        write_snapshot(
            path,
            open_mode=open_mode,
            expect_warnings=expect_warnings,
            version=version,
            writer=writer,
        )
        size = path.stat().st_size
        total += size
        if size > PER_FILE_BUDGET:
            sys.exit(
                f"error: {path.name} is {size} bytes, over the "
                f"{PER_FILE_BUDGET}-byte per-file budget"
            )
        print(f"  {path.name:28} {size:>7} bytes")

    print(f"  {'total':28} {total:>7} bytes")
    if total > TOTAL_BUDGET:
        sys.exit(
            f"error: fixtures total {total} bytes, over the {TOTAL_BUDGET}-byte budget"
        )
    print(f"Generated {len(plan)} fixtures with {version}.")


if __name__ == "__main__":
    main()
