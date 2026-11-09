"""The plugin's algorithms, run through headless QGIS Processing."""

import datetime
import sqlite3

import pytest
from qgis.core import (
    QgsCoordinateReferenceSystem,
    QgsCoordinateTransform,
    QgsFeature,
    QgsGeometry,
    QgsPointXY,
    QgsProject,
    QgsVectorLayer,
)

import geopackage_rs as g
from gpkg_toolkit import compat
from helpers import coverage_gpkg, points_gpkg


def table_rows(layer):
    """The attribute rows of a Processing output, as dicts."""
    if isinstance(layer, str):
        layer = QgsVectorLayer(layer, "out", "ogr")
    names = [f.name() for f in layer.fields()]
    return [dict(zip(names, f.attributes())) for f in layer.getFeatures()]


def is_null(value):
    """QGIS 3 returns NULL attributes as a null QVariant, QGIS 4 as None."""
    return value is None or (hasattr(value, "isNull") and value.isNull())


def test_the_provider_loads_every_algorithm():
    from qgis.core import QgsApplication

    from gpkg_toolkit.algorithms import ALGORITHMS

    provider = QgsApplication.processingRegistry().providerById("gpkgtoolkit")
    assert provider is not None
    assert {a.id() for a in provider.algorithms()} == {
        f"gpkgtoolkit:{cls.NAME}" for cls in ALGORITHMS
    }


def test_validate_clean_and_damaged(tmp_path, run):
    path = str(tmp_path / "a.gpkg")
    points_gpkg(path)

    clean = run("validate", {"INPUT": path, "OUTPUT": "memory:"})
    assert clean["ERRORS"] == 0
    rows = table_rows(clean["OUTPUT"])
    # The unindexed layer is an advisory, not an error.
    assert {r["table_name"] for r in rows if r["severity"] == "advisory"} == {
        "unindexed"
    }

    con = sqlite3.connect(path)
    con.execute("DELETE FROM rtree_pts_geom WHERE id < 5")
    con.commit()
    con.close()
    damaged = run("validate", {"INPUT": path, "OUTPUT": "memory:"})
    assert damaged["ERRORS"] == 1
    (error,) = [r for r in table_rows(damaged["OUTPUT"]) if r["severity"] == "error"]
    assert error["code"] == "SpatialIndexOutOfStep"
    assert error["table_name"] == "pts"
    assert "rebuild_spatial_index" in error["repair"]


def test_repair_spatial_indexes(tmp_path, run):
    path = str(tmp_path / "a.gpkg")
    points_gpkg(path)
    con = sqlite3.connect(path)
    con.execute("DELETE FROM rtree_pts_geom WHERE id < 5")
    con.commit()
    con.close()

    result = run("repairspatialindexes", {"INPUT": path, "OUTPUT": "memory:"})
    assert result["REPAIRED"] == 1
    report = {r["layer"]: r for r in table_rows(result["OUTPUT"])}
    assert report["pts"]["action"] == "rebuilt"
    assert report["pts"]["missing"] == 4
    assert report["unindexed"]["action"] == "none"
    assert run("validate", {"INPUT": path, "OUTPUT": "memory:"})["ERRORS"] == 0

    built = run(
        "repairspatialindexes",
        {
            "INPUT": path,
            "LAYER": "unindexed",
            "BUILD_MISSING": True,
            "OUTPUT": "memory:",
        },
    )
    assert built["REPAIRED"] == 1
    with g.GeoPackage(path, read_only=True) as gpkg:
        assert gpkg.layer("unindexed").spatial_index_status() == "current"


def test_repair_a_legacy_trigger_set(tmp_path, run):
    path = str(tmp_path / "a.gpkg")
    points_gpkg(path)
    # A pre-1.4 trigger beside the 1.4 set: a complete mixed set, which is
    # legacy.
    con = sqlite3.connect(path)
    con.execute(
        "CREATE TRIGGER rtree_pts_geom_update1 AFTER UPDATE OF geom ON pts BEGIN SELECT 1; END"
    )
    con.commit()
    con.close()

    result = run(
        "repairspatialindexes", {"INPUT": path, "LAYER": "pts", "OUTPUT": "memory:"}
    )
    assert result["REPAIRED"] == 1
    (row,) = table_rows(result["OUTPUT"])
    assert (row["status_before"], row["action"], row["status_after"]) == (
        "legacy",
        "repaired to the 1.4 trigger set",
        "current",
    )


