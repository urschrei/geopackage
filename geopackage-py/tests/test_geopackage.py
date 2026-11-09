"""Tests for the geopackage_rs bindings, run against the built module."""

import datetime
import sqlite3
import struct
import zlib

import geopackage_rs as g
import pytest


def point(x, y):
    return struct.pack("<BIdd", 1, 1, x, y)


def circular_string(*coords):
    """ISO WKB CIRCULARSTRING (type 8): a curve geo-traits cannot express."""
    body = b"".join(struct.pack("<dd", x, y) for x, y in coords)
    return struct.pack("<BII", 1, 8, len(coords)) + body


@pytest.fixture
def path(tmp_path):
    return tmp_path / "test.gpkg"


@pytest.fixture
def points(path):
    gpkg = g.GeoPackage.create(path)
    gpkg.add_epsg_srs(4326)
    layer = gpkg.create_layer(
        "pts",
        "POINT",
        4326,
        [("name", "TEXT"), ("n", "INTEGER"), ("x", "DOUBLE"), ("ok", "BOOLEAN")],
    )
    layer.insert(
        (None, point(i, i), [f"p{i}", i, i / 2, i % 2 == 0]) for i in range(100)
    )
    return gpkg


def test_create_and_read_back(points, path):
    layer = points.layer("pts")
    assert layer.kind == "features"
    assert layer.field_names == ["name", "n", "x", "ok"]
    assert layer.primary_key == "fid"
    geom = layer.geometry_column
    assert (geom.column_name, geom.geometry_type, geom.srs_id) == (
        "geom",
        "POINT",
        4326,
    )
    assert layer.count() == 100
    assert layer.extent() == (0.0, 0.0, 99.0, 99.0)
    assert points.layer_names() == ["pts"]
    rows = list(layer.features())
    assert len(rows) == 100
    fid, wkb, values = rows[3]
    assert wkb == point(3, 3)
    assert values == ["p3", 3, 1.5, False]
    assert isinstance(values[3], bool)


def test_a_second_handle_sees_committed_rows(points, path):
    with g.GeoPackage(path, read_only=True) as other:
        assert other.layer("pts").count() == 100


def test_bbox_where_and_projection(points):
    layer = points.layer("pts")
    assert [r[0] for r in layer.features(bbox=(10, 10, 12, 12))] == [11, 12, 13]
    filtered = layer.features(bbox=(0, 0, 50, 50), where="n % ?1 = 0", params=[10])
    assert [r[2][1] for r in filtered] == [0, 10, 20, 30, 40, 50]
    projected = layer.features(columns=["n", "name"], geometry=False)
    assert projected.columns == ["name", "n"]
    first = next(projected)
    assert first == (1, None, ["p0", 0])
    projected.close()
    assert list(projected) == []


def test_bbox_where_without_an_index_matches(points):
    layer = points.layer("pts")
    with_index = list(layer.features(bbox=(5, 5, 20, 20), where="n > ?1", params=[8]))
    layer.drop_spatial_index()
    assert layer.spatial_index_status() == "absent"
    without = list(layer.features(bbox=(5, 5, 20, 20), where="n > ?1", params=[8]))
    assert with_index == without
    assert len(without) == 12


def test_chunks(points):
    it = points.layer("pts").features(chunk_size=7)
    sizes = []
    while chunk := it.next_chunk():
        sizes.append(len(chunk))
    assert sum(sizes) == 100
    assert max(sizes) == 7


def test_value_round_trip(path):
    gpkg = g.GeoPackage.create(path)
    table = gpkg.create_layer(
        "t",
        None,
        columns=[
            ("b", "BLOB"),
            ("d", "DATE"),
            ("dt", "DATETIME"),
            ("t", "TEXT", True),
        ],
    )
    assert table.kind == "attributes"
    when = datetime.datetime(2024, 5, 6, 7, 8, 9, 123000, tzinfo=datetime.timezone.utc)
    local = datetime.datetime(
        2024,
        5,
        6,
        9,
        8,
        9,
        123000,
        tzinfo=datetime.timezone(datetime.timedelta(hours=2)),
    )
    fids = table.insert(
        [
            (None, None, [b"\x00\x01", datetime.date(2020, 2, 29), when, "x"]),
            (None, None, [bytearray(b"z"), None, local, "y"]),
            (10, None, [None, None, datetime.datetime(2001, 1, 1), "z"]),
        ]
    )
    assert fids == [1, 2, 10]
    assert gpkg.attribute_table_names() == ["t"]
    rows = list(gpkg.attributes("t").features())
    assert rows[0][2] == [b"\x00\x01", datetime.date(2020, 2, 29), when, "x"]
    # An aware datetime is stored in UTC.
    assert rows[1][2][2] == when
    assert rows[2][0] == 10
    with pytest.raises(TypeError):
        table.insert([(None, None, [object(), None, None, "q"])])
    with pytest.raises(g.GeoPackageError):
        table.insert([(None, None, [None, None, None, None])])  # NOT NULL


