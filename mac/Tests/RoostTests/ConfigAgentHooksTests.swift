// Config parser tests for the `agent-hooks` setting (plan 064; supersedes
// plan 046 §3.6), plus the launch-time decision it drives. Mirrors
// `crates/roost-ui-model/src/config.rs::tests` (`agent_hooks_*`; shared
// by iced) so the two UIs agree on the switch that decides whether
// Roost edits the user's dotfiles.

import Foundation
import Testing

@testable import Roost

@Suite("RoostConfig agent-hooks parsing")
struct ConfigAgentHooksTests {
    @Test func defaultsToAsk() {
        #expect(parse("").agentHooks == .ask)
        #expect(RoostConfig.empty.agentHooks == .ask)
    }

    @Test func acceptsANameListNormalisedIntoAgentNamesOrder() {
        #expect(parse("agent-hooks = claude, codex").agentHooks == .allow(["claude", "codex"]))
        // Mixed case, extra whitespace, and codex-first input all
        // normalise the same way: `agentNames` order, not source order.
        #expect(parse(" agent-hooks = Codex , CLAUDE ").agentHooks == .allow(["claude", "codex"]))
        #expect(parse("agent-hooks = \"claude,codex\"").agentHooks == .allow(["claude", "codex"]))
    }

    @Test func acceptsTheOffSpellings() {
        #expect(parse("agent-hooks = off").agentHooks == .off)
        #expect(parse("agent-hooks = false").agentHooks == .off)
        #expect(parse("agent-hooks = no").agentHooks == .off)
        #expect(parse("agent-hooks = OFF").agentHooks == .off)
    }

    /// Absent or empty resolves to `.ask` — silently. A fresh install
    /// that never wrote the key is the common path, not a mistake, and
    /// must never log.
    @Test func absentOrEmptyIsAskWithNoLog() {
        #expect(parse("").agentHooks == .ask)
        #expect(parse("agent-hooks =").agentHooks == .ask)
        #expect(parse("agent-hooks = \"\"").agentHooks == .ask)
    }

    /// The retired switch spellings (plan 046's `auto`/`on`/`true`/
    /// `yes`) and plain garbage both resolve to `.ask` — values somebody
    /// actually wrote, so a typo must not read as `off` or as consent
    /// nobody gave.
    @Test func unrecognisedValuesAreAsk() {
        for body in ["auto", "on", "true", "yes", "banana"] {
            #expect(parse("agent-hooks = \(body)").agentHooks == .ask, "\(body)")
        }
    }

    /// A reserved word beside anything else makes the whole value
    /// ambiguous rather than "off with an extra".
    @Test func aReservedWordMixedWithAnythingElseIsAsk() {
        for body in ["off, claude", "auto, claude", "no, codex"] {
            #expect(parse("agent-hooks = \(body)").agentHooks == .ask, "\(body)")
        }
    }

    /// An unrecognised name beside a recognised one is not ambiguous the
    /// same way — the recognised name is the answer.
    @Test func dropsUnrecognisedNamesAndKeepsTheRest() {
        #expect(parse("agent-hooks = claude, banana").agentHooks == .allow(["claude"]))
    }

    @Test func collapsesDuplicates() {
        #expect(parse("agent-hooks = claude, claude").agentHooks == .allow(["claude"]))
    }

    /// The key is last-wins like every other scalar here, including when
    /// the later line fails to parse: it returns to `.ask`, not to
    /// whatever an earlier line set. Mirrors the Rust
    /// `agent_hooks_is_last_wins`.
    @Test func repeatedKeyReturnsToAsk() {
        #expect(parse("agent-hooks = claude\nagent-hooks = off").agentHooks == .off)
        #expect(parse("agent-hooks = claude\nagent-hooks = banana").agentHooks == .ask)
    }

    @Test func configValueRoundTrips() {
        #expect(AgentHooks.allow(["cursor", "claude"]).configValue == "claude, cursor")
        #expect(AgentHooks.off.configValue == "off")
        #expect(AgentHooks.ask.configValue == nil)
    }

    /// The retired `agent-hooks-skip` key now falls through the parser
    /// as an unknown key and must not disturb `agent-hooks` on the same
    /// line set.
    @Test func theSkipKeyIsIgnoredHereWithoutDisturbingItsSibling() {
        let cfg = parse("agent-hooks-skip = codex, grok\nagent-hooks = off\n")
        #expect(cfg.agentHooks == .off)
        #expect(cfg.themeName == nil)
    }
}

