// swift-tools-version: 6.0
//
// Roost Mac UI — Phase 2 SwiftPM skeleton.
//
// This package builds the Swift Mac client as an executable. Phase 5
// converts it to an Xcode project for proper .app bundling + notarization;
// for the skeleton stage SwiftPM is the simplest CI-friendly path.
//
// Dependencies are declared on grpc-swift v2 + the NIO HTTP/2 transport so
// the Mac CI job exercises full Swift package resolution. Generated proto
// bindings live under Sources/Roost/Generated/ and are produced by
// proto/gen-swift.sh (run in CI before `swift build`).

import PackageDescription

let package = Package(
    name: "Roost",
    platforms: [
        .macOS(.v14),
    ],
    products: [
        .executable(name: "Roost", targets: ["Roost"]),
    ],
    dependencies: [
        // grpc-swift v2 lives across three packages:
        //   - grpc-swift                (core: services, calls, errors)
        //   - grpc-swift-protobuf       (proto runtime, integrates swift-protobuf)
        //   - grpc-swift-nio-transport  (HTTP/2 over TCP and Unix domain socket)
        // Versions: track the 2.x line. Lock to a specific minor in CI as it
        // stabilises.
        .package(url: "https://github.com/grpc/grpc-swift.git", from: "2.0.0"),
        .package(url: "https://github.com/grpc/grpc-swift-protobuf.git", from: "2.0.0"),
        .package(url: "https://github.com/grpc/grpc-swift-nio-transport.git", from: "2.0.0"),
        .package(url: "https://github.com/apple/swift-protobuf.git", from: "1.27.0"),
    ],
    targets: [
        .executableTarget(
            name: "Roost",
            dependencies: [
                .product(name: "GRPCCore", package: "grpc-swift"),
                .product(name: "GRPCProtobuf", package: "grpc-swift-protobuf"),
                .product(name: "GRPCNIOTransportHTTP2", package: "grpc-swift-nio-transport"),
                .product(name: "SwiftProtobuf", package: "swift-protobuf"),
            ],
            path: "Sources/Roost",
            exclude: [
                // Generated bindings are produced at CI time by proto/gen-swift.sh.
                // Phase 5 adds the AppKit window + cell renderer; .xib/.storyboard
                // resources will land in this exclude list when they do.
                "Generated",
            ]
        ),
        .testTarget(
            name: "RoostTests",
            dependencies: ["Roost"],
            path: "Tests/RoostTests"
        ),
    ]
)
