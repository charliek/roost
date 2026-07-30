// Git-metrics tests for the agents palette (plan 005 §3.7).
//
// Mirrors `crates/roost-linux/src/git_metrics.rs`'s `mod tests` case for
// case: the same formatter rules, shortstat fixtures, `ls-files -z`
// counting, dedupe/invocation counts, timeout + cancellation, and session
// cache semantics — asserted on the same strings, through the same
// injectable runner seam, with no `git` and no repo on disk. A divergence
// between the two UIs is a red test here rather than a user noticing the
// palettes disagree.

import Foundation
import Testing

@testable import Roost

// MARK: - formatter

@Test func cleanMetricsRenderAsTheUnknownDash() {
    #expect(GitMetrics().text() == GitMetrics.unknown)
    #expect(GitMetrics(files: 0, adds: 0, dels: 0).text() == "—")
}

@Test func dirtyMetricsRenderFilesAddsDelsWithAnAsciiMinus() {
    let text = GitMetrics(files: 4, adds: 86, dels: 12).text()
    #expect(text == "4f +86 -12")
    #expect(text.contains("-"), "the minus must be ASCII, not U+2212")
    #expect(!text.contains("−"))
    // A file touched with no line delta (a binary edit, a mode change)
    // is still not clean.
    #expect(GitMetrics(files: 1, adds: 0, dels: 0).text() == "1f +0 -0")
}

// MARK: - shortstat parser

@Test func parsesTheSingularForm() throws {
    let m = try GitMetrics.parseShortstat(" 1 file changed, 1 insertion(+)\n")
    #expect(m == GitMetrics(files: 1, adds: 1, dels: 0))
}

@Test func parsesThePluralForm() throws {
    let m = try GitMetrics.parseShortstat(" 3 files changed, 12 insertions(+), 4 deletions(-)\n")
    #expect(m == GitMetrics(files: 3, adds: 12, dels: 4))
}

@Test func parsesDeletionOnlyAndInsertionOnlyLines() throws {
    // git omits the zero clause entirely.
    let dels = try GitMetrics.parseShortstat(" 2 files changed, 9 deletions(-)\n")
    #expect(dels == GitMetrics(files: 2, adds: 0, dels: 9))
    let adds = try GitMetrics.parseShortstat(" 2 files changed, 9 insertions(+)\n")
    #expect(adds == GitMetrics(files: 2, adds: 9, dels: 0))
}

@Test func filesOnlyLinesParse() throws {
    // A mode change or a binary-only edit yields no insertion/deletion
    // clause at all.
    let m = try GitMetrics.parseShortstat(" 2 files changed\n")
    #expect(m == GitMetrics(files: 2, adds: 0, dels: 0))
    #expect(m.text() == "2f +0 -0")
}

@Test func emptyOutputIsACleanWorktreeNotAnError() throws {
    #expect(try GitMetrics.parseShortstat("") == GitMetrics())
    #expect(try GitMetrics.parseShortstat("\n \n") == GitMetrics())
    #expect(try GitMetrics.parseShortstat("").text() == GitMetrics.unknown)
}

@Test func renameLinesDoNotBreakTheSummaryTotals() throws {
    let out =
        " rename src/{old => new}/mod.rs (98%)\n 2 files changed, 3 insertions(+), 1 deletion(-)\n"
    #expect(try GitMetrics.parseShortstat(out) == GitMetrics(files: 2, adds: 3, dels: 1))
}

@Test func binaryFileLinesDoNotBreakTheSummaryTotals() throws {
    let out = " assets/logo.png | Bin 0 -> 15340 bytes\n 1 file changed, 0 insertions(+), 0 deletions(-)\n"
    #expect(try GitMetrics.parseShortstat(out) == GitMetrics(files: 1, adds: 0, dels: 0))
}

@Test func stagedChangesParseIdenticallyToUnstagedOnes() throws {
    // `diff HEAD` folds the index and the worktree together, so the same
    // edit staged or not produces one summary line.
    let staged = try GitMetrics.parseShortstat(" 1 file changed, 5 insertions(+), 2 deletions(-)\n")
    let unstaged = try GitMetrics.parseShortstat(
        " 1 file changed, 5 insertions(+), 2 deletions(-)\n")
    #expect(staged == unstaged)
}

