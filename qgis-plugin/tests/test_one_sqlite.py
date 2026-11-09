"""The plugin's module uses the SQLite that QGIS, GDAL and QGIS's Python share.

Two copies of SQLite in one process do not see each other's locks, and
closing a file in one copy releases the locks that the other holds on that
file. Python's ``sqlite3`` stands for QGIS's SQLite here: in QGIS, and in a
Python that imports QGIS's bindings, it is the same library.
"""

import sqlite3
import subprocess
import sys

import pytest
from helpers import point, points_gpkg

import geopackage_rs as g

WRITE = "INSERT INTO gpkg_extensions VALUES (NULL, NULL, 'x_probe', 'x', 'read-write')"
OTHER_PROCESS = f"""
import sqlite3, sys
connection = sqlite3.connect(sys.argv[1], isolation_level=None, timeout=0)
try:
    connection.execute({WRITE!r})
except sqlite3.OperationalError:
    print("refused")
else:
    print("written")
"""


@pytest.fixture
def path(tmp_path):
    path = str(tmp_path / "locked.gpkg")
    points_gpkg(path)
    return path


@pytest.fixture
def held(path):
    """A write lock on `path`, held through QGIS's SQLite."""
    holder = sqlite3.connect(path, isolation_level=None, timeout=0)
    holder.execute("BEGIN IMMEDIATE")
    yield
    holder.execute("ROLLBACK")
    holder.close()


def other_process_writes(path):
    result = subprocess.run(
        [sys.executable, "-c", OTHER_PROCESS, path],
        capture_output=True,
        text=True,
        check=True,
    )
    return result.stdout.strip() == "written"


def test_a_write_waits_for_a_lock_that_qgis_holds(path, held):
    with g.GeoPackage(path) as gpkg, pytest.raises(g.GeoPackageError, match="locked"):
        gpkg.layer("pts").insert([(None, point(1, 1), ["locked out"])])


def test_closing_a_file_keeps_the_locks_that_qgis_holds(path, held):
    assert not other_process_writes(path)
    g.GeoPackage(path, read_only=True).close()
    assert not other_process_writes(path)


def test_a_write_goes_ahead_once_the_lock_is_released(path):
    with sqlite3.connect(path, isolation_level=None, timeout=0) as holder:
        holder.execute("BEGIN IMMEDIATE")
        holder.execute("ROLLBACK")
    with g.GeoPackage(path) as gpkg:
        gpkg.layer("pts").insert([(None, point(1, 1), ["written"])])
        assert gpkg.layer("pts").count() == 21
