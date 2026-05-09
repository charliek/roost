// Roost Mac client — Phase 2 entry point.
//
// Doesn't open a window yet. Phase 5 wires AppKit + libghostty-vt + the
// Core Graphics / Metal cell renderer; for now this is a CLI-shaped skeleton
// that proves the SwiftPM + grpc-swift v2 + Unix-domain-socket toolchain
// works on macOS CI.
//
// Run it with `swift run Roost` from the mac/ directory once `roost-core`
// is listening; or `swift build -c release` to verify the build path.

import Foundation
import GRPCCore
import GRPCNIOTransportHTTP2

@main
struct RoostMain {
    static func main() async {
        let socket = defaultSocketPath()
        FileHandle.standardError.write(Data("[Roost.mac] socket: \(socket)\n".utf8))

        // Phase 2 sanity touch: instantiate one grpc-swift type so the
        // compiler proves the dependency is wired correctly. Phase 5
        // replaces this with a real client + AppKit window.
        let _ = RPCError(code: .unavailable, message: "Phase 2 stub")

        FileHandle.standardError.write(
            Data("[Roost.mac] Phase 2 skeleton — see docs/development/vision.md.\n".utf8)
        )
    }

    static func defaultSocketPath() -> String {
        if let xdg = ProcessInfo.processInfo.environment["XDG_RUNTIME_DIR"] {
            return "\(xdg)/roost/roost.sock"
        }
        if let home = ProcessInfo.processInfo.environment["HOME"] {
            return "\(home)/Library/Caches/roost/roost.sock"
        }
        return "/tmp/roost.sock"
    }
}