def test_lenient_reads_other_datetime_forms(path):
    gpkg = g.GeoPackage.create(path)
    gpkg.create_layer("t", None, columns=[("dt", "DATETIME")])
    gpkg.close()
    # One fractional digit, as one Esri export writes it.
    damage(path, "INSERT INTO t (dt) VALUES ('2017-08-01T00:00:00.0Z')")
    expected = datetime.datetime(2017, 8, 1, tzinfo=datetime.timezone.utc)

    with g.GeoPackage(path, read_only=True) as strict:
        with pytest.raises(g.GeoPackageError, match="invalid date/datetime"):
            list(strict.attributes("t").features())
    with g.GeoPackage(path, read_only=True, lenient=True) as lenient:
        table = lenient.attributes("t")
        assert [values for _, _, values in table.features()] == [[expected]]
        assert table.get(1)[2] == [expected]


def test_update_and_delete(points):
    layer = points.layer("pts")
    assert layer.update_attributes(5, {"name": "five", "n": 555})
    assert layer.update_geometry(5, point(500, 500))
    fid, wkb, values = layer.get(5)
    assert wkb == point(500, 500)
    assert values[:2] == ["five", 555]
    assert not layer.update_attributes(5000, {"n": 1})
    assert layer.get(5000) is None
    assert [r[0] for r in layer.features(bbox=(499, 499, 501, 501))] == [5]
    assert layer.delete([1, 2, 3, 9999]) == 3
    assert layer.count() == 97


def test_curves_pass_through(path):
    gpkg = g.GeoPackage.create(path)
    gpkg.add_epsg_srs(4326)
    layer = gpkg.create_layer("arcs", "CIRCULARSTRING", 4326)
    wkb = circular_string((0, 0), (1, 1), (2, 0))
    layer.insert([(None, wkb, [])])
    ((fid, back, values),) = layer.features()
    assert back == wkb
    # The arc's apex is at y = 1, so the envelope is exact, not the chord's.
    assert [r[0] for r in layer.features(bbox=(0.9, 0.9, 1.1, 1.1))] == [1]


def test_missing_names_raise_key_error(points):
    with pytest.raises(KeyError):
        points.layer("nope")
    with pytest.raises(KeyError):
        points.coverage("nope")
    with pytest.raises(KeyError):
        points.layer("pts").features(columns=["nope"])


def test_closed_handle(points):
    layer = points.layer("pts")
    points.close()
    assert not points.is_open
    with pytest.raises(g.GeoPackageError, match="closed"):
        layer.count()
    points.close()  # idempotent


def damage(path, *statements):
    con = sqlite3.connect(path)
    for statement in statements:
        con.execute(statement)
    con.commit()
    con.close()


def test_validate_and_rebuild_an_out_of_step_index(points, path):
    points.close()
    damage(path, "DELETE FROM rtree_pts_geom WHERE id <= 5")

    with g.GeoPackage(path, lenient=True) as gpkg:
        layer = gpkg.layer("pts")
        audit = layer.audit_spatial_index()
        assert audit["missing"] == 5 and not audit["consistent"]
        findings = [f for f in gpkg.validate() if f.table_name == "pts"]
        assert [f.code for f in findings] == ["SpatialIndexOutOfStep"]
        assert findings[0].severity == "error"
        assert "rebuild_spatial_index" in findings[0].repair
        layer.rebuild_spatial_index()
        assert layer.audit_spatial_index()["consistent"]
        assert not [f for f in gpkg.validate() if f.severity == "error"]
        assert gpkg.integrity_check() == ["ok"]


def test_repair_an_index_missing_its_triggers(points, path):
    points.close()
    con = sqlite3.connect(path)
    triggers = [
        name
        for (name,) in con.execute(
            "SELECT name FROM sqlite_master WHERE type = 'trigger' AND name LIKE 'rtree_pts_geom_%'"
        )
    ]
    con.close()
    damage(path, *(f'DROP TRIGGER "{t}"' for t in triggers))

    with g.GeoPackage(path, lenient=True) as gpkg:
        layer = gpkg.layer("pts")
        assert layer.spatial_index_status() == "stale"
        # A stale index is not trusted for reads, which stay correct.
        assert not layer.has_spatial_index()
        assert len(list(layer.features(bbox=(0, 0, 4, 4)))) == 5
        layer.repair_spatial_index()
        assert layer.spatial_index_status() == "current"


