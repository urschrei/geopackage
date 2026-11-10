"""The Plugins menu entries for the algorithms."""

from gpkg_toolkit import plugin
from gpkg_toolkit.algorithms import ALGORITHMS


def test_menu_has_every_algorithm():
    entries = [entry for entry in plugin.menu_algorithms() if entry is not None]
    assert [name for _label, name in entries] == [a.NAME for a in ALGORITHMS]
    assert [label for label, _name in entries] == [
        f"{a.DISPLAY_NAME}…" for a in ALGORITHMS
    ]


def test_menu_separates_groups():
    groups = {a.NAME: a.GROUP_ID for a in ALGORITHMS}
    runs = [[]]
    for entry in plugin.menu_algorithms():
        if entry is None:
            runs.append([])
        else:
            runs[-1].append(groups[entry[1]])
    assert all(len(set(run)) == 1 for run in runs)
    assert len(runs) == len(set(groups.values()))
