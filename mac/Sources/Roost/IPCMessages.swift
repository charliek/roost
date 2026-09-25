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

/// Tab snapshot. Used in `tab.open` / `tab.list` / `tab.agent_report`.
///
/// `state` and `hookActive` are **derived** server-side from the three
/// agent axes below (`Agent.effective` / `Agent.isLive`), not stored
/// alongside them. They stay on the wire because every shipped client
/// reads them; `state` stays a closed four-value enum for the reason
/// spelled out on `Agent.effective`.
///
/// The axes themselves decode with `decodeIfPresent` + a default
/// without exception — this decoder is strict everywhere else, so a
/// payload from a server predating plan 002 (or from the other UI mid-
/// rollout) would otherwise throw.
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
    var shellState: ShellState
    var agentLifecycle: AgentLifecycle
    var ownership: Ownership?

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
        case shellState = "shell_state"
        case agentLifecycle = "agent_lifecycle"
        case ownership
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
        hookActive: Bool,
        shellState: ShellState,
        agentLifecycle: AgentLifecycle,
        ownership: Ownership?
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
        self.shellState = shellState
        self.agentLifecycle = agentLifecycle
        self.ownership = ownership
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
        self.shellState = try c.decodeIfPresent(ShellState.self, forKey: .shellState) ?? .unknown
        self.agentLifecycle =
            try c.decodeIfPresent(AgentLifecycle.self, forKey: .agentLifecycle) ?? .inactive
        self.ownership = try c.decodeIfPresent(Ownership.self, forKey: .ownership)
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
        try c.encode(shellState, forKey: .shellState)
        try c.encode(agentLifecycle, forKey: .agentLifecycle)
        // Omitted rather than null when absent, matching the Rust
        // `skip_serializing_if = "Option::is_none"`.
        try c.encodeIfPresent(ownership, forKey: .ownership)
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
        // Reject unknown top-level keys to match the Rust side's
        // `#[serde(deny_unknown_fields)]` on `RawRequest`. Without
        // this check, a malformed client could pass on macOS and
        // fail on the Rust side for the same request — the kind
        // of cross-platform skew CR flagged. The op-specific
        // params struct already gets the same treatment via
        // `IPCHandlerImpl.decodeParams(expected:)`.
        let allowed: Set<String> = ["id", "op", "params"]
        try Self.rejectUnknownKeys(in: c, allowed: allowed)
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

    private static func rejectUnknownKeys(
        in container: KeyedDecodingContainer<CodingKeys>,
        allowed: Set<String>
    ) throws {
        let present = Set(container.allKeys.map(\.stringValue))
        let unknown = present.subtracting(allowed)
        if !unknown.isEmpty {
            let joined = unknown.sorted().joined(separator: ", ")
            throw DecodingError.dataCorrupted(
                .init(
                    codingPath: container.codingPath,
                    debugDescription: "unknown request fields: \(joined)"
                )
            )
        }
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
    /// Why the last attempt to write `state.json` failed, absent while
    /// the layout is landing (#481) — the Mac twin of Rust's
    /// `IdentifyResult.persist_error`. `encodeIfPresent` keeps the key
    /// off the wire entirely when there's no error, matching Rust's
    /// `skip_serializing_if = "Option::is_none"` so existing golden
    /// vectors still decode unchanged.
    var persistError: String?

    enum CodingKeys: String, CodingKey {
        case socketPath = "socket_path"
        case pid
        case activeProjectID = "active_project_id"
        case activeTabID = "active_tab_id"
        case appLabel = "app_label"
        case appID = "app_id"
        case uiVersion = "ui_version"
        case protocolVersion = "protocol_version"
        case persistError = "persist_error"
    }

    init(
        socketPath: String, pid: Int32,
        activeProjectID: Int64, activeTabID: Int64,
        appLabel: String, appID: String,
        uiVersion: String, protocolVersion: UInt32,
        persistError: String? = nil
    ) {
        self.socketPath = socketPath
        self.pid = pid
        self.activeProjectID = activeProjectID
        self.activeTabID = activeTabID
        self.appLabel = appLabel
        self.appID = appID
        self.uiVersion = uiVersion
        self.protocolVersion = protocolVersion
        self.persistError = persistError
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
        self.persistError = try c.decodeIfPresent(String.self, forKey: .persistError)
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
        try c.encodeIfPresent(persistError, forKey: .persistError)
    }
}

// MARK: - Host sessions (wire types only — no op serves them yet)

/// What a host session can encode a tab's attach payload as. Mirrors
/// Rust's `AttachPayloadKind`: an open string, not a closed enum, so a
/// client reading a newer host's `payload_kinds` preserves the values
/// it doesn't recognize instead of failing to decode.
struct IPCAttachPayloadKind: Codable, Equatable, Hashable, Sendable {
    var value: String