@Test func unparseableOutputIsAnError() {
    for out in [
        "fatal: not a git repository\n",
        "garbage\n",
        " 12 insertions(+)\n",  // no files clause
        " x files changed, 1 insertion(+)\n",
    ] {
        #expect(throws: GitProbeError.self, "\(out) must not parse as a shortstat") {
            try GitMetrics.parseShortstat(out)
        }
    }
}

// MARK: - untracked counter

@Test func countsNulSeparatedUntrackedEntries() {
    #expect(GitMetrics.countUntracked(Data()) == 0)
    #expect(GitMetrics.countUntracked(bytes("a.txt\0")) == 1)
    // A new directory is listed as its individual files, which is exactly
    // why we use ls-files instead of status --porcelain.
    #expect(GitMetrics.countUntracked(bytes("new/a\0new/b\0new/c\0")) == 3)
    // Trailing/duplicate separators and newline-bearing paths.
    #expect(GitMetrics.countUntracked(bytes("a\0\0b\0")) == 2)
    #expect(GitMetrics.countUntracked(bytes("weird\nname\0b\0")) == 2)
}

// MARK: - strip-one-EOL

@Test func stripEolTakesExactlyOneTerminatorNotAGeneralTrim() {
    #expect(GitMetrics.stripEOL("/w/roost\n") == "/w/roost")
    #expect(GitMetrics.stripEOL("/w/roost\r\n") == "/w/roost")
    #expect(GitMetrics.stripEOL("/w/roost") == "/w/roost")
    // A root whose last component ends in a space or a tab is legal on
    // every filesystem Roost runs on; trimming would send every later
    // command to the wrong directory.
    #expect(GitMetrics.stripEOL("/w/space dir \n") == "/w/space dir ")
    #expect(GitMetrics.stripEOL("/w/tab\t\n") == "/w/tab\t")
    #expect(GitMetrics.stripEOL("\n\n") == "\n")
}

// MARK: - the injectable runner

/// Records every invocation, answers from canned fixtures, and can stall
/// so the timeout / cancellation paths are exercised without a real repo.
/// Lock-guarded (`@unchecked Sendable`) because the probe drives it from
/// several concurrent child tasks.
private final class FakeGit: CommandRunner, @unchecked Sendable {
    private let lock = NSLock()
    /// cwd → repo root (absent = `rev-parse` fails: not a repo).
    private var roots: [String: String] = [:]
    /// root → (shortstat stdout, `ls-files -z` stdout).
    private var repos: [String: (shortstat: String, untracked: Data)] = [:]
    /// Roots whose `git diff HEAD` exits non-zero (unborn HEAD).
    private var unbornRoots: Set<String> = []
    private var stall: Duration?
    private var calls: [(cwd: String, argv: [String])] = []
    private var inFlight = 0
    private var peak = 0
    /// Runs that were cancelled mid-stall — how the tests observe the
    /// kill-on-cancel seam the real runner escalates to SIGTERM/SIGKILL.
    private var cancelledCount = 0

    @discardableResult
    func repo(_ cwd: String, _ root: String, _ shortstat: String, _ untracked: String) -> FakeGit {
        lock.lock()
        defer { lock.unlock() }
        roots[cwd] = root
        repos[root] = (shortstat, bytes(untracked))
        return self
    }

    @discardableResult
    func unborn(_ root: String) -> FakeGit {
        lock.lock()
        defer { lock.unlock() }
        unbornRoots.insert(root)
        return self
    }

    @discardableResult
    func stalling(_ duration: Duration) -> FakeGit {
        lock.lock()
        defer { lock.unlock() }
        stall = duration
        return self
    }

    func callCount(of argv: [String]) -> Int {
        lock.lock()
        defer { lock.unlock() }
        return calls.filter { $0.argv == argv }.count
    }

    func callCwds(of argv: [String]) -> [String] {
        lock.lock()
        defer { lock.unlock() }
        return calls.filter { $0.argv == argv }.map(\.cwd)
    }

    var totalCalls: Int {
        lock.lock()
        defer { lock.unlock() }
        return calls.count
    }

    var peakInFlight: Int {
        lock.lock()
        defer { lock.unlock() }
        return peak
    }

    var cancelled: Int {
        lock.lock()
        defer { lock.unlock() }
        return cancelledCount
    }

