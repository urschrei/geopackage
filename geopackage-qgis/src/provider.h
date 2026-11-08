// The C++ shim of the native provider: QGIS's provider, source and iterator
// classes, forwarding to the Rust functions in lib.rs.
#pragma once

#include <cstdint>

#include "rust/cxx.h"

namespace gpkgrs {

// Registers the provider with QgsProviderRegistry under `key`. Returns false
// if a provider with that key is registered already.
bool register_provider(rust::Str key);

// The QGIS version the shim was compiled against, as QGIS writes it in
// `Qgis::versionInt()`: 33404 for 3.34.4.
int32_t built_qgis_version();

}  // namespace gpkgrs
