// The agent-hooks consent sheet's model (plan 064 §3.5): one card, two
// modes.
//
// **First run** goes up once per launch when nobody has answered the
// `agent-hooks` key; **preferences** is the same card, opened
// deliberately from `Agent Hooks…` in the command palette. They differ
// in what the two buttons say and in whether each row carries a status
// line — nothing else, because they are one card.
//
// AppKit-free on purpose. `AgentHooksSheet.swift` is the `NSAlert` that
// draws this; everything worth getting right for every combination —
// the prefill rule, the button wording, the status line, the row order
// — is decided here, where a test reaches it without a window.
//
// The strings are the plan's, verbatim, and the iced card
// (`crates/roost-iced/src/app/agent_hooks_dialog.rs`) says the same
// words. Change one and change the other.

import Foundation

/// The card's fixed copy.
enum AgentHooksCopy {
    static let title = "Agent hooks"

    static let lede =
        "Roost adds a hook to each agent you switch on so its tabs show status and send "
        + "notifications. It edits the agent files named below, plus Roost's own config, and "
        + "nothing else. roostctl agent uninstall --all puts the agent files back."

    static let footer =
        "Switching an agent on applies here and on every host this Roost connects to. "
        + "Switching one off applies here only. Change it any time from Agent Hooks… in "
        + "the command palette."

    /// codex is the one agent whose install writes a second file, and
    /// the only one that would otherwise put a dialog of its own in
    /// front of the user the first time a hook fires.
    static let codexNote =
        "Also pre-trusts Roost's hooks in config.toml so codex won't show its own review dialog."

    /// OpenCode has no command hooks at all, so what Roost installs
    /// there is a plugin — a different kind of thing in a different
    /// place, and the row says so rather than letting "hook" stand for
    /// both.
    static let opencodeNote = "This is a plugin that runs inside OpenCode, not a hook entry."
}

/// Which of the two cards this is.
enum AgentHooksCardMode: String, Sendable, Equatable {
    /// Nobody has answered the key yet, and the app asked unprompted.
    case firstRun = "first_run"
    /// The user opened it from the palette.
    case preferences

    var dismissLabel: String {
        switch self {
        case .firstRun: return "Decide later"
        case .preferences: return "Cancel"
        }
    }

    /// The primary button. Both modes name the count, because the
    /// button is the last thing read before five config files are
    /// edited — and both have a distinct spelling for zero, since
    /// "Instrument 0" and a bare "Apply" would each hide that
    /// confirming now writes `off`.
    func confirmLabel(on: Int) -> String {
        switch (self, on) {
        case (.firstRun, 0): return "Turn off"
        case (.firstRun, let on): return "Instrument \(on)"
        case (.preferences, 0): return "Apply: turn off"
        case (.preferences, _): return "Apply"
        }
    }
}

/// Mirrors `roost_agent_install::INTEGRATION_VERSION`. Bump both
/// together: it is the version a row's `wired vN` names, and the Mac
/// sheet and the iced card must not disagree about what "current" reads
/// as.
let agentHooksIntegrationVersion = 3

/// One row of `roostctl agent status --json`.
///
/// `allowed` and `files` are what make the sheet possible at all — the
/// key's own reading of this agent, and the paths an uninstall would
/// put back. The row also carries `noticed`, `skipped` and `warnings`;
/// nothing here needs them, and Swift's decoder ignores them.
struct AgentHooksStatus: Decodable, Equatable, Sendable {
    var agent: String
    var present: Bool
    var wired: Int?
    var entriesOnDisk: Bool
    var upToDate: Bool
    var allowed: Bool
    var files: [String]

    enum CodingKeys: String, CodingKey {
        case agent
        case present
        case wired
        case entriesOnDisk = "entries_on_disk"
        case upToDate = "up_to_date"
        case allowed
        case files
    }

    init(
        agent: String,
        present: Bool = true,
        wired: Int? = nil,
        entriesOnDisk: Bool = false,
        upToDate: Bool = false,
        allowed: Bool = false,
        files: [String] = []
    ) {
        self.agent = agent
        self.present = present
        self.wired = wired
        self.entriesOnDisk = entriesOnDisk
        self.upToDate = upToDate
        self.allowed = allowed
        self.files = files
    }
}

