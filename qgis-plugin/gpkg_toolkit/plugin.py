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
    ("Repair geometry and datetime encodings…", "repairencodings"),
    ("List extensions…", "listextensions"),
]


def register_data_provider():
    """Registers the ``geopackage_rs`` vector provider: the native build for
    the running QGIS where the plugin has one, otherwise the Python provider,
    if the ``geopackage_rs`` module loaded. Both serve the same key, so a
    project opens with either. Called from both entry points, since a
    headless Processing run never calls ``initGui``. Returns which provider
    serves the key: ``"native"``, ``"python"`` or ``None``."""
    from . import native_provider

    if native_provider.register():
        _log_once(f"Using the native data provider: {native_provider.LOADED_PATH}")
        return "native"
    if native.gpkg is None:
        return None
    from . import data_provider

    data_provider.register()
    if native_provider.LOAD_ERROR:
        _log_once(f"Using the Python data provider: {native_provider.LOAD_ERROR}")
    return "python"


_LOGGED = set()


def _log_once(message):
    """Writes `message` to QGIS's message log, with the development build's
    record where there is one. QGIS calls both entry points, so each message
    is written once per session."""
    from qgis.core import QgsMessageLog

    from . import compat

    build = native.development_build()
    if build:
        message = f"{message}\nDevelopment build:\n{build}"
    if message in _LOGGED:
        return
    _LOGGED.add(message)
    QgsMessageLog.logMessage(message, "GeoPackage Toolkit", compat.MESSAGE_INFO)


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
            add_layer = QAction(
                "Add GeoPackage layer (geopackage crate)…", self.iface.mainWindow()
            )
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
        path, _ = QFileDialog.getOpenFileName(
            window, "GeoPackage", "", "GeoPackage (*.gpkg)"
        )
        if not path:
            return
        try:
            with native.gpkg.GeoPackage(path, read_only=True, lenient=True) as gpkg:
                names = gpkg.layer_names() + gpkg.attribute_table_names()
        except native.gpkg.GeoPackageError as error:
            self.iface.messageBar().pushCritical("GeoPackage Toolkit", str(error))
            return
        if not names:
            self.iface.messageBar().pushWarning(
                "GeoPackage Toolkit", f"{path} has no layers"
            )
            return
        name = names[0]
        if len(names) > 1:
            name, ok = QInputDialog.getItem(
                window, "GeoPackage", "Layer", names, 0, False
            )
            if not ok:
                return
        layer = QgsVectorLayer(
            encode_uri({"path": path, "layerName": name}), name, PROVIDER_KEY
        )
        if not layer.isValid():
            error = layer.dataProvider().errors() if layer.dataProvider() else []
            self.iface.messageBar().pushCritical(
                "GeoPackage Toolkit",
                f"cannot open {name}: {'; '.join(error) or 'invalid layer'}",
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
