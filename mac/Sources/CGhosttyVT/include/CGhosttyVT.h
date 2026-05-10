// Shim header that re-exports libghostty-vt's C API as the `CGhosttyVT`
// Swift module. The real header lives in the vendored Ghostty build
// output at third_party/ghostty/out/include/ghostty/vt.h. We don't
// reference it directly via a relative path because vt.h itself does
// `#include <ghostty/vt/types.h>` (sibling header), and that
// angle-bracket include only resolves when the include search path
// has third_party/ghostty/out/include/ on it. SwiftPM's
// `systemLibrary` target type doesn't accept `cSettings`, which is
// why this is a regular `.target` with `cSettings: [.headerSearchPath(...)]`
// in Package.swift.
//
// Both this shim and the third_party/ghostty/out/lib/libghostty-vt.a
// static archive must exist before `swift build` runs. CI builds them
// in the swift-mac job's "Build libghostty-vt" step; local users run
// `./third_party/ghostty/build.sh` from the repo root.

#pragma once

#include "ghostty/vt.h"
