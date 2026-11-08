"""Plugin entry point: registers the Processing provider and a menu."""

from qgis.core import QgsApplication

from .provider import GpkgToolkitProvider

MENU = "&GeoPackage Toolkit"

# Algorithms offered from the Plugins menu as well as the toolbox: the ones a
# user reaches for with a file in hand rather than inside a model.
MENU_ALGORITHMS = [
    ("Validate GeoPackage…", "validate"),
    ("Repair spatial indexes…", "repairspatialindexes"),
    ("List extensions…", "listextensions"),
]


class GpkgToolkitPlugin:
    def __init__(self, iface):
        self.iface = iface
        self.provider = None
        self.actions = []

    def initProcessing(self):  # noqa: N802 - QGIS calls this name
        self.provider = GpkgToolkitProvider()
        QgsApplication.processingRegistry().addProvider(self.provider)

    def initGui(self):  # noqa: N802
        self.initProcessing()
        from qgis.PyQt.QtWidgets import QAction

        for label, name in MENU_ALGORITHMS:
            action = QAction(label, self.iface.mainWindow())
            action.triggered.connect(lambda _checked=False, n=name: self._open(n))
            self.iface.addPluginToMenu(MENU, action)
            self.actions.append(action)

    def _open(self, name):
        import processing

        processing.execAlgorithmDialog(f"{GpkgToolkitProvider.ID}:{name}")

    def unload(self):
        for action in self.actions:
            self.iface.removePluginMenu(MENU, action)
        self.actions = []
        if self.provider is not None:
            QgsApplication.processingRegistry().removeProvider(self.provider)
            self.provider = None
