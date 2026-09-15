// The agent-hooks consent sheet (plan 064 §3.5, W6), tested through its
// view-model rather than through a window: the switch callbacks, the
// derived button label, the prefill rule, the row order and the two
// per-agent notes are all decided in `AgentHooksCard.swift`, which is
// AppKit-free for exactly that reason.
//
// The iced card's table test
// (`crates/roost-iced/src/app/agent_hooks_dialog.rs`) is the mirror of
// this one; the two cards say the same words and follow the same rules,
// so a divergence should surface on whichever side regressed.

import Foundation
import Testing

@testable import Roost

// MARK: - Shared test doubles

/// A runner that records what it was asked to spawn and answers from a
/// canned reply, so a test can watch the argv and the environment
/// without a `roostctl` on disk and without touching a real dotfile.
final class AgentHooksSpawnLog: @unchecked Sendable {
    private let lock = NSLock()
    private var spawned: [AgentHooksCommand] = []
    private let path: String?
    private let env: [String: String]
    private let onResolve: @Sendable () -> Void
    private let reply: @Sendable (AgentHooksCommand) -> AgentHooksRun

    init(
        roostctl: String? = "/Apps/Roost.app/Contents/Resources/bin/roostctl",
        environment: [String: String] = agentHooksEnvironment(
            base: ["HOME": "/home/test-u", "PATH": "/usr/bin"],
            home: "/home/unused",
            configPath: "/tmp/roost-test/config.conf"
        ),
        onResolve: @escaping @Sendable () -> Void = {},
        reply: @escaping @Sendable (AgentHooksCommand) -> AgentHooksRun = { _ in
            AgentHooksRun(status: 0)
        }
    ) {
        self.path = roostctl
        self.env = environment
        self.onResolve = onResolve
        self.reply = reply
    }

    var runner: AgentHooksRunner {
        AgentHooksRunner(
            roostctl: { [self] in
                onResolve()
                return path
            },
            environment: { [env] in env },
            run: { [self] command in
                lock.lock()
                spawned.append(command)
                lock.unlock()
                return reply(command)
            }
        )
    }

    func commands() -> [AgentHooksCommand] {
        lock.lock()
        defer { lock.unlock() }
        return spawned
    }
}

/// A one-slot mailbox for what a launch's status walk hands back on the
/// main queue.
final class AgentHooksFoundBox: @unchecked Sendable {
    private let lock = NSLock()
    private var found: AgentHooksFound?
    private let arrived = DispatchSemaphore(value: 0)

    func put(_ found: AgentHooksFound) {
        lock.lock()
        self.found = found
        lock.unlock()
        arrived.signal()
    }

    func take() -> AgentHooksFound? {
        lock.lock()
        defer { lock.unlock() }
        return found
    }

    /// Wait without blocking the main thread — the result is delivered
    /// *onto* it, so a semaphore wait here would deadlock.
    func wait(timeout: TimeInterval = 10) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while take() == nil {
            if Date() > deadline { throw AgentHooksTestTimeout() }
            try await Task.sleep(for: .milliseconds(5))
        }
    }
}

struct AgentHooksTestTimeout: Error {}

/// `roostctl agent status --json` for a set of rows, in the shape the
/// CLI prints (`print_status`), including the three keys the sheet does
/// not read.
func agentHooksStatusJSON(
    _ rows: [AgentHooksStatus] = AgentHooks.agentNames.map {
        AgentHooksStatus(agent: $0, files: ["/home/test-u/.config/\($0)/hooks.json"])
    }
) -> String {
    let body = rows.map { row -> String in
        let files = row.files.map { "\"\($0)\"" }.joined(separator: ",")
        return "{\"agent\":\"\(row.agent)\",\"present\":\(row.present),"
            + "\"wired\":\(row.wired.map(String.init) ?? "null"),"
            + "\"entries_on_disk\":\(row.entriesOnDisk),\"up_to_date\":\(row.upToDate),"
            + "\"noticed\":false,\"allowed\":\(row.allowed),\"files\":[\(files)],"
            + "\"skipped\":null,\"warnings\":[]}"
    }
    return "[\(body.joined(separator: ","))]"
}

