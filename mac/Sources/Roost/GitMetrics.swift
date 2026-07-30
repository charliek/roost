// Git metrics for the agents palette (plan 005 §3.7).
//
// The Swift mirror of `crates/roost-linux/src/git_metrics.rs`, semantics
// for semantics: each agent row shows what its repo looks like right now
// — `"Nf +A -D"` (files touched, lines added, lines deleted) — probed
// asynchronously so the palette never blocks on `git`. Everything here is
// seam-shaped: a `CommandRunner` is injected, so the pipeline (dedupe →
// resolve root → diff + untracked → parse) is unit-tested without a repo
// on disk, and a future `roost-core` lift is mechanical.
//
// Threading: the batch runs off the main actor (`Task.detached` from
// `App.swift`, and the production runner spawns + waits on a background
// `DispatchQueue` — `waitUntilExit` never runs on the calling queue);
// results hop back to `@MainActor` before touching the cache or the panel.
//
// Every failure path — no cwd, not a repo, unborn HEAD, missing `git`,
// timeout, unparseable output, oversized output — surfaces as a thrown
// error, and the *caller* maps that to `GitMetrics.unknown` (errors are
// returned, not swallowed; the palette logs at that boundary). A clean
// repo renders `—` too: "clean" and "not a repo" deliberately look the
// same.

import Foundation

/// What one repo's worktree looks like versus `HEAD`.
struct GitMetrics: Equatable, Sendable {
    /// Tracked files changed **plus** untracked files.
    var files: UInt64 = 0
    var adds: UInt64 = 0
    var dels: UInt64 = 0

    /// Rendered when metrics can't be produced *or* the repo is clean.
    static let unknown = "—"

    var isClean: Bool { files == 0 && adds == 0 && dels == 0 }

    /// The rendered column. The minus is ASCII `-` (not `−`) on both UIs
    /// — greppability beats typography here.
    func text() -> String {
        if isClean { return Self.unknown }
        return "\(files)f +\(adds) -\(dels)"
    }
}

/// A probe failure. String-carrying like the Rust `Result<_, String>`
/// half, so the two sides' messages read the same in a log.
struct GitProbeError: Error, Equatable, Sendable, CustomStringConvertible {
    let message: String
    init(_ message: String) { self.message = message }
    var description: String { message }
}

/// What a runner reports back: exit success plus captured stdout. stderr
/// is discarded — every failure here is expressible as "this command did
/// not succeed", and a piped-but-unread stderr risks blocking the child.
struct CommandOutput: Sendable {
    let ok: Bool
    let stdout: Data

    init(ok: Bool, stdout: Data) {
        self.ok = ok
        self.stdout = stdout
    }
}

/// The injectable exec seam.
protocol CommandRunner: Sendable {
    func run(cwd: String, argv: [String]) async throws -> CommandOutput
}

// MARK: - The production runner

/// argv exec (no shell), `LC_ALL=C`, no stdin, stderr to `/dev/null`,
/// stdout capped. Cancellation — which is how `GitProbe` enforces its
/// timeout, and how a dropped batch unwinds — escalates SIGTERM →
/// SIGKILL at the child, the analog of the GTK runner's `kill_on_drop`.
final class GitRunner: CommandRunner {
    /// Captured stdout ceiling. Past it we can't trust a count anyway (a
    /// truncated `ls-files -z` under-counts), so the probe errors out.
    static let maxOutputBytes = 64 * 1024

