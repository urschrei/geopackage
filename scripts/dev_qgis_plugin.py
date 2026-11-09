#!/usr/bin/env python3
"""Build the plugin's native parts into the repository's copy of the plugin.

    scripts/dev_qgis_plugin.py [--no-module] [--no-provider]

For a QGIS whose profile links to `qgis-plugin/gpkg_toolkit`. The script
builds `geopackage_rs` with maturin (through `uvx`) and unpacks it into
`gpkg_toolkit/ext-libs/<platform>/`. Then it builds the native data provider
and links it into `gpkg_toolkit/ext-libs/<platform>/qgis-<major>.<minor>/`,
with the QGIS version from the headers the build used. Both builds link the
SQLite of the QGIS that loads them, with the settings from
`scripts/host_sqlite.py`, and the script checks that they do. Run it again
after a change, and restart QGIS after each build.

The provider build finds QGIS's headers and Qt as `geopackage-qgis/build.rs`
does: from `QGIS_INCLUDE_DIR` and `QT_DIR`, else from `QGIS_PREFIX` or the
active conda environment. If none of these is set and `target/` holds one
header kit and one Qt, as the macOS steps in `geopackage-qgis/README.md` make
them, the script uses those.

The script records the revision it built in `ext-libs/BUILD`. The plugin
writes that record to QGIS's message log at startup.
"""

import argparse
import datetime
import os
import pathlib
import platform
import re
import shutil
import subprocess
import sys
import tempfile
import zipfile

import host_sqlite

ROOT = pathlib.Path(__file__).resolve().parent.parent
# Away from the workspace's `target`, where a build with the host's SQLite
# and a workspace build would replace each other's `libsqlite3-sys`.
MODULE_TARGET = ROOT / "target" / "qgis-plugin-module"
EXT_LIBS = ROOT / "qgis-plugin" / "gpkg_toolkit" / "ext-libs"
PROVIDER = ROOT / "geopackage-qgis"


def platform_tag():
    """The `ext-libs` directory for this machine, as
    `gpkg_toolkit.native._platform_tag` names it."""
    machine = platform.machine().lower()
    machine = {"amd64": "x86_64", "arm64": "aarch64"}.get(machine, machine)
    if sys.platform.startswith("win"):
        return f"windows-{machine}"
    if sys.platform == "darwin":
        return "macos-universal2"
    return f"linux-{machine}"


def library_name():
    if sys.platform.startswith("win"):
        return "geopackage_qgis.dll"
    if sys.platform == "darwin":
        return "libgeopackage_qgis.dylib"
    return "libgeopackage_qgis.so"


def run(command, **kwargs):
    print("+", " ".join(str(part) for part in command), flush=True)
    subprocess.run(command, check=True, **kwargs)


def install_module(tag):
    """Builds `geopackage_rs` and replaces the copy in `ext-libs/<tag>`."""
    command = ["uvx", "maturin", "build", "--release", "-m", "geopackage-py/Cargo.toml"]
    if not sys.platform.startswith("win"):
        # The host's SQLite, which the wheel must not carry a copy of.
        command += ["--no-default-features", "--auditwheel", "skip"]
    env = dict(
        os.environ, CARGO_TARGET_DIR=str(MODULE_TARGET), **host_sqlite.settings()
    )
    with tempfile.TemporaryDirectory() as out:
        run([*command, "-o", out], cwd=ROOT, env=env)
        (wheel,) = pathlib.Path(out).glob("*.whl")
        destination = EXT_LIBS / tag
        shutil.rmtree(destination / "geopackage_rs", ignore_errors=True)
        with zipfile.ZipFile(wheel) as source:
            members = [
                name
                for name in source.namelist()
                if name.startswith("geopackage_rs/") and "__pycache__" not in name
            ]
            source.extractall(destination, members)
    print(f"{wheel.name} -> {destination.relative_to(ROOT)}")
    for module in (destination / "geopackage_rs").iterdir():
        if module.suffix in (".so", ".pyd"):
            host_sqlite.check(str(module))


def default_environment():
    """`QGIS_INCLUDE_DIR` and `QT_DIR` from `target/`, where neither is set
    nor can be found from a QGIS or conda prefix."""
    env = dict(os.environ)
    if any(
        env.get(name) for name in ("QGIS_INCLUDE_DIR", "QGIS_PREFIX", "CONDA_PREFIX")
    ):
        return env
    kits = sorted((ROOT / "target").glob("qgis-*-include"))
    qts = sorted((ROOT / "target" / "qt").glob("*/macos"))
    if len(kits) == 1 and len(qts) == 1 and not env.get("QT_DIR"):
        env["QGIS_INCLUDE_DIR"] = str(kits[0])
        env["QT_DIR"] = str(qts[0])
        print(f"QGIS_INCLUDE_DIR={kits[0]}\nQT_DIR={qts[0]}")
    return env


