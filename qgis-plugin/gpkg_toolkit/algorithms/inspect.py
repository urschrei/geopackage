"""Validation, spatial index repair and the extension catalogue."""

from qgis.core import (
    QgsProcessingOutputNumber,
    QgsProcessingParameterBoolean,
)

from .base import Algorithm, add_row, make_fields, wrap_errors

GROUP = "Inspect and repair"
GROUP_ID = "inspect"


class ValidateAlgorithm(Algorithm):
    NAME = "validate"
    DISPLAY_NAME = "Validate GeoPackage"
    GROUP = GROUP
    GROUP_ID = GROUP_ID
    HELP = (
        "Reports what the geopackage crate can see wrong with a GeoPackage, "
        "most severe first, and the repair for each finding where there is "
        "one. The file is opened read-only and leniently and is not modified.\n\n"
        "<b>error</b>: a reader can get a wrong answer from the file. "
        "<b>warning</b>: the file breaks a rule but reads correctly. "
        "<b>advisory</b>: a choice worth knowing about, such as a layer with "
        "no spatial index.\n\n"
        "These are this crate's checks, not the OGC test suite's: no findings "
        "is not a conformance claim."
    )

    ERRORS = "ERRORS"
    WARNINGS = "WARNINGS"
    ADVISORIES = "ADVISORIES"

    FIELDS = (
        ("severity", "string"),
        ("code", "string"),
        ("table_name", "string"),
        ("message", "string"),
        ("repair", "string"),
    )

    def initAlgorithm(self, config=None):  # noqa: N802
        self.add_gpkg_parameter()
        self.add_table_sink(description="Findings")
        for name, label in (
            (self.ERRORS, "Errors"),
            (self.WARNINGS, "Warnings"),
            (self.ADVISORIES, "Advisories"),
        ):
            self.addOutput(QgsProcessingOutputNumber(name, label))

    def processAlgorithm(self, parameters, context, feedback):  # noqa: N802
        fields = make_fields(*self.FIELDS)
        sink, dest = self.sink(parameters, context, fields)
        counts = {"error": 0, "warning": 0, "advisory": 0}
        with self.open(parameters, context, read_only=True) as gpkg:
            for warning in gpkg.open_warnings():
                counts["warning"] += 1
                add_row(sink, fields, ["warning", "OpenWarning", None, warning, None])
            for finding in wrap_errors(gpkg.validate):
                counts[finding.severity] = counts.get(finding.severity, 0) + 1
                add_row(
                    sink,
                    fields,
                    [
                        finding.severity,
                        finding.code,
                        finding.table_name,
                        finding.message,
                        finding.repair,
                    ],
                )
                feedback.pushInfo(f"{finding.severity}: {finding.message}")
        if not any(counts.values()):
            feedback.pushInfo(
                "No findings (this crate's checks; the OGC ETS is the authority)."
            )
        return {
            self.OUTPUT: dest,
            self.ERRORS: counts["error"],
            self.WARNINGS: counts["warning"],
            self.ADVISORIES: counts["advisory"],
        }


