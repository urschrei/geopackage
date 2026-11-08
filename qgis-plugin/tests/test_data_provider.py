"""The ``geopackage_rs`` vector data provider, in headless QGIS."""

import datetime
import os
import shutil
import struct

import pytest
from qgis.core import (
    QgsCoordinateReferenceSystem,
    QgsCoordinateTransform,
    QgsFeature,
    QgsFeatureRequest,
    QgsGeometry,
    QgsMapRendererParallelJob,
    QgsMapSettings,
    QgsPointXY,
    QgsProject,
    QgsRectangle,
    QgsVectorLayer,
    QgsWkbTypes,
)
from qgis.PyQt.QtCore import QDate, QDateTime, QSize, Qt, QTime
from qgis.PyQt.QtGui import QColor

import geopackage_rs as g
from gpkg_toolkit import compat
from gpkg_toolkit.data_provider import PROVIDER_KEY, decode_uri, encode_uri, register

FIXTURES = os.path.join(
    os.path.dirname(__file__), "..", "..", "geopackage", "tests", "fixtures"
)

register()


def point(x, y):
    return struct.pack("<BIdd", 1, 1, x, y)


def is_null(value):
    return value is None or (hasattr(value, "isNull") and value.isNull())


@pytest.fixture
def path(tmp_path):
    path = str(tmp_path / "data.gpkg")
    gpkg = g.GeoPackage.create(path)
    gpkg.add_epsg_srs(4326)
    layer = gpkg.create_layer(
        "pts",
        "POINT",
        4326,
        [("name", "TEXT(10)"), ("n", "INTEGER"), ("x", "DOUBLE"), ("at", "DATETIME"), ("day", "DATE")],
    )
    layer.insert(
        (
            None,
            point(i, i),
            [
                f"p{i}",
                i,
                i / 4,
                datetime.datetime(2024, 1, 1, 0, 0, i % 60, tzinfo=datetime.timezone.utc),
                datetime.date(2024, 1, 1 + i % 28),
            ],
        )
        for i in range(100)
    )
    table = gpkg.create_layer("notes", None, columns=[("text", "TEXT")])
    table.insert([(None, None, ["hello"]), (None, None, ["world"])])
    gpkg.close()
    return path


def open_layer(path, name="pts"):
    layer = QgsVectorLayer(encode_uri({"path": path, "layerName": name}), name, PROVIDER_KEY)
    assert layer.isValid(), layer.dataProvider().errors()
    return layer


def ids(features):
    return sorted(f.id() for f in features)


def test_uri_round_trip():
    parts = decode_uri("/a/b.gpkg|layername=roads")
    assert parts == {"path": "/a/b.gpkg", "layerName": "roads"}
    assert encode_uri(parts) == "/a/b.gpkg|layername=roads"


def test_the_layer_describes_itself(path):
    layer = open_layer(path)
    assert layer.providerType() == PROVIDER_KEY
    assert layer.featureCount() == 100
    assert layer.crs().authid() == "EPSG:4326"
    assert layer.wkbType() == QgsWkbTypes.Point
    fields = layer.fields()
    assert [f.name() for f in fields] == ["name", "n", "x", "at", "day"]
    assert [compat.field_kind(f) for f in fields] == ["string", "int64", "double", "datetime", "date"]
    assert fields[0].length() == 10
    assert layer.extent() == QgsRectangle(0, 0, 99, 99)


def test_requests(path):
    layer = open_layer(path)
    assert ids(layer.getFeatures(QgsFeatureRequest(QgsRectangle(10, 10, 12, 12)))) == [11, 12, 13]
    assert ids(layer.getFeatures(QgsFeatureRequest().setFilterFids([3, 7, 500]))) == [3, 7]
    assert ids(layer.getFeatures(QgsFeatureRequest().setFilterFids([]))) == []
    assert ids(layer.getFeatures('"n" >= 97 OR "name" = \'p3\'')) == [4, 98, 99, 100]

    feature = layer.getFeature(5)
    assert feature.attributes()[:3] == ["p4", 4, 1.0]
    assert feature.attribute("at") == QDateTime(QDate(2024, 1, 1), QTime(0, 0, 4), Qt.UTC)
    assert feature.attribute("day") == QDate(2024, 1, 5)
    assert feature.geometry().asWkt() == "Point (4 4)"
    assert not layer.getFeature(5000).isValid()

    request = QgsFeatureRequest()
    request.setFlags(QgsFeatureRequest.NoGeometry)
    request.setSubsetOfAttributes(["n"], layer.fields())
    first = next(layer.getFeatures(request))
    assert not first.hasGeometry()
    assert is_null(first["name"]) and first["n"] == 0

    limited = QgsFeatureRequest().setLimit(5)
    assert len(list(layer.getFeatures(limited))) == 5

    ordered = QgsFeatureRequest().addOrderBy('"n"', False).setLimit(2)
    assert [f["n"] for f in layer.getFeatures(ordered)] == [99, 98]


