// swift-tools-version: 6.0
//
// Roost Mac UI — Phase 2 SwiftPM skeleton.
//
// This package builds the Swift Mac client as an executable. Phase 5
// converts it to an Xcode project for proper .app bundling + notarization;
// for the skeleton stage SwiftPM is the simplest CI-friendly path.
//
// Dependencies are declared on grpc-swift v2 + the NIO HTTP/2 transport so
// the Mac CI job exercises full Swift package resolution. Swift bindings
// for the proto schema are generated at every `swift build` by the
// `GRPCSwiftProtobufGenerator` SwiftPM build plugin from
// `grpc-swift-protobuf` — see Sources/Roost/Proto/ for the symlinked
// .proto and the plugin's config. Nothing generated lives in VCS.

import PackageDescription

let package = Package(
    name: "Roost",
    platforms: [
        // grpc-swift-nio-transport's HTTP2ClientTransport.Posix +
        // .unixDomainSocket(path:) target are gated to macOS 15+.
        // Bumping the minimum platform is cheaper than scattering
        // `@available(macOS 15, *)` annotations across every call site.
        .macOS(.v15),
    ],
    products: [
        .executable(name: "Roost", targets: ["Roost"]),
    ],
    dependencies: [
        // grpc-swift v2 lives across three packages. Note the `-2` suffix
        // on the core URL — `https://github.com/grpc/grpc-swift.git`
        // (without the suffix) still points at v1, and pulling both
        // results in SwiftPM's "multiple similar targets" duplication
        // error since the product names overlap. Lock to the 2.x line.
        .package(url: "https://github.com/grpc/grpc-swift-2.git", from: "2.0.0"),
        .package(url: "https://github.com/grpc/grpc-swift-protobuf.git", from: "2.0.0"),
        .package(url: "https://github.com/grpc/grpc-swift-nio-transport.git", from: "2.0.0"),
        .package(url: "https://github.com/apple/swift-protobuf.git", from: "1.27.0"),
    ],
    targets: [
        // libghostty-vt's C API exposed to Swift via a module map.
        // Sources/CGhosttyVT/module.modulemap reaches up into
        // ../../../third_party/ghostty/out/include/ghostty/vt.h, which
        // means `./third_party/ghostty/build.sh` must have produced
        // the headers before `swift build` runs. The static archive
        // is linked on the consuming target (Roost) below; the module
        // map deliberately doesn't carry a `link "ghostty-vt"`
        // directive so this target stays reusable for any future
        // consumer that wants to re-export headers without re-linking.
        .systemLibrary(
            name: "CGhosttyVT",
            path: "Sources/CGhosttyVT"
        ),
        .executableTarget(
            name: "Roost",
            dependencies: [
                .target(name: "CGhosttyVT"),
                .product(name: "GRPCCore", package: "grpc-swift-2"),
                .product(name: "GRPCProtobuf", package: "grpc-swift-protobuf"),
                // Posix variant is the one that exposes Unix-domain-socket
                // targets (`HTTP2ClientTransport.Posix(target:
                // .unixDomainSocket(...), ...)`). The TransportServices
                // variant — Apple's Network.framework backend — doesn't
                // support UDS.
                .product(name: "GRPCNIOTransportHTTP2Posix", package: "grpc-swift-nio-transport"),
                .product(name: "SwiftProtobuf", package: "swift-protobuf"),
            ],
            path: "Sources/Roost",
            exclude: [
                // Phase 5 adds the AppKit window + cell renderer; .xib /
                // .storyboard resources will land in this exclude list when
                // they do.
            ],
            // Linker settings for libghostty-vt: the static archive lives
            // under ../third_party/ghostty/out/lib (relative to the
            // package directory `mac/`). Both the -L path and the
            // archive itself must exist before `swift build` runs.
            linkerSettings: [
                .unsafeFlags(["-L../third_party/ghostty/out/lib"]),
                .linkedLibrary("ghostty-vt"),
            ],
            plugins: [
                // Generates Swift bindings + client stubs from
                // Sources/Roost/Proto/roost.proto (a symlink to the
                // canonical proto/roost.proto at the repo root) at
                // `swift build` time. Configured by
                // Sources/Roost/Proto/grpc-swift-proto-generator-config.json
                // (note the `-swift-` infix — the plugin target is
                // `GRPCProtobufGenerator` but the config filename
                // it scans for is `grpc-swift-proto-generator-config.json`,
                // a deliberate decoupling on the plugin author's side).
                .plugin(
                    name: "GRPCProtobufGenerator",
                    package: "grpc-swift-protobuf"
                ),
            ]
        ),
        .testTarget(
            name: "RoostTests",
            dependencies: ["Roost", "CGhosttyVT"],
            path: "Tests/RoostTests",
            // Tests link the static archive too because the FFI smoke
            // calls C symbols directly. Same path as the Roost target.
            linkerSettings: [
                .unsafeFlags(["-L../third_party/ghostty/out/lib"]),
                .linkedLibrary("ghostty-vt"),
            ]
        ),
    ]
)
