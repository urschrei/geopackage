"""The ``geopackage_rs`` vector data providers, in headless QGIS.

Every test that takes `key` runs against both: the Python provider, and the
native one when a build of it is available (``GPKG_TOOLKIT_NATIVE_PROVIDER``,
or ``geopackage-qgis/target/release`` in this checkout). The plugin
registers one or the other under ``geopackage_rs``; here the native one is
registered beside it, as ``geopackage_rs_native``.
"""

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
)
from qgis.PyQt.QtCore import QByteArray, QDate, QDateTime, QSize, QTime, QTimeZone
from qgis.PyQt.QtGui import QColor

import geopackage_rs as g
from gpkg_toolkit import compat, native_provider
from gpkg_toolkit.data_provider import PROVIDER_KEY, decode_uri, encode_uri, register

FIXTURES = os.path.join(
    os.path.dirname(__file__), "..", "..", "geopackage", "tests", "fixtures"
)

register()

NATIVE_KEY = "geopackage_rs_native"
REPO = os.path.join(os.path.dirname(__file__), "..", "..")


def _native_library():
    override = os.environ.get(native_provider.ENV)
    if override:
        return None if override == "0" else override
    built = os.path.join(REPO, "geopackage-qgis", "target", "release", native_provider.LIBRARY)
    return built if os.path.isfile(built) else None


NATIVE = _native_library() is not None and native_provider.register(
    side_by_side=True, path=_native_library()
)


@pytest.fixture(params=[PROVIDER_KEY, NATIVE_KEY], ids=["python", "native"])
def key(request):
    if request.param == NATIVE_KEY and not NATIVE:
        pytest.skip(f"no native provider: {native_provider.LOAD_ERROR or 'not built'}")
    return request.param


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


def open_layer(path, name="pts", key=PROVIDER_KEY):
    layer = QgsVectorLayer(encode_uri({"path": path, "layerName": name}), name, key)
    assert layer.isValid(), layer.dataProvider().errors()
    return layer


def ids(features):
    return sorted(f.id() for f in features)


def test_uri_round_trip():
    parts = decode_uri("/a/b.gpkg|layername=roads")
    assert parts == {"path": "/a/b.gpkg", "layerName": "roads"}
    assert encode_uri(parts) == "/a/b.gpkg|layername=roads"


def test_the_layer_describes_itself(path, key):
    layer = open_layer(path, key=key)
    assert layer.providerType() == key
    assert layer.featureCount() == 100
    assert layer.crs().authid() == "EPSG:4326"
    assert layer.wkbType() == compat.WKB_POINT
    fields = layer.fields()
    assert [f.name() for f in fields] == ["name", "n", "x", "at", "day"]
    assert [compat.field_kind(f) for f in fields] == ["string", "int64", "double", "datetime", "date"]
    assert fields[0].length() == 10
    assert layer.extent() == QgsRectangle(0, 0, 99, 99)


def test_requests(path, key):
    layer = open_layer(path, key=key)
    assert ids(layer.getFeatures(QgsFeatureRequest(QgsRectangle(10, 10, 12, 12)))) == [11, 12, 13]
    assert ids(layer.getFeatures(QgsFeatureRequest().setFilterFids([3, 7, 500]))) == [3, 7]
    assert ids(layer.getFeatures(QgsFeatureRequest().setFilterFids([]))) == []
    assert ids(layer.getFeatures('"n" >= 97 OR "name" = \'p3\'')) == [4, 98, 99, 100]

    feature = layer.getFeature(5)
    assert feature.attributes()[:3] == ["p4", 4, 1.0]
    assert feature.attribute("at") == QDateTime(QDate(2024, 1, 1), QTime(0, 0, 4), QTimeZone.utc())
    assert feature.attribute("day") == QDate(2024, 1, 5)
    assert feature.geometry().asWkt() == "Point (4 4)"
    assert not layer.getFeature(5000).isValid()

    request = QgsFeatureRequest()
    request.setFlags(compat.REQUEST_NO_GEOMETRY)
    request.setSubsetOfAttributes(["n"], layer.fields())
    first = next(layer.getFeatures(request))
    assert not first.hasGeometry()
    assert is_null(first["name"]) and first["n"] == 0

    limited = QgsFeatureRequest().setLimit(5)
    assert len(list(layer.getFeatures(limited))) == 5

    ordered = QgsFeatureRequest().addOrderBy('"n"', False).setLimit(2)
    assert [f["n"] for f in layer.getFeatures(ordered)] == [99, 98]


