// Roost Mac client — Phase 5 AppKit skeleton.
//
// First runnable Mac UI on the refactor branch. Opens a single window
// with a status panel and performs a one-shot `Identify()` handshake
// against `roost-core` over Unix domain socket. The status text updates
// live with the daemon's pid + version + protocol version on success,
// or the failure reason if the daemon isn't running.
//
// What this commit adds (vs. the previous AppKit skeleton):
//   * Real grpc-swift v2 client wired through Sources/Roost/Proto/
//     (the SwiftPM build plugin generates bindings at `swift build` time
//     from the symlinked roost.proto).
//   * Async Identify() round-trip; UI updates on the main actor.
//
// Still deferred to follow-up commits:
//   * libghostty-vt FFI from Swift.
//   * Cell renderer (Core Graphics first; Metal later if profiling demands).
//   * StreamPty + keystroke routing.
//   * Sidebar + tabs + projects (Phase 6a).
//
// To run from the repo root:
//   1. Start the daemon in another terminal:
//        cargo run -p roost-core
//   2. Then:
//        cd mac && swift run Roost
// You should see a window come up with the daemon's actual pid + version
// printed in the status panel within a second or two of launch.

import AppKit
import Foundation

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
        let socketPath = Self.defaultSocketPath()

        // Phase 5.4a: window now wraps a TerminalView. Size it to fit
        // an 80x24 grid plus the status header above; minSize keeps
        // the grid fully visible even if the user resizes down.
        let terminalView = TerminalView(cols: 80, rows: 24)
        let terminalSize = terminalView.intrinsicContentSize
        // Header slice fits the up-to-four-line "daemon: connected
        // / pid / version / active" status block above the terminal.
        let headerSliceHeight: CGFloat = 112
        let windowWidth = max(720, terminalSize.width + 48)
        let windowHeight = terminalSize.height + headerSliceHeight + 32

        let window = NSWindow(
            contentRect: NSRect(x: 200, y: 200, width: windowWidth, height: windowHeight),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "Roost"
        window.minSize = NSSize(
            width: terminalSize.width + 48,
            height: terminalSize.height + headerSliceHeight + 32
        )

        let content = NSView(frame: window.contentRect(forFrameRect: window.frame))
        content.translatesAutoresizingMaskIntoConstraints = false
        window.contentView = content

        let socketLabel = NSTextField(labelWithString: "socket: \(socketPath)")
        socketLabel.font = .monospacedSystemFont(ofSize: 11, weight: .regular)
        socketLabel.textColor = .secondaryLabelColor
        socketLabel.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(socketLabel)

        let statusLabel = NSTextField(labelWithString: "daemon: connecting…")
        statusLabel.font = .monospacedSystemFont(ofSize: 11, weight: .regular)
        statusLabel.textColor = .secondaryLabelColor
        statusLabel.translatesAutoresizingMaskIntoConstraints = false
        statusLabel.lineBreakMode = .byWordWrapping
        statusLabel.maximumNumberOfLines = 0
        content.addSubview(statusLabel)

        terminalView.translatesAutoresizingMaskIntoConstraints = false
        content.addSubview(terminalView)

        NSLayoutConstraint.activate([
            socketLabel.topAnchor.constraint(equalTo: content.topAnchor, constant: 12),
            socketLabel.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 16),
            socketLabel.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -16),

            statusLabel.topAnchor.constraint(equalTo: socketLabel.bottomAnchor, constant: 4),
            statusLabel.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 16),
            statusLabel.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -16),

            // TerminalView keeps its intrinsic 80x24-grid size as the
            // hard minimum. Letting the window grow beyond that just
            // surrounds the grid with empty content; the renderer
            // will start tracking real-window resize in 5.5.
            terminalView.topAnchor.constraint(equalTo: statusLabel.bottomAnchor, constant: 12),
            terminalView.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 16),
            terminalView.widthAnchor.constraint(greaterThanOrEqualToConstant: terminalSize.width),
            terminalView.heightAnchor.constraint(greaterThanOrEqualToConstant: terminalSize.height),
            terminalView.bottomAnchor.constraint(lessThanOrEqualTo: content.bottomAnchor, constant: -16),
        ])

        window.center()
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)

        self.window = window
        self.statusLabel = statusLabel

        // Kick off the handshake. We deliberately don't block window
        // presentation on it — if the daemon isn't running, the user
        // still sees the window come up immediately and gets a clear
        // failure message in the status panel.
        Task { [weak self] in
            let outcome = await runIdentify(socketPath: socketPath)
            await MainActor.run { [weak self] in
                self?.applyIdentifyOutcome(outcome)
            }
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }

    @MainActor
    private func applyIdentifyOutcome(_ outcome: IdentifyOutcome) {
        guard let label = statusLabel else { return }
        switch outcome {
        case .ok(let id):
            label.textColor = .labelColor
            label.stringValue = """
                daemon: connected
                  pid: \(id.pid)
                  version: \(id.daemonVersion)  (proto v\(id.protocolVersion))
                  active project: \(id.activeProjectID)  active tab: \(id.activeTabID)
                """
        case .failed(let reason):
            label.textColor = .systemRed
            label.stringValue = """
                daemon: not reachable
                  reason: \(reason)
                  hint: start it with \"cargo run -p roost-core\"
                """
        }
    }

    /// Resolve the same default socket path as `roost-core`'s
    /// `default_socket_path` for macOS — always
    /// `~/Library/Caches/roost/roost.sock` when `HOME` is set;
    /// `/tmp/roost.sock` only as a last resort.
    ///
    /// We deliberately do NOT consult `XDG_RUNTIME_DIR` here even
    /// though the daemon does on Linux. The Roost Mac client is
    /// macOS-only (Package.swift gates `.macOS(.v15)`); the daemon's
    /// macOS path is unconditionally HOME-derived. A shell that
    /// happens to export `XDG_RUNTIME_DIR` (some dev setups do)
    /// would otherwise make the UI dial a different socket than the
    /// daemon created. Both sides agreeing on the macOS default
    /// matters more than mirroring the Linux ladder.
    ///
    /// The `environment` parameter defaults to the process's
    /// environment but is injectable so unit tests can pin behavior.
    /// Empty / non-absolute `HOME` falls through to `/tmp` —
    /// matching the daemon's robustness to malformed env vars in
    /// sandboxed launchd setups.
    static func defaultSocketPath(
        environment env: [String: String] = ProcessInfo.processInfo.environment
    ) -> String {
        if let home = env["HOME"], !home.isEmpty, home.hasPrefix("/") {
            return "\(home)/Library/Caches/roost/roost.sock"
        }
        return "/tmp/roost.sock"
    }
}