# A pre-1.4 trigger beside the 1.4 set: a complete mixed set, which is legacy.
LEGACY_TRIGGER = "CREATE TRIGGER rtree_pts_geom_update1 AFTER UPDATE OF geom ON pts BEGIN SELECT 1; END"


def test_repair_a_legacy_trigger_set(points, path):
    points.close()
    damage(path, LEGACY_TRIGGER)

    with g.GeoPackage(path) as gpkg:
        layer = gpkg.layer("pts")
        assert layer.spatial_index_status() == "legacy"
        findings = [f for f in gpkg.validate() if f.table_name == "pts"]
        assert [f.code for f in findings] == ["LegacySpatialIndexTriggers"]
        layer.repair_spatial_index()
        assert layer.spatial_index_status() == "current"


def test_srs(points):
    srs = points.srs(4326)
    assert srs.authid == "EPSG:4326"
    assert "WGS" in srs.definition
    assert points.srs(12345) is None
    assert points.add_srs(
        100000, "local grid", "NONE", 100000, "undefined", description="test"
    )
    assert points.srs(100000).authid is None
    assert {s.srs_id for s in points.srs_list()} >= {4326, 100000, 0, -1}


def png16(width, height, samples):
    """A 16-bit greyscale PNG, the payload an integer coverage stores."""

    def chunk(kind, data):
        return (
            struct.pack(">I", len(data))
            + kind
            + data
            + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
        )

    raw = b"".join(
        b"\x00" + struct.pack(f">{width}H", *samples[r * width : (r + 1) * width])
        for r in range(height)
    )
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 16, 0, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )


@pytest.fixture
def coverage(path):
    gpkg = g.GeoPackage.create(path)
    gpkg.add_epsg_srs(4326)
    cov = gpkg.create_coverage(
        "dem",
        4326,
        (0.0, 0.0, 8.0, 4.0),
        [(0, 2, 1, 4, 4)],
        scale=0.5,
        offset=100.0,
        data_null=65535,
        uom="m",
    )
    # Left tile: samples 0..15 row-major; right tile: all null but one.
    cov.put_tile(0, 0, 0, png16(4, 4, list(range(16))))
    right = [65535] * 16
    right[0] = 10
    cov.put_tile(0, 1, 0, png16(4, 4, right))
    return gpkg, cov


def test_coverage_metadata(coverage):
    gpkg, cov = coverage
    assert gpkg.coverage_names() == ["dem"]
    assert (cov.datatype, cov.scale, cov.offset, cov.data_null, cov.uom) == (
        "integer",
        0.5,
        100.0,
        65535.0,
        "m",
    )
    assert cov.extent == (0.0, 0.0, 8.0, 4.0)
    assert cov.zoom_levels == [0]
    assert cov.tile_count() == 2
    assert cov.tile_bounds(0, 1, 0) == (4.0, 0.0, 8.0, 4.0)
    cov.validate()


def test_coverage_values(coverage):
    _, cov = coverage
    # Cell centres of the left tile are at x = 0.5, 1.5, ...; row 0 is north.
    values = cov.values_at([(0.5, 3.5), (3.5, 0.5), (4.5, 3.5), (5.5, 3.5), (100, 100)])
    assert values == [100.0, 107.5, 105.0, None, None]
    width, height, samples = cov.samples(0, 0, 0)
    assert (width, height) == (4, 4)
    assert samples[:3] == [100.0, 100.5, 101.0]
    assert cov.samples(0, 1, 0)[2][1] is None
    assert cov.samples(0, 5, 5) is None
    bilinear = cov.values_at([(1.0, 3.5)], interpolation="bilinear")[0]
    assert bilinear == pytest.approx(100.25)


def test_coverage_statistics_and_requirement_21(coverage):
    _, cov = coverage
    stats = {(s["column"], s["row"]): s for s in cov.tile_statistics()}
    left = stats[(0, 0)]
    assert left["error"] is None
    assert left["calculated"]["count"] == 16
    assert left["calculated"]["min"] == 100.0
    assert left["calculated"]["max"] == 107.5
    # `put_tile(statistics=True)` recorded them.
    assert (left["min"], left["max"]) == (100.0, 107.5)
    assert stats[(1, 0)]["calculated"]["count"] == 1
    assert cov.check_samples() == []


def test_threads_can_share_a_handle(points):
    import concurrent.futures

    layer = points.layer("pts")
    with concurrent.futures.ThreadPoolExecutor(4) as pool:
        counts = list(pool.map(lambda _: layer.count(), range(20)))
    assert counts == [100] * 20