/// One agent's row. All five are always drawn, present or not: an agent
/// the user has only on a host is still something they can consent to
/// here, and a row that vanished would make the card's list depend on
/// what happens to be installed today.
struct AgentHooksSheetRow: Equatable, Sendable {
    /// The `source` token — `claude`, `codex`, … — which is also what
    /// the config key and the `--local` spec spell.
    let agent: String
    var on: Bool
    let found: Bool
    /// Rendered in preferences mode only, so `nil` on first run.
    let status: String?
    let files: [String]

    /// The chip beside the name.
    var chip: String { found ? "found here" : "not found here" }

    /// What a person calls the product, not the `source` token.
    var displayName: String { AgentPalette.agentDisplayName(agent) }

    /// The extra sentence two of the five rows carry.
    var note: String? {
        switch agent {
        case "codex": return AgentHooksCopy.codexNote
        case "opencode": return AgentHooksCopy.opencodeNote
        default: return nil
        }
    }
}

/// Which switches start on.
///
/// **The key, never the disk.** A row that is wired but not named in
/// the key starts off, so Apply taking its entries back out is a
/// visible act the user chose rather than a silent narrowing of what
/// they already had. `.ask` is the one state with no key to read: there
/// the fallback is what is installed here, which is plan 064's D2 — an
/// agent that exists only on a host stays off until somebody switches
/// it on once.
func agentHooksPrefill(key: AgentHooks, status: AgentHooksStatus) -> Bool {
    switch key {
    case .ask: return status.present
    case .off, .allow: return status.allowed
    }
}

/// The status line preferences mode shows under each row.
///
/// The order of the tests is the meaning: the state record and the
/// agent's own files are independent sources, and "is there anything of
/// Roost's in these files" has to be answered before any version can
/// be. `upToDate` alone cannot say it — an agent the key does not name
/// plans no edits and so looks current while carrying nothing at all.
func agentHooksRowStatus(_ status: AgentHooksStatus) -> String {
    if !status.present { return "not found" }
    if !status.entriesOnDisk { return "found, not wired" }
    if !status.allowed { return "wired, not allowed" }
    if status.upToDate { return "wired v\(agentHooksIntegrationVersion)" }
    guard let version = status.wired else {
        // Entries on disk with no record to name their version: a wiped
        // `~/.config/roost`, or a record restored from a backup.
        return "wired, out of date"
    }
    return "wired v\(version), out of date"
}

/// Every row the card will draw, from one `roostctl agent status --json`
/// walk plus the key it is read against.
///
/// **The inventory is the list, not the reply.** The five rows are
/// `AgentHooks.agentNames`, in that order, and each one is *looked up*
/// in what the walk returned: a reply that omits an agent draws it as
/// not found rather than dropping it, because a missing row is a switch
/// the user never saw and confirming would write a key without it —
/// removing that agent's hook without ever having offered the choice. A
/// name the inventory does not know is ignored for the same reason in
/// reverse: this card can only consent to what it can name.
func agentHooksRows(
    mode: AgentHooksCardMode,
    key: AgentHooks,
    statuses: [AgentHooksStatus]
) -> [AgentHooksSheetRow] {
    AgentHooks.agentNames.map { agent in
        let status =
            statuses.first { $0.agent == agent } ?? AgentHooksStatus(agent: agent, present: false)
        return AgentHooksSheetRow(
            agent: agent,
            on: agentHooksPrefill(key: key, status: status),
            found: status.present,
            status: mode == .preferences ? agentHooksRowStatus(status) : nil,
            files: status.files
        )
    }
}

/// The card's live contents: what the sheet draws, and what its
/// switches mutate.
struct AgentHooksCard: Equatable, Sendable {
    let mode: AgentHooksCardMode
    private(set) var rows: [AgentHooksSheetRow]

    init(mode: AgentHooksCardMode, rows: [AgentHooksSheetRow]) {
        self.mode = mode
        self.rows = rows
    }

    /// What a switch reports: its new state, and which row it sits on.
    /// Out of range is a no-op — the index is a tag on a control the
    /// sheet built, and a stale one must not take the app down.
    mutating func set(_ on: Bool, at index: Int) {
        guard rows.indices.contains(index) else { return }
        rows[index].on = on
    }

    var onCount: Int { rows.filter(\.on).count }

    var dismissLabel: String { mode.dismissLabel }
    var confirmLabel: String { mode.confirmLabel(on: onCount) }

    /// Both buttons in the order `NSAlert` adds them — the primary
    /// first, since AppKit's first button is the rightmost/default one.
    var buttons: [String] { [confirmLabel, dismissLabel] }