private func allPresent() -> [AgentHooksStatus] {
    AgentHooks.agentNames.map {
        AgentHooksStatus(agent: $0, files: ["/home/test-u/.config/\($0)/hooks.json"])
    }
}

// MARK: - The card

@Suite("Agent-hooks sheet: rows")
struct AgentHooksCardRowTests {
    /// The five rows, in `agentNames` order, whatever the key says.
    @Test func everyAgentGetsARowInTheInventoryOrder() {
        let rows = agentHooksRows(mode: .firstRun, key: .ask, statuses: allPresent())
        #expect(rows.map(\.agent) == ["claude", "codex", "grok", "cursor", "opencode"])
        #expect(
            rows.map(\.displayName) == ["Claude Code", "Codex", "Grok", "Cursor", "OpenCode"])
        // First run carries no status line: the dump shows what the card
        // shows, not what it could have.
        #expect(rows.allSatisfy { $0.status == nil })
    }

    /// Only two rows carry an extra sentence, and they are the two whose
    /// install is not "one hook entry in one file".
    @Test func theTwoRowsWithSomethingMoreToSayAreCodexAndOpencode() {
        let rows = agentHooksRows(mode: .preferences, key: .ask, statuses: allPresent())
        let note = { (agent: String) in rows.first { $0.agent == agent }?.note }
        #expect(note("codex") == AgentHooksCopy.codexNote)
        #expect(note("opencode") == AgentHooksCopy.opencodeNote)
        for agent in ["claude", "grok", "cursor"] {
            #expect(note(agent) == nil, "\(agent)")
        }
    }

    /// First run has no key, so what is installed here is the fallback —
    /// and an agent that exists only on a host stays off (D2).
    @Test func anUnansweredKeyStartsTheInstalledAgentsOnAndTheRestOff() {
        var statuses = allPresent()
        statuses[2].present = false
        let rows = agentHooksRows(mode: .firstRun, key: .ask, statuses: statuses)
        #expect(rows.map(\.on) == [true, true, false, true, true])
        #expect(rows[2].chip == "not found here")
        #expect(rows[0].chip == "found here")
    }

    /// The prefill is the key, not the disk: an agent that is wired and
    /// not named starts OFF, so Apply removing it is something the user
    /// can see they are doing.
    @Test func aWiredAgentTheKeyDoesNotNameStartsOff() {
        var statuses = allPresent()
        statuses[0].allowed = true
        statuses[1].entriesOnDisk = true
        statuses[1].wired = 3
        let rows = agentHooksRows(
            mode: .preferences, key: .allow(["claude"]), statuses: statuses)
        #expect(rows[0].on, "the key names claude")
        #expect(!rows[1].on, "codex is wired and unnamed: it starts off")
        #expect(rows[1].status == "wired, not allowed")
    }

    /// `off` is an answer, so every switch starts off — including for an
    /// agent whose entries are still on disk.
    @Test func offStartsEverySwitchOff() {
        var statuses = allPresent()
        statuses[0].entriesOnDisk = true
        let rows = agentHooksRows(mode: .preferences, key: .off, statuses: statuses)
        #expect(rows.allSatisfy { !$0.on })
    }

    /// The five wordings, and the order they are decided in.
    @Test func theStatusLineReadsTheRecordAndTheDiskApart() {
        var absent = AgentHooksStatus(agent: "grok")
        absent.present = false
        #expect(agentHooksRowStatus(absent) == "not found")

        var bare = AgentHooksStatus(agent: "grok")
        bare.allowed = true
        #expect(agentHooksRowStatus(bare) == "found, not wired")

        var unnamed = AgentHooksStatus(agent: "grok")
        unnamed.entriesOnDisk = true
        unnamed.upToDate = true
        #expect(
            agentHooksRowStatus(unnamed) == "wired, not allowed",
            "an unnamed agent plans no edits, which is not the same as being current")

        var current = AgentHooksStatus(agent: "grok")
        current.entriesOnDisk = true
        current.allowed = true
        current.upToDate = true
        current.wired = 1
        #expect(
            agentHooksRowStatus(current) == "wired v\(agentHooksIntegrationVersion)",
            "the disk is current, whatever version the record remembers")

        var stale = current
        stale.upToDate = false
        stale.wired = 2
        #expect(agentHooksRowStatus(stale) == "wired v2, out of date")

        stale.wired = nil
        #expect(agentHooksRowStatus(stale) == "wired, out of date")
    }

    /// The rows are the inventory, not the reply. An agent the walk did
    /// not mention still gets a switch — a missing row is a choice the
    /// user never saw, and confirming would write a key without that
    /// agent, taking its hook back out unasked.
    @Test func anAgentTheReplyOmitsIsStillARowAndReadsAsNotFound() {
        var statuses = allPresent()
        statuses.remove(at: 2)  // grok
        for key in [AgentHooks.ask, .allow(["grok"]), .off] {
            let rows = agentHooksRows(mode: .preferences, key: key, statuses: statuses)
            #expect(rows.map(\.agent) == AgentHooks.agentNames, "\(key)")
            #expect(!rows[2].found, "\(key)")
            #expect(!rows[2].on, "an agent nobody reported must not start on: \(key)")
            #expect(rows[2].status == "not found", "\(key)")
            #expect(rows[2].files.isEmpty, "\(key)")
        }
    }

    /// A duplicate is read once and an unknown name is not read at all:
    /// this card can only consent to agents it can name, and its list of
    /// those is fixed.
    @Test func aDuplicateOrUnknownNameInTheReplyChangesNothing() {
        var statuses = allPresent()
        var second = statuses[0]
        second.present = false
        statuses.append(second)
        statuses.append(AgentHooksStatus(agent: "gemini", files: ["/home/test-u/.gemini"]))
        let rows = agentHooksRows(mode: .firstRun, key: .ask, statuses: statuses)
        #expect(rows.map(\.agent) == AgentHooks.agentNames)
        #expect(rows[0].found, "the first claude row is the one that counts")
        #expect(rows.count == 5)
    }

    /// A row names the files an uninstall would put back, straight from
    /// the status walk — the sheet has no second table of paths.
    @Test func theRowsNameTheFilesTheStatusWalkReported() {
        var statuses = allPresent()
        statuses[1].files = ["/home/test-u/.codex/hooks.json", "/home/test-u/.codex/config.toml"]
        let rows = agentHooksRows(mode: .preferences, key: .ask, statuses: statuses)
        #expect(rows[1].files.count == 2)
        #expect(rows.allSatisfy { !$0.files.isEmpty })
    }
}

