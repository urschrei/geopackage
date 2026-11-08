#!/usr/bin/env python3
"""Package the QGIS plugin, with the native module for each platform inside.

    scripts/package_qgis_plugin.py WHEEL [WHEEL ...] [--out DIR]

Each WHEEL is a geopackage-rs abi3 wheel (from `maturin build`). Its
`geopackage_rs` package is unpacked into `gpkg_toolkit/ext-libs/<tag>/`, where
`gpkg_toolkit/native.py` looks for it, with `<tag>` derived from the wheel's
platform tag. The result is `<out>/gpkg_toolkit-<version>.zip`, which installs
through QGIS's "Install from ZIP".

A QGIS plugin cannot pip-install its dependencies, so shipping every platform
in one zip is the price of a one-step install. Each native module is a few
megabytes; pass only the wheels for the platforms you want.
"""

import argparse
import configparser
import pathlib
import re
import sys
import zipfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
PLUGIN = ROOT / "qgis-plugin" / "gpkg_toolkit"


def platform_dir(wheel_name):
    """`native._platform_tag()`'s spelling for a wheel's platform tag."""
    tag = wheel_name.rsplit("-", 1)[-1].removesuffix(".whl")
    if tag.startswith("win_amd64"):
        return "windows-x86_64"
    if tag.startswith("win_arm64"):
        return "windows-aarch64"
    if tag.startswith("macosx"):
        if not tag.endswith("universal2"):
            sys.exit(f"{wheel_name}: build macOS as universal2, which native.py expects")
        return "macos-universal2"
    match = re.search(r"(x86_64|aarch64)$", tag)
    if tag.startswith(("manylinux", "musllinux", "linux")) and match:
        return f"linux-{match.group(1)}"
    sys.exit(f"{wheel_name}: unrecognised platform tag {tag!r}")


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("wheels", nargs="+", type=pathlib.Path)
    parser.add_argument("--out", type=pathlib.Path, default=ROOT / "target" / "qgis-plugin")
    args = parser.parse_args()

    metadata = configparser.ConfigParser()
    metadata.read(PLUGIN / "metadata.txt")
    version = metadata["general"]["version"]

    args.out.mkdir(parents=True, exist_ok=True)
    target = args.out / f"gpkg_toolkit-{version}.zip"
    seen = set()
    with zipfile.ZipFile(target, "w", zipfile.ZIP_DEFLATED) as out:
        for path in sorted(PLUGIN.rglob("*")):
            relative = path.relative_to(PLUGIN.parent)
            if path.is_dir() or "__pycache__" in path.parts or "ext-libs" in path.parts:
                continue
            out.write(path, relative.as_posix())
        for wheel in args.wheels:
            platform = platform_dir(wheel.name)
            if platform in seen:
                sys.exit(f"two wheels for {platform}")
            seen.add(platform)
            with zipfile.ZipFile(wheel) as source:
                members = [
                    n
                    for n in source.namelist()
                    if n.startswith("geopackage_rs/") and "__pycache__" not in n
                ]
                if not members:
                    sys.exit(f"{wheel}: no geopackage_rs package inside")
                for name in members:
                    destination = f"gpkg_toolkit/ext-libs/{platform}/{name}"
                    out.writestr(destination, source.read(name))
            print(f"{wheel.name} -> ext-libs/{platform}")
    print(target)


if __name__ == "__main__":
    main()
