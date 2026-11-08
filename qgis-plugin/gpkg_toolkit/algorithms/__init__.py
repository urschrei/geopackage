"""The Processing algorithms."""

from .coverage import (
    CheckCoverageSamplesAlgorithm,
    CoverageStatisticsAlgorithm,
    SampleCoverageAlgorithm,
)
from .export import ExportLayerAlgorithm
from .inspect import ListExtensionsAlgorithm, RepairSpatialIndexesAlgorithm, ValidateAlgorithm

ALGORITHMS = [
    ValidateAlgorithm,
    RepairSpatialIndexesAlgorithm,
    ListExtensionsAlgorithm,
    SampleCoverageAlgorithm,
    CoverageStatisticsAlgorithm,
    CheckCoverageSamplesAlgorithm,
    ExportLayerAlgorithm,
]
