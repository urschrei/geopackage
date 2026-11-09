#!/usr/bin/env python3
"""Package the QGIS plugin, with the native module for each platform inside.

    scripts/package_qgis_plugin.py WHEEL [WHEEL ...] [--native DIR] [--out DIR]

Each WHEEL is a geopackage-rs abi3 wheel (from `maturin build`). Its
`geopackage_rs` package is unpacked into `gpkg_toolkit/ext-libs/<tag>/`, where
`gpkg_toolkit/native.py` looks for it, with `<tag>` derived from the wheel's
platform tag. The result is `<out>/gpkg_toolkit-<version>.zip`, which installs
through QGIS's "Install from ZIP". The plugin's version, in `metadata.txt`,
must be the workspace's, in `Cargo.toml`: a release sets both.

`--native DIR` adds builds of the native data provider (`geopackage-qgis`),
laid out as `DIR/<tag>/qgis-<major>.<minor>/<library>`, at the same place
under `ext-libs`, where `gpkg_toolkit/native_provider.py` looks for the one
matching the running QGIS.

A QGIS plugin cannot pip-install its dependencies, so shipping every platform
in one zip is the price of a one-step install. Each native module is a few
megabytes; pass only the wheels for the platforms you want.
"""

import argparse
import configparser
import pathlib
import re
import sys
import tomllib
import zipfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
# What `cargo build` names the native provider on Linux, macOS and Windows.
NATIVE_LIBRARIES = {
    "libgeopackage_qgis.so",
    "libgeopackage_qgis.dylib",
    "geopackage_qgis.dll",
}
PLUGIN = ROOT / "qgis-plugin" / "gpkg_toolkit"


def platform_dir(wheel_name):
    """`native._platform_tag()`'s spelling for a wheel's platform tag.

    A wheel can carry a compressed tag set, joined by dots: maturin names a
    universal2 macOS wheel `macosx_10_12_universal2.macosx_10_12_x86_64.
    macosx_11_0_arm64`, and a manylinux one `manylinux_2_17_x86_64.
    manylinux2014_x86_64`. Each component is checked.
    """
    tags = wheel_name.rsplit("-", 1)[-1].removesuffix(".whl").split(".")
    if any(tag.startswith("win_amd64") for tag in tags):
        return "windows-x86_64"
    if any(tag.startswith("win_arm64") for tag in tags):
        return "windows-aarch64"
    if any(tag.startswith("macosx") for tag in tags):
        if not any(tag.endswith("universal2") for tag in tags):
            sys.exit(
                f"{wheel_name}: build macOS as universal2, which native.py expects"
            )
        return "macos-universal2"
    for tag in tags:
        match = re.search(r"(x86_64|aarch64)$", tag)
        if tag.startswith(("manylinux", "musllinux", "linux")) and match:
            return f"linux-{match.group(1)}"
    sys.exit(f"{wheel_name}: unrecognised platform tag {'.'.join(tags)!r}")


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("wheels", nargs="+", type=pathlib.Path)
    parser.add_argument(
        "--native",
        type=pathlib.Path,
        help="native provider builds, by platform and QGIS version",
    )
    parser.add_argument(
        "--out", type=pathlib.Path, default=ROOT / "target" / "qgis-plugin"
    )
    args = parser.parse_args()

    metadata = configparser.ConfigParser()
    metadata.read(PLUGIN / "metadata.txt")
    version = metadata["general"]["version"]
    with open(ROOT / "Cargo.toml", "rb") as manifest:
        workspace = tomllib.load(manifest)["workspace"]["package"]["version"]
    if version != workspace:
        sys.exit(
            f"metadata.txt gives the plugin version {version}, but Cargo.toml "
            f"gives {workspace}: set metadata.txt's version to {workspace}"
        )

    args.out.mkdir(parents=True, exist_ok=True)
    target = args.out / f"gpkg_toolkit-{version}.zip"
    seen = set()
    with zipfile.ZipFile(target, "w", zipfile.ZIP_DEFLATED) as out:
        for path in sorted(PLUGIN.rglob("*")):
            relative = path.relative_to(PLUGIN.parent)
            if path.is_dir() or "__pycache__" in path.parts or "ext-libs" in path.parts:
                continue
            out.write(path, relative.as_posix())
        # The licence texts that gpkg_toolkit/LICENSE names, from the
        # repository's root.
        for name in ("LICENSE-APACHE", "LICENSE-MIT"):
            out.write(ROOT / name, f"gpkg_toolkit/{name}")
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
        if args.native is not None:
            libraries = sorted(
                path
                for path in args.native.glob("*/qgis-*/*")
                if path.name in NATIVE_LIBRARIES and path.is_file()
            )
            if not libraries:
                sys.exit(
                    f"{args.native}: no native provider builds as <platform>/qgis-<M.m>/<library>"
                )
            for library in libraries:
                platform, version = library.parent.parent.name, library.parent.name
                if platform not in seen:
                    sys.exit(
                        f"{library}: no wheel for {platform}, whose plugin would lack its module"
                    )
                if not re.fullmatch(r"qgis-\d+\.\d+", version):
                    sys.exit(f"{library}: {version} is not qgis-<major>.<minor>")
                out.write(
                    library,
                    f"gpkg_toolkit/ext-libs/{platform}/{version}/{library.name}",
                )
                print(f"{library} -> ext-libs/{platform}/{version}")
    print(target)


if __name__ == "__main__":
    main()