def qgis_include_dir(env):
    """The directory with `qgsconfig.h` that `geopackage-qgis/build.rs`
    selects for `env`."""
    if env.get("QGIS_INCLUDE_DIR"):
        return pathlib.Path(env["QGIS_INCLUDE_DIR"])
    windows = sys.platform.startswith("win")
    if env.get("QGIS_PREFIX"):
        prefix = pathlib.Path(env["QGIS_PREFIX"])
    elif env.get("CONDA_PREFIX"):
        prefix = pathlib.Path(env["CONDA_PREFIX"]) / ("Library" if windows else "")
    else:
        prefix = pathlib.Path("/usr")
    for candidate in (prefix / "include" / "qgis", prefix / "include"):
        if (candidate / "qgsconfig.h").is_file():
            return candidate
    sys.exit(
        f"no QGIS headers under {prefix}: set QGIS_INCLUDE_DIR (see geopackage-qgis/README.md)"
    )


def qgis_minor_version(include_dir):
    """`(major, minor)` from `_QGIS_VERSION_INT` in `qgsconfig.h`, the value
    that the build's `geopackage_qgis_qgis_version` returns."""
    config = (include_dir / "qgsconfig.h").read_text(encoding="utf-8")
    match = re.search(r"#define\s+_QGIS_VERSION_INT\s+(\d+)", config)
    if match is None:
        sys.exit(f"{include_dir / 'qgsconfig.h'}: no _QGIS_VERSION_INT")
    version = int(match.group(1))
    return version // 10000, version // 100 % 100


def install_provider(tag):
    """Builds the native provider and links it into
    `ext-libs/<tag>/qgis-<major>.<minor>`. Returns that QGIS version."""
    env = dict(default_environment(), **host_sqlite.settings())
    major, minor = qgis_minor_version(qgis_include_dir(env))
    run(
        ["cargo", "build", "--release", "--manifest-path", PROVIDER / "Cargo.toml"],
        env=env,
    )
    library = PROVIDER / "target" / "release" / library_name()
    host_sqlite.check(str(library))
    destination = EXT_LIBS / tag / f"qgis-{major}.{minor}" / library.name
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.unlink(missing_ok=True)
    try:
        destination.symlink_to(library)
        print(f"{destination.relative_to(ROOT)} -> {library.relative_to(ROOT)}")
    except OSError:
        # Windows without the privilege to make symbolic links.
        shutil.copy2(library, destination)
        print(f"{library.relative_to(ROOT)} copied to {destination.relative_to(ROOT)}")
    return major, minor


def revision():
    """The working copy's revision, from jj, else git."""
    for command in (
        [
            "jj",
            "log",
            "--no-graph",
            "-r",
            "@",
            "-T",
            'change_id.short(8) ++ " (commit " ++ commit_id.short(8) ++ ")"',
        ],
        ["git", "describe", "--always", "--dirty"],
    ):
        try:
            result = subprocess.run(
                command, cwd=ROOT, capture_output=True, text=True, check=True
            )
        except (OSError, subprocess.CalledProcessError):
            continue
        return result.stdout.strip()
    return "an unknown revision"


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument(
        "--no-module", action="store_true", help="do not build geopackage_rs"
    )
    parser.add_argument(
        "--no-provider", action="store_true", help="do not build the native provider"
    )
    args = parser.parse_args()

    tag = platform_tag()
    built = []
    if not args.no_module:
        install_module(tag)
        built.append("geopackage_rs")
    if not args.no_provider:
        major, minor = install_provider(tag)
        built.append(f"native provider for QGIS {major}.{minor}")
    if not built:
        return
    # One line per part, so that a build of one part keeps the other's line.
    record = EXT_LIBS / "BUILD"
    try:
        lines = record.read_text(encoding="utf-8").splitlines()
    except OSError:
        lines = []
    stamp = datetime.datetime.now().isoformat(timespec="minutes")
    source = f"built from {revision()} at {stamp}"
    lines = [
        line
        for line in lines
        if not line.startswith(("geopackage_rs:", "native provider"))
    ]
    lines = [f"{part}: {source}" for part in built] + lines
    record.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print("\n".join(lines))
    print("Restart QGIS to load the new build.")


if __name__ == "__main__":
    main()