@Suite("Agent-hooks sheet: switches and buttons")
struct AgentHooksCardButtonTests {
    private func card(_ mode: AgentHooksCardMode, _ on: [Bool]) -> AgentHooksCard {
        var rows = agentHooksRows(mode: mode, key: .ask, statuses: allPresent())
        for (index, value) in on.enumerated() { rows[index].on = value }
        return AgentHooksCard(mode: mode, rows: rows)
    }

    /// Both modes name the count on the primary, and both have their own
    /// spelling for zero — "Instrument 0" would hide that confirming
    /// writes `off`.
    @Test func theButtonsSayWhatConfirmingWillDo() {
        #expect(
            card(.firstRun, [true, true, false, false, false]).buttons
                == ["Instrument 2", "Decide later"])
        #expect(card(.firstRun, [false, false, false, false, false]).buttons
            == ["Turn off", "Decide later"])
        #expect(
            card(.preferences, [true, false, false, false, false]).buttons == ["Apply", "Cancel"])
        #expect(
            card(.preferences, [false, false, false, false, false]).buttons
                == ["Apply: turn off", "Cancel"])
    }

    /// `set(_:at:)` is the switch's own callback, so it is what is
    /// exercised — the label it re-derives is the last thing read
    /// before five config files are edited.
    @Test func togglingASwitchRederivesThePrimary() {
        var live = card(.firstRun, [false, false, false, false, false])
        #expect(live.confirmLabel == "Turn off")
        live.set(true, at: 0)
        #expect(live.rows[0].on)
        #expect(live.confirmLabel == "Instrument 1")
        live.set(true, at: 3)
        #expect(live.confirmLabel == "Instrument 2")
        live.set(false, at: 0)
        #expect(!live.rows[0].on, "switching it back off is the same callback")
        #expect(live.confirmLabel == "Instrument 1")
        #expect(live.setSpec == "cursor")
        // A stale tag is a no-op, not a crash.
        live.set(true, at: 99)
        #expect(live.confirmLabel == "Instrument 1")
    }

    /// Every switch off travels as the word `off`, never as an empty
    /// spec — an empty `agent-hooks` value parses back as `.ask` and
    /// would bring the sheet round again.
    @Test func whatConfirmingWritesIsTheSwitchesThatAreOn() {
        #expect(card(.firstRun, [true, false, true, false, false]).setSpec == "claude,grok")
        #expect(card(.firstRun, [false, false, false, false, false]).setSpec == "off")
        #expect(
            card(.preferences, [true, true, true, true, true]).setSpec
                == "claude,codex,grok,cursor,opencode")
    }
}

