// AgentState.swift — Swift port of `crates/roost-ipc/src/agent.rs`.
//
// The four independent axes of plan 002 §3.1 plus the pure state
// machine that projects them onto the legacy `Workspace.TabState`
// wire field.
//
// Everything here is I/O-free, AppKit-free and actor-free, and takes
// its input as parameters: the workspace calls it from its mutation
// path, and the shared corpus under `tests/agent-state-fixtures/`
// drives it and the Rust original from the same files
// (`AgentStateFixtureTests`), so a divergence between the two UIs is
// a red test rather than a user's sidebar.
//
// Three axes persist (`ShellState`, `AgentLifecycle`, `Ownership`).
// Attention is not state but an effect: `Agent.applyReport` returns it
// as an `AttentionEffect` so the caller can fire or clear a
// notification without re-deriving anything.

import Foundation

// MARK: - Axes

/// Shell activity, written by OSC 133 marks. `unknown` is the state of
/// a shell that has not emitted a mark yet (no shell integration, or
/// nothing has run).
enum ShellState: String, Codable, Hashable, Sendable {
    case unknown
    case atPrompt = "at_prompt"
    case foregroundProcess = "foreground_process"
}

/// Agent turn state, written by adapters. Independent of `ShellState`
/// — an agent can be `working` while the shell sits at a prompt.
enum AgentLifecycle: String, Codable, Hashable, Sendable {
    case inactive
    case working
    case waiting
    case finished
    case failed
}

/// Notification severity.
enum Severity: String, Codable, Hashable, Sendable {
    case info
    case warn
    case error
}

/// What a report intends to do to ownership. Required on every report —
/// there is no sensible default, since "take the tab" and "I already
/// own the tab" have opposite failure modes.
enum OwnershipAction: String, Codable, Hashable, Sendable {
    case claim
    case preserve
    case release
}

/// What a report intends to do to the tab's attention (notification)
/// state.
enum AttentionOp: String, Codable, Hashable, Sendable {
    case set
    case clear
    case preserve
}

/// Who owns the tab. Identity is the **pair** `(source, sessionID)`:
/// two agents can collide on an opaque session id, so neither half is
/// sufficient alone.
///
/// `source` is an open string (AD-8) — adding a second agent must not
/// require touching this enum-free type.
struct Ownership: Codable, Hashable, Sendable {
    var source: String
    var sessionID: String
    /// Server receipt time of the most recent accepted report. Stamped
    /// from `Agent.applyReport`'s `now` argument, never by the caller.
    var lastEventAt: Int64
    var detail: String
    /// Known, accepted divergence from Rust's `BTreeMap<String,String>`:
    /// Swift's `Dictionary` hashes keys by canonical equivalence, so
    /// `"e\u{0301}"` and `"\u{00E9}"` collapse to one entry here and stay
    /// two there. A byte-keyed map to close it is disproportionate —
    /// metadata keys are adapter-authored ASCII (`model`, `cron_count`),
    /// never user input. The identity fields above are byte-compared
    /// (see `Agent.sameBytes`) because those *do* carry foreign input.
    var metadata: [String: String]

    init(
        source: String,
        sessionID: String = "",
        lastEventAt: Int64 = 0,
        detail: String = "",
        metadata: [String: String] = [:]
    ) {
        self.source = source
        self.sessionID = sessionID
        self.lastEventAt = lastEventAt
        self.detail = detail
        self.metadata = metadata
    }

    enum CodingKeys: String, CodingKey {
        case source
        case sessionID = "session_id"
        case lastEventAt = "last_event_at"
        case detail
        case metadata
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.source = try c.decode(String.self, forKey: .source)
        self.sessionID = try c.decodeIfPresent(String.self, forKey: .sessionID) ?? ""
        self.lastEventAt = try c.decodeIfPresent(Int64.self, forKey: .lastEventAt) ?? 0
        self.detail = try c.decodeIfPresent(String.self, forKey: .detail) ?? ""
        self.metadata = try c.decodeIfPresent([String: String].self, forKey: .metadata) ?? [:]
    }
}

