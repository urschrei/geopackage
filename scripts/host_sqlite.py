#!/usr/bin/env python3
"""Link settings that make a build use the SQLite of the QGIS that loads it.

    scripts/host_sqlite.py [--github-env]
    scripts/host_sqlite.py --check LIBRARY|WHEEL [LIBRARY|WHEEL ...]

For the QGIS plugin's native parts on Linux and macOS: `geopackage_rs`
(built with `--no-default-features`) and the native provider. A process
with two copies of SQLite loses its locks: one copy does not see the locks
that the other holds, and closing a file in one copy releases the locks
that the other holds on that file. QGIS, GDAL and QGIS's Python share one
copy, so these parts link that copy instead of a copy of their own.

On Linux, a build links `libsqlite3.so.0`, which the loader resolves to the
library that QGIS loaded already. This needs no settings.

On macOS, the build must name the library as QGIS.app does,
`@rpath/libsqlite3.dylib`, and dyld then uses the library that QGIS loaded.
The script writes a link stub with that name to `target/host-sqlite`, from
the symbols in the macOS SDK's `libsqlite3.tbd`, and gives the settings that
make `libsqlite3-sys` link it: `SQLITE3_LIB_DIR`, and `SQLITE3_NO_PKG_CONFIG`
so that pkg-config does not select another SQLite.

The script prints the settings as `NAME=value` lines, or with `--github-env`
appends them to the file that `$GITHUB_ENV` names. `--check` fails unless
each library given, or the module in each `geopackage_rs` wheel given, links
the host's SQLite. Windows needs none: there
the parts keep their own SQLite, because Windows keeps a lock for each file
handle, and a copy in the same process sees it.
"""

import argparse
import os
import pathlib
import re
import subprocess
import sys
import tempfile
import zipfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
STUB_DIR = ROOT / "target" / "host-sqlite"
INSTALL_NAME = "@rpath/libsqlite3.dylib"


def macos_stub():
    """Writes `libsqlite3.tbd` for arm64 and x86_64 to `STUB_DIR`, and
    returns `STUB_DIR`."""
    sdk = subprocess.run(
        ["xcrun", "--show-sdk-path"], capture_output=True, text=True, check=True
    ).stdout.strip()
    apple = (pathlib.Path(sdk) / "usr" / "lib" / "libsqlite3.tbd").read_text()
    # The public API, sessions included: Apple's additions start
    # `_sqlite3_apple` or carry a double underscore.
    symbols = sorted(
        {
            name
            for name in re.findall(r"\b(_sqlite3\w+)", apple)
            if not name.startswith("_sqlite3_apple") and "__" not in name[1:]
        }
    )
    if not symbols:
        sys.exit(f"{sdk}: no SQLite symbols in libsqlite3.tbd")
    # compatibility-version 0, as QGIS.app's library gives: dyld refuses a
    # library older than the version that the stub records.
    stub = "\n".join(
        [
            "--- !tapi-tbd",
            "tbd-version:     4",
            "targets:         [ x86_64-macos, arm64-macos ]",
            f"install-name:    '{INSTALL_NAME}'",
            "current-version: 0",
            "compatibility-version: 0",
            "exports:",
            "  - targets:         [ x86_64-macos, arm64-macos ]",
            f"    symbols:         [ {', '.join(symbols)} ]",
            "...",
            "",
        ]
    )
    STUB_DIR.mkdir(parents=True, exist_ok=True)
    (STUB_DIR / "libsqlite3.tbd").write_text(stub)
    return STUB_DIR


def settings():
    """The environment for this platform's build, as a dict."""
    if sys.platform == "darwin":
        return {"SQLITE3_LIB_DIR": str(macos_stub()), "SQLITE3_NO_PKG_CONFIG": "1"}
    return {}


def check(library, label=None):
    """Exits unless `library` links the host's SQLite: on macOS by
    `INSTALL_NAME`, on Linux by soname. Messages name it `label`, if given."""
    label = label or library
    if sys.platform == "darwin":
        command, wanted = ["otool", "-L", library], INSTALL_NAME
    elif sys.platform.startswith("linux"):
        command, wanted = ["objdump", "-p", library], "libsqlite3.so.0"
    else:
        return
    output = subprocess.run(command, capture_output=True, text=True, check=True).stdout
    if wanted not in output:
        dependencies = [
            line.strip()
            for line in output.splitlines()
            if "NEEDED" in line or (sys.platform == "darwin" and line.startswith("\t"))
        ]
        symbols = subprocess.run(
            ["nm", library], capture_output=True, text=True, check=False
        ).stdout
        opens = [
            line.strip() for line in symbols.splitlines() if "sqlite3_open_v2" in line
        ]
        sys.exit(
            f"{label} does not link {wanted}, the SQLite of the QGIS that loads it.\n"
            f"Dependencies: {dependencies}\n"
            f"sqlite3_open_v2 (U: referenced, t or T: compiled in): {opens}"
        )
    print(f"{label} links {wanted}")


def check_wheel(wheel):
    """Checks the compiled module in a `geopackage_rs` wheel."""
    with zipfile.ZipFile(wheel) as source, tempfile.TemporaryDirectory() as out:
        modules = [
            name
            for name in source.namelist()
            if name.startswith("geopackage_rs/_geopackage_rs")
            and name.endswith((".so", ".pyd"))
        ]
        if not modules:
            sys.exit(f"{wheel}: no compiled geopackage_rs module inside")
        for name in modules:
            check(source.extract(name, out), f"{wheel}: {name}")


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument(
        "--github-env", action="store_true", help="append to the file $GITHUB_ENV names"
    )
    parser.add_argument(
        "--check",
        nargs="+",
        metavar="LIBRARY",
        help="check that each library, or the module in each wheel, links the host's SQLite",
    )
    args = parser.parse_args()
    if args.check:
        for path in args.check:
            if path.endswith(".whl"):
                check_wheel(path)
            else:
                check(path)
        return
    lines = [f"{name}={value}" for name, value in settings().items()]
    if args.github_env:
        with open(os.environ["GITHUB_ENV"], "a", encoding="utf-8") as env:
            env.writelines(line + "\n" for line in lines)
    for line in lines:
        print(line)


if __name__ == "__main__":
    main()
