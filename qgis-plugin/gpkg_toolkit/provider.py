"""The Processing provider."""

import configparser
import os

from qgis.core import QgsMessageLog, QgsProcessingProvider

from . import native


def plugin_version():
    """The plugin's version, from ``metadata.txt``. It is independent of
    the version of the bundled ``geopackage_rs`` module."""
    metadata = configparser.ConfigParser(interpolation=None)
    metadata.read(
        os.path.join(os.path.dirname(__file__), "metadata.txt"), encoding="utf-8"
    )
    return metadata["general"]["version"]


class GpkgToolkitProvider(QgsProcessingProvider):
    ID = "gpkgtoolkit"

    def id(self):
        return self.ID

    def name(self):
        return "GeoPackage Toolkit"

    def longName(self):  # noqa: N802
        name = f"GeoPackage Toolkit {plugin_version()}"
        return name if native.gpkg is not None else f"{name} (not loaded)"

    def loadAlgorithms(self):  # noqa: N802
        if native.gpkg is None:
            QgsMessageLog.logMessage(native.LOAD_ERROR, self.name())
            return
        from .algorithms import ALGORITHMS

        for algorithm in ALGORITHMS:
            self.addAlgorithm(algorithm())
