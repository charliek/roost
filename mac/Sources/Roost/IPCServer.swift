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
    /// `(dev, ino)` of the socket file this server bound, captured
    /// right after `bind`. `deinit` unlinks only if the path still
    /// resolves to it — see the comment there.
    private var boundIdentity: (dev: dev_t, ino: ino_t)?

    /// Bind a fresh server at `socketPath`.
    ///
    /// `recoverStaleSocket` is the M6 stale-socket recovery flag.
    /// The TOCTOU-safe protocol is:
    ///
    ///   1. Caller holds the `SingleInstance` flock (M4c).
    ///   2. We try to `bind` the socket.
    ///   3. If `EADDRINUSE`, the previous instance was probably
    ///      killed with -9 and left the socket on disk. With the
    ///      flock held, no live writer can race us — we probe the
    ///      path with `connect()`. If the connect *succeeds*
    ///      anyway, surface `.alreadyBound` (something is listening
    ///      that we didn't expect; better to error than steal it).
    ///      Otherwise unlink and retry the bind once.
    ///
    /// Pass `false` only from contexts that don't hold the lock —
    /// e.g. tests, or the `ROOST_ALLOW_MULTI=1` bypass path. In
    /// those cases we surface `.alreadyBound` on contention rather
    /// than unlinking a possibly-live socket.
    init(socketPath: String, handler: IPCHandler, recoverStaleSocket: Bool = false) throws {
        self.socketPath = socketPath
        self.handler = handler

        let parent = (socketPath as NSString).deletingLastPathComponent
        try? FileManager.default.createDirectory(
            atPath: parent,
            withIntermediateDirectories: true
        )

        self.listenFD = try Self.bindWithRecovery(
            socketPath: socketPath,
            recoverStaleSocket: recoverStaleSocket
        )
        var st = stat()
        if lstat(socketPath, &st) == 0 {
            self.boundIdentity = (dev: st.st_dev, ino: st.st_ino)
        }
    }

    private static func bindWithRecovery(
        socketPath: String,
        recoverStaleSocket: Bool
    ) throws -> Int32 {
        switch try tryBindOnce(socketPath: socketPath) {
        case .ok(let fd):
            return fd
        case .addrInUse:
            if !recoverStaleSocket {
                throw IPCServerError.alreadyBound(path: socketPath)
            }
            // The flock holder said "stale socket is safe to clean".
            // First, sanity-check that no live listener is there
            // via a connect() probe. If something answers — or if we
            // can't prove nothing does — bail rather than steal a live
            // UI's socket.
            if Self.connectProbe(socketPath: socketPath) == .live {
                throw IPCServerError.alreadyBound(path: socketPath)
            }
            try? FileManager.default.removeItem(atPath: socketPath)
            switch try tryBindOnce(socketPath: socketPath) {
            case .ok(let fd):
                return fd
            case .addrInUse:
                // Two bind attempts in a row hitting EADDRINUSE with
                // the flock held is genuinely surprising — surface it
                // rather than retrying forever.
                throw IPCServerError.alreadyBound(path: socketPath)
            }
        }
    }

    /// Single bind attempt. Returns `.addrInUse` for the recoverable
    /// case; throws for anything else.
    private enum BindOutcome {
        case ok(Int32)
        case addrInUse
    }

    private static func tryBindOnce(socketPath: String) throws -> BindOutcome {
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
            if e == EADDRINUSE {
                return .addrInUse
            }
            throw IPCServerError.bind(path: socketPath, errno: e)
        }

        if listen(fd, 32) < 0 {
            let e = errno
            Darwin.close(fd)
            throw IPCServerError.listen(errno: e)
        }

        chmod(socketPath, 0o600)
        return .ok(fd)
    }

    /// What a `connect()` probe found. Deliberately two-valued: the
    /// caller only ever needs "may I unlink this?".
    enum SocketLiveness {
        case live
        case stale
    }

    /// The errno rule, isolated from the socket dance so it can be
    /// tested. Mirrors `roost_ipc::socket_state::classify_connect_error`
    /// and it is **fail-safe**: only `ECONNREFUSED` (nothing queued for
    /// accept) and `ENOENT` (path gone) mean stale. Everything else
    /// means assume-live-and-refuse.
    ///
    /// The old predicate here was `rc == 0`, i.e. every non-zero errno
    /// collapsed to "stale, unlink it". That happens to be right on
    /// Darwin and is wrong the moment the same rule meets Linux, where
    /// `connect(2)` to an `AF_UNIX` stream socket whose accept backlog
    /// is full returns `EAGAIN` — from a very much live listener. The
    /// two UIs keep this rule in lockstep so neither can drift into
    /// unlinking a busy peer's socket.
    nonisolated static func classifyConnect(result: Int32, errnoValue: Int32) -> SocketLiveness {
        if result == 0 { return .live }
        if errnoValue == ECONNREFUSED || errnoValue == ENOENT { return .stale }
        return .live
    }

    /// Brief `connect()` probe — `.live` if a listener is answering on
    /// `socketPath`, or if we cannot prove otherwise. Used by the
    /// stale-socket recovery path to refuse to unlink an alive socket.
    /// No timeout is set; a UNIX-domain `connect()` resolves in the
    /// kernel without waiting on the peer.
    private static func connectProbe(socketPath: String) -> SocketLiveness {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        // Can't probe → can't prove stale.
        if fd < 0 { return .live }
        defer { Darwin.close(fd) }

        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let pathBytes = Array(socketPath.utf8)
        if pathBytes.count >= MemoryLayout.size(ofValue: addr.sun_path) {
            return .live
        }
        withUnsafeMutablePointer(to: &addr.sun_path) { ptr in
            ptr.withMemoryRebound(to: CChar.self, capacity: pathBytes.count + 1) { c in
                for (i, b) in pathBytes.enumerated() {
                    c[i] = CChar(b)
                }
                c[pathBytes.count] = 0
            }
        }

        let rc = withUnsafePointer(to: &addr) { addrPtr in
            addrPtr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                Darwin.connect(fd, sa, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        // Captured immediately: anything between the syscall and the
        // read could clobber the thread's errno.
        let connectErrno = errno
        return classifyConnect(result: rc, errnoValue: connectErrno)
    }

    deinit {
        if listenFD >= 0 {
            Darwin.close(listenFD)
        }
        // Only unlink the socket file we ourselves bound. Without the
        // identity check this deinit could delete a *successor's*
        // socket: a late-released server (an autorelease pool draining
        // after a replacement instance has already bound the same path)
        // would take the new listener's socket down with it, leaving
        // `roostctl` and the hooks with nothing to dial. The socket
        // lock guards the socket's lifetime; this is the same guarantee
        // on the release side.
        if let identity = boundIdentity {
            var st = stat()
            if lstat(socketPath, &st) == 0,
                st.st_dev == identity.dev, st.st_ino == identity.ino
            {
                try? FileManager.default.removeItem(atPath: socketPath)
            }
        }
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
    case alreadyBound(path: String)
    case listen(errno: Int32)
    case read(errno: Int32)
    case frameTooLarge

    var description: String {
        switch self {
        case .socketCreate(let e): return "socket() failed: \(strerrorString(e))"
        case .pathTooLong(let p): return "socket path too long: \(p)"
        case .bind(let p, let e): return "bind(\(p)) failed: \(strerrorString(e))"
        case .alreadyBound(let p): return "socket already in use: \(p)"
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
