"""Names the DLL imports Windows cannot resolve for a module that fails with
"DLL load failed ... The specified procedure could not be found".

    python scripts/ci/missing_imports.py PATH [PATH ...]

For each PE file, and recursively for each DLL it imports, finds the DLL
Windows would load (beside the importer, then on PATH, then System32) and
checks that it exports every function imported from it. Prints the DLL path and the missing names. Needs
`pefile` (pip install pefile). Diagnostic only: it exits 0.
"""

import os
import sys

import pefile

# pefile stops reading an export table after 8,192 entries by default, and
# Qt's and GDAL's DLLs export far more: without this, every late name reads
# as missing.
pefile.MAX_SYMBOL_EXPORT_COUNT = 1 << 20

SKIP = ("api-ms-win-", "ext-ms-win-")


def resolve(name, near):
    """The file Windows would load for `name`: beside the importer, then on PATH."""
    for directory in [near, *os.environ.get("PATH", "").split(os.pathsep)]:
        candidate = os.path.join(directory, name)
        if os.path.isfile(candidate):
            return candidate
    system = os.path.join(os.environ.get("SystemRoot", r"C:\Windows"), "System32", name)
    return system if os.path.isfile(system) else None


def exports(path):
    pe = pefile.PE(path, fast_load=True)
    pe.parse_data_directories(directories=[pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_EXPORT"]])
    names = set()
    if hasattr(pe, "DIRECTORY_ENTRY_EXPORT"):
        for symbol in pe.DIRECTORY_ENTRY_EXPORT.symbols:
            if symbol.name:
                names.add(symbol.name.decode())
    return names


def check(path, seen):
    if path.lower() in seen:
        return
    seen.add(path.lower())
    pe = pefile.PE(path, fast_load=True)
    pe.parse_data_directories(directories=[pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_IMPORT"]])
    for entry in getattr(pe, "DIRECTORY_ENTRY_IMPORT", []):
        name = entry.dll.decode()
        if name.lower().startswith(SKIP):
            continue
        target = resolve(name, os.path.dirname(path))
        if target is None:
            print(f"{path}: imports {name}, which is not on PATH")
            continue
        available = exports(target)
        missing = [i.name.decode() for i in entry.imports if i.name and i.name.decode() not in available]
        if missing:
            print(f"{path}: {len(missing)} functions missing from {target}:")
            for function in missing[:15]:
                print(f"    {function}")
        check(target, seen)


seen = set()
for root in sys.argv[1:]:
    check(os.path.abspath(root), seen)
print(f"checked {len(seen)} files")
