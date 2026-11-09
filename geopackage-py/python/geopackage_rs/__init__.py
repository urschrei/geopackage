"""Read, write, validate and repair OGC GeoPackage files.

A thin layer over the Rust ``geopackage`` crate. Start from
:class:`GeoPackage`::

    from geopackage_rs import GeoPackage

    with GeoPackage("roads.gpkg", read_only=True) as gpkg:
        layer = gpkg.layer("roads")
        for fid, wkb, values in layer.features(bbox=(0, 0, 10, 10)):
            ...
"""

from ._geopackage_rs import (
    Coverage,
    Extension,
    FeatureIterator,
    Field,
    Finding,
    GeometryColumn,
    GeoPackage,
    GeoPackageError,
    Layer,
    Srs,
    TileMatrix,
    __version__,
)

__all__ = [
    "Coverage",
    "Extension",
    "FeatureIterator",
    "Field",
    "Finding",
    "GeoPackage",
    "GeoPackageError",
    "GeometryColumn",
    "Layer",
    "Srs",
    "TileMatrix",
    "__version__",
]
