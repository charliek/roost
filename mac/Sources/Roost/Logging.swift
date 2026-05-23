// Logging.swift — daemon-removal refactor M4c.
//
// Process-wide logger for the Mac UI. Two outputs:
//   * `os.Logger` (Apple Unified Logging) — what `log show
//     --predicate 'process == "Roost"'` returns. Survives crashes,
//     time-stamped, integrated with Console.app. The `privacy:
//     .public` annotation defeats the default `<private>`
//     redaction so the logged strings show through.
//   * A line-appended file at the bundle profile's `logPath`
//     (`~/Library/Logs/Roost/roost.log` on Mac, `~/Library/Logs/
//     Roost-gtk/roost.log` for the gtk profile). The file is easier
//     to grep and survives reboots without spinning up Console.
//
// File writes hop to a serial `DispatchQueue` so concurrent callers
// (the @MainActor and the supervisor background queue both produce
// log lines) can't interleave a single line's bytes. The logger
// itself is nonisolated so any actor can call it without an
// `await`.

import Foundation
import os

final class RoostLogger: @unchecked Sendable {
    static let shared = RoostLogger()

    private let queue = DispatchQueue(label: "ai.stridelabs.Roost.logger")
    private var fileHandle: FileHandle?
    private let osLogger = Logger(subsystem: "ai.stridelabs.Roost", category: "app")

    private init() {}

    /// Attach to a file appender at `path`. Idempotent — calling
    /// twice swaps the underlying handle but doesn't drop pending
    /// writes (the queue serialises everything). Called once from
    /// `RoostBackend.start` after the profile is resolved.
    func attach(path: String) {
        queue.async { [weak self] in
            guard let self else { return }
            let parent = (path as NSString).deletingLastPathComponent
            try? FileManager.default.createDirectory(
                atPath: parent,
                withIntermediateDirectories: true
            )
            if !FileManager.default.fileExists(atPath: path) {
                FileManager.default.createFile(atPath: path, contents: nil)
            }
            let handle = FileHandle(forWritingAtPath: path)
            _ = try? handle?.seekToEnd()
            self.fileHandle = handle
        }
    }

    /// Detach the file appender (drops the handle so the inode can
    /// be released). Used by tests; the app itself just exits.
    func detach() {
        queue.async { [weak self] in
            try? self?.fileHandle?.close()
            self?.fileHandle = nil
        }
    }

    func info(_ message: @autoclosure () -> String) {
        let m = message()
        osLogger.info("\(m, privacy: .public)")
        appendLine(level: "info", message: m)
    }

    func warn(_ message: @autoclosure () -> String) {
        let m = message()
        osLogger.warning("\(m, privacy: .public)")
        appendLine(level: "warn", message: m)
    }

    func error(_ message: @autoclosure () -> String) {
        let m = message()
        osLogger.error("\(m, privacy: .public)")
        appendLine(level: "error", message: m)
    }

    private func appendLine(level: String, message: String) {
        let line = "\(Self.timestamp()) [\(level)] \(message)\n"
        let data = Data(line.utf8)
        queue.async { [weak self] in
            self?.fileHandle?.write(data)
        }
    }

    // ISO8601DateFormatter.string(from:) is thread-safe per docs
    // (unlike DateFormatter), but the type itself isn't marked
    // Sendable in the Foundation overlay. `nonisolated(unsafe)`
    // documents the invariant + lets Swift 6 strict concurrency
    // accept the static-let.
    nonisolated(unsafe) private static let formatter: ISO8601DateFormatter = {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return f
    }()

    private static func timestamp() -> String {
        formatter.string(from: Date())
    }
}