class RepairSpatialIndexesAlgorithm(Algorithm):
    NAME = "repairspatialindexes"
    DISPLAY_NAME = "Repair spatial indexes"
    GROUP = GROUP
    GROUP_ID = GROUP_ID
    HELP = (
        "Audits the RTree spatial index of every feature layer, or of one "
        "named layer, and repairs what is wrong. <b>The file is modified in "
        "place</b>; close any layer that QGIS is editing from it first.\n\n"
        "A legacy index (pre-1.4 or mixed triggers) or a stale one (a table "
        "without its triggers) is replaced by the GeoPackage 1.4 trigger set "
        "and rebuilt. An index whose entries are out of step with the table "
        "is rebuilt. A layer with no index is left alone unless 'Build "
        "missing indexes' is checked, since an absent index is a choice "
        "rather than a defect."
    )

    LAYER = "LAYER"
    BUILD_MISSING = "BUILD_MISSING"
    REPAIRED = "REPAIRED"

    FIELDS = (
        ("layer", "string"),
        ("status_before", "string"),
        ("action", "string"),
        ("status_after", "string"),
        ("missing", "int"),
        ("extra", "int"),
    )

    def initAlgorithm(self, config=None):  # noqa: N802
        self.add_gpkg_parameter()
        self.add_name_parameter(self.LAYER, "Layer (all layers if empty)")
        self.addParameter(
            QgsProcessingParameterBoolean(
                self.BUILD_MISSING, "Build missing indexes", False
            )
        )
        self.add_table_sink(description="Index report")
        self.addOutput(QgsProcessingOutputNumber(self.REPAIRED, "Layers changed"))

    def processAlgorithm(self, parameters, context, feedback):  # noqa: N802
        only = self.parameterAsString(parameters, self.LAYER, context).strip()
        build_missing = self.parameterAsBool(parameters, self.BUILD_MISSING, context)
        fields = make_fields(*self.FIELDS)
        sink, dest = self.sink(parameters, context, fields)
        changed = 0
        with self.open(parameters, context, read_only=False) as gpkg:
            names = [only] if only else gpkg.layer_names()
            for i, name in enumerate(names):
                if feedback.isCanceled():
                    break
                feedback.setProgress(100 * i / max(len(names), 1))
                layer = wrap_errors(gpkg.layer, name)
                before = layer.spatial_index_status()
                audit = layer.audit_spatial_index() if before == "current" else None
                action = "none"
                if before in ("legacy", "stale"):
                    wrap_errors(layer.repair_spatial_index)
                    action = "repaired to the 1.4 trigger set"
                elif audit is not None and not audit["consistent"]:
                    wrap_errors(layer.rebuild_spatial_index)
                    action = "rebuilt"
                elif before == "absent" and build_missing:
                    wrap_errors(layer.create_spatial_index)
                    action = "built"
                after = layer.spatial_index_status()
                if action != "none":
                    changed += 1
                    feedback.pushInfo(f"{name}: {before} index {action}")
                add_row(
                    sink,
                    fields,
                    [
                        name,
                        before,
                        action,
                        after,
                        audit["missing"] if audit else None,
                        audit["extra"] if audit else None,
                    ],
                )
        if not changed:
            feedback.pushInfo("Nothing to repair.")
        return {self.OUTPUT: dest, self.REPAIRED: changed}


class ListExtensionsAlgorithm(Algorithm):
    NAME = "listextensions"
    DISPLAY_NAME = "List extensions"
    GROUP = GROUP
    GROUP_ID = GROUP_ID
    HELP = (
        "Lists the rows of gpkg_extensions, each with this crate's support "
        "for it: <b>implemented</b> (read and written correctly), "
        "<b>known</b> (recognised but not implemented; writes to a table it "
        "covers are refused if its scope is read-write), <b>removed</b> "
        "(withdrawn from the standard) or <b>unrecognised</b>."
    )

    FIELDS = (
        ("name", "string"),
        ("table_name", "string"),
        ("column_name", "string"),
        ("scope", "string"),
        ("support", "string"),
        ("definition", "string"),
    )

    def initAlgorithm(self, config=None):  # noqa: N802
        self.add_gpkg_parameter()
        self.add_table_sink(description="Extensions")

    def processAlgorithm(self, parameters, context, feedback):  # noqa: N802
        fields = make_fields(*self.FIELDS)
        sink, dest = self.sink(parameters, context, fields)
        with self.open(parameters, context, read_only=True) as gpkg:
            for row in wrap_errors(gpkg.extensions):
                add_row(
                    sink,
                    fields,
                    [
                        row.name,
                        row.table_name,
                        row.column_name,
                        row.scope,
                        row.support,
                        row.definition,
                    ],
                )
        return {self.OUTPUT: dest}
