// IPCMessages.swift — daemon-removal refactor M4b.
//
// Swift Codable types for the JSON IPC protocol defined in
// `docs/reference/ipc.md`. Mirrors `crates/roost-ipc/src/messages.rs`
// 1:1 — every shape and field name. Cross-language fidelity is
// pinned by the shared `tests/ipc-vectors/*.json` corpus, which
// both `cargo test -p roost-ipc` and Swift `IPCMessagesTests`
// (added in M4b1) load + round-trip.
//
// Wire format rules (from the spec):
//   * Ids are int64 wrapped as strings (JSON numbers lose
//     precision past 2^53).
//   * Bytes are base64-encoded strings.
//   * Server-side request structs reject unknown fields; client-
//     side response/event structs are permissive (Swift Codable's
//     default).
//   * `TabState` is a JSON string enum (`"none"`, `"running"`,
//     `"needs_input"`, `"idle"`).

import Foundation

// MARK: - Shared types

enum IPCTabState: String, Codable, Sendable {
    case none
    case running
    case needsInput = "needs_input"
    case idle
}

struct IPCTab: Codable, Equatable, Sendable {
    var id: Int64
    var projectID: Int64
    var title: String
    var cwd: String
    var state: IPCTabState
    var hasNotification: Bool
    var isActive: Bool
    var userTitled: Bool
    var position: Int32
    var createdAt: Int64
    var lastActive: Int64
    var hookActive: Bool

    enum CodingKeys: String, CodingKey {
        case id
        case projectID = "project_id"
        case title
        case cwd
        case state
        case hasNotification = "has_notification"
        case isActive = "is_active"
        case userTitled = "user_titled"
        case position
        case createdAt = "created_at"
        case lastActive = "last_active"
        case hookActive = "hook_active"
    }

    init(
        id: Int64,
        projectID: Int64,
        title: String,
        cwd: String,
        state: IPCTabState,
        hasNotification: Bool,
        isActive: Bool,
        userTitled: Bool,
        position: Int32,
        createdAt: Int64,
        lastActive: Int64,
        hookActive: Bool
    ) {
        self.id = id
        self.projectID = projectID
        self.title = title
        self.cwd = cwd
        self.state = state
        self.hasNotification = hasNotification
        self.isActive = isActive
        self.userTitled = userTitled
        self.position = position
        self.createdAt = createdAt
        self.lastActive = lastActive
        self.hookActive = hookActive
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.id = try decodeStringInt64(c, .id)
        self.projectID = try decodeStringInt64(c, .projectID)
        self.title = try c.decode(String.self, forKey: .title)
        self.cwd = try c.decode(String.self, forKey: .cwd)
        self.state = try c.decode(IPCTabState.self, forKey: .state)
        self.hasNotification = try c.decode(Bool.self, forKey: .hasNotification)
        self.isActive = try c.decode(Bool.self, forKey: .isActive)
        self.userTitled = try c.decode(Bool.self, forKey: .userTitled)
        self.position = try c.decode(Int32.self, forKey: .position)
        self.createdAt = try c.decode(Int64.self, forKey: .createdAt)
        self.lastActive = try c.decode(Int64.self, forKey: .lastActive)
        self.hookActive = try c.decode(Bool.self, forKey: .hookActive)
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try encodeStringInt64(&c, .id, id)
        try encodeStringInt64(&c, .projectID, projectID)
        try c.encode(title, forKey: .title)
        try c.encode(cwd, forKey: .cwd)
        try c.encode(state, forKey: .state)
        try c.encode(hasNotification, forKey: .hasNotification)
        try c.encode(isActive, forKey: .isActive)
        try c.encode(userTitled, forKey: .userTitled)
        try c.encode(position, forKey: .position)
        try c.encode(createdAt, forKey: .createdAt)
        try c.encode(lastActive, forKey: .lastActive)
        try c.encode(hookActive, forKey: .hookActive)
    }
}

struct IPCProject: Codable, Equatable, Sendable {
    var id: Int64
    var name: String
    var cwd: String
    var position: Int32
    var createdAt: Int64
    var tabs: [IPCTab]

    enum CodingKeys: String, CodingKey {
        case id, name, cwd, position
        case createdAt = "created_at"
        case tabs
    }

