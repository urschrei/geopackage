"""Finding and loading the native provider, and falling back without it."""

import ctypes
import os

import pytest
from qgis.core import QgsProviderRegistry

from gpkg_toolkit import native_provider
from gpkg_toolkit.data_provider import PROVIDER_KEY, register

BUILT = os.environ.get(native_provider.ENV) or os.path.join(
    os.path.dirname(__file__), "..", "..", "geopackage-qgis", "target", "release", native_provider.LIBRARY
)


@pytest.fixture
def fresh(monkeypatch):
    """The loader as if nothing had been loaded in this process yet."""
    monkeypatch.setattr(native_provider, "_LIBRARY", None)
    monkeypatch.setattr(native_provider, "LOAD_ERROR", None)
    return native_provider


def test_zero_turns_the_native_provider_off(fresh, monkeypatch):
    monkeypatch.setenv(native_provider.ENV, "0")
    assert fresh.library_path() is None
    assert fresh.load() is None
    assert "no native provider build" in fresh.LOAD_ERROR


def test_without_a_build_for_this_qgis_there_is_none(fresh, monkeypatch, tmp_path):
    monkeypatch.delenv(native_provider.ENV, raising=False)
    monkeypatch.setattr(native_provider, "EXT_LIBS", str(tmp_path))
    assert fresh.library_path() is None


def test_the_build_for_this_qgis_is_found_by_version(fresh, monkeypatch, tmp_path):
    monkeypatch.delenv(native_provider.ENV, raising=False)
    monkeypatch.setattr(native_provider, "EXT_LIBS", str(tmp_path))
    major, minor = native_provider.running_version()
    folder = tmp_path / native_provider._platform_tag() / f"qgis-{major}.{minor}"
    folder.mkdir(parents=True)
    (folder / native_provider.LIBRARY).write_bytes(b"")
    assert fresh.library_path() == str(folder / native_provider.LIBRARY)


def test_a_file_that_is_not_a_library_is_refused(fresh, tmp_path):
    bogus = tmp_path / native_provider.LIBRARY
    bogus.write_bytes(b"not a library")
    assert fresh.load(str(bogus)) is None
    assert "cannot load" in fresh.LOAD_ERROR


def _loadable():
    """Whether the built library loads into this QGIS at all: a build for
    QGIS 4 cannot, into QGIS 3, which it does not link against."""
    try:
        ctypes.CDLL(BUILT)
    except OSError:
        return False
    return True


@pytest.mark.skipif(
    not os.path.isfile(BUILT) or not _loadable(), reason="no native provider build for this QGIS"
)
def test_a_build_for_another_qgis_is_refused(fresh, monkeypatch):
    monkeypatch.setattr(native_provider, "running_version", lambda: (9, 99))
    assert fresh.load(BUILT) is None
    assert "not 9.99" in fresh.LOAD_ERROR


@pytest.mark.skipif(
    not os.path.isfile(BUILT) or not _loadable(), reason="no native provider build for this QGIS"
)
def test_the_python_provider_keeps_a_key_it_holds():
    # The plugin registers the native provider first; registered second, it
    # leaves the Python provider in place and says it is not serving the key.
    register()
    assert not native_provider.register(path=BUILT)
    metadata = QgsProviderRegistry.instance().providerMetadata(PROVIDER_KEY)
    assert "native" not in metadata.description()