@Suite("Agent-hooks sheet: the copy")
struct AgentHooksCopyTests {
    /// PINNED, verbatim, and identical to the iced card's — the strings
    /// live in one place per UI so the sheet and the dump cannot
    /// disagree about them.
    @Test func theSheetSaysTheSameWordsAsTheIcedCard() {
        #expect(AgentHooksCopy.title == "Agent hooks")
        #expect(
            AgentHooksCopy.lede == """
                Roost adds a hook to each agent you switch on so its tabs show status and send \
                notifications. It edits the agent files named below, plus Roost's own config, \
                and nothing else. roostctl agent uninstall --all puts the agent files back.
                """)
        #expect(
            AgentHooksCopy.footer == """
                Switching an agent on applies here and on every host this Roost connects to. \
                Switching one off applies here only. Change it any time from Agent Hooks… in \
                the command palette.
                """)
        #expect(
            AgentHooksCopy.codexNote == """
                Also pre-trusts Roost's hooks in config.toml so codex won't show its own \
                review dialog.
                """)
        #expect(
            AgentHooksCopy.opencodeNote
                == "This is a plugin that runs inside OpenCode, not a hook entry.")
    }

    /// The footer points the user at the palette row by name, so the row
    /// has to exist and be spelled the same way.
    @Test func thePaletteCarriesTheRowTheFooterNames() {
        let row = PaletteCommands.specs.first { $0.id == KeybindAction.agentHooks }
        #expect(row?.title == "Agent Hooks…")
        #expect(AgentHooksCopy.footer.contains("Agent Hooks…"))
    }
}

// MARK: - Reading a status walk back

@Suite("Agent-hooks sheet: the survey")
struct AgentHooksSurveyTests {
    private func survey(
        _ mode: AgentHooksCardMode, key: AgentHooks, reply: AgentHooksRun
    ) -> AgentHooksSurveyOutcome {
        agentHooksSurvey(
            mode: mode,
            command: agentHooksStatusCommand(roostctl: "/bin/roostctl", environment: [:]),
            key: { key },
            run: { _ in reply }
        )
    }