    func run(cwd: String, argv: [String]) async throws -> CommandOutput {
        let child = ChildProcess()
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation {
                (cont: CheckedContinuation<CommandOutput, Error>) in
                // Off the calling queue: `readData` + `waitUntilExit`
                // below both block, and the caller may be the main actor.
                DispatchQueue.global(qos: .userInitiated).async {
                    cont.resume(with: Result { try Self.exec(cwd: cwd, argv: argv, child: child) })
                }
            }
        } onCancel: {
            child.cancel()
        }
    }

    private static func exec(cwd: String, argv: [String], child: ChildProcess) throws
        -> CommandOutput
    {
        let proc = Process()
        // `/usr/bin/env` resolves argv[0] against PATH the way the GTK
        // side's `Command::new("git")` does — no shell, no quoting, and a
        // missing `git` surfaces as a non-zero exit (→ `—`).
        proc.executableURL = URL(fileURLWithPath: "/usr/bin/env")
        proc.arguments = argv
        var environment = ProcessInfo.processInfo.environment
        // Byte-stable output regardless of the user's locale — the
        // shortstat parser matches English words.
        environment["LC_ALL"] = "C"
        proc.environment = environment
        proc.currentDirectoryURL = URL(fileURLWithPath: cwd)
        proc.standardInput = FileHandle.nullDevice
        proc.standardError = FileHandle.nullDevice
        let outPipe = Pipe()
        proc.standardOutput = outPipe

        // The cancelled check and the spawn are one atomic step: a cancel
        // that lands between them would otherwise see no running child,
        // schedule no kill, and leave `git` alive with this thread parked
        // in `waitUntilExit` forever (holding a permit).
        do {
            try child.launch(proc)
        } catch let error as GitProbeError {
            throw error
        } catch {
            // A deleted cwd lands here too.
            throw GitProbeError("spawn \(argv[0]): \(error.localizedDescription)")
        }
        // A cancel that raced the spawn blocked on the same lock; it has
        // either already killed the child or is about to. This re-check
        // covers the ordering where it observed the child before `launch`
        // published it, so the kill is guaranteed either way.
        if child.isCancelled { child.cancel() }

        let handle = outPipe.fileHandleForReading
        var stdout = Data()
        var overflowed = false
        while true {
            let chunk = handle.readData(ofLength: 64 * 1024)
            if chunk.isEmpty { break }
            stdout.append(chunk)
            if stdout.count > maxOutputBytes {
                overflowed = true
                break
            }
        }
        if overflowed {
            // A child still writing into a pipe nobody reads would hang
            // `waitUntilExit`, so kill it and drain what's buffered.
            child.cancel()
            _ = try? handle.readToEnd()
        }
        try? handle.close()
        proc.waitUntilExit()
        if overflowed {
            throw GitProbeError("\(argv[0]) output exceeded \(maxOutputBytes) bytes")
        }
        return CommandOutput(ok: proc.terminationStatus == 0, stdout: stdout)
    }
}

/// The spawn/kill gate: lock-guarded handle on the child so the
/// cancellation handler (any thread) can reach a `Process` the worker
/// queue owns. `Process` isn't `Sendable`; the lock is what makes this
/// safe. Internal rather than file-private so the cancel-vs-spawn
/// orderings are unit-testable — that race is the whole reason this type
/// exists (the GTK side gets it for free from `kill_on_drop`).
final class ChildProcess: @unchecked Sendable {
    private let lock = NSLock()
    private var proc: Process?
    private var cancelled = false

    var isCancelled: Bool {
        lock.lock()
        defer { lock.unlock() }
        return cancelled
    }

    /// Spawn `proc` **under the lock**, so "was this cancelled?" and "is
    /// there a live child?" can never disagree: a cancel racing this call
    /// either wins (this throws and nothing spawns) or blocks until the
    /// child is published and then kills it. Throws `GitProbeError` when
    /// a cancel already landed, and rethrows `Process.run`'s error
    /// otherwise.
    func launch(_ proc: Process) throws {
        lock.lock()
        defer { lock.unlock() }
        if cancelled { throw GitProbeError("cancelled before spawn") }
        try proc.run()
        self.proc = proc
    }

    /// SIGTERM now, SIGKILL shortly after — a child that ignores SIGTERM
    /// (or a descendant holding the pipe open) can't keep the blocking
    /// reader stuck past the deadline. Idempotent: the escalation only
    /// signals a still-running child.
    func cancel() {
        lock.lock()
        cancelled = true
        let proc = self.proc
        lock.unlock()
        guard let proc, proc.isRunning else { return }
        proc.terminate()
        let pid = proc.processIdentifier
        let escalate = DispatchWorkItem {
            if proc.isRunning { _ = Darwin.kill(pid, SIGKILL) }
        }
        DispatchQueue.global(qos: .userInitiated)
            .asyncAfter(deadline: .now() + .milliseconds(500), execute: escalate)
    }
}

