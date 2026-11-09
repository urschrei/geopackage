"""Loads the ``geopackage_rs`` native module.

A plugin cannot ``pip install``, so the module ships inside it: the packaging
script unpacks one abi3 wheel per platform into ``ext-libs/<platform tag>``.
An installed ``geopackage_rs`` (in a development environment, say) takes
precedence over the bundled copies.
"""

import os
import platform
import sys

EXT_LIBS = os.path.join(os.path.dirname(__file__), "ext-libs")


def _platform_tag():
    machine = platform.machine().lower()
    machine = {"amd64": "x86_64", "arm64": "aarch64"}.get(machine, machine)
    if sys.platform.startswith("win"):
        return f"windows-{machine}"
    if sys.platform == "darwin":
        return "macos-universal2"
    return f"linux-{machine}"


def _load():
    try:
        import geopackage_rs

        return geopackage_rs, None
    except ImportError:
        pass
    candidate = os.path.join(EXT_LIBS, _platform_tag())
    if os.path.isdir(candidate) and candidate not in sys.path:
        sys.path.insert(0, candidate)
    try:
        import geopackage_rs

        return geopackage_rs, None
    except ImportError as error:
        return None, (
            f"The geopackage_rs native module is not available for this "
            f"platform ({_platform_tag()}): {error}. Install the plugin "
            f"package built for your platform, or `pip install geopackage-rs` "
            f"into QGIS's Python."
        )


def development_build():
    """What ``scripts/dev_qgis_plugin.py`` recorded in ``ext-libs/BUILD``:
    the revision each part was built from. ``None`` for a released plugin."""
    try:
        with open(os.path.join(EXT_LIBS, "BUILD"), encoding="utf-8") as record:
            return record.read().strip() or None
    except OSError:
        return None


gpkg, LOAD_ERROR = _load()
