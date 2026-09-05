// Config parser tests for the `agent-hooks` setting (plan 046 §3.6),
// plus the launch-time decision it drives. Mirrors
// `crates/roost-ui-model/src/config.rs::tests` (`agent_hooks_*`; shared
// by iced) so the two UIs agree on the switch that decides whether
// Roost edits the user's dotfiles.

import Foundation
import Testing

@testable import Roost

@Suite("RoostConfig agent-hooks parsing")
struct ConfigAgentHooksTests {
    @Test func defaultsToAuto() {
        #expect(parse("").agentHooks == .auto)
        #expect(RoostConfig.empty.agentHooks == .auto)
    }

    @Test func parsesAutoAndOff() {
        #expect(parse("agent-hooks = auto").agentHooks == .auto)
        #expect(parse("agent-hooks = on").agentHooks == .auto)
        #expect(parse("agent-hooks = off").agentHooks == .off)
        #expect(parse("agent-hooks = false").agentHooks == .off)
        #expect(parse("agent-hooks = no").agentHooks == .off)
    }

    // Quoted and CRLF forms must agree with the Rust mirror.
    @Test func parsesQuotedAndCRLFValues() {
        #expect(parse("agent-hooks = \"off\"").agentHooks == .off)
        #expect(parse("agent-hooks = 'off'").agentHooks == .off)
        #expect(parse("agent-hooks = off\r\n").agentHooks == .off)
        #expect(parse("agent-hooks = \"off\"\r\n").agentHooks == .off)
        #expect(parse("agent-hooks = OFF").agentHooks == .off)
    }

    /// A typo must not read as `off`: silently disabling the wiring is
    /// the failure that is hardest to notice.
    @Test func unknownValueKeepsDefault() {
        #expect(parse("agent-hooks = pancakes").agentHooks == .auto)
    }

    /// …and "keeps the default" has to mean the *default*, not "keeps
    /// whatever an earlier line said". Mirrors the Rust
    /// `agent_hooks_is_last_wins_including_an_empty_or_invalid_repeat`.
    @Test func repeatedKeyReturnsToTheDefault() {
        #expect(parse("agent-hooks = off\nagent-hooks =").agentHooks == .auto)
        #expect(parse("agent-hooks = off\nagent-hooks = \"\"").agentHooks == .auto)
        #expect(parse("agent-hooks = off\nagent-hooks = pancakes").agentHooks == .auto)
        // The reverse repeat is ordinary last-wins and must still work.
        #expect(parse("agent-hooks = auto\nagent-hooks = off").agentHooks == .off)
    }

    /// `agent-hooks-skip` has no Swift mirror — the `roostctl` this app
    /// spawns reads it — so it must fall through the parser as an
    /// unknown key rather than disturbing anything.
    @Test func theSkipKeyIsIgnoredHereWithoutDisturbingItsSibling() {
        let cfg = parse("agent-hooks-skip = codex, grok\nagent-hooks = off\n")
        #expect(cfg.agentHooks == .off)
        #expect(cfg.themeName == nil)
    }
}

@Suite("Launch-time agent-hooks ensure")
struct AgentHooksLaunchPlanTests {
    @Test func autoRunsTheEnsureVerb() {
        #expect(
            agentHooksLaunchPlan(mode: .auto, roostctl: "/Apps/Roost.app/Resources/bin/roostctl")
                == .run(argv: [
                    "/Apps/Roost.app/Resources/bin/roostctl", "agent", "ensure", "--json",
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

    /// A `swift run` dev build has no embedded CLI. Nothing to run is
    /// not an error.
    @Test func noBundledRoostctlIsNotAFailure() {
        #expect(agentHooksLaunchPlan(mode: .auto, roostctl: nil) == .noRoostctl)
    }
}

/// Collects the lines `startAgentHooksEnsure` logs, from whichever queue
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

/// `startAgentHooksEnsure` is called from `applicationDidFinishLaunching`,
/// so what it does *before* it dispatches runs on the main thread.
/// `bundledRoostctl()` asks the filesystem whether a file is executable,
/// which is exactly the kind of work AppKit must not wait on — and as a
/// default argument it ran at the call site on every launch, including
/// the one the user turned off.
@Suite("Launch-time agent-hooks ensure: what runs on the main thread")
struct AgentHooksLaunchThreadingTests {
    /// The off arm returns before it dispatches anything, so this is a
    /// plain synchronous check: nothing asked the filesystem a question.
    @MainActor
    @Test func offResolvesNothingAndTouchesNoFilesystem() {
        var config = RoostConfig.empty
        config.agentHooks = .off
        let resolved = TimeoutFlag()
        let sink = LogSink()

        startAgentHooksEnsure(
            config: config,
            roostctl: {
                resolved.set()
                return nil
            },
            log: sink.log
        )

        #expect(resolved.get() == false, "`off` still probed the bundle for roostctl")
        #expect(sink.all() == ["agent hooks: agent-hooks = off; not wiring"])
    }

    /// And when it is on, the resolution happens — but off the main
    /// thread, which is the thread `applicationDidFinishLaunching` calls
    /// this from.
    @MainActor
    @Test func autoResolvesRoostctlOffTheMainThread() {
        let config = RoostConfig.empty  // `agent-hooks` defaults to auto
        let ran = TimeoutFlag()
        let onMain = TimeoutFlag()
        let sink = LogSink()

        startAgentHooksEnsure(
            config: config,
            roostctl: {
                if Thread.isMainThread { onMain.set() }
                ran.set()
                return nil
            },
            log: sink.log
        )

        #expect(sink.arrived.wait(timeout: .now() + 10) == .success)
        #expect(ran.get() == true)
        #expect(onMain.get() == false, "roostctl was resolved on the main thread")
        #expect(sink.all() == ["agent hooks: no bundled roostctl; not wiring"])
    }
}