// MARK: - The probe pipeline

/// One cwd's probe result, plus the repo root it resolved to (the key the
/// caller caches under, so two tabs in one repo share a value). `root` is
/// nil when the cwd never resolved to a repo at all.
struct ProbeOutcome: Sendable {
    let cwd: String
    let root: String?
    let value: ProbeValue
}

/// Where a row's metrics came from.
enum ProbeValue: Sendable {
    /// A fresh measurement — or the reason one couldn't be taken, which
    /// the caller renders as `—`.
    case measured(Result<GitMetrics, GitProbeError>)
    /// The session had already measured this repo root; no git ran and
    /// the existing text carries over verbatim.
    case reused(String)
}

/// The probe pipeline: a runner, a per-command timeout, and the
/// concurrency ceiling.
final class GitProbe: Sendable {
    /// Per-command budget. A pathological repo (fsmonitor cold, huge
    /// tree) resolves to `—` rather than holding a row pending forever.
    static let probeTimeout: TimeInterval = 1.5
    /// Ceiling on git processes in flight, so opening a palette over a
    /// dozen agent tabs can't fork-bomb the box.
    static let maxConcurrent = 4

    /// `--no-optional-locks` keeps a probe from perturbing the repo the
    /// user is working in (strix uses the same flag for the same reason).
    static let revParseArgv = ["git", "--no-optional-locks", "rev-parse", "--show-toplevel"]
    /// `HEAD --` diffs staged **and** unstaged work against the last
    /// commit; `--no-ext-diff --no-textconv` keep user diff filters out
    /// of the path.
    static let shortstatArgv = [
        "git", "--no-optional-locks", "diff", "--no-ext-diff", "--no-textconv", "--shortstat",
        "HEAD", "--",
    ]
    /// Real untracked *files*: `status --porcelain` collapses an
    /// untracked directory to one entry and would also re-list the
    /// modified files the shortstat already counted.
    static let untrackedArgv = [
        "git", "--no-optional-locks", "ls-files", "--others", "--exclude-standard", "-z",
    ]

    private let runner: CommandRunner
    private let timeout: TimeInterval
    private let slots: ProbeSlots

    convenience init() {
        self.init(runner: GitRunner(), timeout: Self.probeTimeout, slots: Self.maxConcurrent)
    }

    init(runner: CommandRunner, timeout: TimeInterval, slots: Int) {
        self.runner = runner
        self.timeout = timeout
        self.slots = ProbeSlots(count: slots)
    }

    /// Probe every cwd in one pass, deduping three ways: identical cwds
    /// share a `rev-parse`, cwds resolving to the same root share one
    /// diff + `ls-files` pair, and a root already in `known` isn't
    /// measured at all. Returns one outcome per **unique** cwd, in the
    /// order they first appear.
    ///
    /// `known` is the caller's session cache flattened to plain data
    /// (root → rendered text) so the batch stays pure — it never reads or
    /// writes the cache, and a rebuild that adds a second tab in an
    /// already-measured repo costs one `rev-parse`, not a whole
    /// re-measure that could fail and overwrite a good value.
    func probeBatch(cwds: [String], known: [String: String]) async -> [ProbeOutcome] {
        var seen = Set<String>()
        let unique = cwds.filter { seen.insert($0).inserted }

        let roots = await mapConcurrently(unique) { await self.resolveRoot($0) }

        var seenRoots = Set<String>()
        var wanted: [String] = []
        for case .success(let root) in roots where known[root] == nil {
            if seenRoots.insert(root).inserted { wanted.append(root) }
        }
        let measured = await mapConcurrently(wanted) { await self.metrics(forRoot: $0) }
        var byRoot: [String: Result<GitMetrics, GitProbeError>] = [:]
        for (root, result) in zip(wanted, measured) { byRoot[root] = result }

        return zip(unique, roots).map { cwd, resolved in
            switch resolved {
            case .success(let root):
                let value: ProbeValue
                if let text = known[root] {
                    value = .reused(text)
                } else {
                    value = .measured(
                        byRoot[root] ?? .failure(GitProbeError("no probe result for \(root)")))
                }
                return ProbeOutcome(cwd: cwd, root: root, value: value)
            case .failure(let error):
                return ProbeOutcome(cwd: cwd, root: nil, value: .measured(.failure(error)))
            }
        }
    }