/// Re-reading the one key, which is what every surface that has to know
/// what is true *now* asks for — a finished Apply, a finished status
/// walk. Mirrors `agent_hooks::hooks_on_disk` on the iced side.
@Suite("RoostConfig.agentHooksOnDisk")
struct ConfigAgentHooksOnDiskTests {
    private func inTempDir(_ body: (URL) throws -> Void) throws {
        let fm = FileManager.default
        let tmp = fm.temporaryDirectory.appendingPathComponent("roost-hooks-\(UUID().uuidString)")
        try fm.createDirectory(at: tmp, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: tmp) }
        try body(tmp)
    }

    @Test func readsTheKeyOutOfTheFileOnDisk() throws {
        try inTempDir { dir in
            let path = dir.appendingPathComponent("config.conf")
            try "theme = roost-dark\nagent-hooks = Codex, CLAUDE\n"
                .write(to: path, atomically: true, encoding: .utf8)
            #expect(
                RoostConfig.agentHooksOnDisk(fallback: .off, at: path) == .allow(["claude", "codex"])
            )
            try "agent-hooks = off\n".write(to: path, atomically: true, encoding: .utf8)
            #expect(RoostConfig.agentHooksOnDisk(fallback: .ask, at: path) == .off)
        }
    }

    /// An absent file is unanswered, not a failure: it is what a machine
    /// nobody has consented on looks like, and the fallback must not
    /// stand in for it.
    @Test func anAbsentFileIsAskWhateverTheCallerBelieved() throws {
        try inTempDir { dir in
            let path = dir.appendingPathComponent("nothing-here.conf")
            #expect(RoostConfig.agentHooksOnDisk(fallback: .allow(["claude"]), at: path) == .ask)
        }
    }

    /// A file that exists and cannot be read is the one case the
    /// fallback is for — failing to read is not a reason to say
    /// something different.
    @Test func anUnreadableFileKeepsWhatTheCallerBelieved() throws {
        try inTempDir { dir in
            // A directory where a file is expected: readable as an
            // entry, not as text.
            let path = dir.appendingPathComponent("config.conf")
            try FileManager.default.createDirectory(at: path, withIntermediateDirectories: true)
            #expect(
                RoostConfig.agentHooksOnDisk(fallback: .allow(["codex"]), at: path)
                    == .allow(["codex"]))
        }
    }
}

@Suite("Launch-time agent-hooks ensure")
struct AgentHooksLaunchPlanTests {
    /// `--startup` is not decoration: the bare verb reconciles, which
    /// would have a launch remove a hook the user added by hand.
    @Test func allowRunsTheNonDestructiveStartupEnsure() {
        #expect(
            agentHooksLaunchPlan(
                mode: .allow(["claude"]),
                roostctl: "/Apps/Roost.app/Resources/bin/roostctl"
            )
                == .run(argv: [
                    "/Apps/Roost.app/Resources/bin/roostctl", "agent", "ensure", "--startup",
                    "--json",
                ])
        )
    }

    /// The whole point of the key: `off` must not spawn anything, so no
    /// launch of this app can reach the user's dotfiles.
    @Test func offSpawnsNothing() {
        #expect(
            agentHooksLaunchPlan(mode: .off, roostctl: "/Apps/Roost.app/Resources/bin/roostctl")
                == .disabledByConfig
        )
        // Even with no binary, `off` reports the config reason: which of
        // the two stopped it is what a log reader needs to know.
        #expect(agentHooksLaunchPlan(mode: .off, roostctl: nil) == .disabledByConfig)
    }

    /// `ask` — nobody has answered the consent sheet — writes nothing.
    /// What it does instead is *read*: the status walk comes first, and
    /// the sheet goes up over what it found (plan 064 §3.5).
    @Test func askReadsStatusBeforeItAsks() {
        #expect(
            agentHooksLaunchPlan(mode: .ask, roostctl: "/Apps/Roost.app/Resources/bin/roostctl")
                == .survey(argv: [
                    "/Apps/Roost.app/Resources/bin/roostctl", "agent", "status", "--json",
                ])
        )
        #expect(agentHooksLaunchPlan(mode: .ask, roostctl: nil) == .noRoostctl)
    }

    /// The one thing the `ask` arm may never do is write, so the verb it
    /// names is checked rather than assumed: `status` is the read-only
    /// one, and `ensure`/`set`/`install` are not.
    @Test func theAskArmNamesNoVerbThatWrites() {
        guard
            case .survey(let argv) = agentHooksLaunchPlan(
                mode: .ask, roostctl: "/bin/roostctl")
        else {
            Issue.record("ask did not plan a survey")
            return
        }
        #expect(argv.contains("status"))
        for verb in ["ensure", "set", "install", "uninstall"] {
            #expect(!argv.contains(verb), "the ask arm would have run `agent \(verb)`")
        }
    }

    /// A `swift run` dev build has no embedded CLI. Nothing to run is
    /// not an error.
    @Test func noBundledRoostctlIsNotAFailure() {
        #expect(agentHooksLaunchPlan(mode: .allow(["claude"]), roostctl: nil) == .noRoostctl)
    }
}

