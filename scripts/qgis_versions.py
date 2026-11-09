#!/usr/bin/env python3
"""Report QGIS 4 minor versions newer than the native provider's builds.

    scripts/qgis_versions.py [--body FILE]

The `native` job in `.github/workflows/qgis-plugin.yml` builds the provider
for each QGIS minor version in its matrix. A build works only in the minor
version it was compiled against, so each new minor version needs a matrix
entry: the release's tag, from which the job makes the header kit, and for
Windows the name and SHA-256 of OSGeo4W's `qgis-devel` archive for it.

The script reads the matrix, finds the newest release of each QGIS 4 minor
version from QGIS's `final-4_<minor>_<patch>` tags, and reports each minor
version newer than the newest in the matrix, with its OSGeo4W archive and
whether conda-forge, which CI tests against, has it. With `--body FILE` it
also writes the report as Markdown, for an issue. It exits 0 either way, and
ends with two lines for `$GITHUB_OUTPUT`: `minors=` and the new minor
versions, then `new=true` or `new=false`.
"""

import argparse
import hashlib
import json
import pathlib
import re
import subprocess
import sys
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github" / "workflows" / "qgis-plugin.yml"
OSGEO4W = "https://download.osgeo.org/osgeo4w/v2/x86_64/release/qgis/qgis/qgis-devel/"
CONDA = "https://api.anaconda.org/package/conda-forge/qgis"


def minor(version):
    major, minor_, *_ = (int(part) for part in version.split("."))
    return major, minor_


def key(version):
    return tuple(int(part) for part in version.split("."))


def built_versions():
    """The QGIS versions in the `native` job's matrix."""
    match = re.search(r"^\s+qgis: \[([^\]]*)\]", WORKFLOW.read_text(), re.MULTILINE)
    if match is None:
        sys.exit(f"{WORKFLOW}: no `qgis: [...]` matrix line")
    return re.findall(r'"([^"]+)"', match.group(1))


def released_versions():
    """The newest release of each QGIS 4 minor version, from QGIS's tags."""
    output = subprocess.run(
        [
            "git",
            "ls-remote",
            "--tags",
            "https://github.com/qgis/QGIS",
            "refs/tags/final-4_*",
        ],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    newest = {}
    for major_, minor_, patch in re.findall(
        r"refs/tags/final-(4)_(\d+)_(\d+)$", output, re.MULTILINE
    ):
        version = f"{major_}.{minor_}.{patch}"
        current = newest.get(minor(version))
        if current is None or key(version) > key(current):
            newest[minor(version)] = version
    return newest


def fetch(url):
    with urllib.request.urlopen(url, timeout=60) as response:
        return response.read()


def osgeo4w_archive(version):
    """The name of OSGeo4W's newest `qgis-devel` archive for `version`, or
    `None`."""
    listing = fetch(OSGEO4W).decode()
    builds = re.findall(rf'qgis-devel-{re.escape(version)}-(\d+)\.tar\.bz2"', listing)
    if not builds:
        return None
    return f"qgis-devel-{version}-{max(builds, key=int)}.tar.bz2"


def conda_has(version):
    """Whether conda-forge has a `qgis` package of `version`'s minor version."""
    versions = json.loads(fetch(CONDA)).get("versions", [])
    return any(
        minor(v) == minor(version)
        for v in versions
        if re.fullmatch(r"\d+\.\d+\.\d+", v)
    )


def report(version):
    """Markdown describing what a matrix entry for `version` needs."""
    tag = f"final-{version.replace('.', '_')}"
    lines = [
        f"## QGIS {'.'.join(map(str, minor(version)))}",
        "",
        f"- Newest release: {version}, tag `{tag}`.",
    ]
    archive = osgeo4w_archive(version)
    if archive is None:
        lines.append(
            f"- OSGeo4W has no `qgis-devel` archive for {version} yet; the Windows build needs one."
        )
    else:
        digest = hashlib.sha256(fetch(OSGEO4W + archive)).hexdigest()
        lines += [
            f"- OSGeo4W archive: `{archive}`, SHA-256 `{digest}`.",
            "",
            "Matrix entries for `.github/workflows/qgis-plugin.yml`:",
            "",
            "```yaml",
            f'qgis: [..., "{version}"]',
            f'- qgis: "{version}"',
            f"  osgeo4w: {archive}",
            f"  osgeo4w_sha256: {digest}",
            "```",
            "",
        ]
    if conda_has(version):
        lines.append(
            "- conda-forge has this minor version, so `ci.yml`'s `native-provider` job could test against it."
        )
    else:
        lines.append("- conda-forge does not have this minor version yet.")
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument(
        "--body", type=pathlib.Path, help="write the report as Markdown to this file"
    )
    args = parser.parse_args()

    built = built_versions()
    newest_built = max(minor(v) for v in built)
    released = released_versions()
    new = [released[m] for m in sorted(released) if m > newest_built]
    print(f"matrix: {', '.join(built)}")
    print(f"released: {', '.join(released[m] for m in sorted(released))}")
    if new:
        body = (
            "QGIS has released a minor version that the native provider has no build for. "
            "A build works only in the minor version it was compiled against, so the plugin "
            "uses its Python provider there until the `native` job's matrix has an entry for "
            "it (see `geopackage-qgis/README.md`).\n\n"
            + "\n".join(report(v) for v in new)
        )
        print(body)
        if args.body:
            args.body.write_text(body)
    print(f"minors={', '.join('.'.join(map(str, minor(v))) for v in new)}")
    print(f"new={'true' if new else 'false'}")


if __name__ == "__main__":
    main()