    /// The repo containing `cwd`. A deleted cwd fails at spawn; a
    /// non-repo fails the command — both land here as a failure.
    private func resolveRoot(_ cwd: String) async -> Result<String, GitProbeError> {
        if cwd.isEmpty { return .failure(GitProbeError("tab has no cwd")) }
        do {
            let out = try await run(cwd: cwd, argv: Self.revParseArgv)
            if !out.ok { return .failure(GitProbeError("not a git repository: \(cwd)")) }
            let root = GitMetrics.stripEOL(String(decoding: out.stdout, as: UTF8.self))
            if root.isEmpty {
                return .failure(GitProbeError("empty repository root for \(cwd)"))
            }
            return .success(root)
        } catch {
            return .failure(Self.probeError(error))
        }
    }

    /// The expensive half, run from the repo root exactly once per root.
    private func metrics(forRoot root: String) async -> Result<GitMetrics, GitProbeError> {
        // Independent commands; the permit pool still bounds the total in
        // flight, and neither holds a permit while waiting on the other.
        async let shortstatTask = runResult(cwd: root, argv: Self.shortstatArgv)
        async let untrackedTask = runResult(cwd: root, argv: Self.untrackedArgv)
        let (shortstat, untracked) = await (shortstatTask, untrackedTask)

        do {
            let diff = try shortstat.get()
            if !diff.ok {
                // Overwhelmingly: an unborn HEAD (fresh `git init`).
                return .failure(GitProbeError("git diff HEAD failed in \(root)"))
            }
            var metrics = try GitMetrics.parseShortstat(
                String(decoding: diff.stdout, as: UTF8.self))

            let others = try untracked.get()
            if !others.ok {
                return .failure(GitProbeError("git ls-files failed in \(root)"))
            }
            // Untracked files are *additional* files touched — the
            // shortstat only ever counts tracked ones, so there's nothing
            // to subtract. Both halves are attacker-adjacent numbers (a
            // crafted shortstat line parses as any UInt64), so the sum is
            // checked rather than allowed to trap or wrap.
            let (files, overflow) = metrics.files.addingReportingOverflow(
                GitMetrics.countUntracked(others.stdout))
            if overflow { return .failure(GitProbeError("implausible file count in \(root)")) }
            metrics.files = files
            return .success(metrics)
        } catch {
            return .failure(Self.probeError(error))
        }
    }

    private func runResult(cwd: String, argv: [String]) async -> Result<
        CommandOutput, GitProbeError
    > {
        do { return .success(try await run(cwd: cwd, argv: argv)) } catch {
            return .failure(Self.probeError(error))
        }
    }

    /// One command, permit-gated and time-boxed. On timeout the runner's
    /// task is cancelled, which kills the child (`ChildProcess.cancel`) —
    /// the same shape as the GTK side dropping a `kill_on_drop` future.
    private func run(cwd: String, argv: [String]) async throws -> CommandOutput {
        try await slots.withPermit {
            try await withThrowingTaskGroup(of: CommandOutput.self) { group in
                group.addTask { try await self.runner.run(cwd: cwd, argv: argv) }
                group.addTask {
                    try await Task.sleep(for: .seconds(self.timeout))
                    throw GitProbeError("\(argv[0]) timed out after \(self.timeout)s")
                }
                defer { group.cancelAll() }
                guard let first = try await group.next() else {
                    throw GitProbeError("\(argv[0]) produced no result")
                }
                return first
            }
        }
    }