    @Test func aStatusWalkBecomesTheRowsTheSheetDraws() throws {
        let outcome = survey(
            .preferences, key: .allow(["claude"]),
            reply: AgentHooksRun(status: 0, stdout: agentHooksStatusJSON()))
        guard case .found(let found) = outcome else {
            Issue.record("expected rows, got \(outcome)")
            return
        }
        #expect(found.mode == .preferences)
        #expect(found.key == .allow(["claude"]))
        #expect(found.anyPresent)
        #expect(found.card.rows.count == 5)
        #expect(found.card.rows.allSatisfy { $0.status != nil })
    }

    /// The key rides back with the rows because it is the reading the
    /// rows were built from — and it is taken **after** the walk, which
    /// is where the twenty seconds go. Here the key is answered while
    /// the walk runs, exactly as `roostctl agent set --local` or a
    /// connecting client's `roost-session` would answer it.
    @Test func theKeyIsReadAfterTheWalkAndIsWhatThePrefillUses() {
        let walked = TimeoutFlag()
        let outcome = agentHooksSurvey(
            mode: .firstRun,
            command: agentHooksStatusCommand(roostctl: "/bin/roostctl", environment: [:]),
            key: { walked.get() ? .allow(["codex"]) : .ask },
            run: { _ in
                walked.set()
                return AgentHooksRun(status: 0, stdout: agentHooksStatusJSON())
            }
        )
        guard case .found(let found) = outcome else {
            Issue.record("expected rows, got \(outcome)")
            return
        }
        #expect(found.key == .allow(["codex"]), "the survey kept the reading from before the walk")
        // `.ask` would have prefilled every installed agent on; the
        // answer that arrived allows codex and nothing is wired yet, so
        // every switch is off.
        #expect(
            found.rows.allSatisfy { !$0.on },
            "the prefill followed the old snapshot's `.ask` branch")
        #expect(
            agentHooksSurveyVerdict(
                mode: .firstRun, anyPresent: found.anyPresent, key: found.key, sheetOpen: false)
                == .alreadyAnswered,
            "the raise decision followed a key the walk had already outlived")
    }

    /// A status walk that did not finish takes the question away rather
    /// than turning into an alert at launch: one line, no sheet.
    @Test func aFailedWalkIsOneLineAndNoSheet() {
        let exited = survey(
            .firstRun, key: .ask, reply: AgentHooksRun(status: 1, stderr: "no $HOME"))
        #expect(exited == .failed("agent hooks: roostctl agent status exited 1: no $HOME"))

        let timedOut = survey(
            .firstRun, key: .ask, reply: AgentHooksRun(status: -1, timedOut: true))
        #expect(
            timedOut
                == .failed(
                    "agent hooks: roostctl agent status timed out after "
                        + "\(Int(agentHooksSpawnTimeout))s"))

        guard
            case .failed = survey(
                .firstRun, key: .ask, reply: AgentHooksRun(status: 0, stdout: "not json"))
        else {
            Issue.record("unreadable JSON should have failed")
            return
        }
    }

    /// A machine with none of the five installed is one the walk reports
    /// as empty — which is what the verdict then refuses to ask about.
    @Test func aWalkOfAnEmptyMachineFindsNothingPresent() {
        let absent = AgentHooks.agentNames.map { AgentHooksStatus(agent: $0, present: false) }
        guard
            case .found(let found) = survey(
                .firstRun, key: .ask,
                reply: AgentHooksRun(status: 0, stdout: agentHooksStatusJSON(absent)))
        else {
            Issue.record("expected rows")
            return
        }
        #expect(!found.anyPresent)
        #expect(found.rows.count == 5, "the rows are the inventory, empty machine or not")
    }
}