def test_list_extensions(tmp_path, run):
    path = str(tmp_path / "a.gpkg")
    points_gpkg(path)
    result = run("listextensions", {"INPUT": path, "OUTPUT": "memory:"})
    rows = table_rows(result["OUTPUT"])
    assert [(r["name"], r["table_name"], r["support"]) for r in rows] == [
        ("gpkg_rtree_index", "pts", "implemented")
    ]


def memory_layer(uri, features):
    layer = QgsVectorLayer(uri, "source", "memory")
    provider = layer.dataProvider()
    out = []
    for geometry, attributes in features:
        feature = QgsFeature(layer.fields())
        if geometry is not None:
            feature.setGeometry(QgsGeometry.fromWkt(geometry))
        feature.setAttributes(attributes)
        out.append(feature)
    provider.addFeatures(out)
    return layer


def test_export_round_trips_through_gdal(tmp_path, run):
    from qgis.PyQt.QtCore import QDate, QDateTime, QTime, QTimeZone

    source = memory_layer(
        "MultiPolygon?crs=EPSG:3857&field=fid:integer&field=name:string(20)"
        "&field=area:double&field=day:date&field=at:datetime&field=flag:boolean",
        [
            (
                "Polygon((0 0, 1 0, 1 1, 0 0))",
                [
                    7,
                    "single",
                    0.5,
                    QDate(2024, 2, 29),
                    QDateTime(QDate(2024, 1, 2), QTime(3, 4, 5), QTimeZone.utc()),
                    True,
                ],
            ),
            (
                "MultiPolygon(((10 10, 12 10, 12 12, 10 10)))",
                [9, None, None, None, None, False],
            ),
            (None, [11, "no geometry", 1.0, None, None, None]),
        ],
    )
    QgsProject.instance().addMapLayer(source)
    path = str(tmp_path / "out.gpkg")
    result = run(
        "exportlayer",
        {
            "INPUT": source,
            "OUTPUT": path,
            "LAYER_NAME": "shapes",
            "SPATIAL_INDEX": True,
        },
    )
    assert result["WRITTEN"] == 3

    with g.GeoPackage(path, read_only=True) as gpkg:
        layer = gpkg.layer("shapes")
        assert layer.field_names == ["name", "area", "day", "at", "flag"]
        assert [f.declared_type for f in layer.fields] == [
            "TEXT(20)",
            "DOUBLE",
            "DATE",
            "DATETIME",
            "BOOLEAN",
        ]
        geom = layer.geometry_column
        assert (geom.geometry_type, geom.srs_id) == ("MULTIPOLYGON", 3857)
        assert layer.spatial_index_status() == "current"
        assert layer.extent() == (0.0, 0.0, 12.0, 12.0)
        rows = {fid: values for fid, _, values in layer.features()}
        assert sorted(rows) == [7, 9, 11]
        assert rows[7] == [
            "single",
            0.5,
            datetime.date(2024, 2, 29),
            datetime.datetime(2024, 1, 2, 3, 4, 5, tzinfo=datetime.timezone.utc),
            True,
        ]
        assert not [f for f in gpkg.validate() if f.severity == "error"]

    # GDAL reads what the crate wrote: ids, promoted geometry and values.
    ogr = QgsVectorLayer(result["OUTPUT_LAYER"], "check", "ogr")
    assert ogr.isValid()
    assert ogr.featureCount() == 3
    assert ogr.crs().authid() == "EPSG:3857"
    by_id = {f.id(): f for f in ogr.getFeatures()}
    assert by_id[7].geometry().isMultipart()
    assert by_id[7]["name"] == "single"
    assert by_id[7]["day"] == QDate(2024, 2, 29)
    assert not by_id[11].hasGeometry()

    # A second table in the same file.
    again = run("exportlayer", {"INPUT": source, "OUTPUT": path, "LAYER_NAME": "copy"})
    assert again["WRITTEN"] == 3
    with g.GeoPackage(path, read_only=True) as gpkg:
        assert sorted(gpkg.layer_names()) == ["copy", "shapes"]
    QgsProject.instance().removeMapLayer(source.id())


