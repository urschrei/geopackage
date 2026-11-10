"""The Processing provider."""

from qgis.core import QgsMessageLog, QgsProcessingProvider

from . import native


class GpkgToolkitProvider(QgsProcessingProvider):
    ID = "gpkgtoolkit"

    def id(self):
        return self.ID

    def name(self):
        return "GeoPackage Toolkit"

    def longName(self):  # noqa: N802
        if native.gpkg is None:
            return "GeoPackage Toolkit (not loaded)"
        return f"GeoPackage Toolkit {native.gpkg.__version__}"

    def loadAlgorithms(self):  # noqa: N802
        if native.gpkg is None:
            QgsMessageLog.logMessage(native.LOAD_ERROR, self.name())
            return
        from .algorithms import ALGORITHMS

        for algorithm in ALGORITHMS:
            self.addAlgorithm(algorithm())