/// The four arms, each of which decides whether Roost puts a question in
/// front of somebody. Mirrors `survey_verdict`'s table test in
/// `crates/roost-iced/src/app/agent_hooks_dialog.rs`.
@Suite("Agent-hooks sheet: the verdict")
struct AgentHooksSurveyVerdictTests {
    /// The key is judged as late as the walk finishes, so an answer
    /// given by `roostctl` or a connecting client while it ran stands
    /// down the first-run sheet.
    @Test func aFirstRunStandsDownForAnAnswerGivenWhileItRan() {
        for key in [AgentHooks.off, .allow(["claude"])] {
            #expect(
                agentHooksSurveyVerdict(
                    mode: .firstRun, anyPresent: true, key: key, sheetOpen: false)
                    == .alreadyAnswered, "\(key)")
        }
        #expect(
            agentHooksSurveyVerdict(
                mode: .firstRun, anyPresent: true, key: .ask, sheetOpen: false) == .raise)
    }

    /// Nothing installed here is nothing to consent about — the key
    /// stays unanswered rather than being answered by a sheet nobody
    /// could act on.
    @Test func nothingInstalledHereAsksNothing() {
        #expect(
            agentHooksSurveyVerdict(
                mode: .firstRun, anyPresent: false, key: .ask, sheetOpen: false) == .noAgent)
    }

    /// Preferences is a question the user just asked for, so an answered
    /// key — and an empty machine — is exactly what it is there to show.
    @Test func preferencesIsRaisedOverAnyKey() {
        #expect(
            agentHooksSurveyVerdict(
                mode: .preferences, anyPresent: false, key: .off, sheetOpen: false) == .raise)
    }

    /// Either mode stands down rather than replacing the sheet the user
    /// is already answering — with their switches on it.
    @Test func aSurveyNeverTakesTheScreenFromASheetTheUserIsAnswering() {
        for mode in [AgentHooksCardMode.firstRun, .preferences] {
            #expect(
                agentHooksSurveyVerdict(
                    mode: mode, anyPresent: true, key: .ask, sheetOpen: true) == .screenTaken,
                "\(mode)")
        }
    }
}

// MARK: - The environment a spawned roostctl gets

@Suite("Agent hooks: what the spawned roostctl sees")
struct AgentHooksEnvironmentTests {
    /// The child is handed the config path this app resolved, so a
    /// parent that never set `ROOST_CONFIG` cannot leave the two ends
    /// reading different files — and the harness fence rides along with
    /// everything else inherited.
    @Test func theChildIsPinnedToTheConfigPathThisAppResolved() {
        let env = agentHooksEnvironment(
            base: ["HOME": "/home/u", "ROOST_TEST_MODE": "1", "PATH": "/usr/bin"],
            home: "/home/fallback",
            configPath: "/home/u/.config/roost/config.conf"
        )
        #expect(env["ROOST_CONFIG"] == "/home/u/.config/roost/config.conf")
        #expect(env["ROOST_TEST_MODE"] == "1")
        #expect(env["PATH"] == "/usr/bin")
    }

    /// `HOME` is the inherited one wherever there is one: an E2E jail
    /// sets it, and a child that ignored it would reach the real
    /// dotfiles instead of the jail's.
    @Test func anInheritedHomeWins() {
        let env = agentHooksEnvironment(
            base: ["HOME": "/tmp/jail"], home: "/Users/real", configPath: "/tmp/jail/config.conf")
        #expect(env["HOME"] == "/tmp/jail")
    }

    /// And where there is none, the app's own answer stands in: a GUI
    /// app's environment is not a shell's, and the install engine fails
    /// outright without `HOME`.
    @Test func anAbsentHomeFallsBackToThisAppsOwn() {
        let env = agentHooksEnvironment(
            base: ["PATH": "/usr/bin"], home: "/Users/real",
            configPath: "/Users/real/.config/roost/config.conf")
        #expect(env["HOME"] == "/Users/real")
    }

    @Test func theSetCommandIsTheLocalFormWithJSON() {
        let command = agentHooksSetCommand(
            roostctl: "/bin/roostctl", spec: "claude,codex", environment: ["HOME": "/home/u"])
        #expect(
            command.argv == [
                "/bin/roostctl", "agent", "set", "--local", "claude,codex", "--json",
            ])
        #expect(command.environment["HOME"] == "/home/u")
    }
}