    private static func probeError(_ error: Error) -> GitProbeError {
        (error as? GitProbeError) ?? GitProbeError("\(error)")
    }

    /// `map` over a task group, preserving input order.
    private func mapConcurrently<T: Sendable>(
        _ inputs: [String], _ body: @escaping @Sendable (String) async -> T
    ) async -> [T] {
        await withTaskGroup(of: (Int, T).self) { group in
            for (index, input) in inputs.enumerated() {
                group.addTask { (index, await body(input)) }
            }
            var out = [T?](repeating: nil, count: inputs.count)
            for await (index, value) in group { out[index] = value }
            return out.compactMap { $0 }
        }
    }
}

/// Async permit pool — the `tokio::sync::Semaphore` stand-in. An actor
/// rather than a `DispatchSemaphore` so waiting suspends the task instead
/// of blocking a thread from the cooperative pool.
private actor ProbeSlots {
    private var available: Int
    private var waiters: [CheckedContinuation<Void, Never>] = []

    init(count: Int) { available = max(count, 1) }

    func withPermit<T: Sendable>(_ body: @Sendable () async throws -> T) async rethrows -> T {
        await acquire()
        defer { release() }
        return try await body()
    }

    private func acquire() async {
        if available > 0 {
            available -= 1
            return
        }
        await withCheckedContinuation { waiters.append($0) }
    }

    private func release() {
        if waiters.isEmpty {
            available += 1
        } else {
            waiters.removeFirst().resume()
        }
    }
}

// MARK: - Parsers

extension GitMetrics {
    /// Strip the single line terminator git appends to a one-line answer.
    /// Trimming would corrupt a real root whose last path component ends
    /// in a space or a tab — legal on every filesystem Roost runs on.
    /// Works on UTF-8 bytes, not `Character`s: Swift folds `"\r\n"` into
    /// a single grapheme cluster, so a `hasSuffix("\n")` test would miss
    /// a CRLF entirely and leave the `\r` in the path.
    static func stripEOL(_ text: String) -> String {
        var bytes = Array(text.utf8)
        if bytes.last == UInt8(ascii: "\n") { bytes.removeLast() }
        if bytes.last == UInt8(ascii: "\r") { bytes.removeLast() }
        return String(decoding: bytes, as: UTF8.self)
    }

    /// Parse `git diff --shortstat` output.
    ///
    /// Empty output means "nothing changed" — a clean worktree, not a
    /// failure. Otherwise the summary is the last line that parses;
    /// earlier lines (rename / binary stat lines a repo's diff config can
    /// emit) are skipped. Partial lines are normal: git omits the
    /// insertions or deletions clause entirely when it is zero.
    static func parseShortstat(_ stdout: String) throws -> GitMetrics {
        let trimmed = stdout.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty { return GitMetrics() }
        // `"\r\n"` is one Swift `Character`, so it has to be named as a
        // separator explicitly — splitting on `"\n"` alone would keep a
        // CRLF line whole.
        let lines = stdout.split(omittingEmptySubsequences: false) { $0 == "\n" || $0 == "\r\n" }
        for line in lines.reversed() {
            let line = line.hasSuffix("\r") ? line.dropLast() : line
            if let metrics = parseShortstatLine(line) { return metrics }
        }
        throw GitProbeError("unparseable shortstat: \(trimmed)")
    }

    private static func parseShortstatLine(_ line: some StringProtocol) -> GitMetrics? {
        var metrics = GitMetrics()
        var sawFiles = false
        for rawSegment in line.split(separator: ",", omittingEmptySubsequences: false) {
            let segment = rawSegment.trimmingCharacters(in: .whitespacesAndNewlines)
            guard let space = segment.firstIndex(of: " ") else { return nil }
            guard let count = UInt64(segment[segment.startIndex..<space]) else { return nil }
            let rest = segment[segment.index(after: space)...]
            if rest.hasPrefix("file") {
                metrics.files = count
                sawFiles = true
            } else if rest.hasPrefix("insertion") {
                metrics.adds = count
            } else if rest.hasPrefix("deletion") {
                metrics.dels = count
            } else {
                return nil
            }
        }
        return sawFiles ? metrics : nil
    }