/// Collects the lines `startAgentHooksLaunch` logs, from whichever queue
/// logs them, and releases a waiter when one arrives.
private final class LogSink: @unchecked Sendable {
    private let lock = NSLock()
    private var lines: [String] = []
    let arrived = DispatchSemaphore(value: 0)

    func log(_ line: String) {
        lock.lock()
        lines.append(line)
        lock.unlock()
        arrived.signal()
    }

    func all() -> [String] {
        lock.lock()
        defer { lock.unlock() }
        return lines
    }
}

/// `startAgentHooksLaunch` is called from
/// `applicationDidFinishLaunching`, so what it does *before* it
/// dispatches runs on the main thread. `bundledRoostctl()` asks the
/// filesystem whether a file is executable, which is exactly the kind of
/// work AppKit must not wait on — and as a default argument it ran at the
/// call site on every launch, including the one the user turned off.
@Suite("Launch-time agent hooks: what runs on the main thread")
struct AgentHooksLaunchThreadingTests {
    /// The off arm returns before it dispatches anything, so this is a
    /// plain synchronous check: nothing asked the filesystem a question.
    @MainActor
    @Test func offResolvesNothingAndTouchesNoFilesystem() {
        var config = RoostConfig.empty
        config.agentHooks = .off
        let resolved = TimeoutFlag()
        let sink = LogSink()

        startAgentHooksLaunch(
            config: config,
            runner: AgentHooksSpawnLog(
                roostctl: nil, onResolve: { resolved.set() }
            ).runner,
            log: sink.log,
            key: { .off },
            present: { _ in }
        )

        #expect(resolved.get() == false, "`off` still probed the bundle for roostctl")
        #expect(sink.all() == ["agent hooks: agent-hooks = off; not wiring"])
    }

    /// `ask` now has work to do — it reads every agent's status before it
    /// asks — so the invariant it has to keep is the same one `allow`
    /// keeps: none of that touches the main thread.
    @MainActor
    @Test func askResolvesRoostctlOffTheMainThread() {
        var config = RoostConfig.empty
        config.agentHooks = .ask
        let onMain = TimeoutFlag()
        let sink = LogSink()

        startAgentHooksLaunch(
            config: config,
            runner: AgentHooksSpawnLog(
                roostctl: nil,
                onResolve: { if Thread.isMainThread { onMain.set() } }
            ).runner,
            log: sink.log,
            key: { .ask },
            present: { _ in }
        )

        #expect(sink.arrived.wait(timeout: .now() + 10) == .success)
        #expect(onMain.get() == false, "roostctl was resolved on the main thread")
        #expect(sink.all() == ["agent hooks: no bundled roostctl; not wiring"])
    }

    /// And when it is allowed, the resolution happens — but off the main
    /// thread, which is the thread `applicationDidFinishLaunching` calls
    /// this from.
    @MainActor
    @Test func allowResolvesRoostctlOffTheMainThread() {
        var config = RoostConfig.empty
        config.agentHooks = .allow(["claude"])
        let ran = TimeoutFlag()
        let onMain = TimeoutFlag()
        let sink = LogSink()

        startAgentHooksLaunch(
            config: config,
            runner: AgentHooksSpawnLog(
                roostctl: nil,
                onResolve: {
                    if Thread.isMainThread { onMain.set() }
                    ran.set()
                }
            ).runner,
            log: sink.log,
            key: { .allow(["claude"]) },
            present: { _ in }
        )

        #expect(sink.arrived.wait(timeout: .now() + 10) == .success)
        #expect(ran.get() == true)
        #expect(onMain.get() == false, "roostctl was resolved on the main thread")
        #expect(sink.all() == ["agent hooks: no bundled roostctl; not wiring"])
    }

    /// The launch's `ask` arm runs the read-only walk and hands the sheet
    /// what it found — one spawn, and `agent status`, not `agent set`.
    @MainActor
    @Test func askSpawnsTheStatusWalkAndRaisesTheSheet() async throws {
        var config = RoostConfig.empty
        config.agentHooks = .ask
        let log = AgentHooksSpawnLog(
            roostctl: "/bin/roostctl",
            reply: { _ in AgentHooksRun(status: 0, stdout: agentHooksStatusJSON()) }
        )
        let raised = AgentHooksFoundBox()

        startAgentHooksLaunch(
            config: config,
            runner: log.runner,
            log: { _ in },
            key: { .ask },
            present: { found in raised.put(found) }
        )

        try await raised.wait()
        #expect(log.commands().map(\.argv) == [["/bin/roostctl", "agent", "status", "--json"]])
        let found = try #require(raised.take())
        #expect(found.mode == .firstRun)
        #expect(found.key == .ask)
        #expect(found.card.rows.map(\.agent) == AgentHooks.agentNames)
    }
}