    func run(cwd: String, argv: [String]) async throws -> CommandOutput {
        let stall = enter(cwd: cwd, argv: argv)
        defer { leave() }
        if let stall {
            do {
                try await Task.sleep(for: stall)
            } catch {
                noteCancelled()
                throw error
            }
        }
        let (root, repo, unborn) = fixture(cwd)

        if argv == GitProbe.revParseArgv {
            guard let root else { return CommandOutput(ok: false, stdout: Data()) }
            return CommandOutput(ok: true, stdout: bytes("\(root)\n"))
        }
        guard let repo else { throw GitProbeError("fake: no repo at \(cwd)") }
        if argv == GitProbe.shortstatArgv {
            return CommandOutput(ok: !unborn, stdout: bytes(repo.shortstat))
        }
        return CommandOutput(ok: true, stdout: repo.untracked)
    }

    /// Every lock take lives in a *synchronous* helper: `NSLock` is
    /// unavailable from an async context (it isn't scope-safe across a
    /// suspension), and `run` is async.
    private func fixture(_ cwd: String) -> (String?, (shortstat: String, untracked: Data)?, Bool) {
        lock.lock()
        defer { lock.unlock() }
        return (roots[cwd], repos[cwd], unbornRoots.contains(cwd))
    }

    private func enter(cwd: String, argv: [String]) -> Duration? {
        lock.lock()
        defer { lock.unlock() }
        calls.append((cwd, argv))
        inFlight += 1
        peak = max(peak, inFlight)
        return stall
    }

    private func leave() {
        lock.lock()
        inFlight -= 1
        lock.unlock()
    }

    private func noteCancelled() {
        lock.lock()
        cancelledCount += 1
        lock.unlock()
    }
}

private func bytes(_ text: String) -> Data { Data(text.utf8) }

private func probeWith(_ fake: FakeGit, timeout: TimeInterval = 0.25) -> GitProbe {
    GitProbe(runner: fake, timeout: timeout, slots: GitProbe.maxConcurrent)
}

/// A batch with nothing already known — the fresh-open case.
private func runBatch(_ probe: GitProbe, _ cwds: [String]) async -> [ProbeOutcome] {
    await probe.probeBatch(cwds: cwds, known: [:])
}

/// The measurement an outcome carries, asserting it wasn't reused.
private func measured(_ outcome: ProbeOutcome) throws -> Result<GitMetrics, GitProbeError> {
    guard case .measured(let result) = outcome.value else {
        Issue.record("expected a measurement, got \(outcome.value)")
        throw GitProbeError("not a measurement")
    }
    return result
}

private func probeOne(_ fake: FakeGit, _ cwd: String) async throws -> Result<
    GitMetrics, GitProbeError
> {
    let out = await runBatch(probeWith(fake), [cwd])
    #expect(out.count == 1)
    return try measured(out[0])
}

// MARK: - pipeline

@Test func combinesTrackedChangesWithUntrackedFiles() async throws {
    // 2 modified tracked files + 1 untracked → 3 files, no double count
    // of the modified ones.
    let fake = FakeGit().repo(
        "/w/roost/sub", "/w/roost", " 2 files changed, 12 insertions(+), 4 deletions(-)\n",
        "notes.md\0")
    let metrics = try await probeOne(fake, "/w/roost/sub").get()
    #expect(metrics == GitMetrics(files: 3, adds: 12, dels: 4))
    #expect(metrics.text() == "3f +12 -4")
}

@Test func untrackedOnlyIsNotClean() async throws {
    // The clean check runs *after* untracked files are folded in, so a
    // repo with nothing but new files renders a count, not `—`.
    let fake = FakeGit().repo("/w/new", "/w/new", "", "a\0b\0c\0")
    let metrics = try await probeOne(fake, "/w/new").get()
    #expect(metrics == GitMetrics(files: 3, adds: 0, dels: 0))
    #expect(metrics.text() == "3f +0 -0")
}

@Test func aCleanRepoResolvesToTheDash() async throws {
    let fake = FakeGit().repo("/w/clean", "/w/clean", "", "")
    let metrics = try await probeOne(fake, "/w/clean").get()
    #expect(metrics.isClean)
    #expect(metrics.text() == GitMetrics.unknown)
}