    /// What confirming writes: the `roostctl agent set --local` spec.
    ///
    /// Every switch off is the word `off`, never an empty spec — an
    /// empty `agent-hooks` value parses back as `.ask` and would bring
    /// this card round again on the next launch.
    var setSpec: String {
        let names = rows.filter(\.on).map(\.agent)
        return names.isEmpty ? "off" : names.joined(separator: ",")
    }
}

// MARK: - Spawning roostctl

/// One `roostctl` invocation: argv plus the environment it runs in.
struct AgentHooksCommand: Equatable, Sendable {
    var argv: [String]
    var environment: [String: String]
}

/// What running one finished with.
struct AgentHooksRun: Equatable, Sendable {
    var status: Int32
    var stdout: String
    var stderr: String
    var timedOut: Bool

    init(status: Int32, stdout: String = "", stderr: String = "", timedOut: Bool = false) {
        self.status = status
        self.stdout = stdout
        self.stderr = stderr
        self.timedOut = timedOut
    }
}

/// How a surface reaches the install engine: which `roostctl` to run,
/// the environment to run it in, and the call that runs it.
///
/// One value rather than three parameters so every surface — the
/// launch, the sheet's Apply, the `agent.set_hooks` op — is injected
/// identically, and a test substitutes one thing.
struct AgentHooksRunner: Sendable {
    var roostctl: @Sendable () -> String?
    var environment: @Sendable () -> [String: String]
    var run: @Sendable (AgentHooksCommand) -> AgentHooksRun

    static let bundled = AgentHooksRunner(
        roostctl: { bundledRoostctl() },
        environment: {
            agentHooksEnvironment(
                base: ProcessInfo.processInfo.environment,
                home: FileManager.default.homeDirectoryForCurrentUser.path,
                configPath: RoostConfig.defaultPath().path
            )
        },
        run: agentHooksRun
    )
}

/// The environment a spawned `roostctl` runs in.
///
/// Inherited for everything else — `ROOST_TEST_MODE` among it, which is
/// the fence that keeps a harness out of real dotfiles — and explicit
/// for the two variables that decide *which* `config.conf` and *whose*
/// dotfiles the install engine reaches.
///
/// `ROOST_CONFIG` carries the path **this app resolved**, so the child
/// cannot land the key in a file this app never reads; a parent that
/// never set the variable would otherwise leave the child to its own
/// idea of the default. `HOME` is the inherited one wherever there is
/// one — that is what every other Roost on this machine reads and what
/// an E2E jail sets — and this app's own answer where there is not,
/// because a GUI app's environment is not a shell's and the install
/// engine's `Home::from_env` hard-fails on a missing `HOME`.
///
/// Pure, so the contract is a test rather than a comment — the same
/// shape `childEnvironment` uses for a spawned tab.
func agentHooksEnvironment(
    base: [String: String], home: String, configPath: String
) -> [String: String] {
    var env = base
    env["HOME"] = base["HOME"] ?? home
    env["ROOST_CONFIG"] = configPath
    return env
}

func agentHooksStatusCommand(roostctl: String, environment: [String: String]) -> AgentHooksCommand {
    AgentHooksCommand(argv: [roostctl, "agent", "status", "--json"], environment: environment)
}

/// `--local` is the whole point: this machine's own key and its own
/// files, written by the one process the plan lets write them here
/// (§3.4). The UI-routed spelling of the same verb dials *this* app.
func agentHooksSetCommand(
    roostctl: String, spec: String, environment: [String: String]
) -> AgentHooksCommand {
    AgentHooksCommand(
        argv: [roostctl, "agent", "set", "--local", spec, "--json"],
        environment: environment
    )
}

// MARK: - Reading a survey back

/// What one finished status walk found: the rows it makes, whether this
/// machine has any of the five at all, and — the part that is not the
/// walk's — the `agent-hooks` key that was on disk when it finished.
///
/// The key travels with the rows because it is the one the rows were
/// built from. Deciding to raise off a *different* reading than the
/// prefill used is the bug this struct exists to make impossible: the
/// sheet would be asking a question somebody had just answered, with
/// switches mixing the old answer's rule and the new status's flags.
struct AgentHooksFound: Equatable, Sendable {
    let mode: AgentHooksCardMode
    let anyPresent: Bool
    let key: AgentHooks
    let rows: [AgentHooksSheetRow]

    var card: AgentHooksCard { AgentHooksCard(mode: mode, rows: rows) }
}

