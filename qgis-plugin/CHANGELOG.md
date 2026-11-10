# Changelog

All notable changes to the GeoPackage Toolkit QGIS plugin are documented in
this file. The plugin's releases are tagged `qgis-plugin-vX.Y.Z`, and its
version is independent of the crates' version. For the plugin's changes up
to 0.14.4, which were released with the crates, see the repository's
[`CHANGELOG.md`](../CHANGELOG.md).

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
`scripts/package_qgis_plugin.py` copies a release's section into the
`changelog` field of the zip's `metadata.txt`, which QGIS's plugin manager
shows.

## [Unreleased]

### Changed

- The plugin has its own releases, separate from the crates'. Its version
  continues from 0.14.4.
- The Processing provider's name gives the plugin's version, such as
  *GeoPackage Toolkit 0.15.0*.
- The menu entries, algorithm help, parameter labels and messages are
  rewritten for QGIS users. *Check coverage samples (Requirement 21)* is now
  *Find NaN and infinite values in coverages*; its identifier,
  `checkcoveragesamples`, is unchanged. *Add GeoPackage layer (geopackage
  crate)* is now *Add GeoPackage layer with this plugin*. The data
  provider's description is *GeoPackage (GeoPackage Toolkit)*. In the report
  of *Repair spatial indexes*, the `action` for a replaced trigger set is
  `rebuilt with 1.4 triggers`.
- The data provider says why it cannot open a source that names no layer:
  the file has no layers or tables, or it has more than one and the source
  must add `|layername=<name>`.

[Unreleased]: https://github.com/urschrei/geopackage/compare/v0.14.4...HEAD
