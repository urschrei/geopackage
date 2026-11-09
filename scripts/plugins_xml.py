#!/usr/bin/env python3
"""Write the `plugins.xml` of a QGIS plugin repository for a released plugin.

    scripts/plugins_xml.py ZIP --url URL --date DATE --out DIR --base BASE

QGIS's plugin manager reads a repository as one XML file that lists each
plugin's metadata and the URL of its zip. This repository has one plugin,
GeoPackage Toolkit, and serves the file from the book's site, so that QGIS
installs and updates the plugin from GitHub's release assets.

ZIP is a released `gpkg_toolkit-<version>.zip`. The script reads
`gpkg_toolkit/metadata.txt` from it, so the file describes the zip that
QGIS downloads, which is at URL. DATE is the release's publication time, in
ISO 8601. The script writes `plugins.xml`, and the icon that the metadata
names, to DIR. BASE is the URL at which DIR is served; the icon's URL is
made from it.
"""

import argparse
import configparser
import pathlib
import posixpath
import zipfile
from xml.etree import ElementTree

PLUGIN = "gpkg_toolkit"

# Repository element names, from the metadata keys they take their values
# from. The element names are those of plugins.qgis.org's plugins.xml, which
# QGIS's plugin manager reads.
ELEMENTS = [
    ("description", "description"),
    ("about", "about"),
    ("version", "version"),
    ("qgis_minimum_version", "qgisMinimumVersion"),
    ("qgis_maximum_version", "qgisMaximumVersion"),
    ("homepage", "homepage"),
    ("author_name", "author"),
    ("tracker", "tracker"),
    ("repository", "repository"),
    ("tags", "tags"),
    ("experimental", "experimental"),
    ("deprecated", "deprecated"),
]


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("zip", type=pathlib.Path)
    parser.add_argument("--url", required=True, help="where QGIS downloads the zip")
    parser.add_argument("--date", required=True, help="the release's publication time")
    parser.add_argument(
        "--out", type=pathlib.Path, required=True, help="directory to write to"
    )
    parser.add_argument(
        "--base", required=True, help="the URL at which --out is served"
    )
    args = parser.parse_args()

    with zipfile.ZipFile(args.zip) as plugin:
        metadata = configparser.ConfigParser(interpolation=None)
        metadata.read_string(plugin.read(f"{PLUGIN}/metadata.txt").decode("utf-8"))
        general = metadata["general"]
        icon = general.get("icon")
        icon_bytes = plugin.read(f"{PLUGIN}/{icon}") if icon else None

    root = ElementTree.Element("plugins")
    entry = ElementTree.SubElement(
        root,
        "pyqgis_plugin",
        name=general["name"],
        version=general["version"],
        plugin_id=PLUGIN,
    )
    for element, key in ELEMENTS:
        ElementTree.SubElement(entry, element).text = general.get(key, "")
    ElementTree.SubElement(entry, "file_name").text = args.zip.name
    ElementTree.SubElement(entry, "download_url").text = args.url
    ElementTree.SubElement(entry, "uploaded_by").text = general["author"]
    ElementTree.SubElement(entry, "create_date").text = args.date
    ElementTree.SubElement(entry, "update_date").text = args.date
    ElementTree.SubElement(entry, "server").text = "False"

    args.out.mkdir(parents=True, exist_ok=True)
    if icon_bytes is not None:
        name = posixpath.basename(icon)
        (args.out / name).write_bytes(icon_bytes)
        ElementTree.SubElement(entry, "icon").text = f"{args.base.rstrip('/')}/{name}"
    ElementTree.indent(root)
    ElementTree.ElementTree(root).write(
        args.out / "plugins.xml", encoding="UTF-8", xml_declaration=True
    )
    print(args.out / "plugins.xml")


if __name__ == "__main__":
    main()
