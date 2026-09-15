// Launch-time agent-hook wiring for the Mac app (plan 046 §3.7, plan
// 064 §3.5).
//
// The install engine is Rust and lives in `roost-agent-install`; the
// only thing that links it on this platform is `roostctl`. So the app
// spawns `roostctl` instead of reimplementing the engine in Swift — the
// same binary, the same `config.conf`, the same state record the Linux
// UI writes.
//
// Two rules the shape here exists to keep:
//
//   * It never runs on the main thread. `ensure` reads and writes files
//     under an advisory `flock` and can block behind another Roost doing
//     the same; AppKit would be frozen for the duration.
//   * It never alerts about a write it made. Plan 064 removed the case
//     that could: a launch no longer touches anybody's dotfiles without
//     a prior answer to `agent-hooks`, so there is nothing to report
//     after the fact and `roostctl agent status` plus `roostctl doctor`
//     stay the durable places to look. A failure is one line in the log.
//     What a launch *may* now do is **ask** — an unanswered key puts the
//     consent sheet up before anything is written, which is a question,
//     not a receipt. A status walk that fails takes the question away
//     again rather than turning into an alert nobody asked for.
//
// The toast the Linux UI shows for a first-time wiring has no Mac
// counterpart yet — the Swift chrome has no transient status surface —
// so the record's `noticed` stays false here and the first Linux launch
// on the same machine says it instead. That works because the toast is
// driven by `noticed: false` in the record rather than by "this run
// wired it" (`Outcome::unnoticed`, plan 046 §3.3): the later launch
// finds every agent already current and still has the sentence to say.

import Foundation

/// What the launch-time decision comes to, from pure inputs so it is
/// testable without spawning anything.
enum AgentHooksLaunchPlan: Equatable {
    /// `agent-hooks = off` — Roost wires nothing at launch.
    case disabledByConfig
    /// No `roostctl` to run (a `swift run` dev build with no embedded
    /// CLI). Nothing to do, and nothing wrong.
    case noRoostctl
    /// `agent-hooks` is unanswered (plan 064): read every agent's
    /// status, then put the consent sheet up over what it found.
    /// Nothing is written until the user answers it.
    case survey(argv: [String])
    case run(argv: [String])
}

/// The `roostctl` invocation this launch makes, or the reason there
/// isn't one.
func agentHooksLaunchPlan(mode: AgentHooks, roostctl: String?) -> AgentHooksLaunchPlan {
    switch mode {
    case .off:
        return .disabledByConfig
    case .ask:
        guard let roostctl else { return .noRoostctl }
        // Status before sheet, and status is read-only: the card names
        // five agents' files and says which are here, and it may not
        // guess at either.
        return .survey(argv: [roostctl, "agent", "status", "--json"])
    case .allow:
        guard let roostctl else { return .noRoostctl }
        // `--startup` is the non-destructive shape: wire and refresh
        // what `agent-hooks` names, remove nothing. The bare verb is a
        // reconcile, and a launch has no business undoing a hook
        // somebody added by hand (plan 064 §3.2).
        return .run(argv: [roostctl, "agent", "ensure", "--startup", "--json"])
    }
}

/// How long a spawned `roostctl` gets before it is terminated. The
/// engine's own work is milliseconds; the budget is for the `flock`,
/// which is held by whichever Roost got there first.
let agentHooksSpawnTimeout: TimeInterval = 20

