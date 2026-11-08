"""Loads the native data provider, the C++ and Rust build in ``geopackage-qgis``.

QGIS's C++ interface changes between minor versions, so a build of the native
provider works only in the QGIS minor version it was compiled against. The
plugin therefore ships one build per platform and QGIS version, as

    ext-libs/<platform tag>/qgis-<major>.<minor>/<library>

and loads the one matching the running QGIS. Where there is none, the
plugin's Python provider serves the layers instead, under the same provider
key, so a project opens with whichever of the two the machine has.

``GPKG_TOOLKIT_NATIVE_PROVIDER`` overrides the search: a path to a library to
load, or ``0`` to use the Python provider whatever is shipped.
"""

import ctypes
import os
import sys

from qgis.core import Qgis

from .native import EXT_LIBS, _platform_tag

ENV = "GPKG_TOOLKIT_NATIVE_PROVIDER"

if sys.platform.startswith("win"):
    LIBRARY = "geopackage_qgis.dll"
elif sys.platform == "darwin":
    LIBRARY = "libgeopackage_qgis.dylib"
else:
    LIBRARY = "libgeopackage_qgis.so"

# Kept loaded for the life of the process: QGIS holds the provider metadata
# the library registered, and cannot unregister it.
_LIBRARY = None
LOAD_ERROR = None


def running_version():
    """The running QGIS's version as ``(major, minor)``."""
    version = Qgis.QGIS_VERSION_INT
    return version // 10000, version // 100 % 100


def library_path():
    """The library to load, or ``None`` with no build for this QGIS."""
    override = os.environ.get(ENV, "")
    if override == "0":
        return None
    if override:
        return override
    major, minor = running_version()
    candidate = os.path.join(EXT_LIBS, _platform_tag(), f"qgis-{major}.{minor}", LIBRARY)
    return candidate if os.path.isfile(candidate) else None


def load(path=None):
    """Loads the library at `path` (by default, :func:`library_path`) and
    checks it was built for the running QGIS. Returns the library, or
    ``None``, setting :data:`LOAD_ERROR` to say why."""
    global _LIBRARY, LOAD_ERROR
    if _LIBRARY is not None:
        return _LIBRARY
    path = path or library_path()
    if path is None:
        LOAD_ERROR = "no native provider build for QGIS {}.{}".format(*running_version())
        return None
    try:
        library = ctypes.CDLL(path)
    except OSError as error:
        # Built against another QGIS's libraries, typically: their sonames
        # carry the version, so the loader refuses the mismatch here.
        LOAD_ERROR = f"cannot load {path}: {error}"
        return None
    library.geopackage_qgis_qgis_version.restype = ctypes.c_int32
    library.geopackage_qgis_register.restype = ctypes.c_bool
    library.geopackage_qgis_register_side_by_side.restype = ctypes.c_bool
    built = library.geopackage_qgis_qgis_version()
    if (built // 10000, built // 100 % 100) != running_version():
        LOAD_ERROR = (
            f"{path} was built for QGIS {built // 10000}.{built // 100 % 100}, "
            "not {}.{}".format(*running_version())
        )
        return None
    _LIBRARY = library
    LOAD_ERROR = None
    return library


def register(side_by_side=False, path=None):
    """Registers the native provider: under the plugin's key, in place of the
    Python provider, or with `side_by_side` under ``geopackage_rs_native``.
    Returns ``True`` if it is registered under that key now, whether by this
    call or an earlier one."""
    from qgis.core import QgsProviderRegistry

    key = "geopackage_rs_native" if side_by_side else "geopackage_rs"
    library = load(path)
    if library is None:
        return False
    if side_by_side:
        library.geopackage_qgis_register_side_by_side()
    else:
        library.geopackage_qgis_register()
    registry = QgsProviderRegistry.instance()
    metadata = registry.providerMetadata(key)
    # The key may be the Python provider's, registered before this call.
    return metadata is not None and "native" in metadata.description()