/// The three persistent axes as one record.
struct AgentTabState: Decodable, Hashable, Sendable {
    var shell: ShellState
    var lifecycle: AgentLifecycle
    var ownership: Ownership?

    init(
        shell: ShellState = .unknown,
        lifecycle: AgentLifecycle = .inactive,
        ownership: Ownership? = nil
    ) {
        self.shell = shell
        self.lifecycle = lifecycle
        self.ownership = ownership
    }

    enum CodingKeys: String, CodingKey {
        case shell
        case lifecycle
        case ownership
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.shell = try c.decodeIfPresent(ShellState.self, forKey: .shell) ?? .unknown
        self.lifecycle = try c.decodeIfPresent(AgentLifecycle.self, forKey: .lifecycle) ?? .inactive
        self.ownership = try c.decodeIfPresent(Ownership.self, forKey: .ownership)
    }
}

// MARK: - `tab.agent_report` params

/// `tab.agent_report` request — the single op every agent adapter
/// writes through (plan §3.6).
///
/// Patch semantics are explicit rather than inferred: `ownershipAction`
/// is required, an omitted `lifecycle` means "unchanged", and
/// `attention` defaults to `preserve`. That keeps adapters pure — they
/// never need to read current state to describe an event.
///
/// `metadata` is the additive channel: the dispatcher rejects unknown
/// fields (`decodeParams(expected:)`), so a new *named* field is not
/// actually backwards-compatible and extensions go in the map.
struct AgentReport: Decodable, Hashable, Sendable {
    var tabID: Int64
    var source: String
    /// Empty for sources that have no session concept (e.g. `manual`).
    var sessionID: String
    var ownershipAction: OwnershipAction
    /// `nil` (omitted on the wire) means "leave lifecycle unchanged".
    var lifecycle: AgentLifecycle?
    var attention: AttentionOp
    var severity: Severity
    /// Required when `attention == .set`, ignored otherwise. See
    /// `Agent.validate`.
    var title: String
    var body: String
    /// Free-form reason for the report (`permission_prompt`,
    /// `2 background tasks`, an error name…). Recorded on the owner
    /// when non-empty.
    var detail: String
    var metadata: [String: String]

    init(
        tabID: Int64,
        source: String,
        sessionID: String = "",
        ownershipAction: OwnershipAction,
        lifecycle: AgentLifecycle? = nil,
        attention: AttentionOp = .preserve,
        severity: Severity = .info,
        title: String = "",
        body: String = "",
        detail: String = "",
        metadata: [String: String] = [:]
    ) {
        self.tabID = tabID
        self.source = source
        self.sessionID = sessionID
        self.ownershipAction = ownershipAction
        self.lifecycle = lifecycle
        self.attention = attention
        self.severity = severity
        self.title = title
        self.body = body
        self.detail = detail
        self.metadata = metadata
    }

    /// `CaseIterable` so the dispatcher's `decodeParams(expected:)` set —
    /// the Swift stand-in for Rust's `deny_unknown_fields` — is derived
    /// from these keys rather than hand-copied beside them.
    enum CodingKeys: String, CodingKey, CaseIterable {
        case tabID = "tab_id"
        case source
        case sessionID = "session_id"
        case ownershipAction = "ownership_action"
        case lifecycle
        case attention
        case severity
        case title
        case body
        case detail
        case metadata
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.tabID = try decodeStringInt64(c, CodingKeys.tabID)
        self.source = try c.decode(String.self, forKey: .source)
        self.sessionID = try c.decodeIfPresent(String.self, forKey: .sessionID) ?? ""
        self.ownershipAction = try c.decode(OwnershipAction.self, forKey: .ownershipAction)
        self.lifecycle = try c.decodeIfPresent(AgentLifecycle.self, forKey: .lifecycle)
        self.attention = try c.decodeIfPresent(AttentionOp.self, forKey: .attention) ?? .preserve
        self.severity = try c.decodeIfPresent(Severity.self, forKey: .severity) ?? .info
        self.title = try c.decodeIfPresent(String.self, forKey: .title) ?? ""
        self.body = try c.decodeIfPresent(String.self, forKey: .body) ?? ""
        self.detail = try c.decodeIfPresent(String.self, forKey: .detail) ?? ""
        self.metadata = try c.decodeIfPresent([String: String].self, forKey: .metadata) ?? [:]
    }
}

