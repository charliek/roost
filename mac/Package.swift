// swift-tools-version: 6.0
//
// Roost Mac UI — daemon-removal refactor (M4b3b onwards).
//
// Phase 2 set this up as a SwiftPM skeleton that pulled in
// grpc-swift v2 + grpc-swift-protobuf + grpc-swift-nio-transport +
// the GRPCProtobufGenerator plugin so the UI could dial the
// `roost-core` daemon over a Unix-domain socket. M4b3b ripped the
// gRPC stack out: the workspace, PTY supervisor, and IPC server
// all live in-process, and the JSON IPC server (Sources/Roost/
// IPCServer.swift) serves any external clients (roostctl, Claude
// hooks). No proto codegen, no daemon dependency, no `protoc`
// precheck — `swift build` works against a stock toolchain.

import PackageDescription

let package = Package(
    name: "Roost",
    platforms: [
        // Tier-1 macOS for the in-process Workspace + AppKit
        // ergonomics. Bumping the minimum platform is cheaper than
        // scattering `@available(macOS 15, *)` annotations across
        // every call site.
        .macOS(.v15),
    ],
    products: [
        .executable(name: "Roost", targets: ["Roost"]),
    ],
    dependencies: [
        // Sparkle 2 auto-update. The release DMG is ad-hoc-signed
        // (no Apple Developer ID yet — issue #83), so update
        // authenticity rests on Sparkle's EdDSA signature, not on a
        // Team ID. The embedded framework requires the
        // `@executable_path/../Frameworks` rpath (linkerSettings
        // below) and the `cs.disable-library-validation` entitlement
        // (Resources/Roost.entitlements) to load under the hardened
        // runtime without a matching signing team.
        .package(url: "https://github.com/sparkle-project/Sparkle", from: "2.6.0"),
    ],
    targets: [
        // libghostty-vt's C API exposed to Swift as `CGhosttyVT`.
        //
        // The headers live in the vendored Ghostty build output at
        // third_party/ghostty/out/include/ghostty/. Reaching them
        // from inside this target is fiddly:
        //
        //   * `systemLibrary` targets don't accept `cSettings`, so we
        //     can't add -I via that target type.
        //   * `cSettings: [.headerSearchPath(...)]` gets validated at
        //     manifest-parse time and SwiftPM rejects paths outside
        //     the package root (we'd need ../../../third_party/...).
        //   * `cSettings: [.unsafeFlags(["-I..."])]` works in theory
        //     but flakes on path resolution depending on clang's
        //     working directory.
        //
        // The cleanest answer: a relative symlink at
        // Sources/CGhosttyVT/include/ghostty pointing back at the
        // vendored include tree. The headers visually live inside the
        // package, SwiftPM is happy, and vt.h's own
        // `#include <ghostty/vt/types.h>` resolves because the
        // target's `publicHeadersPath = "include"` puts include/ on
        // the C compiler's search path. Sources/CGhosttyVT/include/
        // contains:
        //   * CGhosttyVT.h          -- shim that re-exports vt.h
        //   * module.modulemap      -- exposes CGhosttyVT to Swift
        //   * ghostty/  (symlink)   -- to third_party/ghostty/out/include/ghostty
        //
        // The static archive that backs the symbols lives at
        // third_party/ghostty/out/lib/libghostty-vt.a and is linked on
        // the consuming targets (Roost + RoostTests) via
        // `linkerSettings` below.
        //
        // Both the symlink target and the static archive require
        // `./third_party/ghostty/build.sh` to have run before
        // `swift build`. CI does that; local users run it once.
        .target(
            name: "CGhosttyVT",
            path: "Sources/CGhosttyVT",
            publicHeadersPath: "include"
        ),
        .executableTarget(
            name: "Roost",
            dependencies: [
                .target(name: "CGhosttyVT"),
                .product(name: "Sparkle", package: "Sparkle"),
            ],
            path: "Sources/Roost",
            exclude: [
                // Phase 2 stub directory for the grpc-swift-protobuf
                // generator plugin. M4b3b removed the plugin; the
                // directory is empty (or holds a dangling symlink
                // from the gRPC era). Excluded explicitly so SwiftPM
                // doesn't trip on the leftover artifacts in a stale
                // checkout. The directory itself is deleted by M4b3b
                // in the same commit.
            ],
            // Bundled theme files. The source-of-truth copy lives in the
            // Rust crate at `crates/roost-linux/src/resources/themes/`;
            // this is a byte-identical copy (kept in sync by
            // `make themes-check`) because SwiftPM `.copy` can't reach
            // outside the package. SwiftPM exposes them via
            // `Bundle.module.url(forResource:withExtension:)` — see
            // Theme.swift. The directory is copied (not processed) so the
            // theme names ("Dracula", "Catppuccin Mocha", "roost-dark", …)
            // resolve without renaming.
            resources: [
                .copy("Resources/themes"),
                .copy("Resources/shell-integration"),
            ],
            // Linker settings for libghostty-vt. We deliberately pass
            // the static archive's path positionally instead of using
            // `-L../third_party/ghostty/out/lib` + `-lghostty-vt`,
            // because zig's `-Demit-lib-vt=true` build emits BOTH
            // libghostty-vt.a and libghostty-vt.dylib into out/lib/.
            // macOS `ld` with `-L<dir> -lname` prefers the dylib and
            // embeds an `@rpath/libghostty-vt.dylib` reference in the
            // binary; without a matching `-Wl,-rpath` the result
            // aborts at launch with `dyld: Library not loaded:
            // @rpath/libghostty-vt.dylib / Reason: no LC_RPATH's
            // found`.
            //
            // Passing the archive as a positional argument forces
            // static linking and side-steps dyld entirely.
            //
            // The `-rpath @executable_path/../Frameworks` flag is for
            // Sparkle, NOT libghostty-vt: Sparkle.framework's install
            // name is `@rpath/Sparkle.framework/Versions/B/Sparkle`,
            // and bundle.sh embeds it under Contents/Frameworks/. The
            // binary needs this rpath entry or it aborts at launch
            // with `dyld: Library not loaded: @rpath/Sparkle.framework
            // /...`. (libghostty-vt stays static and needs no rpath.)
            linkerSettings: [
                .unsafeFlags(["../third_party/ghostty/out/lib/libghostty-vt.a"]),
                // `-Xlinker` prefixes are required: a bare `-rpath` is
                // rejected by the Swift compiler driver ("unknown
                // argument") — only `-Xlinker` passes the token straight
                // through to `ld`.
                .unsafeFlags([
                    "-Xlinker", "-rpath",
                    "-Xlinker", "@executable_path/../Frameworks",
                ]),
            ]
        ),
        .testTarget(
            name: "RoostTests",
            dependencies: ["Roost", "CGhosttyVT"],
            path: "Tests/RoostTests",
            // Tests link the static archive too because the FFI smoke
            // calls C symbols directly. Same positional-path trick as
            // the Roost target above to avoid dyld picking up the
            // dylib and producing a broken @rpath reference.
            linkerSettings: [
                .unsafeFlags(["../third_party/ghostty/out/lib/libghostty-vt.a"]),
            ]
        ),
    ]
)
