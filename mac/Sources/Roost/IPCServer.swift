// IPCServer.swift — daemon-removal refactor M4b3.
//
// Newline-delimited JSON over Unix-domain socket. Mirrors the
// Rust IpcServer in `crates/roost-ipc/src/server.rs`:
//   * One process-wide listener bound at the bundle profile's
//     `socketPath`.
//   * Each accepted connection runs its own read loop.
//   * Frames are read with the 16 MiB cap; responses are JSON +
//     '\n'.
//   * Handler dispatch hops to `@MainActor` before mutating the
//     workspace.
//
// Uses Darwin sockets directly rather than NWListener — NWListener
// on Unix-domain sockets is fragile (the path-vs-endpoint shape
// is finicky and the connection's queue semantics around frame
// boundaries are easier to get wrong than to get right).

import Darwin
import Foundation

@MainActor
final class IPCServer {
    private var listenFD: Int32 = -1
    private let socketPath: String
    private let handler: IPCHandler

    init(socketPath: String, handler: IPCHandler) throws {
        self.socketPath = socketPath
        self.handler = handler

        // Best-effort: nuke a stale socket from a prior run. The
        // flock-based single-instance lock in main is the
        // authoritative "is anyone alive?" check; if we're here,
        // we own the slot and any leftover socket file is stale.
        try? FileManager.default.removeItem(atPath: socketPath)

        // Make sure the parent directory exists.
        let parent = (socketPath as NSString).deletingLastPathComponent
        try? FileManager.default.createDirectory(
            atPath: parent,
            withIntermediateDirectories: true
        )

        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        if fd < 0 {
            throw IPCServerError.socketCreate(errno: errno)
        }

        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let pathBytes = Array(socketPath.utf8)
        if pathBytes.count >= MemoryLayout.size(ofValue: addr.sun_path) {
            Darwin.close(fd)
            throw IPCServerError.pathTooLong(socketPath)
        }
        withUnsafeMutablePointer(to: &addr.sun_path) { ptr in
            ptr.withMemoryRebound(to: CChar.self, capacity: pathBytes.count + 1) { c in
                for (i, b) in pathBytes.enumerated() {
                    c[i] = CChar(b)
                }
                c[pathBytes.count] = 0
            }
        }

        let bindResult = withUnsafePointer(to: &addr) { addrPtr in
            addrPtr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                bind(fd, sa, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        if bindResult < 0 {
            let e = errno
            Darwin.close(fd)
            throw IPCServerError.bind(path: socketPath, errno: e)
        }

        if listen(fd, 32) < 0 {
            let e = errno
            Darwin.close(fd)
            throw IPCServerError.listen(errno: e)
        }

        chmod(socketPath, 0o600)
        self.listenFD = fd
    }

    deinit {
        if listenFD >= 0 {
            Darwin.close(listenFD)
        }
        try? FileManager.default.removeItem(atPath: socketPath)
    }

    /// Begin accepting connections on a background queue.
    /// Returns immediately. The accept loop runs on a detached
    /// task so it cannot block the main actor — CR-flagged on
    /// PR #78.
    nonisolated func start() {
        // Snapshot the actor-owned fields onto the detached task.
        let fdTask = Task { @MainActor in self.listenFD }
        let handlerTask = Task { @MainActor in self.handler }
        Task.detached {
            let listenFD = await fdTask.value
            let handler = await handlerTask.value
            IPCServer.acceptLoop(listenFD: listenFD, handler: handler)
        }
    }

    private nonisolated static func acceptLoop(listenFD: Int32, handler: IPCHandler) {
        while listenFD >= 0 {
            let conn = accept(listenFD, nil, nil)
            if conn < 0 {
                if errno == EINTR { continue }
                NSLog("ipc: accept failed: \(errno)")
                return
            }
            // Hand the connection to a per-connection task.
            Task.detached {
                await IPCServer.serveConnection(fd: conn, handler: handler)
            }
        }
    }

    private nonisolated static func serveConnection(fd: Int32, handler: IPCHandler) async {
        defer { Darwin.close(fd) }
        var reader = FrameReader(fd: fd)
        while true {
            do {
                guard let line = try reader.readLine() else { return }
                let response = await IPCServer.dispatch(line: line, handler: handler)
                let body = try JSONEncoder().encode(response) + Data([0x0a])
                if !writeAll(fd: fd, data: body) {
                    // Partial-write retry exhausted or hard error
                    // — bail; the client will reconnect.
                    return
                }
            } catch {
                NSLog("ipc: connection error: \(error)")
                return
            }
        }
    }

    /// Write `data` in full, retrying on EINTR and handling
    /// partial writes by advancing the offset. Returns false on
    /// unrecoverable error. CR-flagged the prior single-write
    /// call on PR #78.
    private nonisolated static func writeAll(fd: Int32, data: Data) -> Bool {
        var offset = 0
        let total = data.count
        return data.withUnsafeBytes { buf -> Bool in
            guard let base = buf.baseAddress else { return true }
            while offset < total {
                let remaining = total - offset
                let written = Darwin.write(fd, base.advanced(by: offset), remaining)
                if written < 0 {
                    if errno == EINTR { continue }
                    NSLog("ipc: write failed: \(errno)")
                    return false
                }
                if written == 0 {
                    // 0 from write() on a regular fd is unusual;
                    // treat as a peer disconnect.
                    return false
                }
                offset += written
            }
            return true
        }
    }

    private nonisolated static func dispatch(
        line: Data, handler: IPCHandler
    ) async -> IPCResponse {
        let request: IPCRequest
        do {
            request = try JSONDecoder().decode(IPCRequest.self, from: line)
        } catch {
            return IPCResponse.failure(
                id: 0, code: "parse-error",
                message: "envelope decode failed: \(error)"
            )
        }
        do {
            let result = try await handler.handle(op: request.op, params: request.params)
            return IPCResponse.success(id: request.id, result: result)
        } catch let err as IPCHandlerError {
            return IPCResponse.failure(id: request.id, code: err.code, message: err.message)
        } catch {
            return IPCResponse.failure(
                id: request.id, code: "internal", message: "\(error)"
            )
        }
    }
}

// MARK: - Handler

/// Handler abstraction. The Mac UI's wiring lives in
/// `RoostApp.applicationDidFinishLaunching`, which constructs an
/// `IPCHandlerImpl` over the shared `LocalClient`.
protocol IPCHandler: Sendable {
    func handle(op: String, params: AnyCodable?) async throws -> AnyCodable?
}

struct IPCHandlerError: Error, CustomStringConvertible {
    let code: String
    let message: String

    var description: String { "\(code): \(message)" }

    static func unknownOp(_ op: String) -> IPCHandlerError {
        IPCHandlerError(code: "unknown-op", message: "no such op: \(op)")
    }

    static func invalidParam(_ message: String) -> IPCHandlerError {
        IPCHandlerError(code: "invalid-param", message: message)
    }

    static func notFound(_ message: String) -> IPCHandlerError {
        IPCHandlerError(code: "not-found", message: message)
    }

    static func internalError(_ message: String) -> IPCHandlerError {
        IPCHandlerError(code: "internal", message: message)
    }
}

// MARK: - Framing

/// Newline-delimited frame reader. Mirrors the Rust `FrameReader`
/// in `crates/roost-ipc/src/framing.rs`. 16 MiB line cap.
private struct FrameReader {
    let fd: Int32
    var pending: Data = Data()
    var scanCursor: Int = 0

    mutating func readLine() throws -> Data? {
        while true {
            // Look for the next newline starting at the cursor —
            // the cursor advance ensures we don't re-scan bytes
            // we already inspected. Same O(n²) protection as
            // the Rust side.
            if scanCursor < pending.count {
                if let pos = pending[scanCursor...].firstIndex(of: 0x0a) {
                    let line = pending[..<pos]
                    let rest = pending[(pos + 1)...]
                    let lineData = Data(line)
                    pending = Data(rest)
                    scanCursor = 0
                    if lineData.count > ipcMaxFrameBytes {
                        throw IPCServerError.frameTooLarge
                    }
                    return lineData
                }
                scanCursor = pending.count
            }
            if pending.count > ipcMaxFrameBytes {
                throw IPCServerError.frameTooLarge
            }
            var buf = [UInt8](repeating: 0, count: 65536)
            let n = buf.withUnsafeMutableBufferPointer { ptr -> Int in
                Darwin.read(fd, ptr.baseAddress, ptr.count)
            }
            if n == 0 {
                return pending.isEmpty ? nil : nil
            }
            if n < 0 {
                if errno == EINTR { continue }
                throw IPCServerError.read(errno: errno)
            }
            pending.append(contentsOf: buf.prefix(n))
        }
    }
}

// MARK: - Errors

enum IPCServerError: Error, CustomStringConvertible {
    case socketCreate(errno: Int32)
    case pathTooLong(String)
    case bind(path: String, errno: Int32)
    case listen(errno: Int32)
    case read(errno: Int32)
    case frameTooLarge

    var description: String {
        switch self {
        case .socketCreate(let e): return "socket() failed: \(strerrorString(e))"
        case .pathTooLong(let p): return "socket path too long: \(p)"
        case .bind(let p, let e): return "bind(\(p)) failed: \(strerrorString(e))"
        case .listen(let e): return "listen() failed: \(strerrorString(e))"
        case .read(let e): return "read() failed: \(strerrorString(e))"
        case .frameTooLarge: return "frame larger than \(ipcMaxFrameBytes) bytes"
        }
    }
}

private func strerrorString(_ code: Int32) -> String {
    if let c = strerror(code), let s = String(validatingUTF8: c) {
        return s
    }
    return "errno \(code)"
}