def test_a_request_in_another_crs(path):
    layer = open_layer(path)
    mercator = QgsCoordinateReferenceSystem("EPSG:3857")
    to_mercator = QgsCoordinateTransform(layer.crs(), mercator, QgsProject.instance())
    rect = to_mercator.transformBoundingBox(QgsRectangle(19.5, 19.5, 20.5, 20.5))
    request = QgsFeatureRequest(rect).setDestinationCrs(mercator, QgsProject.instance().transformContext())
    (feature,) = layer.getFeatures(request)
    assert feature["n"] == 20
    expected = to_mercator.transform(QgsPointXY(20, 20))
    assert feature.geometry().asPoint().x() == pytest.approx(expected.x())


def test_subset_string_is_sql(path):
    layer = open_layer(path)
    assert layer.setSubsetString('"n" < 10 AND name <> \'p0\'')
    assert layer.featureCount() == 9
    assert layer.extent() == QgsRectangle(1, 1, 9, 9)
    assert ids(layer.getFeatures(QgsFeatureRequest(QgsRectangle(0, 0, 3, 3)))) == [2, 3, 4]
    assert ids(layer.getFeatures(QgsFeatureRequest().setFilterFid(50))) == []
    assert not layer.setSubsetString("nope >")
    assert layer.subsetString() == '"n" < 10 AND name <> \'p0\''
    assert layer.setSubsetString("")
    assert layer.featureCount() == 100


def test_an_edit_session_commits_through_the_crate(path):
    layer = open_layer(path)
    caps = layer.dataProvider().capabilities()
    assert caps & compat.CAP_ADD_FEATURES and caps & compat.CAP_CHANGE_GEOMETRIES

    assert layer.startEditing()
    new = QgsFeature(layer.fields())
    new.setAttributes(["new", 1000, 2.5, QDateTime(QDate(2025, 6, 7), QTime(8, 9, 10), Qt.UTC), QDate(2025, 6, 7)])
    new.setGeometry(QgsGeometry.fromWkt("Point (500 500)"))
    assert layer.addFeature(new)
    assert layer.changeAttributeValue(1, layer.fields().indexOf("name"), "renamed")
    assert layer.changeGeometry(2, QgsGeometry.fromWkt("Point (-10 -10)"))
    assert layer.deleteFeature(3)
    assert layer.commitChanges(), layer.commitErrors()

    assert layer.featureCount() == 100
    assert layer.extent() == QgsRectangle(-10, -10, 500, 500)
    with g.GeoPackage(path, read_only=True) as gpkg:
        pts = gpkg.layer("pts")
        assert pts.get(1)[2][0] == "renamed"
        assert pts.get(2)[1] == point(-10, -10)
        assert pts.get(3) is None
        (added,) = [row for row in pts.features(where="name = 'new'")]
        assert added[2][:3] == ["new", 1000, 2.5]
        assert added[2][3] == datetime.datetime(2025, 6, 7, 8, 9, 10, tzinfo=datetime.timezone.utc)
        assert pts.spatial_index_status() == "current"
        assert pts.audit_spatial_index()["consistent"]
        assert not [f for f in gpkg.validate() if f.severity == "error"]

    # The index followed the edits, so a bbox read finds the moved and new rows.
    assert ids(layer.getFeatures(QgsFeatureRequest(QgsRectangle(-11, -11, -9, -9)))) == [2]
    ogr = QgsVectorLayer(f"{path}|layername=pts", "ogr", "ogr")
    assert ogr.featureCount() == 100
    (from_ogr,) = ogr.getFeatures('"name" = \'new\'')
    assert from_ogr.geometry().asWkt() == "Point (500 500)"


def test_a_failed_write_fails_the_commit_with_the_crate_error(tmp_path):
    path = str(tmp_path / "strict.gpkg")
    gpkg = g.GeoPackage.create(path)
    gpkg.add_epsg_srs(4326)
    gpkg.create_layer("pts", "POINT", 4326, [("name", "TEXT", True)])
    gpkg.close()
    layer = open_layer(path)
    layer.startEditing()
    bad = QgsFeature(layer.fields())
    bad.setAttributes([None])  # NOT NULL
    bad.setGeometry(QgsGeometry.fromWkt("Point (1 1)"))
    layer.addFeature(bad)
    assert not layer.commitChanges()
    errors = layer.commitErrors() + layer.dataProvider().errors()
    assert any("add features failed" in e and "NOT NULL" in e for e in errors), errors
    layer.rollBack()
    assert layer.featureCount() == 0