def test_a_request_in_another_crs(path, key):
    layer = open_layer(path, key=key)
    mercator = QgsCoordinateReferenceSystem("EPSG:3857")
    to_mercator = QgsCoordinateTransform(layer.crs(), mercator, QgsProject.instance())
    rect = to_mercator.transformBoundingBox(QgsRectangle(19.5, 19.5, 20.5, 20.5))
    request = QgsFeatureRequest(rect).setDestinationCrs(mercator, QgsProject.instance().transformContext())
    (feature,) = layer.getFeatures(request)
    assert feature["n"] == 20
    expected = to_mercator.transform(QgsPointXY(20, 20))
    assert feature.geometry().asPoint().x() == pytest.approx(expected.x())


def test_subset_string_is_sql(path, key):
    layer = open_layer(path, key=key)
    assert layer.setSubsetString('"n" < 10 AND name <> \'p0\'')
    assert layer.featureCount() == 9
    assert layer.extent() == QgsRectangle(1, 1, 9, 9)
    assert ids(layer.getFeatures(QgsFeatureRequest(QgsRectangle(0, 0, 3, 3)))) == [2, 3, 4]
    assert ids(layer.getFeatures(QgsFeatureRequest().setFilterFid(50))) == []
    assert not layer.setSubsetString("nope >")
    assert layer.subsetString() == '"n" < 10 AND name <> \'p0\''
    assert layer.setSubsetString("")
    assert layer.featureCount() == 100


def test_an_edit_session_commits_through_the_crate(path, key):
    layer = open_layer(path, key=key)
    caps = layer.dataProvider().capabilities()
    assert caps & compat.CAP_ADD_FEATURES and caps & compat.CAP_CHANGE_GEOMETRIES

    assert layer.startEditing()
    new = QgsFeature(layer.fields())
    new.setAttributes(["new", 1000, 2.5, QDateTime(QDate(2025, 6, 7), QTime(8, 9, 10), QTimeZone.utc()), QDate(2025, 6, 7)])
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


def test_a_failed_write_fails_the_commit_with_the_crate_error(tmp_path, key):
    path = str(tmp_path / "strict.gpkg")
    gpkg = g.GeoPackage.create(path)
    gpkg.add_epsg_srs(4326)
    gpkg.create_layer("pts", "POINT", 4326, [("name", "TEXT", True)])
    gpkg.close()
    layer = open_layer(path, key=key)
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


def test_rendering_runs_iterators_on_worker_threads(path, key):
    layer = open_layer(path, key=key)
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


def test_an_attribute_table(path, key):
    layer = open_layer(path, "notes", key)
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
    # check the provider makes is what is patched. That check is Python's
    # own, so this runs against the Python provider only.
    from gpkg_toolkit import data_provider

    monkeypatch.setattr(data_provider.os, "access", lambda *_: False)
    layer = open_layer(path)
    assert not layer.dataProvider().capabilities() & compat.CAP_ADD_FEATURES
    assert layer.featureCount() == 100
    assert not layer.startEditing() or not layer.addFeature(QgsFeature(layer.fields()))


def test_a_project_stores_a_relative_path_and_reopens(path, tmp_path, key):
    project = QgsProject()
    project.addMapLayer(open_layer(path, key=key))
    project_path = str(tmp_path / "project.qgs")
    assert project.write(project_path)
    with open(project_path) as f:
        text = f.read()
    assert "./data.gpkg|layername=pts" in text
    assert f'provider="{key}"' in text or f">{key}<" in text

    moved = tmp_path / "moved"
    moved.mkdir()
    shutil.copy(project_path, moved / "project.qgs")
    shutil.copy(path, moved / "data.gpkg")
    reopened = QgsProject()
    assert reopened.read(str(moved / "project.qgs"))
    (layer,) = reopened.mapLayers().values()
    assert layer.isValid()
    assert layer.providerType() == key
    assert decode_uri(layer.source())["path"] == str(moved / "data.gpkg")
    assert layer.featureCount() == 100


@pytest.mark.parametrize("name", ["points", "lines", "polygons", "points3d"])
def test_it_reads_a_gdal_file_as_ogr_does(name, key):
    path = os.path.join(FIXTURES, "gdal_multilayer_1_4.gpkg")
    ours = QgsVectorLayer(f"{path}|layername={name}", name, key)
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


