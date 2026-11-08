// The C++ shim of the native provider: QGIS's provider, source and iterator
// classes, forwarding to the Rust functions in lib.rs.
#pragma once

namespace gpkgrs {

// Registers the `geopackage_rs_native` provider with QgsProviderRegistry.
// Returns false if a provider with that key is registered already.
bool register_provider();

}  // namespace gpkgrs
