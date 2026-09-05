// Launch-time agent-hook wiring for the Mac app (plan 046 §3.7).
//
// The install engine is Rust and lives in `roost-agent-install`; the
// only thing that links it on this platform is `roostctl`. So the app
// spawns `roostctl agent ensure --json` instead of reimplementing the
// engine in Swift — the same binary, the same `config.conf`, the same
// state record the Linux UI writes.
//
// Two rules the shape here exists to keep:
//
//   * It never runs on the main thread. `ensure` reads and writes files
//     under an advisory `flock` and can block behind another Roost doing
//     the same; AppKit would be frozen for the duration.
//   * It never alerts. A launch is the wrong moment to interrupt
//     someone about their dotfiles, and `roostctl agent status` plus
//     `roostctl doctor` are the durable places to look. A failure is one
//     line in the log.
//
// The toast the Linux UI shows for a first-time wiring has no Mac
// counterpart yet — the Swift chrome has no transient status surface —
// so the record's `noticed` stays false here and the first Linux launch
// on the same machine says it instead. That works because the toast is
// driven by `noticed: false` in the record rather than by "this run
// wired it" (`Outcome::unnoticed`, plan 046 §3.3): the later launch
// finds every agent already current and still has the sentence to say.

import Foundation

/// What the launch-time ensure should do, decided from pure inputs so
/// the decision is testable without spawning anything.
enum AgentHooksLaunchPlan: Equatable {
    /// `agent-hooks = off` — Roost wires nothing at launch.
    case disabledByConfig
    /// No `roostctl` to run (a `swift run` dev build with no embedded
    /// CLI). Nothing to do, and nothing wrong.
    case noRoostctl
    case run(argv: [String])
}

/// The `roostctl agent ensure` invocation for this launch, or the reason
/// there isn't one.
func agentHooksLaunchPlan(mode: AgentHooks, roostctl: String?) -> AgentHooksLaunchPlan {
    if mode == .off { return .disabledByConfig }
    guard let roostctl else { return .noRoostctl }
    return .run(argv: [roostctl, "agent", "ensure", "--json"])
}

/// How long the spawned `roostctl` gets before it is terminated. The
/// engine's own work is milliseconds; the budget is for the `flock`,
/// which is held by whichever Roost got there first.
let agentHooksEnsureTimeout: TimeInterval = 20

/// Run the launch-time ensure on a background queue, or don't.
///
/// Call from `applicationDidFinishLaunching`; returns immediately.
///
/// `roostctl` arrives as a **closure**, not as a resolved path: a
/// default argument is evaluated at the call site, and
/// `bundledRoostctl()` asks the filesystem whether a file is executable.
/// As a default value that stat ran on the main thread on every launch,
/// `agent-hooks = off` included. The only thing decided here is the one
/// fact already in memory — the config key; everything that touches a
/// disk happens inside the closure below.
func startAgentHooksEnsure(
    config: RoostConfig,
    roostctl: @escaping @Sendable () -> String? = { bundledRoostctl() },
    log: @escaping @Sendable (String) -> Void = { RoostLogger.shared.info($0) }
) {
    let mode = config.agentHooks
    if mode == .off {
        log("agent hooks: agent-hooks = off; not wiring")
        return
    }
    DispatchQueue.global(qos: .utility).async {
        switch agentHooksLaunchPlan(mode: mode, roostctl: roostctl()) {
        case .disabledByConfig:
            log("agent hooks: agent-hooks = off; not wiring")
        case .noRoostctl:
            log("agent hooks: no bundled roostctl; not wiring")
        case .run(let argv):
            log(agentHooksEnsureOutcome(argv: argv))
        }
    }
}

/// Run `argv` to completion (blocking — call off the main thread) and
/// return the one line worth logging about it.
///
/// Errors are returned rather than thrown into the void: this is the
/// boundary that handles them, and "handles" here means "says so once".
private func agentHooksEnsureOutcome(argv: [String]) -> String {
    let proc = Process()
    proc.executableURL = URL(fileURLWithPath: argv[0])
    proc.arguments = Array(argv.dropFirst())
    let outPipe = Pipe()
    let errPipe = Pipe()
    proc.standardOutput = outPipe
    proc.standardError = errPipe
    // stdin must not be the app's: a child inheriting a terminal it can
    // read from is a child that can block forever on it.
    proc.standardInput = FileHandle.nullDevice

    do {
        try proc.run()
    } catch {
        return "agent hooks: spawn \(argv[0]): \(error.localizedDescription)"
    }

    let box = ProcBox(proc)
    let timedOut = TimeoutFlag()
    let watchdog = DispatchWorkItem {
        timedOut.set()
        box.p.terminate()
        let pid = box.p.processIdentifier
        DispatchQueue.global(qos: .utility).asyncAfter(deadline: .now() + .milliseconds(500)) {
            if box.p.isRunning { kill(pid, SIGKILL) }
        }
    }
    DispatchQueue.global(qos: .utility)
        .asyncAfter(deadline: .now() + agentHooksEnsureTimeout, execute: watchdog)

    // Both pipes are drained concurrently: a child that fills one while
    // we block on the other deadlocks otherwise.
    let outDrain = PipeDrain(outPipe.fileHandleForReading)
    let errDrain = PipeDrain(errPipe.fileHandleForReading)
    let group = DispatchGroup()
    let readQ = DispatchQueue.global(qos: .utility)
    group.enter()
    readQ.async {
        outDrain.drain()
        group.leave()
    }
    group.enter()
    readQ.async {
        errDrain.drain()
        group.leave()
    }
    group.wait()
    proc.waitUntilExit()
    watchdog.cancel()

    if timedOut.get() {
        return "agent hooks: roostctl agent ensure timed out after \(Int(agentHooksEnsureTimeout))s"
    }
    let stdout = String(decoding: outDrain.result(), as: UTF8.self)
        .trimmingCharacters(in: .whitespacesAndNewlines)
    if proc.terminationStatus != 0 {
        let stderr = String(decoding: errDrain.result(), as: UTF8.self)
            .trimmingCharacters(in: .whitespacesAndNewlines)
        return "agent hooks: roostctl agent ensure exited \(proc.terminationStatus): "
            + (stderr.isEmpty ? stdout : stderr)
    }
    return "agent hooks: \(stdout)"
}
