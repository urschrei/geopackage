# How to install the QGIS plugin and add a layer through it

The GeoPackage Toolkit plugin puts this library in QGIS. It adds a data
provider, `geopackage_rs`, which reads and edits GeoPackage layers through
the library instead of GDAL. It also adds Processing algorithms that
validate and repair files, and that read coverages. This guide shows you
how to install it from the project's plugin repository, and how to add a
layer through its provider.

The plugin works in QGIS 3.28 and later, on Linux (x86_64 and aarch64,
glibc 2.28 or later), macOS and Windows (x86_64). It needs nothing else
installed. It is experimental: work on copies of files you can't lose.

## Install the plugin

1. In QGIS, open *Plugins → Manage and Install Plugins*, and select
   *Settings*.
2. Tick *Show also experimental plugins*.
3. Under *Plugin Repositories*, select *Add*, and enter:
   - Name: `GeoPackage Toolkit`
   - URL: `https://urschrei.github.io/geopackage/qgis/plugins.xml`
4. Select *OK*, then *Reload all repositories*.
5. Select *All*, search for `GeoPackage Toolkit`, and select *Install
   Plugin*.

QGIS then offers each new release under *Upgradeable*.

If you can't reach the repository from QGIS, download
`gpkg_toolkit-<version>.zip` from the newest `qgis-plugin-v<version>`
release on [GitHub](https://github.com/urschrei/geopackage/releases), and
install it with *Install from ZIP*. Install the zip as it is: don't
unpack it.

## Check which provider serves the layers

Open *View → Panels → Log Messages*, and select the *GeoPackage Toolkit*
tab. At startup the plugin writes one of these:

- `Using the native data provider: <path>`: the compiled provider, built for
  your QGIS minor version, serves the layers.
- `Using the Python data provider: <reason>`: the plugin has no native build
  for your QGIS, and its Python provider serves the layers instead, more
  slowly. On QGIS 3 this is always the case.

Both serve the same layers, so a project opens with either.

## Add a layer through the provider

1. Select *Plugins → GeoPackage Toolkit → Add GeoPackage layer with this
   plugin*.
2. Choose the file. If it has more than one layer or attribute table,
   choose one.

The layer is added to the project. Its source is the same as an OGR
layer's, `/path/to/file.gpkg|layername=roads`, with the provider
`geopackage_rs`. To add one from the Python console:

```python
layer = QgsVectorLayer("/data/roads.gpkg|layername=roads", "roads", "geopackage_rs")
QgsProject.instance().addMapLayer(layer)
```

You can edit the layer as you would any other. Each commit writes through
the library, so the layer's spatial index and extent stay up to date.
Adding, deleting and renaming fields isn't possible through this provider.

## Run the algorithms

Select an algorithm from *Plugins → GeoPackage Toolkit*, or open
*Processing → Toolbox* and expand *GeoPackage Toolkit*. Both list the same
algorithms.

Start with *Validate GeoPackage*. Its `plugin_algorithm` column names the
algorithm that repairs each finding. The repair algorithms change the file
in place, so run them on a copy if you need to keep the original.

## Then

- [QGIS plugin reference](../qgis-plugin-reference.md): every algorithm's
  parameters and outputs, and how the provider serves QGIS's requests.
- [How to validate a GeoPackage and act on the findings](validate.md)
- [How to repair geometry and datetime encodings](repair-encodings.md)