@Test func aNonRepoErrorsWithoutRunningTheExpensiveHalf() async throws {
    let fake = FakeGit()
    let outcomes = await runBatch(probeWith(fake), ["/tmp/plain"])
    #expect(throws: GitProbeError.self) { try measured(outcomes[0]).get() }
    #expect(outcomes[0].root == nil)
    #expect(fake.callCount(of: GitProbe.revParseArgv) == 1)
    #expect(fake.callCount(of: GitProbe.shortstatArgv) == 0)
    #expect(fake.callCount(of: GitProbe.untrackedArgv) == 0)
}

@Test func anEmptyCwdResolvesToTheDashWithoutExecingAnything() async throws {
    let fake = FakeGit()
    let outcomes = await runBatch(probeWith(fake), [""])
    // Errors here, which the caller renders as `—` — the row must never
    // be left pending just because the tab has no cwd.
    #expect(throws: GitProbeError.self) { try measured(outcomes[0]).get() }
    #expect(outcomes[0].root == nil)
    #expect(fake.totalCalls == 0)

    var cache = MetricsCache()
    cache.beginSession(1)
    #expect(cache.claimUnprobed([""]) == [""])
    cache.storeUnresolved(cwd: "")
    #expect(cache.text(forSession: 1, cwd: "") == GitMetrics.unknown)
}

@Test func aRootEndingInWhitespaceSurvivesResolution() async throws {
    // Trimming the rev-parse answer would eat the trailing space and
    // every later command would run in the wrong directory.
    let root = "/w/space dir "
    let fake = FakeGit().repo("/w/space dir /sub", root, " 1 file changed, 1 insertion(+)\n", "")
    let outcomes = await runBatch(probeWith(fake), ["/w/space dir /sub"])
    #expect(outcomes[0].root == root)
    #expect(try measured(outcomes[0]).get().text() == "1f +1 -0")
    #expect(fake.callCwds(of: GitProbe.shortstatArgv) == [root])
}

@Test func anOverflowingFileCountErrorsInsteadOfWrapping() async throws {
    let fake = FakeGit().repo(
        "/w/huge", "/w/huge", " 18446744073709551615 files changed, 1 insertion(+)\n", "one-more\0")
    let result = try await probeOne(fake, "/w/huge")
    guard case .failure(let error) = result else {
        Issue.record("expected an overflow error")
        return
    }
    #expect(error.message.contains("implausible file count"), "\(error)")
}

@Test func anUnbornHeadErrors() async throws {
    let fake = FakeGit().repo("/w/fresh", "/w/fresh", "", "a\0").unborn("/w/fresh")
    let result = try await probeOne(fake, "/w/fresh")
    guard case .failure(let error) = result else {
        Issue.record("expected an unborn-HEAD error")
        return
    }
    #expect(error.message.contains("git diff HEAD failed"), "\(error)")
}

@Test func unparseableShortstatErrors() async throws {
    let fake = FakeGit().repo("/w/odd", "/w/odd", "nonsense\n", "")
    await #expect(throws: GitProbeError.self) { try await probeOne(fake, "/w/odd").get() }
}

// MARK: - dedupe

@Test func identicalCwdsAreProbedOnce() async throws {
    let fake = FakeGit().repo("/w/roost", "/w/roost", " 1 file changed, 1 insertion(+)\n", "")
    let outcomes = await runBatch(probeWith(fake), ["/w/roost", "/w/roost", "/w/roost"])
    // One outcome per unique cwd, one exec per command.
    #expect(outcomes.count == 1)
    #expect(fake.callCount(of: GitProbe.revParseArgv) == 1)
    #expect(fake.callCount(of: GitProbe.shortstatArgv) == 1)
    #expect(fake.callCount(of: GitProbe.untrackedArgv) == 1)
}