/// Why a report is malformed. Thrown by `Agent.validate` so the op
/// dispatcher can reject with `invalid-param` before mutating anything.
enum ReportError: Error, Equatable, CustomStringConvertible {
    case emptySource
    case missingTitle
    case missingBody

    var description: String {
        switch self {
        case .emptySource: return "source must not be empty"
        case .missingTitle: return "attention=set requires a non-empty title"
        case .missingBody: return "attention=set requires a non-empty body"
        }
    }
}

// MARK: - Report application

/// What `Agent.applyReport` wants the caller to do about attention.
///
/// `Decodable` so the shared fixtures can state the expected effect
/// directly (`{"kind":"set",…}` / `{"kind":"clear"}` /
/// `{"kind":"unchanged"}`); nothing encodes it — the Mac UI applies
/// the effect in-process rather than putting it on the wire.
enum AttentionEffect: Decodable, Hashable, Sendable {
    case set(title: String, body: String, severity: Severity)
    case clear
    case unchanged

    enum CodingKeys: String, CodingKey {
        case kind
        case title
        case body
        case severity
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let kind = try c.decode(String.self, forKey: .kind)
        switch kind {
        case "set":
            self = .set(
                title: try c.decode(String.self, forKey: .title),
                body: try c.decode(String.self, forKey: .body),
                severity: try c.decodeIfPresent(Severity.self, forKey: .severity) ?? .info
            )
        case "clear":
            self = .clear
        case "unchanged":
            self = .unchanged
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .kind, in: c,
                debugDescription: "unknown attention effect kind: \(kind)"
            )
        }
    }
}

/// Result of applying a report: the new state plus everything the
/// caller needs to emit events without re-deriving it.
struct ApplyOutcome: Equatable, Sendable {
    var state: AgentTabState
    /// False when the report was dropped on an ownership mismatch; the
    /// state is then returned unchanged.
    var accepted: Bool
    /// Whether the owner's **identity or presence** changed. A refreshed
    /// `lastEventAt` (or merged metadata) is not an ownership change —
    /// otherwise every accepted report would look like one.
    var ownershipChanged: Bool
    var lifecycleChanged: Bool
    var attention: AttentionEffect
}

// MARK: - The state machine

/// Namespace for the pure agent-state functions, mirroring the Rust
/// `roost_ipc::agent` module.
enum Agent {
    /// Ownership is "live" iff it is present with a non-empty source.
    ///
    /// There is deliberately **no timestamp or TTL heuristic** (AD-3):
    /// Claude fires no periodic hook, so a long tool call would look
    /// stale and get released mid-turn. Ownership is cleared only by
    /// the explicit rules in `applyReport`, by `applyShellMark`
    /// dropping the lifecycle at a prompt, or by PTY replacement.
    static func isLive(_ state: AgentTabState) -> Bool {
        guard let owner = state.ownership else { return false }
        return !owner.source.isEmpty
    }

    /// Whether the agent axis wins derivation: a live owner that is
    /// doing something. The single expression of plan §3.2's precedence
    /// rule — `effectiveLifecycle` and `suppressRawOsc` are both this
    /// predicate, so a dead agent can never keep driving one of them
    /// while having released the other.
    private static func agentDrives(_ state: AgentTabState) -> Bool {
        isLive(state) && state.lifecycle != .inactive
    }