    init(
        id: Int64, name: String, cwd: String, position: Int32, createdAt: Int64,
        tabs: [IPCTab]
    ) {
        self.id = id
        self.name = name
        self.cwd = cwd
        self.position = position
        self.createdAt = createdAt
        self.tabs = tabs
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.id = try decodeStringInt64(c, .id)
        self.name = try c.decode(String.self, forKey: .name)
        self.cwd = try c.decode(String.self, forKey: .cwd)
        self.position = try c.decode(Int32.self, forKey: .position)
        self.createdAt = try c.decode(Int64.self, forKey: .createdAt)
        self.tabs = try c.decodeIfPresent([IPCTab].self, forKey: .tabs) ?? []
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try encodeStringInt64(&c, .id, id)
        try c.encode(name, forKey: .name)
        try c.encode(cwd, forKey: .cwd)
        try c.encode(position, forKey: .position)
        try c.encode(createdAt, forKey: .createdAt)
        try c.encode(tabs, forKey: .tabs)
    }
}

// MARK: - Envelopes

struct IPCRequest: Codable, Sendable {
    var id: Int64
    var op: String
    var params: AnyCodable?

    enum CodingKeys: String, CodingKey {
        case id, op, params
    }

    init(id: Int64, op: String, params: AnyCodable? = nil) {
        self.id = id
        self.op = op
        self.params = params
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.id = try decodeStringInt64(c, .id)
        self.op = try c.decode(String.self, forKey: .op)
        self.params = try c.decodeIfPresent(AnyCodable.self, forKey: .params)
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try encodeStringInt64(&c, .id, id)
        try c.encode(op, forKey: .op)
        try c.encodeIfPresent(params, forKey: .params)
    }
}

struct IPCResponse: Codable, Sendable {
    var id: Int64
    var ok: Bool
    var result: AnyCodable?
    var error: IPCResponseError?

    enum CodingKeys: String, CodingKey {
        case id, ok, result, error
    }

    init(id: Int64, ok: Bool, result: AnyCodable? = nil, error: IPCResponseError? = nil) {
        self.id = id
        self.ok = ok
        self.result = result
        self.error = error
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.id = try decodeStringInt64(c, .id)
        self.ok = try c.decode(Bool.self, forKey: .ok)
        self.result = try c.decodeIfPresent(AnyCodable.self, forKey: .result)
        self.error = try c.decodeIfPresent(IPCResponseError.self, forKey: .error)
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try encodeStringInt64(&c, .id, id)
        try c.encode(ok, forKey: .ok)
        try c.encodeIfPresent(result, forKey: .result)
        try c.encodeIfPresent(error, forKey: .error)
    }

    static func success(id: Int64, result: AnyCodable?) -> IPCResponse {
        IPCResponse(id: id, ok: true, result: result, error: nil)
    }

    static func failure(id: Int64, code: String, message: String) -> IPCResponse {
        IPCResponse(
            id: id,
            ok: false,
            result: nil,
            error: IPCResponseError(code: code, message: message)
        )
    }
}

struct IPCResponseError: Codable, Equatable, Sendable {
    var code: String
    var message: String
}

struct IPCEventEnvelope: Codable, Sendable {
    var event: String
    var data: AnyCodable
}

// MARK: - Op-specific params + results (server-side request types reject unknown fields via custom decoders)

struct IPCIdentifyParams: Codable, Sendable {
    var clientName: String?
    var clientVersion: String?

    enum CodingKeys: String, CodingKey {
        case clientName = "client_name"
        case clientVersion = "client_version"
    }
}

struct IPCIdentifyResult: Codable, Sendable {
    var socketPath: String
    var pid: Int32
    var activeProjectID: Int64
    var activeTabID: Int64
    var appLabel: String
    var appID: String
    var uiVersion: String
    var protocolVersion: UInt32

    enum CodingKeys: String, CodingKey {
        case socketPath = "socket_path"
        case pid
        case activeProjectID = "active_project_id"
        case activeTabID = "active_tab_id"
        case appLabel = "app_label"
        case appID = "app_id"
        case uiVersion = "ui_version"
        case protocolVersion = "protocol_version"
    }