@Test func distinctCwdsInOneRepoShareTheExpensiveHalf() async throws {
    let shortstat = " 2 files changed, 5 insertions(+)\n"
    let fake = FakeGit()
        .repo("/w/roost", "/w/roost", shortstat, "x\0")
        .repo("/w/roost/crates", "/w/roost", shortstat, "x\0")
    let outcomes = await runBatch(probeWith(fake), ["/w/roost", "/w/roost/crates"])
    #expect(outcomes.count == 2)
    // Every cwd needs its own rev-parse; the root's diff runs once.
    #expect(fake.callCount(of: GitProbe.revParseArgv) == 2)
    #expect(fake.callCount(of: GitProbe.shortstatArgv) == 1)
    #expect(fake.callCount(of: GitProbe.untrackedArgv) == 1)
    #expect(fake.callCwds(of: GitProbe.shortstatArgv) == ["/w/roost"])
    // Both rows get the same value, keyed by the shared root.
    for outcome in outcomes {
        #expect(outcome.root == "/w/roost")
        #expect(try measured(outcome).get().text() == "3f +5 -0")
    }
}

@Test func separateReposAreProbedSeparately() async throws {
    let fake = FakeGit()
        .repo("/w/a", "/w/a", " 1 file changed, 1 insertion(+)\n", "")
        .repo("/w/b", "/w/b", " 2 files changed, 2 deletions(-)\n", "")
    let outcomes = await runBatch(probeWith(fake), ["/w/a", "/w/b"])
    #expect(fake.callCount(of: GitProbe.shortstatArgv) == 2)
    #expect(try measured(outcomes[0]).get().text() == "1f +1 -0")
    #expect(try measured(outcomes[1]).get().text() == "2f +0 -2")
}

// MARK: - timeout + cancellation

@Test func aStalledCommandTimesOutAndCancelsTheChild() async throws {
    let fake = FakeGit().repo("/w/slow", "/w/slow", "", "").stalling(.seconds(30))
    let outcomes = await runBatch(probeWith(fake, timeout: 0.02), ["/w/slow"])
    guard case .failure(let error) = try measured(outcomes[0]) else {
        Issue.record("expected a timeout error")
        return
    }
    #expect(error.message.contains("timed out"), "\(error)")
    // The runner's task was cancelled mid-flight — the real runner's
    // SIGTERM → SIGKILL escalation fires at exactly this point (the GTK
    // twin is `kill_on_drop`).
    #expect(fake.cancelled == 1)
}

@Test func cancellingTheBatchStopsTheInFlightCommand() async throws {
    let fake = FakeGit().repo("/w/slow", "/w/slow", "", "").stalling(.seconds(30))
    let probe = probeWith(fake)
    // The caller (a dismissed palette) cancels the batch task. Unlike the
    // Rust side — where dropping the future abandons it — Swift's
    // structured concurrency unwinds the children and the batch returns;
    // what matters is identical: the in-flight command is cancelled and
    // the expensive half never starts.
    let batch = Task { await probe.probeBatch(cwds: ["/w/slow"], known: [:]) }
    try await Task.sleep(for: .milliseconds(20))
    batch.cancel()
    let outcomes = await batch.value
    #expect(throws: GitProbeError.self) { try measured(outcomes[0]).get() }
    #expect(fake.cancelled == 1)
    #expect(fake.callCount(of: GitProbe.revParseArgv) == 1)
    #expect(fake.callCount(of: GitProbe.shortstatArgv) == 0)
}

@Test func concurrencyIsCapped() async throws {
    let fake = FakeGit()
    var cwds: [String] = []
    for i in 0..<6 {
        let cwd = "/w/r\(i)"
        fake.repo(cwd, cwd, " 1 file changed, 1 insertion(+)\n", "")
        cwds.append(cwd)
    }
    // Every command sleeps, so overlap is guaranteed if the cap lets it
    // happen.
    fake.stalling(.milliseconds(5))
    let probe = GitProbe(runner: fake, timeout: 5, slots: GitProbe.maxConcurrent)
    let outcomes = await probe.probeBatch(cwds: cwds, known: [:])
    #expect(outcomes.count == 6)
    for outcome in outcomes {
        #expect((try? measured(outcome).get()) != nil)
    }
    #expect(
        fake.peakInFlight <= GitProbe.maxConcurrent,
        "peak \(fake.peakInFlight) exceeded the cap")
}

// MARK: - session cache

@Test func cacheHandsOutEachCwdOnceWhileAProbeIsInFlight() {
    var cache = MetricsCache()
    cache.beginSession(1)
    #expect(cache.claimUnprobed(["/w/a", "/w/a", "/w/b"]) == ["/w/a", "/w/b"])
    // A live-refresh rebuild must not re-spawn the in-flight probes.
    #expect(cache.claimUnprobed(["/w/a", "/w/b"]).isEmpty)
    #expect(cache.text(forSession: 1, cwd: "/w/a") == nil)
}