    /// The lifecycle a tab actually presents (plan §3.2): the agent's
    /// own when it drives, otherwise the shell axis lifted onto the
    /// same enum.
    ///
    /// This — not `effective` — is what the UI renders, because it keeps
    /// `failed` distinct from `waiting`; `effective` collapses the two
    /// for the legacy wire field. The stripe, the pill dot and the
    /// future overview all rank and colour off this, so they cannot
    /// disagree.
    static func effectiveLifecycle(_ state: AgentTabState) -> AgentLifecycle {
        if agentDrives(state) { return state.lifecycle }
        switch state.shell {
        case .foregroundProcess: return .working
        case .unknown, .atPrompt: return .inactive
        }
    }

    /// Project the axes onto the legacy `tab.state` field (plan §3.2) —
    /// the lossy half of `effectiveLifecycle`.
    ///
    /// `AgentLifecycle.failed` maps to `.needsInput` on purpose.
    /// `Workspace.TabState` (and its wire twin `IPCTabState`) stays a
    /// **closed four-value enum** — neither has a fallback case, so a
    /// fifth value throws on decode, and `docs/reference/ipc.md`
    /// classifies a new enum value as a breaking protocol change. True
    /// failure is observable on the agent lifecycle axis instead. Do not
    /// "fix" this by adding a `failed` case to `TabState`.
    static func effective(_ state: AgentTabState) -> Workspace.TabState {
        switch effectiveLifecycle(state) {
        case .inactive: return .none
        case .working: return .running
        case .waiting, .failed: return .needsInput
        case .finished: return .idle
        }
    }

    /// Attention ordering for the sidebar stripe, the overview sort, and
    /// the agent switcher (plan §3.2).
    ///
    /// Operates on `AgentLifecycle` — which has `failed` — rather than
    /// on the projected `TabState`, where `failed` and `waiting`
    /// collapse into one value.
    static func rank(_ lifecycle: AgentLifecycle) -> Int {
        switch lifecycle {
        case .failed: return 4
        case .waiting: return 3
        case .working: return 2
        case .finished: return 1
        case .inactive: return 0
        }
    }

    /// Whether raw OSC 9 / 99 / 777 notifications should be dropped
    /// because an agent is actively driving the tab (plan §3.4). An
    /// explicit `notification.create` is never suppressed — only raw
    /// OSC.
    static func suppressRawOsc(_ state: AgentTabState) -> Bool {
        agentDrives(state)
    }

    /// Shape validation, separate from `applyReport` so the pure state
    /// machine stays total: the dispatcher validates, then applies.
    static func validate(_ report: AgentReport) throws {
        if report.source.isEmpty { throw ReportError.emptySource }
        if report.attention == .set {
            if report.title.isEmpty { throw ReportError.missingTitle }
            if report.body.isEmpty { throw ReportError.missingBody }
        }
    }

    /// Apply an OSC 133 shell mark. Returns `nil` for a body the spec
    /// doesn't define, meaning "no change".
    ///
    /// `C` (command start) writes only the shell axis: a foreground
    /// process exists, but if an agent owns the tab its lifecycle still
    /// wins.
    ///
    /// `A`/`B` (prompt) and `D` (command end) additionally drop the
    /// lifecycle to `inactive` while **retaining ownership as a label**
    /// — the failsafe against a killed agent muting a tab forever (plan
    /// §3.4). The shell only reaches a prompt once the foreground
    /// command exited, so an agent that owned the tab is necessarily
    /// gone. Derivation then falls through to the shell axis and
    /// `suppressRawOsc` re-opens raw OSC, so a dead agent degrades
    /// cosmetically instead of silently swallowing notifications.
    static func applyShellMark(_ current: AgentTabState, body: String) -> AgentTabState? {
        // The first *scalar*, not the first `Character`: Rust reads
        // `body.chars().next()`, so a combining mark after the letter
        // (`"C\u{0301}"`) is a second scalar there and would be folded
        // into one grapheme here, making the mark unrecognizable on Mac
        // alone.
        guard let mark = body.unicodeScalars.first else { return nil }
        let shell: ShellState
        let lifecycle: AgentLifecycle
        switch mark {
        case "C":
            shell = .foregroundProcess
            lifecycle = current.lifecycle
        case "A", "B", "D":
            shell = .atPrompt
            lifecycle = .inactive
        default:
            return nil
        }
        return AgentTabState(shell: shell, lifecycle: lifecycle, ownership: current.ownership)
    }