/// Do the launch-time agent-hooks work on a background queue, or don't.
///
/// Call from `applicationDidFinishLaunching`; returns immediately.
/// `present` is called on the main queue with what the status walk
/// found, and only ever for the unanswered key. Whether that becomes a
/// sheet is `agentHooksSurveyVerdict`'s to say, on the main thread,
/// because the last input it needs — is a sheet already up — only exists
/// there.
///
/// The runner arrives as **closures**, not as resolved values: a
/// default argument is evaluated at the call site, and
/// `bundledRoostctl()` asks the filesystem whether a file is
/// executable. As a default value that stat ran on the main thread on
/// every launch, `agent-hooks = off` included. The only thing decided
/// here is the one fact already in memory — the config key; everything
/// that touches a disk happens inside the closure below.
func startAgentHooksLaunch(
    config: RoostConfig,
    runner: AgentHooksRunner = .bundled,
    log: @escaping @Sendable (String) -> Void = { RoostLogger.shared.info($0) },
    key: @escaping @Sendable () -> AgentHooks = { RoostConfig.agentHooksOnDisk() },
    present: @escaping @Sendable @MainActor (AgentHooksFound) -> Void
) {
    let mode = config.agentHooks
    if mode == .off {
        log("agent hooks: agent-hooks = off; not wiring")
        return
    }
    DispatchQueue.global(qos: .utility).async {
        switch agentHooksLaunchPlan(mode: mode, roostctl: runner.roostctl()) {
        case .disabledByConfig:
            log("agent hooks: agent-hooks = off; not wiring")
        case .noRoostctl:
            log("agent hooks: no bundled roostctl; not wiring")
        case .run(let argv):
            let command = AgentHooksCommand(argv: argv, environment: runner.environment())
            log(agentHooksEnsureLine(runner.run(command)))
        case .survey(let argv):
            let command = AgentHooksCommand(argv: argv, environment: runner.environment())
            switch agentHooksSurvey(
                mode: .firstRun, command: command, key: key, run: runner.run)
            {
            case .found(let found):
                Task { @MainActor in present(found) }
            case .failed(let line):
                log(line)
            }
        }
    }
}

/// The one line worth logging about a finished `agent ensure`.
///
/// Errors are returned rather than thrown into the void: this is the
/// boundary that handles them, and "handles" here means "says so once".
private func agentHooksEnsureLine(_ outcome: AgentHooksRun) -> String {
    if outcome.timedOut {
        return "agent hooks: roostctl agent ensure timed out after \(Int(agentHooksSpawnTimeout))s"
    }
    if outcome.status != 0 {
        let detail = outcome.stderr.isEmpty ? outcome.stdout : outcome.stderr
        return "agent hooks: roostctl agent ensure exited \(outcome.status): \(detail)"
    }
    return "agent hooks: \(outcome.stdout)"
}

/// Run one `roostctl` to completion (blocking — call off the main
/// thread) under a watchdog.
func agentHooksRun(_ command: AgentHooksCommand) -> AgentHooksRun {
    let argv = command.argv
    let proc = Process()
    proc.executableURL = URL(fileURLWithPath: argv[0])
    proc.arguments = Array(argv.dropFirst())
    proc.environment = command.environment
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
        return AgentHooksRun(
            status: -1, stderr: "spawn \(argv[0]): \(error.localizedDescription)")
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
        .asyncAfter(deadline: .now() + agentHooksSpawnTimeout, execute: watchdog)

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

    let trim = { (data: Data) in
        String(decoding: data, as: UTF8.self)
            .trimmingCharacters(in: .whitespacesAndNewlines)
    }
    return AgentHooksRun(
        status: proc.terminationStatus,
        stdout: trim(outDrain.result()),
        stderr: trim(errDrain.result()),
        timedOut: timedOut.get()
    )
}

/// Run one `roostctl` off the Swift-concurrency cooperative pool.
///
/// [`agentHooksRun`] blocks on `waitpid`, which an executor thread may
/// not do: there are only as many of those as cores, and one held for
/// the install lock's whole 20 s budget is one the rest of the app
/// cannot get back.
func agentHooksRunOffPool(_ command: AgentHooksCommand, using runner: AgentHooksRunner) async
    -> AgentHooksRun
{
    await withCheckedContinuation { continuation in
        DispatchQueue.global(qos: .userInitiated).async {
            continuation.resume(returning: runner.run(command))
        }
    }
}