    /// Count NUL-separated `ls-files -z` entries. `-z` is what makes this
    /// safe for paths with newlines or quotes in them.
    static func countUntracked(_ stdout: Data) -> UInt64 {
        UInt64(stdout.split(separator: 0, omittingEmptySubsequences: true).count)
    }
}

// MARK: - The session cache

/// Resolved metrics for one open palette session (plan 005 §3.7).
///
/// Keyed by repo root so two tabs in the same repo render one probe's
/// result, with a cwd → root index because a palette row only knows its
/// tab's cwd. `pending` keeps a live-refresh rebuild from re-spawning a
/// probe that is already in flight. The whole thing is discarded when the
/// palette session changes, so a dismiss → reopen re-probes.
struct MetricsCache {
    private(set) var session: Int = 0
    private var rootOf: [String: String] = [:]
    /// Rendered text per repo **root** — the only thing handed to a later
    /// batch as `known`, so nothing but a real root can suppress a
    /// measurement.
    private var textOf: [String: String] = [:]
    /// Cwds that resolved to no repo at all. They render `—` without
    /// occupying a root key.
    private var unresolved: Set<String> = []
    private var pending: Set<String> = []

    /// Bind the cache to a palette session, clearing it if this is a
    /// different session than the one it holds.
    mutating func beginSession(_ session: Int) {
        if self.session == session { return }
        self.session = session
        rootOf.removeAll()
        textOf.removeAll()
        unresolved.removeAll()
        pending.removeAll()
    }

    /// The rendered metrics for a tab's cwd **as of `session`** — nil
    /// while a probe is pending, and nil for any other session.
    ///
    /// The session argument is the point: a palette frame is built before
    /// `beginSession` runs for it, so a plain lookup would let a reopened
    /// palette flash the numbers the previous session resolved.
    func text(forSession session: Int, cwd: String) -> String? {
        if self.session != session { return nil }
        return text(cwd: cwd)
    }

    private func text(cwd: String) -> String? {
        if unresolved.contains(cwd) { return GitMetrics.unknown }
        guard let root = rootOf[cwd] else { return nil }
        return textOf[root]
    }

    /// The roots this session has already measured, with their rendered
    /// text. Handed to the next batch so a newly listed tab inside a
    /// known repo reuses the value instead of re-running the expensive
    /// pair (whose failure would otherwise overwrite a good number).
    func knownRoots() -> [String: String] { textOf }

    /// The cwds that still need a probe — neither resolved nor already in
    /// flight — marking them in flight. Duplicates collapse. An empty cwd
    /// is claimed like any other: the probe errors on it without spawning
    /// git, which is how its row reaches `—` instead of staying pending.
    mutating func claimUnprobed(_ cwds: some Sequence<String>) -> [String] {
        var claimed: [String] = []
        for cwd in cwds {
            if text(cwd: cwd) != nil || pending.contains(cwd) { continue }
            pending.insert(cwd)
            claimed.append(cwd)
        }
        return claimed
    }

    /// Record a probe that landed on a repo root. **First write wins**
    /// for the root's text within a session: two batches can race the
    /// same root (a live refresh spawned while the first was in flight),
    /// and a later failure must not clobber a good number.
    mutating func storeRoot(cwd: String, root: String, text: String) {
        pending.remove(cwd)
        rootOf[cwd] = root
        if textOf[root] == nil { textOf[root] = text }
    }

    /// Record a cwd that resolved to no repo — no cwd at all, a deleted
    /// directory, a non-repo, or a probe whose task died. It renders `—`
    /// for the rest of the session rather than being re-probed on every
    /// refresh (and, critically, rather than staying pending forever).
    mutating func storeUnresolved(cwd: String) {
        pending.remove(cwd)
        unresolved.insert(cwd)
    }
}
