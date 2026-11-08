"""Plugin entry point: registers the data provider, the Processing provider
and a menu."""

from qgis.core import QgsApplication

from . import native
from .provider import GpkgToolkitProvider

MENU = "&GeoPackage Toolkit"

# Algorithms offered from the Plugins menu as well as the toolbox: the ones a
# user reaches for with a file in hand rather than inside a model.
MENU_ALGORITHMS = [
    ("Validate GeoPackage…", "validate"),
    ("Repair spatial indexes…", "repairspatialindexes"),
    ("List extensions…", "listextensions"),
]


def register_data_provider():
    """Registers the ``geopackage_rs`` vector provider, if the native module
    loaded. Called from both entry points, since a headless Processing run
    never calls ``initGui``."""
    if native.gpkg is None:
        return False
    from . import data_provider

    data_provider.register()
    return True


class GpkgToolkitPlugin:
    def __init__(self, iface):
        self.iface = iface
        self.provider = None
        self.actions = []

    def initProcessing(self):  # noqa: N802 - QGIS calls this name
        register_data_provider()
        self.provider = GpkgToolkitProvider()
        QgsApplication.processingRegistry().addProvider(self.provider)

    def initGui(self):  # noqa: N802
        self.initProcessing()
        from qgis.PyQt.QtWidgets import QAction

        if native.gpkg is not None:
            add_layer = QAction("Add GeoPackage layer (geopackage crate)…", self.iface.mainWindow())
            add_layer.triggered.connect(self._add_layer)
            self.iface.addPluginToMenu(MENU, add_layer)
            self.actions.append(add_layer)
        for label, name in MENU_ALGORITHMS:
            action = QAction(label, self.iface.mainWindow())
            action.triggered.connect(lambda _checked=False, n=name: self._open(n))
            self.iface.addPluginToMenu(MENU, action)
            self.actions.append(action)

    def _open(self, name):
        import processing

        processing.execAlgorithmDialog(f"{GpkgToolkitProvider.ID}:{name}")

    def _add_layer(self):
        """Asks for a file and a layer, and adds it through the crate's
        provider rather than OGR."""
        from qgis.core import QgsVectorLayer
        from qgis.PyQt.QtWidgets import QFileDialog, QInputDialog

        from .data_provider import PROVIDER_KEY, encode_uri

        window = self.iface.mainWindow()
        path, _ = QFileDialog.getOpenFileName(window, "GeoPackage", "", "GeoPackage (*.gpkg)")
        if not path:
            return
        try:
            with native.gpkg.GeoPackage(path, read_only=True, lenient=True) as gpkg:
                names = gpkg.layer_names() + gpkg.attribute_table_names()
        except native.gpkg.GeoPackageError as error:
            self.iface.messageBar().pushCritical("GeoPackage Toolkit", str(error))
            return
        if not names:
            self.iface.messageBar().pushWarning("GeoPackage Toolkit", f"{path} has no layers")
            return
        name = names[0]
        if len(names) > 1:
            name, ok = QInputDialog.getItem(window, "GeoPackage", "Layer", names, 0, False)
            if not ok:
                return
        layer = QgsVectorLayer(encode_uri({"path": path, "layerName": name}), name, PROVIDER_KEY)
        if not layer.isValid():
            error = layer.dataProvider().errors() if layer.dataProvider() else []
            self.iface.messageBar().pushCritical(
                "GeoPackage Toolkit", f"cannot open {name}: {'; '.join(error) or 'invalid layer'}"
            )
            return
        from qgis.core import QgsProject

        QgsProject.instance().addMapLayer(layer)

    def unload(self):
        for action in self.actions:
            self.iface.removePluginMenu(MENU, action)
        self.actions = []
        if self.provider is not None:
            QgsApplication.processingRegistry().removeProvider(self.provider)
            self.provider = None
        # The data provider stays registered: QGIS has no way to unregister
        # one, and layers already open still use it.
