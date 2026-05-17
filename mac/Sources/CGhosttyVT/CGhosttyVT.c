// Empty translation unit. SwiftPM requires at least one source file in a
// regular `.target`. The actual API surface is provided by the
// libghostty-vt static archive linked via the consuming target's
// `linkerSettings` in Package.swift; this file produces no useful
// symbols, only a placeholder .o so the target compiles to a static
// library SwiftPM can hand to downstream Swift modules.

#include "CGhosttyVT.h"