@Test func resolvedValuesAreReusedAndSharedByRoot() {
    var cache = MetricsCache()
    cache.beginSession(7)
    _ = cache.claimUnprobed(["/w/roost"])
    cache.storeRoot(cwd: "/w/roost", root: "/w/roost", text: "3f +5 -0")
    #expect(cache.text(forSession: 7, cwd: "/w/roost") == "3f +5 -0")
    // Resolved → never re-probed.
    #expect(cache.claimUnprobed(["/w/roost"]).isEmpty)
    // A second tab in the same repo reads the root's one value.
    cache.storeRoot(cwd: "/w/roost/crates", root: "/w/roost", text: "3f +5 -0")
    #expect(cache.text(forSession: 7, cwd: "/w/roost/crates") == "3f +5 -0")
    // A new root in a rebuild is still claimable.
    #expect(cache.claimUnprobed(["/w/other"]) == ["/w/other"])
    // Only real roots are advertised as measured, so a non-repo cwd can
    // never suppress a later repo's measurement.
    cache.storeUnresolved(cwd: "/tmp/plain")
    #expect(cache.knownRoots().count == 1)
    #expect(cache.knownRoots()["/w/roost"] == "3f +5 -0")
}

@Test func aLaterProbeCannotClobberARootsStoredValue() {
    // Two batches can race the same root (a live refresh spawned while
    // the first was still in flight). First write wins, so a redundant
    // probe that failed can't turn a good number into `—`.
    var cache = MetricsCache()
    cache.beginSession(3)
    cache.storeRoot(cwd: "/w/roost", root: "/w/roost", text: "4f +9 -2")
    cache.storeRoot(cwd: "/w/roost/crates", root: "/w/roost", text: GitMetrics.unknown)
    #expect(cache.text(forSession: 3, cwd: "/w/roost") == "4f +9 -2")
    #expect(cache.text(forSession: 3, cwd: "/w/roost/crates") == "4f +9 -2")
}

@Test func aNonRepoCachesItsDashWithoutClaimingARoot() {
    var cache = MetricsCache()
    cache.beginSession(1)
    _ = cache.claimUnprobed(["/tmp/plain"])
    cache.storeUnresolved(cwd: "/tmp/plain")
    #expect(cache.text(forSession: 1, cwd: "/tmp/plain") == GitMetrics.unknown)
    #expect(cache.claimUnprobed(["/tmp/plain"]).isEmpty)
    #expect(cache.knownRoots().isEmpty)
}

@Test func aDeadProbeTaskReleasesItsClaimsAsUnknown() {
    // The cancelled/failed `Task` path (the Swift analog of a tokio
    // JoinError): without this, every claimed cwd stays pending and its
    // row is stuck pending for the session.
    var cache = MetricsCache()
    cache.beginSession(4)
    let claimed = cache.claimUnprobed(["/w/a", "/w/b"])
    for cwd in claimed { cache.storeUnresolved(cwd: cwd) }
    #expect(cache.text(forSession: 4, cwd: "/w/a") == GitMetrics.unknown)
    #expect(cache.text(forSession: 4, cwd: "/w/b") == GitMetrics.unknown)
    #expect(cache.claimUnprobed(claimed).isEmpty)
}

@Test func aStaleSessionNeverReadsThePreviousSessionsNumbers() {
    // The frame for a reopened palette is built *before* the new
    // session's `beginSession` clears the cache, so the read has to be
    // session-scoped or the row flashes the old repo's numbers.
    var cache = MetricsCache()
    cache.beginSession(1)
    cache.storeRoot(cwd: "/w/roost", root: "/w/roost", text: "3f +5 -0")
    #expect(cache.text(forSession: 1, cwd: "/w/roost") == "3f +5 -0")
    #expect(cache.text(forSession: 2, cwd: "/w/roost") == nil)
    // …and once the new session is bound, it's pending as expected.
    cache.beginSession(2)
    #expect(cache.text(forSession: 2, cwd: "/w/roost") == nil)
}