def test_rendering_runs_iterators_on_worker_threads(path):
    layer = open_layer(path)
    layer.renderer().symbol().setColor(QColor("black"))
    settings = QgsMapSettings()
    settings.setLayers([layer])
    settings.setDestinationCrs(layer.crs())
    settings.setExtent(QgsRectangle(-1, -1, 100, 100))
    settings.setOutputSize(QSize(200, 200))
    settings.setBackgroundColor(QColor("white"))
    job = QgsMapRendererParallelJob(settings)
    job.start()
    job.waitForFinished()
    image = job.renderedImage()
    dark = sum(
        1
        for x in range(0, 200, 2)
        for y in range(0, 200, 2)
        if QColor(image.pixel(x, y)).lightness() < 128
    )
    assert dark > 20, "the rendered points did not reach the image"


def test_an_attribute_table(path):
    layer = open_layer(path, "notes")
    assert layer.wkbType() == compat.WKB_NO_GEOMETRY
    assert [f["text"] for f in layer.getFeatures()] == ["hello", "world"]
    layer.startEditing()
    new = QgsFeature(layer.fields())
    new.setAttributes(["again"])
    layer.addFeature(new)
    assert layer.commitChanges(), layer.commitErrors()
    assert layer.featureCount() == 3


def test_a_read_only_file_offers_no_editing(path, monkeypatch):
    # Tests may run as root, which can write a 0444 file, so the writability
    # check the provider makes is what is patched.
    from gpkg_toolkit import data_provider

    monkeypatch.setattr(data_provider.os, "access", lambda *_: False)
    layer = open_layer(path)
    assert not layer.dataProvider().capabilities() & compat.CAP_ADD_FEATURES
    assert layer.featureCount() == 100
    assert not layer.startEditing() or not layer.addFeature(QgsFeature(layer.fields()))


def test_a_project_stores_a_relative_path_and_reopens(path, tmp_path):
    project = QgsProject()
    project.addMapLayer(open_layer(path))
    project_path = str(tmp_path / "project.qgs")
    assert project.write(project_path)
    with open(project_path) as f:
        text = f.read()
    assert "./data.gpkg|layername=pts" in text
    assert f'provider="{PROVIDER_KEY}"' in text or f">{PROVIDER_KEY}<" in text

    moved = tmp_path / "moved"
    moved.mkdir()
    shutil.copy(project_path, moved / "project.qgs")
    shutil.copy(path, moved / "data.gpkg")
    reopened = QgsProject()
    assert reopened.read(str(moved / "project.qgs"))
    (layer,) = reopened.mapLayers().values()
    assert layer.isValid()
    assert layer.providerType() == PROVIDER_KEY
    assert decode_uri(layer.source())["path"] == str(moved / "data.gpkg")
    assert layer.featureCount() == 100


@pytest.mark.parametrize("name", ["points", "lines", "polygons", "points3d"])
def test_it_reads_a_gdal_file_as_ogr_does(name):
    path = os.path.join(FIXTURES, "gdal_multilayer_1_4.gpkg")
    ours = QgsVectorLayer(f"{path}|layername={name}", name, PROVIDER_KEY)
    ogr = QgsVectorLayer(f"{path}|layername={name}", name, "ogr")
    assert ours.isValid() and ogr.isValid()
    assert ours.wkbType() == ogr.wkbType()
    assert ours.crs() == ogr.crs()
    assert ours.featureCount() == ogr.featureCount()
    # OGR exposes the primary key as a field; this provider gives it as the id.
    ogr_names = [f.name() for f in ogr.fields() if f.name() != "fid"]
    assert [f.name() for f in ours.fields()] == ogr_names
    theirs = {f.id(): f for f in ogr.getFeatures()}
    for feature in ours.getFeatures():
        other = theirs[feature.id()]
        assert [feature[n] for n in ogr_names] == [other[n] for n in ogr_names]
        assert feature.geometry().asWkt() == other.geometry().asWkt()


def test_curves_load_as_curves():
    path = os.path.join(FIXTURES, "gdal_curves.gpkg")
    with g.GeoPackage(path, read_only=True, lenient=True) as gpkg:
        names = gpkg.layer_names()
    assert names
    for name in names:
        ours = QgsVectorLayer(f"{path}|layername={name}", name, PROVIDER_KEY)
        ogr = QgsVectorLayer(f"{path}|layername={name}", name, "ogr")
        assert ours.isValid(), name
        assert sorted(f.geometry().asWkt(6) for f in ours.getFeatures()) == sorted(
            f.geometry().asWkt(6) for f in ogr.getFeatures()
        ), name


def test_a_single_id_with_an_attribute_subset(path):
    # The single-id path reads every column; the subset must still line up.
    layer = open_layer(path)
    request = QgsFeatureRequest().setFilterFid(5)
    request.setSubsetOfAttributes(["n"], layer.fields())
    (feature,) = layer.getFeatures(request)
    assert feature["n"] == 4
