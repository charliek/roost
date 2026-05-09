// Roost Mac client — Phase 5 AppKit skeleton.
//
// First runnable Mac UI on the refactor branch. Opens a single window
// with a status panel that shows the resolved roost-core socket path and
// a placeholder for connection state. No gRPC client wired yet — that
// lands in the next commit, once protoc-gen-grpc-swift-2 codegen is
// integrated. No terminal grid yet — that's libghostty-vt + the cell
// renderer, also follow-up commits.
//
// What this does today:
//   * Opens an `NSWindow` with title "Roost".
//   * Shows the resolved socket path (XDG/HOME-derived, matching
//     roost-core's `default_socket_path`).
//   * Shows daemon connection status as "not connected (Phase 5 stub)".
//   * Quits when the last window closes (standard Mac convention).
//
// To run from the repo root:
//   1. Start the daemon in another terminal:
//        cargo run -p roost-core
//   2. Then:
//        cd mac && swift run Roost
//
// CI exercises `swift build` + `swift test` on macos-latest; see
// .github/workflows/refactor.yml.

import AppKit
import Foundation
import GRPCCore

@main
final class RoostApp: NSObject, NSApplicationDelegate {
    private var window: NSWindow?
    private var statusLabel: NSTextField?

    static func main() {
        let app = NSApplication.shared
        let delegate = RoostApp()
        app.delegate = delegate
        app.setActivationPolicy(.regular)
        app.run()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        let window = NSWindow(
            contentRect: NSRect(x: 200, y: 200, width: 720, height: 480),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "Roost"
        window.minSize = NSSize(width: 480, height: 320)

        // Standard Mac default-window style. Phase 6a replaces this with
        // a sidebar + tab layout.
        let content = NSView(frame: window.contentRect(forFrameRect: window.frame))
        content.translatesAutoresizingMaskIntoConstraints = false
        window.contentView = content

        let header = NSTextField(labelWithString: "Roost — Phase 5 skeleton")
        header.font = .systemFont(ofSize: 18, weight: .semibold)
        header.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(header)

        let socketLabel = NSTextField(
            labelWithString: "socket: \(Self.defaultSocketPath())"
        )
        socketLabel.font = .monospacedSystemFont(ofSize: 12, weight: .regular)
        socketLabel.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(socketLabel)

        let statusLabel = NSTextField(
            labelWithString: "daemon: not connected (Phase 5 stub — gRPC client lands next commit)"
        )
        statusLabel.font = .systemFont(ofSize: 12)
        statusLabel.textColor = .secondaryLabelColor
        statusLabel.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(statusLabel)

        let visionLabel = NSTextField(
            labelWithString: "See docs/development/vision.md for the target architecture."
        )
        visionLabel.font = .systemFont(ofSize: 11)
        visionLabel.textColor = .tertiaryLabelColor
        visionLabel.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(visionLabel)

        NSLayoutConstraint.activate([
            header.topAnchor.constraint(equalTo: content.topAnchor, constant: 24),
            header.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 24),

            socketLabel.topAnchor.constraint(equalTo: header.bottomAnchor, constant: 16),
            socketLabel.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 24),
            socketLabel.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -24),

            statusLabel.topAnchor.constraint(equalTo: socketLabel.bottomAnchor, constant: 8),
            statusLabel.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 24),
            statusLabel.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -24),

            visionLabel.bottomAnchor.constraint(equalTo: content.bottomAnchor, constant: -16),
            visionLabel.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 24),
        ])

        // Phase 2 sanity touch: instantiate one grpc-swift type so the
        // compiler proves the dependency graph still resolves. Replaced
        // with a real client + Identify() call in the next commit.
        let _ = RPCError(code: .unavailable, message: "Phase 5 stub")

        window.center()
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)

        self.window = window
        self.statusLabel = statusLabel
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }

    /// Resolve the same default socket path as `roost-core`'s
    /// `default_socket_path`. Mac uses `~/Library/Caches/roost/roost.sock`;
    /// tests run on Linux runners hit the XDG branch.
    ///
    /// The `environment` parameter defaults to the process's environment
    /// but is injectable so unit tests can assert XDG → HOME → /tmp
    /// precedence without mucking with the real env.
    ///
    /// Empty or non-absolute values for the relevant variables fall
    /// through to the next branch. `XDG_RUNTIME_DIR=""` (set but empty)
    /// is a real shape in some sandboxed launchd setups, and a relative
    /// `HOME` would otherwise yield a silently-broken socket path.
    static func defaultSocketPath(
        environment env: [String: String] = ProcessInfo.processInfo.environment
    ) -> String {
        if let xdg = env["XDG_RUNTIME_DIR"], !xdg.isEmpty, xdg.hasPrefix("/") {
            return "\(xdg)/roost/roost.sock"
        }
        if let home = env["HOME"], !home.isEmpty, home.hasPrefix("/") {
            return "\(home)/Library/Caches/roost/roost.sock"
        }
        return "/tmp/roost.sock"
    }
}