/// What a status walk came back with.
enum AgentHooksSurveyOutcome: Equatable, Sendable {
    case found(AgentHooksFound)
    /// One line worth logging; never an alert at launch.
    case failed(String)
}

/// What a finished survey may do with its result.
///
/// Pure, and separate from the walk, because three of the four arms are
/// races — the key answered by another process while this launch was
/// asking, a sheet taken the screen in the meantime, a machine with
/// nothing installed — and all of them decide whether Roost puts a
/// question in front of somebody who has already answered it. Mirrors
/// `agent_hooks_dialog::survey_verdict` in the iced UI.
enum AgentHooksSurveyVerdict: Equatable, Sendable {
    case raise
    /// Nothing is installed here, so there is nothing to consent about.
    case noAgent
    /// Somebody answered the key while this launch was asking.
    case alreadyAnswered
    /// The user is already answering a different question.
    case screenTaken
}

/// `sheetOpen` is sampled where it lives — on the main thread, at the
/// moment the walk's result arrives — for the same reason `key` is the
/// walk's own reading: both can change while five agents' files are
/// being read.
func agentHooksSurveyVerdict(
    mode: AgentHooksCardMode,
    anyPresent: Bool,
    key: AgentHooks,
    sheetOpen: Bool
) -> AgentHooksSurveyVerdict {
    if mode == .firstRun {
        if !anyPresent { return .noAgent }
        if key != .ask { return .alreadyAnswered }
    }
    if sheetOpen { return .screenTaken }
    return .raise
}

/// Run one status walk and turn it into the rows a card would draw.
///
/// `key` is a closure and is called **after** the walk, as late as
/// possible and from disk, rather than taken from the running app's
/// snapshot: this process is not the key's only writer (`roostctl agent
/// set --local` and a connecting client's `roost-session` both reach the
/// same file), the walk can sit on the install `flock` for its whole
/// budget, and the card's whole job is to show what is true now. What it
/// read rides back in [`AgentHooksFound.key`] so the raise decision and
/// the prefill are the same reading.
///
/// Blocking — call off the main thread.
func agentHooksSurvey(
    mode: AgentHooksCardMode,
    command: AgentHooksCommand,
    key: () -> AgentHooks,
    run: (AgentHooksCommand) -> AgentHooksRun
) -> AgentHooksSurveyOutcome {
    let outcome = run(command)
    if outcome.timedOut {
        return .failed(
            "agent hooks: roostctl agent status timed out after \(Int(agentHooksSpawnTimeout))s")
    }
    if outcome.status != 0 {
        let detail = outcome.stderr.isEmpty ? outcome.stdout : outcome.stderr
        return .failed("agent hooks: roostctl agent status exited \(outcome.status): \(detail)")
    }
    let statuses: [AgentHooksStatus]
    do {
        statuses = try JSONDecoder().decode(
            [AgentHooksStatus].self, from: Data(outcome.stdout.utf8))
    } catch {
        return .failed("agent hooks: could not read roostctl agent status --json: \(error)")
    }
    let onDisk = key()
    let rows = agentHooksRows(mode: mode, key: onDisk, statuses: statuses)
    return .found(
        AgentHooksFound(
            mode: mode,
            anyPresent: rows.contains(where: \.found),
            key: onDisk,
            rows: rows
        ))
}

// MARK: - Writing the key

/// A failure as a sentence, which is all any of these carry: every
/// surface here either logs the line or puts it in an alert.
struct AgentHooksError: Error, Equatable, Sendable, CustomStringConvertible {
    let message: String
    var description: String { message }
    init(_ message: String) { self.message = message }
}

/// The canonical agent name `name` spells, if any.
///
/// Case and separators are ignored, mirroring `roost_agent::Agent::parse`
/// — `OpenCode`, `open-code` and `opencode` are one agent to the wire,
/// and the two ends must not disagree about that. What survives the
/// stripping still has to match a name *whole*: `Claude-Code` becomes
/// `claudecode`, which is nobody.
func agentHooksCanonicalName(_ name: String) -> String? {
    var normalized = String.UnicodeScalarView()
    for scalar in name.unicodeScalars where scalar.isASCII {
        switch scalar.value {
        case 0x30...0x39, 0x61...0x7A: normalized.append(scalar)
        case 0x41...0x5A: normalized.append(UnicodeScalar(scalar.value + 0x20)!)
        default: break
        }
    }
    let lowered = String(normalized)
    return AgentHooks.agentNames.first { $0 == lowered }
}