    init(_ value: String) {
        self.value = value
    }

    init(from decoder: Decoder) throws {
        self.value = try decoder.singleValueContainer().decode(String.self)
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        try c.encode(value)
    }

    /// libghostty's own terminal-state snapshot. Requires an exact
    /// `libghosttyBuild` match between host and client.
    static let ghosttySnapshot = IPCAttachPayloadKind("ghostty-snapshot")
    /// A VT byte stream the client replays into its own terminal.
    static let vt = IPCAttachPayloadKind("vt")
}

/// `session.identify` result — the first thing a client asks a host
/// session for, so every incompatibility is caught on stable JSON
/// before any binary frame exists. `startedAt` is RFC3339, carried as
/// a string.
struct IPCSessionIdentify: Codable, Equatable, Sendable {
    var appVersion: String
    var sessionProtocol: UInt32
    var payloadKinds: [IPCAttachPayloadKind]
    var libghosttyBuild: String
    var sessionID: String
    var startedAt: String

    enum CodingKeys: String, CodingKey {
        case appVersion = "app_version"
        case sessionProtocol = "session_protocol"
        case payloadKinds = "payload_kinds"
        case libghosttyBuild = "libghostty_build"
        case sessionID = "session_id"
        case startedAt = "started_at"
    }
}

/// `data` of the `session.stopping` envelope — the one frame on an
/// events connection that is not an `IPCEventBatch`. `reason` is
/// `"stop"`, and the stream is over. Mirrors Rust's
/// `SessionStoppingEvent`.
struct IPCSessionStoppingEvent: Codable, Equatable, Sendable {
    var reason: String
}

/// The event name of that envelope. Mirrors Rust's
/// `messages::SESSION_STOPPING_EVENT`.
let ipcSessionStoppingEvent = "session.stopping"

/// One atomic push on the events connection. A single workspace commit
/// can publish several events under the same `revision`, so the batch —
/// not the envelope — is the unit a client checks for gaps.
struct IPCEventBatch: Codable, Sendable {
    var revision: UInt64
    var events: [IPCEventEnvelope]

    enum CodingKeys: String, CodingKey {
        case revision, events
    }

    init(revision: UInt64, events: [IPCEventEnvelope]) {
        self.revision = revision
        self.events = events
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.revision = try c.decode(UInt64.self, forKey: .revision)
        // Matches the Rust `#[serde(default)]` exactly: an absent key is
        // an empty fence, but an explicit `"events": null` is a decode
        // error there, so `decodeIfPresent` (which accepts null) would
        // drift the two sides apart.
        self.events = c.contains(.events) ? try c.decode([IPCEventEnvelope].self, forKey: .events) : []
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(revision, forKey: .revision)
        try c.encode(events, forKey: .events)
    }
}

// MARK: - Agent hooks (`agent.set_hooks`)

/// `agent.set_hooks`'s `agents`: this machine's new `agent-hooks`
/// value, spelled the way `config.conf` itself would — either an
/// explicit allow-list or the literal word `off`.
///
/// Decoded by hand rather than as an "any string is off" shape, for
/// Rust's `AgentSetHooksAgents` reason: this field means *exactly* one
/// string, and a typo like `"ofF"` or a stray `"none"` must fail to
/// parse rather than silently become `off`. A list of any length,
/// including empty, is always the list shape — whether it is a *valid*
/// one is the op's call (`invalid-param`), not the wire format's.
enum IPCAgentSetHooksAgents: Equatable, Sendable {
    case list([String])
    case off
}

extension IPCAgentSetHooksAgents: Codable {
    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if let names = try? c.decode([String].self) {
            self = .list(names)
            return
        }
        let word = try c.decode(String.self)
        guard word == "off" else {
            throw DecodingError.dataCorruptedError(
                in: c,
                debugDescription:
                    "agents must be a list of agent names or \"off\", not \"\(word)\""
            )
        }
        self = .off
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .list(let names): try c.encode(names)
        case .off: try c.encode("off")
        }
    }
}

struct IPCAgentSetHooksParams: Codable, Equatable, Sendable {
    var agents: IPCAgentSetHooksAgents
}

struct IPCAgentHooksSkipped: Codable, Equatable, Sendable {
    var agent: String
    var reason: String
}

struct IPCAgentHooksFailed: Codable, Equatable, Sendable {
    var agent: String
    var error: String
}

/// What one agent-hooks install did to one machine's files. Mirrors
/// Rust's `AgentHooksOutcome`, and decodes `roostctl agent set
/// --local --json` directly — the CLI prints these five keys under
/// exactly these names (plus `current` and `warnings`, which nothing
/// here reads).
///
/// One deliberate difference from the iced UI's reply: the CLI's
/// `wired` is *this run's* writes, where the in-process op reports the
/// record's never-announced list. The Mac has no toast to drive off the
/// latter, and what the CLI can honestly report is what it did.
struct IPCAgentHooksOutcome: Codable, Equatable, Sendable {
    var wired: [String] = []
    var refreshed: [String] = []
    var removed: [String] = []
    var skipped: [IPCAgentHooksSkipped] = []
    var errors: [IPCAgentHooksFailed] = []
}

