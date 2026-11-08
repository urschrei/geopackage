"""Headless QGIS for the plugin tests.

Run with a Python that can import both ``qgis`` and ``geopackage_rs``:

    QT_QPA_PLATFORM=offscreen python -m pytest qgis-plugin/tests
"""

import atexit
import os
import sys

import pytest

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")
HERE = os.path.dirname(__file__)
sys.path.insert(0, os.path.dirname(HERE))  # the gpkg_toolkit package
sys.path.insert(0, HERE)  # helpers

# Processing ships as a core plugin; find it beside the QGIS install.
for candidate in (
    os.environ.get("QGIS_PLUGINPATH", ""),
    "/usr/share/qgis/python/plugins",
    "/usr/lib/qgis/python/plugins",
):
    if candidate and os.path.isdir(os.path.join(candidate, "processing")):
        sys.path.append(candidate)
        break

from qgis.core import QgsApplication  # noqa: E402

_app = QgsApplication([], False)
_app.initQgis()
# Shut QGIS down before the interpreter does, as `qgis.testing.start_app`
# does. Without it, QGIS 4 tears down the OGR provider's connection pool
# (which the comparisons with OGR fill) during Qt's exit-time cleanup, after
# QGIS's own statics are gone, and segfaults once the tests have passed.
atexit.register(_app.exitQgis)

from processing.core.Processing import Processing  # noqa: E402

Processing.initialize()

from gpkg_toolkit.provider import GpkgToolkitProvider  # noqa: E402

_provider = GpkgToolkitProvider()
QgsApplication.processingRegistry().addProvider(_provider)


@pytest.fixture
def run():
    """Runs one of the plugin's algorithms and returns its results."""
    import processing
    from qgis.core import QgsProcessingContext, QgsProcessingFeedback, QgsProject

    def run(name, parameters):
        context = QgsProcessingContext()
        context.setProject(QgsProject.instance())
        return processing.run(
            f"{GpkgToolkitProvider.ID}:{name}",
            parameters,
            context=context,
            feedback=QgsProcessingFeedback(),
        )

    return run
