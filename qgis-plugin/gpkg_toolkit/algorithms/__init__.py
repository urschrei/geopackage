"""The Processing algorithms."""

from .coverage import (
    CheckCoverageSamplesAlgorithm,
    CoverageStatisticsAlgorithm,
    SampleCoverageAlgorithm,
)
from .export import ExportLayerAlgorithm
from .inspect import (
    ListExtensionsAlgorithm,
    RepairEncodingsAlgorithm,
    RepairSpatialIndexesAlgorithm,
    ValidateAlgorithm,
)

ALGORITHMS = [
    ValidateAlgorithm,
    RepairSpatialIndexesAlgorithm,
    RepairEncodingsAlgorithm,
    ListExtensionsAlgorithm,
    SampleCoverageAlgorithm,
    CoverageStatisticsAlgorithm,
    CheckCoverageSamplesAlgorithm,
    ExportLayerAlgorithm,
]