/// One connected host's answer to the raise this UI pushed onto it.
///
/// Never populated here: the Mac app has no host connections and
/// answers `unknown-op` to every `host.*` op. The type exists because
/// the reply shape is shared with the Linux UI, which does, and
/// `roostctl agent set` decodes one reply from either.
struct IPCAgentSetHooksHostOutcome: Codable, Equatable, Sendable {
    var host: String
    var result: IPCAgentHooksOutcome?
    var error: String?
}

struct IPCAgentSetHooksResult: Codable, Equatable, Sendable {
    /// Where this machine's `agent-hooks` key now lives, for the
    /// confirmation surface to name.
    var configPath: String
    var local: IPCAgentHooksOutcome
    var hosts: [IPCAgentSetHooksHostOutcome] = []

    enum CodingKeys: String, CodingKey {
        case configPath = "config_path"
        case local
        case hosts
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

func decodeStringInt64<K: CodingKey>(
    _ c: KeyedDecodingContainer<K>,
    _ key: K
) throws -> Int64 {
    let raw = try c.decode(String.self, forKey: key)
    guard let v = Int64(raw) else {
        throw StringInt64DecodeError.notInt64(field: key.stringValue, value: raw)
    }
    return v
}

func decodeOptionalStringInt64<K: CodingKey>(
    _ c: KeyedDecodingContainer<K>,
    _ key: K
) throws -> Int64? {
    guard c.contains(key), try !c.decodeNil(forKey: key) else { return nil }
    return try decodeStringInt64(c, key)
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
        // CRITICAL: NSNumber must be disambiguated BEFORE the
        // `as? Bool` / `as? Int64` cascade, because
        // `JSONSerialization.jsonObject(...)` boxes JSON numbers
        // as `NSNumber`, and in Swift `NSNumber(value: 1) as? Bool`
        // returns `Optional(true)` (the polymorphic NSNumber
        // bridges to either Bool, Int, Double, etc. — the first
        // `as?` cast wins regardless of the underlying type). The
        // classic symptom is `protocol_version: 1` serializing as
        // `"protocol_version": true` on the wire, which the
        // strict-typed Rust client then rejects with
        // `invalid type: boolean true, expected u32`. We use
        // `CFGetTypeID` against `CFBooleanGetTypeID()` to
        // distinguish "actually a Bool" from "an integer that
        // bridges to Bool".
        if value is NSNull {
            try c.encodeNil()
        } else if let n = value as? NSNumber {
            let typeID = CFGetTypeID(n)
            if typeID == CFBooleanGetTypeID() {
                try c.encode(n.boolValue)
            } else if CFNumberIsFloatType(n) {
                try c.encode(n.doubleValue)
            } else {
                try c.encode(n.int64Value)
            }
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

/// Version of the host-session protocol (`session.*` handshake, attach
/// handshake, binary data plane). Separate from `ipcProtocolVersion`,
/// which versions the request/response format every client speaks —
/// the two move independently. Mirrors Rust's
/// `messages::SESSION_PROTOCOL_VERSION`.
///
/// At `5` every same-UID connection to a session became symmetric: no
/// owner, no lease, no foreground. Effects fan out to every subscriber,
/// and the PTY is sized by the last interactor. `6` reshapes
/// `session.set_agent_hooks`: it carries the agents a client's own
/// `agent-hooks` key allows, and a client only ever *raises* the host's
/// setting — `mode` and `skip` are gone, and a client that allows
/// nothing sends no frame at all (plan 064 §3.3). `6` also fans
/// notifications out the way effects already were: a session suppresses
/// nothing, and the client reading a tab answers its
/// `notification.fired` with a generation-checked
/// `tab.clear_notification` — the `session.set_focus` op that used to
/// mute a tab for everyone is deleted (#474).
///
/// The rule: a **session-socket change bumps this when a pre-bump peer
/// could not refuse it meaningfully**, in either direction. A new event
/// name inside an existing batch does not bump it — a client with no
/// name for it ignores it. A client compares the integer for
/// **equality** and refuses anything else, so it is the whole
/// negotiation; what each generation changed is `CHANGELOG.md`'s to
/// tell.
let ipcSessionProtocolVersion: UInt32 = 6

/// Maximum length of a single framed line. Matches roost-ipc's
/// `MAX_FRAME_BYTES`.
let ipcMaxFrameBytes: Int = 16 * 1024 * 1024