    /// Apply a report to the current state, enforcing session scoping
    /// (plan §3.3 + §3.6). Pure: the caller owns the mutation and the
    /// event emission, this decides what they should be.
    ///
    /// * Identity is the pair `(source, sessionID)`; a report that does
    ///   not match the current owner is dropped.
    /// * `claim` always takes ownership, replacing any existing owner.
    ///   It is the sole supersede path.
    /// * `release` requires a match; it clears ownership and forces
    ///   lifecycle `inactive`.
    /// * `lastEventAt` is stamped from `now` (server receipt time).
    static func applyReport(
        _ current: AgentTabState,
        _ report: AgentReport,
        now: Int64
    ) -> ApplyOutcome {
        let authorized: Bool
        switch report.ownershipAction {
        case .claim: authorized = true
        case .preserve, .release: authorized = ownerMatches(current, report)
        }
        guard authorized else {
            return ApplyOutcome(
                state: current,
                accepted: false,
                ownershipChanged: false,
                lifecycleChanged: false,
                attention: .unchanged
            )
        }

        let ownership: Ownership?
        switch report.ownershipAction {
        case .claim:
            ownership = Ownership(
                source: report.source,
                sessionID: report.sessionID,
                lastEventAt: now,
                detail: report.detail,
                metadata: report.metadata
            )
        case .preserve:
            if var owner = current.ownership {
                owner.lastEventAt = now
                // Empty fields mean "this event says nothing about it",
                // not "clear it" — metadata accumulates across a session
                // (model at SessionStart, cron counts at Stop) and v1
                // has no delete channel.
                if !report.detail.isEmpty { owner.detail = report.detail }
                for (key, value) in report.metadata { owner.metadata[key] = value }
                ownership = owner
            } else {
                ownership = nil
            }
        case .release:
            ownership = nil
        }

        let lifecycle: AgentLifecycle
        switch report.ownershipAction {
        case .release: lifecycle = .inactive
        case .claim, .preserve: lifecycle = report.lifecycle ?? current.lifecycle
        }

        let state = AgentTabState(
            shell: current.shell,
            lifecycle: lifecycle,
            ownership: ownership
        )

        let attention: AttentionEffect
        switch report.attention {
        case .set:
            attention = .set(title: report.title, body: report.body, severity: report.severity)
        case .clear:
            attention = .clear
        case .preserve:
            attention = .unchanged
        }

        return ApplyOutcome(
            state: state,
            accepted: true,
            ownershipChanged: identity(state.ownership) != identity(current.ownership),
            lifecycleChanged: state.lifecycle != current.lifecycle,
            attention: attention
        )
    }

    private static func ownerMatches(_ current: AgentTabState, _ report: AgentReport) -> Bool {
        guard let owner = current.ownership else { return false }
        return sameBytes(owner.source, report.source)
            && sameBytes(owner.sessionID, report.sessionID)
    }

    /// Byte-exact string equality. Ownership identity gates who may
    /// release a tab, and Rust compares these as UTF-8 bytes while
    /// Swift's `==` compares canonical equivalence — so `"\u{00E9}"`
    /// and `"e\u{0301}"` would match here and not there, letting a
    /// byte-distinct foreign identity take a tab on Mac only.
    private static func sameBytes(_ a: String, _ b: String) -> Bool {
        a.utf8.elementsEqual(b.utf8)
    }

    /// Owner identity — the pair, not either half.
    private struct OwnerIdentity: Equatable {
        let source: String
        let sessionID: String

        static func == (lhs: Self, rhs: Self) -> Bool {
            Agent.sameBytes(lhs.source, rhs.source)
                && Agent.sameBytes(lhs.sessionID, rhs.sessionID)
        }
    }

    private static func identity(_ ownership: Ownership?) -> OwnerIdentity? {
        ownership.map { OwnerIdentity(source: $0.source, sessionID: $0.sessionID) }
    }
}