def test_export_custom_crs(tmp_path, run):
    proj = "+proj=tmerc +lat_0=0 +lon_0=3 +k=1 +x_0=500000 +y_0=0 +ellps=GRS80 +units=m +no_defs"
    crs = QgsCoordinateReferenceSystem.fromProj(proj)
    assert crs.isValid() and not crs.authid().startswith("EPSG")
    source = memory_layer("Point?field=n:integer", [("Point(1 2)", [1])])
    source.setCrs(crs)
    QgsProject.instance().addMapLayer(source)
    path = str(tmp_path / "custom.gpkg")
    run("exportlayer", {"INPUT": source, "OUTPUT": path, "LAYER_NAME": "p"})
    with g.GeoPackage(path, read_only=True) as gpkg:
        srs_id = gpkg.layer("p").geometry_column.srs_id
        assert srs_id >= 100000
        srs = gpkg.srs(srs_id)
        assert "Transverse_Mercator" in srs.definition
    ogr = QgsVectorLayer(f"{path}|layername=p", "check", "ogr")
    assert ogr.crs().toProj().startswith("+proj=tmerc")
    QgsProject.instance().removeMapLayer(source.id())


def test_sample_coverage(tmp_path, run):
    path = str(tmp_path / "dem.gpkg")
    coverage_gpkg(path)
    # Points in Web Mercator, so the algorithm has to transform them.
    to_3857 = QgsCoordinateTransform(
        QgsCoordinateReferenceSystem("EPSG:4326"),
        QgsCoordinateReferenceSystem("EPSG:3857"),
        QgsProject.instance(),
    )
    lonlat = [(0.5, 3.5), (3.5, 0.5), (5.5, 3.5), (50, 50)]
    wkts = []
    for x, y in lonlat:
        p = to_3857.transform(QgsPointXY(x, y))
        wkts.append(f"Point({p.x()} {p.y()})")
    source = memory_layer(
        "Point?crs=EPSG:3857&field=id:integer",
        [(w, [i]) for i, w in enumerate(wkts)] + [(None, [99])],
    )
    QgsProject.instance().addMapLayer(source)
    result = run(
        "samplecoverage",
        {"INPUT": path, "POINTS": source, "FIELD": "elev", "OUTPUT": "memory:"},
    )
    rows = {r["id"]: r["elev"] for r in table_rows(result["OUTPUT"])}
    assert rows[0] == pytest.approx(100.0)
    assert rows[1] == pytest.approx(107.5)
    # A null sample, a point outside the coverage, and no geometry.
    assert is_null(rows[2]) and is_null(rows[3]) and is_null(rows[99])
    QgsProject.instance().removeMapLayer(source.id())


def test_coverage_statistics(tmp_path, run):
    path = str(tmp_path / "dem.gpkg")
    coverage_gpkg(path)
    result = run("coveragestatistics", {"INPUT": path, "OUTPUT": "memory:"})
    layer = result["OUTPUT"]
    assert layer.crs().authid() == "EPSG:4326"
    stats = {(f["tile_column"], f["tile_row"]): f for f in layer.getFeatures()}
    left = stats[(0, 0)]
    assert left["count"] == 16
    assert (left["min"], left["max"]) == (100.0, 107.5)
    assert (left["rec_min"], left["rec_max"]) == (100.0, 107.5)
    assert left.geometry().boundingBox().toString(0) == "0,0 : 4,4"
    assert stats[(1, 0)]["count"] == 1


def test_check_coverage_samples(tmp_path, run):
    path = str(tmp_path / "dem.gpkg")
    coverage_gpkg(path)
    result = run("checkcoveragesamples", {"INPUT": path, "OUTPUT": "memory:"})
    assert result["FAILURES"] == 0
    with pytest.raises(Exception, match="no coverage named"):
        run(
            "checkcoveragesamples",
            {"INPUT": path, "COVERAGE": "nope", "OUTPUT": "memory:"},
        )


def test_compat_field_kinds():
    for kind in (
        "bool",
        "int",
        "int64",
        "double",
        "string",
        "bytes",
        "date",
        "datetime",
    ):
        assert compat.field_kind(compat.field("x", kind)) == kind