@Test func aNewPaletteSessionDiscardsEverything() {
    var cache = MetricsCache()
    cache.beginSession(1)
    _ = cache.claimUnprobed(["/w/a", "/w/b"])
    cache.storeRoot(cwd: "/w/a", root: "/w/a", text: "1f +1 -0")
    // Same session: values and in-flight marks survive.
    cache.beginSession(1)
    #expect(cache.text(forSession: 1, cwd: "/w/a") == "1f +1 -0")
    #expect(cache.claimUnprobed(["/w/b"]).isEmpty)
    // Reopened palette: nothing carries over, so both re-probe.
    cache.beginSession(2)
    #expect(cache.session == 2)
    #expect(cache.text(forSession: 2, cwd: "/w/a") == nil)
    #expect(cache.knownRoots().isEmpty)
    #expect(cache.claimUnprobed(["/w/a", "/w/b"]) == ["/w/a", "/w/b"])
}

// MARK: - known-root reuse (no redundant measure)

@Test func aCwdInAnAlreadyMeasuredRootReusesItsText() async throws {
    let shortstat = " 2 files changed, 5 insertions(+)\n"
    let fake = FakeGit()
        .repo("/w/roost", "/w/roost", shortstat, "")
        .repo("/w/roost/crates", "/w/roost", shortstat, "")
    // The session already measured /w/roost; a live refresh adds a second
    // tab inside it.
    var cache = MetricsCache()
    cache.beginSession(1)
    cache.storeRoot(cwd: "/w/roost", root: "/w/roost", text: "2f +5 -0")
    let outcomes = await probeWith(fake).probeBatch(
        cwds: ["/w/roost/crates"], known: cache.knownRoots())

    #expect(outcomes[0].root == "/w/roost")
    guard case .reused(let text) = outcomes[0].value else {
        Issue.record("a known root must not be re-measured")
        return
    }
    #expect(text == "2f +5 -0")
    // Only the cheap half ran.
    #expect(fake.callCount(of: GitProbe.revParseArgv) == 1)
    #expect(fake.callCount(of: GitProbe.shortstatArgv) == 0)
    #expect(fake.callCount(of: GitProbe.untrackedArgv) == 0)
}

@Test func aFailingRedundantProbeCannotClobberAKnownRoot() async throws {
    // Same shape, but the repo would now fail to measure (unborn HEAD /
    // timeout). Because the root is known, nothing runs — and even if it
    // had, first-write-wins keeps the stored value.
    let fake = FakeGit().repo("/w/roost/sub", "/w/roost", "", "").unborn("/w/roost")
    var cache = MetricsCache()
    cache.beginSession(1)
    cache.storeRoot(cwd: "/w/roost", root: "/w/roost", text: "4f +9 -2")
    let outcomes = await probeWith(fake).probeBatch(
        cwds: ["/w/roost/sub"], known: cache.knownRoots())

    guard case .reused(let text) = outcomes[0].value else {
        Issue.record("a known root must not be re-measured")
        return
    }
    #expect(text == "4f +9 -2")
    cache.storeRoot(cwd: "/w/roost/sub", root: "/w/roost", text: text)
    #expect(cache.text(forSession: 1, cwd: "/w/roost/sub") == "4f +9 -2")
    #expect(cache.text(forSession: 1, cwd: "/w/roost") == "4f +9 -2")
    #expect(fake.callCount(of: GitProbe.shortstatArgv) == 0)
}

// MARK: - the production runner's spawn/kill gate

/// A path in a fresh temp dir, cleaned up by the caller's `defer`.
private func scratchPath() -> String {
    FileManager.default.temporaryDirectory
        .appendingPathComponent("roost-gitmetrics-\(UUID().uuidString)").path
}

@Test func cancelBeforeLaunchNeverSpawnsTheChild() throws {
    // The race Codex found: a cancel landing between "is it cancelled?"
    // and `run()` used to schedule no kill and then spawn anyway, leaving
    // a live `git` with the worker parked in `waitUntilExit` forever
    // (holding one of the four permits). The check and the spawn are one
    // guarded step now, so this ordering can only refuse to spawn.
    let marker = scratchPath()
    defer { try? FileManager.default.removeItem(atPath: marker) }
    let proc = Process()
    proc.executableURL = URL(fileURLWithPath: "/bin/sh")
    proc.arguments = ["-c", "touch \(marker)"]

    let child = ChildProcess()
    child.cancel()
    #expect(child.isCancelled)
    #expect(throws: GitProbeError.self) { try child.launch(proc) }
    // Never spawned: no pid, no side effect.
    #expect(proc.processIdentifier == 0)
    #expect(!FileManager.default.fileExists(atPath: marker))
}