    init(
        socketPath: String, pid: Int32,
        activeProjectID: Int64, activeTabID: Int64,
        appLabel: String, appID: String,
        uiVersion: String, protocolVersion: UInt32
    ) {
        self.socketPath = socketPath
        self.pid = pid
        self.activeProjectID = activeProjectID
        self.activeTabID = activeTabID
        self.appLabel = appLabel
        self.appID = appID
        self.uiVersion = uiVersion
        self.protocolVersion = protocolVersion
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.socketPath = try c.decode(String.self, forKey: .socketPath)
        self.pid = try c.decode(Int32.self, forKey: .pid)
        self.activeProjectID = try decodeStringInt64(c, .activeProjectID)
        self.activeTabID = try decodeStringInt64(c, .activeTabID)
        self.appLabel = try c.decode(String.self, forKey: .appLabel)
        self.appID = try c.decode(String.self, forKey: .appID)
        self.uiVersion = try c.decode(String.self, forKey: .uiVersion)
        self.protocolVersion = try c.decode(UInt32.self, forKey: .protocolVersion)
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(socketPath, forKey: .socketPath)
        try c.encode(pid, forKey: .pid)
        try encodeStringInt64(&c, .activeProjectID, activeProjectID)
        try encodeStringInt64(&c, .activeTabID, activeTabID)
        try c.encode(appLabel, forKey: .appLabel)
        try c.encode(appID, forKey: .appID)
        try c.encode(uiVersion, forKey: .uiVersion)
        try c.encode(protocolVersion, forKey: .protocolVersion)
    }
}

// MARK: - String-wrapped int64 helpers

enum StringInt64DecodeError: Error, CustomStringConvertible {
    case notString(field: String)
    case notInt64(field: String, value: String)
    var description: String {
        switch self {
        case .notString(let f): return "\(f): expected string-wrapped int64"
        case .notInt64(let f, let v): return "\(f): not a valid int64: \(v)"
        }
    }
}

private func decodeStringInt64<K: CodingKey>(
    _ c: KeyedDecodingContainer<K>,
    _ key: K
) throws -> Int64 {
    let raw = try c.decode(String.self, forKey: key)
    guard let v = Int64(raw) else {
        throw StringInt64DecodeError.notInt64(field: key.stringValue, value: raw)
    }
    return v
}

private func encodeStringInt64<K: CodingKey>(
    _ c: inout KeyedEncodingContainer<K>,
    _ key: K,
    _ value: Int64
) throws {
    try c.encode(String(value), forKey: key)
}

// MARK: - AnyCodable (untyped JSON value)

/// Loose JSON value wrapper. Used for `params` and `result` /
/// `data` envelope fields which the dispatcher decodes per-op
/// after seeing the `op` / `event` string.
///
/// `@unchecked Sendable` because `Any` can't be `Sendable` in
/// Swift 6 strict mode but we treat this purely as opaque JSON
/// — it's set at decode/encode time and never mutated after.
struct AnyCodable: Codable, @unchecked Sendable {
    let value: Any

    init(_ value: Any) {
        self.value = value
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() {
            self.value = NSNull()
        } else if let b = try? c.decode(Bool.self) {
            self.value = b
        } else if let i = try? c.decode(Int64.self) {
            self.value = i
        } else if let d = try? c.decode(Double.self) {
            self.value = d
        } else if let s = try? c.decode(String.self) {
            self.value = s
        } else if let arr = try? c.decode([AnyCodable].self) {
            self.value = arr.map { $0.value }
        } else if let obj = try? c.decode([String: AnyCodable].self) {
            self.value = obj.mapValues { $0.value }
        } else {
            throw DecodingError.dataCorruptedError(
                in: c,
                debugDescription: "Unsupported JSON type"
            )
        }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        try Self.encodeValue(value, into: &c)
    }

    private static func encodeValue(
        _ value: Any, into c: inout SingleValueEncodingContainer
    ) throws {
        if value is NSNull {
            try c.encodeNil()
        } else if let b = value as? Bool {
            try c.encode(b)
        } else if let i = value as? Int64 {
            try c.encode(i)
        } else if let i = value as? Int {
            try c.encode(Int64(i))
        } else if let d = value as? Double {
            try c.encode(d)
        } else if let s = value as? String {
            try c.encode(s)
        } else if let arr = value as? [Any] {
            try c.encode(arr.map(AnyCodable.init))
        } else if let dict = value as? [String: Any] {
            try c.encode(dict.mapValues(AnyCodable.init))
        } else {
            throw EncodingError.invalidValue(
                value,
                EncodingError.Context(
                    codingPath: c.codingPath,
                    debugDescription: "Unsupported JSON value"
                )
            )
        }
    }
}

/// Protocol version on the wire. M0 ships `1`.
let ipcProtocolVersion: UInt32 = 1

/// Maximum length of a single framed line. Matches roost-ipc's
/// `MAX_FRAME_BYTES`.
let ipcMaxFrameBytes: Int = 16 * 1024 * 1024
