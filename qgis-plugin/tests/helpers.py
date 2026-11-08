"""Fixture builders shared by the plugin tests."""

import struct
import zlib

import geopackage_rs as g


def point(x, y):
    return struct.pack("<BIdd", 1, 1, x, y)


def points_gpkg(path, n=20):
    gpkg = g.GeoPackage.create(path)
    gpkg.add_epsg_srs(4326)
    layer = gpkg.create_layer("pts", "POINT", 4326, [("name", "TEXT")])
    layer.insert((None, point(i, i), [f"p{i}"]) for i in range(n))
    gpkg.create_layer("unindexed", "POINT", 4326, spatial_index=False)
    gpkg.close()


def png16(width, height, samples):
    def chunk(kind, data):
        crc = struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
        return struct.pack(">I", len(data)) + kind + data + crc

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


def coverage_gpkg(path):
    """A 2x1-tile integer coverage over (0, 0)-(8, 4) in EPSG:4326, with
    value = 100 + 0.5 * sample; the right tile is null but for one cell."""
    gpkg = g.GeoPackage.create(path)
    gpkg.add_epsg_srs(4326)
    cov = gpkg.create_coverage(
        "dem", 4326, (0.0, 0.0, 8.0, 4.0), [(0, 2, 1, 4, 4)],
        scale=0.5, offset=100.0, data_null=65535, uom="m",
    )
    cov.put_tile(0, 0, 0, png16(4, 4, list(range(16))))
    right = [65535] * 16
    right[0] = 10
    cov.put_tile(0, 1, 0, png16(4, 4, right))
    gpkg.close()