@Test func cancelAfterLaunchKillsTheChild() throws {
    let proc = Process()
    proc.executableURL = URL(fileURLWithPath: "/bin/sh")
    proc.arguments = ["-c", "sleep 30"]
    proc.standardOutput = FileHandle.nullDevice
    proc.standardError = FileHandle.nullDevice

    let child = ChildProcess()
    try child.launch(proc)
    #expect(proc.isRunning)
    child.cancel()
    // Bounded wait rather than `waitUntilExit`, so a regression fails the
    // test instead of hanging the run.
    let deadline = Date().addingTimeInterval(5)
    while proc.isRunning, Date() < deadline { usleep(10_000) }
    #expect(!proc.isRunning)
    #expect(proc.terminationReason == .uncaughtSignal)
}

@Test func anAlreadyCancelledTaskNeverExecsThroughTheRunner() async throws {
    // The same ordering through the public seam, made deterministic: the
    // task is cancelled *before* `run` is entered, so the cancellation
    // handler fires during setup and the queued body must refuse to
    // spawn. The permit is released by the throw either way.
    let marker = scratchPath()
    defer { try? FileManager.default.removeItem(atPath: marker) }
    let runner = GitRunner()
    let task = Task { () async throws -> CommandOutput in
        while !Task.isCancelled { await Task.yield() }
        return try await runner.run(cwd: "/tmp", argv: ["sh", "-c", "touch \(marker)"])
    }
    task.cancel()
    await #expect(throws: GitProbeError.self) { try await task.value }
    try await Task.sleep(for: .milliseconds(200))
    #expect(!FileManager.default.fileExists(atPath: marker))
}

@Test func theProductionRunnerExecsArgvWithACStableLocale() async throws {
    // Also covers the PATH-resolving `/usr/bin/env` exec and the
    // exit-status → `ok` mapping (no `git` needed).
    let runner = GitRunner()
    let locale = try await runner.run(cwd: "/tmp", argv: ["sh", "-c", "printf %s \"$LC_ALL\""])
    #expect(locale.ok)
    #expect(String(decoding: locale.stdout, as: UTF8.self) == "C")

    let failed = try await runner.run(cwd: "/tmp", argv: ["sh", "-c", "printf hi; exit 3"])
    #expect(!failed.ok)
    #expect(String(decoding: failed.stdout, as: UTF8.self) == "hi")
}

// MARK: - the cwd map the palette feeds the probe

@Test func agentTabCwdsCoversExactlyTheRowsPopulation() {
    let owned = Workspace.Tab(
        id: 2, projectId: 1, title: "agent", cwd: "/w/roost",
        agent: AgentTabState(
            shell: .atPrompt, lifecycle: .working,
            ownership: Ownership(
                source: "claude", sessionID: "s", lastEventAt: 0, detail: "", metadata: [:])),
        hasNotification: false, userTitled: false, position: 0, createdAt: 0, lastActive: 0)
    var manual = owned
    manual.id = 3
    manual.cwd = "/w/manual"
    manual.agent.ownership?.source = Workspace.manualSource
    var unowned = owned
    unowned.id = 4
    unowned.cwd = "/w/unowned"
    unowned.agent = AgentTabState(shell: .atPrompt, lifecycle: .inactive, ownership: nil)
    var noCwd = owned
    noCwd.id = 5
    noCwd.cwd = ""

    var orphan = owned
    orphan.id = 6
    orphan.projectId = 99
    orphan.cwd = "/w/orphan"

    let projects = [Workspace.Project(id: 1, name: "roost", cwd: "/tmp", position: 0, createdAt: 0)]
    let cwds = AgentPalette.agentTabCwds(
        projects: projects, tabs: [owned, manual, unowned, noCwd, orphan])
    // manual/legacy and unowned tabs get no row, so they get no probe;
    // a cwd-less agent tab is still claimed (it resolves to `—`); a tab
    // whose project vanished mid-snapshot gets no row, so no probe
    // either (the filter must match agentItems exactly).
    #expect(cwds == [2: "/w/roost", 5: ""])
}