def test_curves_load_as_curves(key):
    path = os.path.join(FIXTURES, "gdal_curves.gpkg")
    with g.GeoPackage(path, read_only=True, lenient=True) as gpkg:
        names = gpkg.layer_names()
    assert names
    for name in names:
        ours = QgsVectorLayer(f"{path}|layername={name}", name, key)
        ogr = QgsVectorLayer(f"{path}|layername={name}", name, "ogr")
        assert ours.isValid(), name
        assert sorted(f.geometry().asWkt(6) for f in ours.getFeatures()) == sorted(
            f.geometry().asWkt(6) for f in ogr.getFeatures()
        ), name


def test_a_single_id_with_an_attribute_subset(path, key):
    # The single-id path reads every column; the subset must still line up.
    layer = open_layer(path, key=key)
    request = QgsFeatureRequest().setFilterFid(5)
    request.setSubsetOfAttributes(["n"], layer.fields())
    (feature,) = layer.getFeatures(request)
    assert feature["n"] == 4


def test_values_of_every_kind_reach_the_file(tmp_path, key):
    path = str(tmp_path / "kinds.gpkg")
    gpkg = g.GeoPackage.create(path)
    gpkg.add_epsg_srs(4326)
    gpkg.create_layer(
        "pts",
        "MULTIPOINT",
        4326,
        [("ok", "BOOLEAN"), ("raw", "BLOB"), ("n", "INTEGER"), ("x", "DOUBLE"), ("s", "TEXT"), ("day", "DATE")],
    )
    gpkg.close()
    layer = open_layer(path, key=key)
    assert layer.startEditing()
    new = QgsFeature(layer.fields())
    new.setAttributes([True, QByteArray(b"\x00\x01\xff"), 2**40, -0.5, "snow ☃", QDate(1969, 12, 31)])
    # A single point with a z, into a 2D multipoint column: made multi, z dropped.
    new.setGeometry(QgsGeometry.fromWkt("PointZ (1 2 3)"))
    assert layer.addFeature(new)
    empty = QgsFeature(layer.fields())
    empty.setAttributes([None, None, None, None, None, None])
    assert layer.addFeature(empty)
    assert layer.commitChanges(), layer.commitErrors()

    with g.GeoPackage(path, read_only=True) as gpkg:
        rows = sorted(gpkg.layer("pts").features(), key=lambda row: row[0])
    (fid, wkb, values), (_, no_wkb, nulls) = rows
    assert values == [True, b"\x00\x01\xff", 2**40, -0.5, "snow ☃", datetime.date(1969, 12, 31)]
    stored = QgsGeometry()
    stored.fromWkb(QByteArray(wkb))
    assert stored.asWkt() == "MultiPoint ((1 2))"
    assert no_wkb is None and nulls == [None] * 6
    assert layer.getFeature(fid).attribute("ok") is True


def test_a_clone_keeps_the_subset(path, key):
    layer = open_layer(path, key=key)
    assert layer.setSubsetString('"n" < 5')
    clone = layer.clone()
    assert clone.isValid()
    assert clone.subsetString() == '"n" < 5'
    assert clone.featureCount() == 5


def test_an_edit_shows_in_the_next_read(path, key):
    # The native provider reads through pooled read-only connections; a
    # committed edit must reach them.
    layer = open_layer(path, key=key)
    assert ids(layer.getFeatures(QgsFeatureRequest(QgsRectangle(-1, -1, 0.5, 0.5)))) == [1]
    assert layer.startEditing()
    assert layer.deleteFeature(1)
    assert layer.commitChanges(), layer.commitErrors()
    assert ids(layer.getFeatures(QgsFeatureRequest(QgsRectangle(-1, -1, 0.5, 0.5)))) == []
    assert layer.featureCount() == 99
    assert layer.dataProvider().featureCount() == 99


def test_curved_layers_offer_curves(key):
    path = os.path.join(FIXTURES, "gdal_curves.gpkg")
    with g.GeoPackage(path, read_only=True, lenient=True) as gpkg:
        name = gpkg.layer_names()[0]
    layer = QgsVectorLayer(f"{path}|layername={name}", name, key)
    assert layer.dataProvider().capabilities() & compat.CAP_CIRCULAR_GEOMETRIES