/// What `agent.set_hooks`'s `agents` resolves to — the
/// `roostctl agent set --local` spec — or the `invalid-param` message it
/// is refused with before anything is written (plan 064 §3.4).
///
/// The same rule the iced UI draws (`agent_hooks::resolve_set`): this op
/// answers the consent question, and a consent answer has no honest
/// partial reading, so one name nothing answers to refuses the whole
/// list rather than silently narrowing it. The one difference from the
/// config parser is that a blank element is an error here — skipping
/// blanks is right for a human-typed CLI list and wrong on a wire, where
/// one can only be a bug in the client.
func agentHooksSetSpec(_ agents: IPCAgentSetHooksAgents) -> Result<String, AgentHooksError> {
    let names: [String]
    switch agents {
    case .off: return .success("off")
    case .list(let list): names = list
    }
    let known = AgentHooks.agentNames.joined(separator: ", ")
    if names.contains(where: { $0.trimmingCharacters(in: .whitespaces).isEmpty }) {
        return .failure(AgentHooksError("agent.set_hooks: `agents` carries an empty name"))
    }
    var resolved: [String] = []
    for name in names {
        let trimmed = name.trimmingCharacters(in: .whitespaces)
        guard let canonical = agentHooksCanonicalName(trimmed) else {
            return .failure(
                AgentHooksError("agent.set_hooks: no agent named \"\(trimmed)\" (\(known))"))
        }
        if !resolved.contains(canonical) { resolved.append(canonical) }
    }
    if resolved.isEmpty {
        return .failure(
            AgentHooksError(
                "agent.set_hooks requires a non-empty `agents` (\(known)) or the word \"off\""))
    }
    // Inventory order, not the caller's: the spec is the same answer
    // however a client spells it, and the key it writes is canonical
    // either way (`AgentHooks.configValue` and the Rust mirror both
    // re-order on serialisation). Sorting here is what makes the two
    // UIs answer `["opencode","claude"]` identically instead of leaving
    // that to whatever re-orders last.
    return .success(AgentHooks.agentNames.filter { resolved.contains($0) }.joined(separator: ","))
}

/// What one finished `roostctl agent set --local --json` answers with.
///
/// A non-zero exit with a decodable outcome is a **success**: that is
/// how the CLI reports a per-agent failure, and one unparseable
/// `config.toml` must not cost the user the answer they just gave. Only
/// a run that produced no outcome at all — a whole-run install failure,
/// a spec the CLI refused, a `roostctl` that would not start — is an
/// error here.
func agentHooksSetReply(
    _ outcome: AgentHooksRun
) -> Result<IPCAgentSetHooksResult, AgentHooksError> {
    if outcome.timedOut {
        return .failure(
            AgentHooksError(
                "roostctl agent set timed out after \(Int(agentHooksSpawnTimeout))s"))
    }
    guard
        let local = try? JSONDecoder().decode(
            IPCAgentHooksOutcome.self, from: Data(outcome.stdout.utf8))
    else {
        let detail = outcome.stderr.isEmpty ? outcome.stdout : outcome.stderr
        return .failure(
            AgentHooksError("roostctl agent set exited \(outcome.status): \(detail)"))
    }
    return .success(
        IPCAgentSetHooksResult(
            // The CLI's `--json` carries the outcome and not the path.
            // Resolving it here is not a guess: the child was handed
            // this app's own `HOME` and `ROOST_CONFIG`
            // (`agentHooksEnvironment`), so it wrote the file this
            // resolves to.
            configPath: RoostConfig.defaultPath().path,
            local: local,
            // The Mac app holds no host connections and answers
            // `unknown-op` to every `host.*` op, so there is never
            // anything to raise.
            hosts: []
        ))
}

/// Set this machine's `agent-hooks` key and reconcile its files, by
/// spawning the one process the plan lets write them here (§3.4).
///
/// Blocking — call off the main thread.
func agentHooksSetLocal(
    spec: String, runner: AgentHooksRunner
) -> Result<IPCAgentSetHooksResult, AgentHooksError> {
    guard let roostctl = runner.roostctl() else {
        return .failure(
            AgentHooksError("no bundled roostctl: this build cannot set agent-hooks"))
    }
    let command = agentHooksSetCommand(
        roostctl: roostctl, spec: spec, environment: runner.environment())
    return agentHooksSetReply(runner.run(command))
}
