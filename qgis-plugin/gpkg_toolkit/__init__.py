"""GeoPackage Toolkit: QGIS Processing algorithms over the geopackage crate."""


def classFactory(iface):  # noqa: N802 - the name QGIS looks up
    from .plugin import GpkgToolkitPlugin

    return GpkgToolkitPlugin(iface)
