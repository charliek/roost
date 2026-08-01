//! Toolkit-neutral git metrics for agent projections (plan 005 §3.7).
//!
//! Each agent row shows what its repo looks like right now — `"Nf +A -D"`
//! (files touched, lines added, lines deleted) — probed asynchronously so
//! the palette never blocks on `git`. Everything here is seam-shaped: a
//! [`CommandRunner`] is injected, so the pipeline (dedupe → resolve root →
//! diff + untracked → parse) is unit-tested without a repo on disk, and
//! both Rust UI adapters consume the same subprocess and cache semantics.
//!
//! Threading: the whole batch runs on the app's tokio runtime (never the
//! UI thread); each adapter owns the explicit hop back to its event loop.
//!
//! Every failure path — no cwd, not a repo, unborn HEAD, missing `git`,
//! timeout, unparseable output — surfaces as `Err`, and the *caller* maps
//! that to [`UNKNOWN`] (errors are returned, not swallowed; the palette
//! logs at that boundary). A clean repo renders [`UNKNOWN`] too: "clean"
//! and "not a repo" deliberately look the same.
//!
//! To be mirrored by `mac/Sources/Roost/GitMetrics.swift` (plan 005 C5).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

/// Rendered when metrics can't be produced *or* the repo is clean.
pub const UNKNOWN: &str = "—";

/// Per-command budget. A pathological repo (fsmonitor cold, huge tree)
/// resolves to [`UNKNOWN`] rather than holding a row pending forever.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
/// Ceiling on git processes in flight, so opening a palette over a dozen
/// agent tabs can't fork-bomb the box.
const MAX_CONCURRENT: usize = 4;
/// Captured stdout ceiling. Past it we can't trust a count anyway (a
/// truncated `ls-files -z` under-counts), so the probe errors out.
const MAX_OUTPUT_BYTES: u64 = 64 * 1024;

/// `--no-optional-locks` keeps a probe from perturbing the repo the user
/// is working in (strix uses the same flag for the same reason).
const REV_PARSE_ARGV: &[&str] = &["git", "--no-optional-locks", "rev-parse", "--show-toplevel"];
/// `HEAD --` diffs staged **and** unstaged work against the last commit;
/// `--no-ext-diff --no-textconv` keep user diff filters out of the path.
const SHORTSTAT_ARGV: &[&str] = &[
    "git",
    "--no-optional-locks",
    "diff",
    "--no-ext-diff",
    "--no-textconv",
    "--shortstat",
    "HEAD",
    "--",
];
/// Real untracked *files*: `status --porcelain` collapses an untracked
/// directory to one entry and would also re-list the modified files the
/// shortstat already counted.
const UNTRACKED_ARGV: &[&str] = &[
    "git",
    "--no-optional-locks",
    "ls-files",
    "--others",
    "--exclude-standard",
    "-z",
];

/// What one repo's worktree looks like versus `HEAD`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GitMetrics {
    /// Tracked files changed **plus** untracked files.
    pub files: u64,
    pub adds: u64,
    pub dels: u64,
}

impl GitMetrics {
    fn is_clean(&self) -> bool {
        self.files == 0 && self.adds == 0 && self.dels == 0
    }

    /// The rendered column. The minus is ASCII `-` (not `−`) on both UIs
    /// — greppability beats typography here.
    pub fn text(&self) -> String {
        if self.is_clean() {
            return UNKNOWN.to_string();
        }
        format!("{}f +{} -{}", self.files, self.adds, self.dels)
    }
}

/// What a runner reports back: exit success plus captured stdout. stderr
/// is discarded — every failure here is expressible as "this command did
/// not succeed", and a piped-but-unread stderr risks blocking the child.
pub struct CommandOutput {
    pub ok: bool,
    pub stdout: Vec<u8>,
}

type RunFuture<'a> = Pin<Box<dyn Future<Output = Result<CommandOutput, String>> + Send + 'a>>;

/// The injectable exec seam. Boxed-future rather than `async fn` in a
/// trait so the trait stays object-safe *and* the future stays `Send`
/// (the batch is spawned onto a multi-threaded runtime).
pub trait CommandRunner: Send + Sync {
    fn run(&self, cwd: PathBuf, argv: &'static [&'static str]) -> RunFuture<'_>;
}

/// The production runner: argv exec (no shell), `LC_ALL=C`, no stdin,
/// `kill_on_drop` so a timed-out child is reaped when the probe's future
/// is dropped.
pub struct GitRunner;

impl CommandRunner for GitRunner {
    fn run(&self, cwd: PathBuf, argv: &'static [&'static str]) -> RunFuture<'_> {
        Box::pin(async move {
            use std::process::Stdio;
            use tokio::io::AsyncReadExt;

            let mut cmd = tokio::process::Command::new(argv[0]);
            cmd.args(&argv[1..])
                .current_dir(&cwd)
                // Byte-stable output regardless of the user's locale —
                // the shortstat parser matches English words.
                .env("LC_ALL", "C")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);

            let mut child = cmd.spawn().map_err(|e| format!("spawn {}: {e}", argv[0]))?;
            let mut stdout = Vec::new();
            if let Some(pipe) = child.stdout.take() {
                // One byte past the cap is enough to *detect* the
                // overflow; we never buffer more than that.
                let mut limited = pipe.take(MAX_OUTPUT_BYTES + 1);
                limited
                    .read_to_end(&mut stdout)
                    .await
                    .map_err(|e| format!("read {}: {e}", argv[0]))?;
            }
            if stdout.len() as u64 > MAX_OUTPUT_BYTES {
                // Dropping `child` kills it (`kill_on_drop`), so a child
                // still writing into a full pipe doesn't linger.
                return Err(format!(
                    "{} output exceeded {MAX_OUTPUT_BYTES} bytes",
                    argv[0]
                ));
            }
            let status = child
                .wait()
                .await
                .map_err(|e| format!("wait {}: {e}", argv[0]))?;
            Ok(CommandOutput {
                ok: status.success(),
                stdout,
            })
        })
    }
}

/// One cwd's probe result, plus the repo root it resolved to (the key the
/// caller caches under, so two tabs in one repo share a value). `root` is
/// `None` when the cwd never resolved to a repo at all.
pub struct ProbeOutcome {
    pub cwd: String,
    pub root: Option<String>,
    pub value: ProbeValue,
}

/// Where a row's metrics came from.
pub enum ProbeValue {
    /// A fresh measurement — or the reason one couldn't be taken, which
    /// the caller renders as [`UNKNOWN`].
    Measured(Result<GitMetrics, String>),
    /// The session had already measured this repo root; no git ran and
    /// the existing text carries over verbatim.
    Reused(String),
}

/// The probe pipeline: a runner, a per-command timeout, and the
/// concurrency ceiling.
pub struct GitProbe {
    runner: Arc<dyn CommandRunner>,
    timeout: Duration,
    slots: Semaphore,
}

impl Default for GitProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl GitProbe {
    pub fn new() -> Self {
        Self::with_config(Arc::new(GitRunner), PROBE_TIMEOUT, MAX_CONCURRENT)
    }

    fn with_config(runner: Arc<dyn CommandRunner>, timeout: Duration, slots: usize) -> Self {
        Self {
            runner,
            timeout,
            slots: Semaphore::new(slots),
        }
    }

    /// One command, permit-gated and time-boxed. On timeout the runner's
    /// future is dropped, which kills the child through `kill_on_drop`.
    async fn run(&self, cwd: &str, argv: &'static [&'static str]) -> Result<CommandOutput, String> {
        let _permit = self
            .slots
            .acquire()
            .await
            .map_err(|_| "probe pool closed".to_string())?;
        match tokio::time::timeout(self.timeout, self.runner.run(PathBuf::from(cwd), argv)).await {
            Err(_) => Err(format!("{} timed out after {:?}", argv[0], self.timeout)),
            Ok(result) => result,
        }
    }

    /// The repo containing `cwd`. A deleted cwd fails at spawn; a
    /// non-repo fails the command — both land here as `Err`.
    async fn resolve_root(&self, cwd: &str) -> Result<String, String> {
        if cwd.is_empty() {
            return Err("tab has no cwd".to_string());
        }
        let out = self.run(cwd, REV_PARSE_ARGV).await?;
        if !out.ok {
            return Err(format!("not a git repository: {cwd}"));
        }
        let raw = String::from_utf8_lossy(&out.stdout);
        let root = strip_eol(&raw).to_string();
        if root.is_empty() {
            return Err(format!("empty repository root for {cwd}"));
        }
        Ok(root)
    }

    /// The expensive half, run from the repo root exactly once per root.
    async fn metrics_for_root(&self, root: &str) -> Result<GitMetrics, String> {
        // Independent commands; the semaphore still bounds the total in
        // flight, and neither holds a permit while waiting on the other.
        let (shortstat, untracked) = futures::future::join(
            self.run(root, SHORTSTAT_ARGV),
            self.run(root, UNTRACKED_ARGV),
        )
        .await;

        let shortstat = shortstat?;
        if !shortstat.ok {
            // Overwhelmingly: an unborn HEAD (fresh `git init`).
            return Err(format!("git diff HEAD failed in {root}"));
        }
        let mut metrics = parse_shortstat(&String::from_utf8_lossy(&shortstat.stdout))?;

        let untracked = untracked?;
        if !untracked.ok {
            return Err(format!("git ls-files failed in {root}"));
        }
        // Untracked files are *additional* files touched — the shortstat
        // only ever counts tracked ones, so there's nothing to subtract.
        // Both halves are attacker-adjacent numbers (a crafted shortstat
        // line parses as any u64), so the sum is checked: an overflow
        // panics in debug and wraps to a nonsense count in release.
        metrics.files = metrics
            .files
            .checked_add(count_untracked(&untracked.stdout))
            .ok_or_else(|| format!("implausible file count in {root}"))?;
        Ok(metrics)
    }
}

/// Strip the single line terminator git appends to a one-line answer.
/// `trim()` would corrupt a real root whose last path component ends in
/// a space or a tab — legal on every filesystem Roost runs on.
fn strip_eol(text: &str) -> &str {
    let text = text.strip_suffix('\n').unwrap_or(text);
    text.strip_suffix('\r').unwrap_or(text)
}

/// Probe every cwd in one pass, deduping three ways: identical cwds
/// share a `rev-parse`, cwds resolving to the same root share one diff +
/// `ls-files` pair, and a root already in `known` isn't measured at all.
/// Returns one outcome per **unique** cwd.
///
/// `known` is the caller's session cache flattened to plain data (root →
/// rendered text) so the batch stays pure — it never reads or writes the
/// cache, and a rebuild that adds a second tab in an already-measured
/// repo costs one `rev-parse`, not a whole re-measure that could fail and
/// overwrite a good value.
pub async fn probe_batch(
    probe: Arc<GitProbe>,
    cwds: Vec<String>,
    known: HashMap<String, String>,
) -> Vec<ProbeOutcome> {
    let mut seen: HashSet<&str> = HashSet::new();
    let unique: Vec<&str> = cwds
        .iter()
        .map(String::as_str)
        .filter(|cwd| seen.insert(cwd))
        .collect();

    let roots: Vec<Result<String, String>> =
        futures::future::join_all(unique.iter().map(|cwd| probe.resolve_root(cwd))).await;

    let mut seen_roots: HashSet<&str> = HashSet::new();
    let wanted: Vec<String> = roots
        .iter()
        .flatten()
        .filter(|root| !known.contains_key(root.as_str()))
        .filter(|root| seen_roots.insert(root.as_str()))
        .cloned()
        .collect();
    let measured: Vec<Result<GitMetrics, String>> =
        futures::future::join_all(wanted.iter().map(|root| probe.metrics_for_root(root))).await;
    let by_root: HashMap<String, Result<GitMetrics, String>> =
        wanted.into_iter().zip(measured).collect();

    unique
        .into_iter()
        .zip(roots)
        .map(|(cwd, root)| match root {
            Ok(root) => {
                let value = match known.get(&root) {
                    Some(text) => ProbeValue::Reused(text.clone()),
                    None => ProbeValue::Measured(
                        by_root
                            .get(&root)
                            .cloned()
                            .unwrap_or_else(|| Err(format!("no probe result for {root}"))),
                    ),
                };
                ProbeOutcome {
                    cwd: cwd.to_string(),
                    root: Some(root),
                    value,
                }
            }
            Err(err) => ProbeOutcome {
                cwd: cwd.to_string(),
                root: None,
                value: ProbeValue::Measured(Err(err)),
            },
        })
        .collect()
}

/// Parse `git diff --shortstat` output.
///
/// Empty output means "nothing changed" — a clean worktree, not a
/// failure. Otherwise the summary is the last line that parses; earlier
/// lines (rename / binary stat lines a repo's diff config can emit) are
/// skipped. Partial lines are normal: git omits the insertions or
/// deletions clause entirely when it is zero.
fn parse_shortstat(stdout: &str) -> Result<GitMetrics, String> {
    if stdout.trim().is_empty() {
        return Ok(GitMetrics::default());
    }
    let parsed = stdout.lines().rev().find_map(parse_shortstat_line);
    parsed.ok_or_else(|| format!("unparseable shortstat: {:?}", stdout.trim()))
}

fn parse_shortstat_line(line: &str) -> Option<GitMetrics> {
    let mut metrics = GitMetrics::default();
    let mut saw_files = false;
    for segment in line.split(',') {
        let segment = segment.trim();
        let (count, rest) = segment.split_once(' ')?;
        let count: u64 = count.parse().ok()?;
        if rest.starts_with("file") {
            metrics.files = count;
            saw_files = true;
        } else if rest.starts_with("insertion") {
            metrics.adds = count;
        } else if rest.starts_with("deletion") {
            metrics.dels = count;
        } else {
            return None;
        }
    }
    saw_files.then_some(metrics)
}

/// Count NUL-separated `ls-files -z` entries. `-z` is what makes this
/// safe for paths with newlines or quotes in them.
fn count_untracked(stdout: &[u8]) -> u64 {
    stdout.split(|b| *b == 0).filter(|e| !e.is_empty()).count() as u64
}

/// Resolved metrics for one open palette session (plan 005 §3.7).
///
/// Keyed by repo root so two tabs in the same repo render one probe's
/// result, with a cwd → root index because a palette row only knows its
/// tab's cwd. `pending` keeps a live-refresh rebuild from re-spawning a
/// probe that is already in flight. The whole thing is discarded when the
/// palette session changes, so a dismiss → reopen re-probes.
#[derive(Default)]
pub struct MetricsCache {
    session: u64,
    root_of: HashMap<String, String>,
    /// Rendered text per repo **root** — the only thing handed to a
    /// later batch as `known`, so nothing but a real root can suppress a
    /// measurement.
    text_of: HashMap<String, String>,
    /// Cwds that resolved to no repo at all. They render `—` without
    /// occupying a root key.
    unresolved: HashSet<String>,
    pending: HashSet<String>,
}

impl MetricsCache {
    /// Bind the cache to a palette session, clearing it if this is a
    /// different session than the one it holds.
    pub fn begin_session(&mut self, session: u64) {
        if self.session == session {
            return;
        }
        self.session = session;
        self.root_of.clear();
        self.text_of.clear();
        self.unresolved.clear();
        self.pending.clear();
    }

    pub fn session(&self) -> u64 {
        self.session
    }

    /// The rendered metrics for a tab's cwd **as of `session`** — `None`
    /// while a probe is pending, and `None` for any other session.
    ///
    /// The session argument is the point: a palette frame is built before
    /// [`begin_session`](Self::begin_session) runs for it, so a plain
    /// lookup would let a reopened palette flash the numbers the previous
    /// session resolved.
    pub fn text_for_session(&self, session: u64, cwd: &str) -> Option<&str> {
        if self.session != session {
            return None;
        }
        self.text_for(cwd)
    }

    fn text_for(&self, cwd: &str) -> Option<&str> {
        if self.unresolved.contains(cwd) {
            return Some(UNKNOWN);
        }
        let root = self.root_of.get(cwd)?;
        self.text_of.get(root).map(String::as_str)
    }

    /// The roots this session has already measured, with their rendered
    /// text. Handed to the next batch so a newly listed tab inside a
    /// known repo reuses the value instead of re-running the expensive
    /// pair (whose failure would otherwise overwrite a good number).
    pub fn known_roots(&self) -> HashMap<String, String> {
        self.text_of.clone()
    }

    /// The cwds that still need a probe — neither resolved nor already in
    /// flight — marking them in flight. Duplicates collapse. An empty cwd
    /// is claimed like any other: the probe errors on it without spawning
    /// git, which is how its row reaches `—` instead of staying pending.
    pub fn claim_unprobed(&mut self, cwds: impl IntoIterator<Item = String>) -> Vec<String> {
        let mut claimed = Vec::new();
        for cwd in cwds {
            if self.text_for(&cwd).is_some() || self.pending.contains(&cwd) {
                continue;
            }
            self.pending.insert(cwd.clone());
            claimed.push(cwd);
        }
        claimed
    }

    /// Record a probe that landed on a repo root. **First write wins**
    /// for the root's text within a session: two batches can race the
    /// same root (a live refresh spawned while the first was in flight),
    /// and a later failure must not clobber a good number.
    pub fn store_root(&mut self, cwd: &str, root: &str, text: String) {
        self.pending.remove(cwd);
        self.root_of.insert(cwd.to_string(), root.to_string());
        self.text_of.entry(root.to_string()).or_insert(text);
    }

    /// Record a cwd that resolved to no repo — no cwd at all, a deleted
    /// directory, a non-repo, or a probe whose task died. It renders `—`
    /// for the rest of the session rather than being re-probed on every
    /// refresh (and, critically, rather than staying pending forever).
    pub fn store_unresolved(&mut self, cwd: &str) {
        self.pending.remove(cwd);
        self.unresolved.insert(cwd.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    // ----- formatter ------------------------------------------------

    #[test]
    fn clean_metrics_render_as_the_unknown_dash() {
        assert_eq!(GitMetrics::default().text(), UNKNOWN);
        assert_eq!(
            GitMetrics {
                files: 0,
                adds: 0,
                dels: 0
            }
            .text(),
            "—"
        );
    }

    #[test]
    fn dirty_metrics_render_files_adds_dels_with_an_ascii_minus() {
        let text = GitMetrics {
            files: 4,
            adds: 86,
            dels: 12,
        }
        .text();
        assert_eq!(text, "4f +86 -12");
        assert!(text.contains('-'), "the minus must be ASCII, not U+2212");
        assert!(!text.contains('−'));
        // A file touched with no line delta (a binary edit, a mode
        // change) is still not clean.
        assert_eq!(
            GitMetrics {
                files: 1,
                adds: 0,
                dels: 0
            }
            .text(),
            "1f +0 -0"
        );
    }

    // ----- shortstat parser -----------------------------------------

    #[test]
    fn parses_the_singular_form() {
        let m = parse_shortstat(" 1 file changed, 1 insertion(+)\n").unwrap();
        assert_eq!(
            m,
            GitMetrics {
                files: 1,
                adds: 1,
                dels: 0
            }
        );
    }

    #[test]
    fn parses_the_plural_form() {
        let m = parse_shortstat(" 3 files changed, 12 insertions(+), 4 deletions(-)\n").unwrap();
        assert_eq!(
            m,
            GitMetrics {
                files: 3,
                adds: 12,
                dels: 4
            }
        );
    }

    #[test]
    fn parses_deletion_only_and_insertion_only_lines() {
        // git omits the zero clause entirely.
        let dels = parse_shortstat(" 2 files changed, 9 deletions(-)\n").unwrap();
        assert_eq!(
            dels,
            GitMetrics {
                files: 2,
                adds: 0,
                dels: 9
            }
        );
        let adds = parse_shortstat(" 2 files changed, 9 insertions(+)\n").unwrap();
        assert_eq!(
            adds,
            GitMetrics {
                files: 2,
                adds: 9,
                dels: 0
            }
        );
    }

    #[test]
    fn empty_output_is_a_clean_worktree_not_an_error() {
        assert_eq!(parse_shortstat("").unwrap(), GitMetrics::default());
        assert_eq!(parse_shortstat("\n \n").unwrap(), GitMetrics::default());
        assert_eq!(parse_shortstat("").unwrap().text(), UNKNOWN);
    }

    #[test]
    fn rename_lines_do_not_break_the_summary_totals() {
        let out = " rename src/{old => new}/mod.rs (98%)\n 2 files changed, 3 insertions(+), 1 deletion(-)\n";
        assert_eq!(
            parse_shortstat(out).unwrap(),
            GitMetrics {
                files: 2,
                adds: 3,
                dels: 1
            }
        );
    }

    #[test]
    fn binary_file_lines_do_not_break_the_summary_totals() {
        let out = " assets/logo.png | Bin 0 -> 15340 bytes\n 1 file changed, 0 insertions(+), 0 deletions(-)\n";
        assert_eq!(
            parse_shortstat(out).unwrap(),
            GitMetrics {
                files: 1,
                adds: 0,
                dels: 0
            }
        );
    }

    #[test]
    fn staged_changes_parse_identically_to_unstaged_ones() {
        // `diff HEAD` folds the index and the worktree together, so the
        // same edit staged or not produces one summary line.
        let staged = parse_shortstat(" 1 file changed, 5 insertions(+), 2 deletions(-)\n").unwrap();
        let unstaged =
            parse_shortstat(" 1 file changed, 5 insertions(+), 2 deletions(-)\n").unwrap();
        assert_eq!(staged, unstaged);
    }

    #[test]
    fn unparseable_output_is_an_error() {
        for out in [
            "fatal: not a git repository\n",
            "garbage\n",
            " 12 insertions(+)\n", // no files clause
            " x files changed, 1 insertion(+)\n",
        ] {
            assert!(
                parse_shortstat(out).is_err(),
                "{out:?} must not parse as a shortstat"
            );
        }
    }

    // ----- untracked counter ----------------------------------------

    #[test]
    fn counts_nul_separated_untracked_entries() {
        assert_eq!(count_untracked(b""), 0);
        assert_eq!(count_untracked(b"a.txt\0"), 1);
        // A new directory is listed as its individual files, which is
        // exactly why we use ls-files instead of status --porcelain.
        assert_eq!(count_untracked(b"new/a\0new/b\0new/c\0"), 3);
        // Trailing/duplicate separators and newline-bearing paths.
        assert_eq!(count_untracked(b"a\0\0b\0"), 2);
        assert_eq!(count_untracked(b"weird\nname\0b\0"), 2);
    }

    // ----- the injectable runner ------------------------------------

    /// Records every invocation, answers from canned fixtures, and can
    /// stall so the timeout / cancellation paths are exercised without a
    /// real repo.
    #[derive(Default)]
    struct FakeGit {
        /// cwd → repo root (absent = `rev-parse` fails: not a repo).
        roots: HashMap<String, String>,
        /// root → (shortstat stdout, `ls-files -z` stdout).
        repos: HashMap<String, (String, Vec<u8>)>,
        /// Roots whose `git diff HEAD` exits non-zero (unborn HEAD).
        unborn: HashSet<String>,
        calls: Mutex<Vec<(String, &'static [&'static str])>>,
        stall: Option<Duration>,
        in_flight: AtomicUsize,
        peak_in_flight: AtomicUsize,
        dropped: AtomicUsize,
    }

    impl FakeGit {
        fn repo(mut self, cwd: &str, root: &str, shortstat: &str, untracked: &[u8]) -> Self {
            self.roots.insert(cwd.to_string(), root.to_string());
            self.repos.insert(
                root.to_string(),
                (shortstat.to_string(), untracked.to_vec()),
            );
            self
        }

        fn calls_of(&self, argv: &'static [&'static str]) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, a)| *a == argv)
                .count()
        }

        fn call_cwds(&self, argv: &'static [&'static str]) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, a)| *a == argv)
                .map(|(cwd, _)| cwd.clone())
                .collect()
        }
    }

    /// Marks the runner future as cancelled if it is dropped before it
    /// finishes — how the tests observe kill-on-drop at the seam.
    struct DropWitness<'a>(&'a AtomicUsize, bool);

    impl Drop for DropWitness<'_> {
        fn drop(&mut self) {
            if !self.1 {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    struct FlightGuard<'a>(&'a FakeGit);

    impl Drop for FlightGuard<'_> {
        fn drop(&mut self) {
            self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl CommandRunner for FakeGit {
        fn run(&self, cwd: PathBuf, argv: &'static [&'static str]) -> RunFuture<'_> {
            let cwd = cwd.to_string_lossy().into_owned();
            self.calls.lock().unwrap().push((cwd.clone(), argv));
            Box::pin(async move {
                let live = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak_in_flight.fetch_max(live, Ordering::SeqCst);
                let _flight = FlightGuard(self);
                let mut witness = DropWitness(&self.dropped, false);
                if let Some(stall) = self.stall {
                    tokio::time::sleep(stall).await;
                }
                witness.1 = true;

                if argv == REV_PARSE_ARGV {
                    return Ok(match self.roots.get(&cwd) {
                        Some(root) => CommandOutput {
                            ok: true,
                            stdout: format!("{root}\n").into_bytes(),
                        },
                        None => CommandOutput {
                            ok: false,
                            stdout: Vec::new(),
                        },
                    });
                }
                let repo = self
                    .repos
                    .get(&cwd)
                    .ok_or_else(|| format!("fake: no repo at {cwd}"))?;
                if argv == SHORTSTAT_ARGV {
                    return Ok(CommandOutput {
                        ok: !self.unborn.contains(&cwd),
                        stdout: repo.0.clone().into_bytes(),
                    });
                }
                Ok(CommandOutput {
                    ok: true,
                    stdout: repo.1.clone(),
                })
            })
        }
    }

    fn probe_with(fake: Arc<FakeGit>) -> Arc<GitProbe> {
        Arc::new(GitProbe::with_config(
            fake,
            Duration::from_millis(250),
            MAX_CONCURRENT,
        ))
    }

    /// A batch with nothing already known — the fresh-open case.
    async fn run_batch(probe: Arc<GitProbe>, cwds: &[&str]) -> Vec<ProbeOutcome> {
        probe_batch(
            probe,
            cwds.iter().map(|c| c.to_string()).collect(),
            HashMap::new(),
        )
        .await
    }

    /// The measurement an outcome carries, asserting it wasn't reused.
    fn measured(outcome: &ProbeOutcome) -> &Result<GitMetrics, String> {
        match &outcome.value {
            ProbeValue::Measured(result) => result,
            ProbeValue::Reused(text) => panic!("expected a measurement, got reused {text:?}"),
        }
    }

    async fn probe_one(fake: Arc<FakeGit>, cwd: &str) -> Result<GitMetrics, String> {
        let out = run_batch(probe_with(fake), &[cwd]).await;
        assert_eq!(out.len(), 1);
        measured(&out[0]).clone()
    }

    // ----- pipeline -------------------------------------------------

    #[tokio::test]
    async fn combines_tracked_changes_with_untracked_files() {
        // 2 modified tracked files + 1 untracked → 3 files, no double
        // count of the modified ones.
        let fake = Arc::new(FakeGit::default().repo(
            "/w/roost/sub",
            "/w/roost",
            " 2 files changed, 12 insertions(+), 4 deletions(-)\n",
            b"notes.md\0",
        ));
        let metrics = probe_one(fake, "/w/roost/sub").await.unwrap();
        assert_eq!(
            metrics,
            GitMetrics {
                files: 3,
                adds: 12,
                dels: 4
            }
        );
        assert_eq!(metrics.text(), "3f +12 -4");
    }

    #[tokio::test]
    async fn a_clean_repo_resolves_to_the_dash() {
        let fake = Arc::new(FakeGit::default().repo("/w/clean", "/w/clean", "", b""));
        let metrics = probe_one(fake, "/w/clean").await.unwrap();
        assert!(metrics.is_clean());
        assert_eq!(metrics.text(), UNKNOWN);
    }

    #[tokio::test]
    async fn a_non_repo_errors_without_running_the_expensive_half() {
        let fake = Arc::new(FakeGit::default());
        let probe = probe_with(fake.clone());
        let outcomes = run_batch(probe, &["/tmp/plain"]).await;
        assert!(measured(&outcomes[0]).is_err());
        assert_eq!(outcomes[0].root, None);
        assert_eq!(fake.calls_of(REV_PARSE_ARGV), 1);
        assert_eq!(fake.calls_of(SHORTSTAT_ARGV), 0);
        assert_eq!(fake.calls_of(UNTRACKED_ARGV), 0);
    }

    #[tokio::test]
    async fn an_empty_cwd_resolves_to_the_dash_without_execing_anything() {
        let fake = Arc::new(FakeGit::default());
        let probe = probe_with(fake.clone());
        let outcomes = run_batch(probe, &[""]).await;
        // Errors here, which the caller renders as `—` — the row must
        // never be left pending just because the tab has no cwd.
        assert!(measured(&outcomes[0]).is_err());
        assert_eq!(outcomes[0].root, None);
        assert_eq!(fake.calls.lock().unwrap().len(), 0);

        let mut cache = MetricsCache::default();
        cache.begin_session(1);
        assert_eq!(
            cache.claim_unprobed(vec![String::new()]),
            vec![String::new()]
        );
        cache.store_unresolved("");
        assert_eq!(cache.text_for_session(1, ""), Some(UNKNOWN));
    }

    #[tokio::test]
    async fn a_root_ending_in_whitespace_survives_resolution() {
        // `trim()` on the rev-parse answer would eat the trailing space
        // and every later command would run in the wrong directory.
        let root = "/w/space dir ";
        let fake = Arc::new(FakeGit::default().repo(
            "/w/space dir /sub",
            root,
            " 1 file changed, 1 insertion(+)\n",
            b"",
        ));
        let outcomes = run_batch(probe_with(fake.clone()), &["/w/space dir /sub"]).await;
        assert_eq!(outcomes[0].root.as_deref(), Some(root));
        assert_eq!(measured(&outcomes[0]).as_ref().unwrap().text(), "1f +1 -0");
        assert_eq!(fake.call_cwds(SHORTSTAT_ARGV), vec![root.to_string()]);
    }

    #[tokio::test]
    async fn an_overflowing_file_count_errors_instead_of_wrapping() {
        let fake = Arc::new(FakeGit::default().repo(
            "/w/huge",
            "/w/huge",
            " 18446744073709551615 files changed, 1 insertion(+)\n",
            b"one-more\0",
        ));
        let err = probe_one(fake, "/w/huge").await.unwrap_err();
        assert!(err.contains("implausible file count"), "{err}");
    }

    #[tokio::test]
    async fn an_unborn_head_errors() {
        let mut fake = FakeGit::default().repo("/w/fresh", "/w/fresh", "", b"a\0");
        fake.unborn.insert("/w/fresh".to_string());
        let err = probe_one(Arc::new(fake), "/w/fresh").await.unwrap_err();
        assert!(err.contains("git diff HEAD failed"), "{err}");
    }

    #[tokio::test]
    async fn unparseable_shortstat_errors() {
        let fake = Arc::new(FakeGit::default().repo("/w/odd", "/w/odd", "nonsense\n", b""));
        assert!(probe_one(fake, "/w/odd").await.is_err());
    }

    // ----- dedupe ---------------------------------------------------

    #[tokio::test]
    async fn identical_cwds_are_probed_once() {
        let fake = Arc::new(FakeGit::default().repo(
            "/w/roost",
            "/w/roost",
            " 1 file changed, 1 insertion(+)\n",
            b"",
        ));
        let probe = probe_with(fake.clone());
        let outcomes = run_batch(probe, &["/w/roost", "/w/roost", "/w/roost"]).await;
        // One outcome per unique cwd, one exec per command.
        assert_eq!(outcomes.len(), 1);
        assert_eq!(fake.calls_of(REV_PARSE_ARGV), 1);
        assert_eq!(fake.calls_of(SHORTSTAT_ARGV), 1);
        assert_eq!(fake.calls_of(UNTRACKED_ARGV), 1);
    }

    #[tokio::test]
    async fn distinct_cwds_in_one_repo_share_the_expensive_half() {
        let fake = Arc::new(
            FakeGit::default()
                .repo(
                    "/w/roost",
                    "/w/roost",
                    " 2 files changed, 5 insertions(+)\n",
                    b"x\0",
                )
                .repo(
                    "/w/roost/crates",
                    "/w/roost",
                    " 2 files changed, 5 insertions(+)\n",
                    b"x\0",
                ),
        );
        let probe = probe_with(fake.clone());
        let outcomes = run_batch(probe, &["/w/roost", "/w/roost/crates"]).await;
        assert_eq!(outcomes.len(), 2);
        // Every cwd needs its own rev-parse; the root's diff runs once.
        assert_eq!(fake.calls_of(REV_PARSE_ARGV), 2);
        assert_eq!(fake.calls_of(SHORTSTAT_ARGV), 1);
        assert_eq!(fake.calls_of(UNTRACKED_ARGV), 1);
        assert_eq!(fake.call_cwds(SHORTSTAT_ARGV), vec!["/w/roost"]);
        // Both rows get the same value, keyed by the shared root.
        for outcome in &outcomes {
            assert_eq!(outcome.root.as_deref(), Some("/w/roost"));
            assert_eq!(measured(outcome).as_ref().unwrap().text(), "3f +5 -0");
        }
    }

    #[tokio::test]
    async fn separate_repos_are_probed_separately() {
        let fake = Arc::new(
            FakeGit::default()
                .repo("/w/a", "/w/a", " 1 file changed, 1 insertion(+)\n", b"")
                .repo("/w/b", "/w/b", " 2 files changed, 2 deletions(-)\n", b""),
        );
        let probe = probe_with(fake.clone());
        let outcomes = run_batch(probe, &["/w/a", "/w/b"]).await;
        assert_eq!(fake.calls_of(SHORTSTAT_ARGV), 2);
        assert_eq!(measured(&outcomes[0]).as_ref().unwrap().text(), "1f +1 -0");
        assert_eq!(measured(&outcomes[1]).as_ref().unwrap().text(), "2f +0 -2");
    }

    // ----- timeout + cancellation -----------------------------------

    #[tokio::test]
    async fn a_stalled_command_times_out_and_drops_the_child_future() {
        let mut fake = FakeGit::default().repo("/w/slow", "/w/slow", "", b"");
        fake.stall = Some(Duration::from_secs(30));
        let fake = Arc::new(fake);
        let probe = Arc::new(GitProbe::with_config(
            fake.clone(),
            Duration::from_millis(20),
            MAX_CONCURRENT,
        ));
        let outcomes = run_batch(probe, &["/w/slow"]).await;
        let err = measured(&outcomes[0]).as_ref().unwrap_err().clone();
        assert!(err.contains("timed out"), "{err}");
        // The runner's future was dropped mid-flight — the real runner's
        // `kill_on_drop` reaps the child at exactly this point.
        assert_eq!(fake.dropped.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancelling_the_batch_stops_the_in_flight_command() {
        let mut fake = FakeGit::default().repo("/w/slow", "/w/slow", "", b"");
        fake.stall = Some(Duration::from_secs(30));
        let fake = Arc::new(fake);
        let probe = probe_with(fake.clone());
        // The caller (a dismissed palette) drops the batch future.
        let batch = run_batch(probe, &["/w/slow"]);
        let cancelled = tokio::time::timeout(Duration::from_millis(20), batch).await;
        assert!(cancelled.is_err(), "batch should not have finished");
        assert_eq!(fake.dropped.load(Ordering::SeqCst), 1);
        assert_eq!(fake.calls_of(REV_PARSE_ARGV), 1);
        assert_eq!(fake.calls_of(SHORTSTAT_ARGV), 0);
    }

    #[tokio::test]
    async fn concurrency_is_capped() {
        let mut fake = FakeGit::default();
        for i in 0..6 {
            let cwd = format!("/w/r{i}");
            fake = fake.repo(&cwd, &cwd, " 1 file changed, 1 insertion(+)\n", b"");
        }
        // Every command sleeps, so overlap is guaranteed if the cap lets
        // it happen.
        fake.stall = Some(Duration::from_millis(5));
        let fake = Arc::new(fake);
        let probe = Arc::new(GitProbe::with_config(
            fake.clone(),
            Duration::from_secs(5),
            MAX_CONCURRENT,
        ));
        let cwds: Vec<String> = (0..6).map(|i| format!("/w/r{i}")).collect();
        let outcomes = probe_batch(probe, cwds, HashMap::new()).await;
        assert_eq!(outcomes.len(), 6);
        assert!(outcomes.iter().all(|o| measured(o).is_ok()));
        assert!(
            fake.peak_in_flight.load(Ordering::SeqCst) <= MAX_CONCURRENT,
            "peak {} exceeded the cap",
            fake.peak_in_flight.load(Ordering::SeqCst)
        );
    }

    // ----- session cache --------------------------------------------

    fn owned(cwds: &[&str]) -> Vec<String> {
        cwds.iter().map(|c| c.to_string()).collect()
    }

    #[test]
    fn cache_hands_out_each_cwd_once_while_a_probe_is_in_flight() {
        let mut cache = MetricsCache::default();
        cache.begin_session(1);
        let claimed = cache.claim_unprobed(owned(&["/w/a", "/w/a", "/w/b"]));
        assert_eq!(claimed, owned(&["/w/a", "/w/b"]));
        // A live-refresh rebuild must not re-spawn the in-flight probes.
        assert!(cache.claim_unprobed(owned(&["/w/a", "/w/b"])).is_empty());
        assert_eq!(cache.text_for_session(1, "/w/a"), None);
    }

    #[test]
    fn resolved_values_are_reused_and_shared_by_root() {
        let mut cache = MetricsCache::default();
        cache.begin_session(7);
        cache.claim_unprobed(owned(&["/w/roost"]));
        cache.store_root("/w/roost", "/w/roost", "3f +5 -0".to_string());
        assert_eq!(cache.text_for_session(7, "/w/roost"), Some("3f +5 -0"));
        // Resolved → never re-probed.
        assert!(cache.claim_unprobed(owned(&["/w/roost"])).is_empty());
        // A second tab in the same repo reads the root's one value.
        cache.store_root("/w/roost/crates", "/w/roost", "3f +5 -0".to_string());
        assert_eq!(
            cache.text_for_session(7, "/w/roost/crates"),
            Some("3f +5 -0")
        );
        // A new root in a rebuild is still claimable.
        assert_eq!(
            cache.claim_unprobed(owned(&["/w/other"])),
            owned(&["/w/other"])
        );
        // Only real roots are advertised as measured, so a non-repo cwd
        // can never suppress a later repo's measurement.
        cache.store_unresolved("/tmp/plain");
        assert_eq!(cache.known_roots().len(), 1);
        assert_eq!(
            cache.known_roots().get("/w/roost").map(String::as_str),
            Some("3f +5 -0")
        );
    }

    #[test]
    fn a_later_probe_cannot_clobber_a_roots_stored_value() {
        // Two batches can race the same root (a live refresh spawned
        // while the first was still in flight). First write wins, so a
        // redundant probe that failed can't turn a good number into `—`.
        let mut cache = MetricsCache::default();
        cache.begin_session(3);
        cache.store_root("/w/roost", "/w/roost", "4f +9 -2".to_string());
        cache.store_root("/w/roost/crates", "/w/roost", UNKNOWN.to_string());
        assert_eq!(cache.text_for_session(3, "/w/roost"), Some("4f +9 -2"));
        assert_eq!(
            cache.text_for_session(3, "/w/roost/crates"),
            Some("4f +9 -2")
        );
    }

    #[test]
    fn a_non_repo_caches_its_dash_without_claiming_a_root() {
        let mut cache = MetricsCache::default();
        cache.begin_session(1);
        cache.claim_unprobed(owned(&["/tmp/plain"]));
        cache.store_unresolved("/tmp/plain");
        assert_eq!(cache.text_for_session(1, "/tmp/plain"), Some(UNKNOWN));
        assert!(cache.claim_unprobed(owned(&["/tmp/plain"])).is_empty());
        assert!(cache.known_roots().is_empty());
    }

    #[test]
    fn a_dead_probe_task_releases_its_claims_as_unknown() {
        // The `rt.spawn` JoinError path: without this, every claimed cwd
        // stays `pending` and its row is stuck pending for the session.
        let mut cache = MetricsCache::default();
        cache.begin_session(4);
        let claimed = cache.claim_unprobed(owned(&["/w/a", "/w/b"]));
        for cwd in &claimed {
            cache.store_unresolved(cwd);
        }
        assert_eq!(cache.text_for_session(4, "/w/a"), Some(UNKNOWN));
        assert_eq!(cache.text_for_session(4, "/w/b"), Some(UNKNOWN));
        assert!(cache.claim_unprobed(claimed).is_empty());
    }

    #[test]
    fn a_stale_session_never_reads_the_previous_sessions_numbers() {
        // The frame for a reopened palette is built *before* the new
        // session's `begin_session` clears the cache, so the read has to
        // be session-scoped or the row flashes the old repo's numbers.
        let mut cache = MetricsCache::default();
        cache.begin_session(1);
        cache.store_root("/w/roost", "/w/roost", "3f +5 -0".to_string());
        assert_eq!(cache.text_for_session(1, "/w/roost"), Some("3f +5 -0"));
        assert_eq!(cache.text_for_session(2, "/w/roost"), None);
        // …and once the new session is bound, it's pending as expected.
        cache.begin_session(2);
        assert_eq!(cache.text_for_session(2, "/w/roost"), None);
    }

    #[test]
    fn a_new_palette_session_discards_everything() {
        let mut cache = MetricsCache::default();
        cache.begin_session(1);
        cache.claim_unprobed(owned(&["/w/a", "/w/b"]));
        cache.store_root("/w/a", "/w/a", "1f +1 -0".to_string());
        // Same session: values and in-flight marks survive.
        cache.begin_session(1);
        assert_eq!(cache.text_for_session(1, "/w/a"), Some("1f +1 -0"));
        assert!(cache.claim_unprobed(owned(&["/w/b"])).is_empty());
        // Reopened palette: nothing carries over, so both re-probe.
        cache.begin_session(2);
        assert_eq!(cache.session(), 2);
        assert_eq!(cache.text_for_session(2, "/w/a"), None);
        assert!(cache.known_roots().is_empty());
        assert_eq!(
            cache.claim_unprobed(owned(&["/w/a", "/w/b"])),
            owned(&["/w/a", "/w/b"])
        );
    }

    // ----- known-root reuse (no redundant measure) --------------------

    #[tokio::test]
    async fn a_cwd_in_an_already_measured_root_reuses_its_text() {
        let fake = Arc::new(
            FakeGit::default()
                .repo(
                    "/w/roost",
                    "/w/roost",
                    " 2 files changed, 5 insertions(+)\n",
                    b"",
                )
                .repo(
                    "/w/roost/crates",
                    "/w/roost",
                    " 2 files changed, 5 insertions(+)\n",
                    b"",
                ),
        );
        // The session already measured /w/roost; a live refresh adds a
        // second tab inside it.
        let mut cache = MetricsCache::default();
        cache.begin_session(1);
        cache.store_root("/w/roost", "/w/roost", "2f +5 -0".to_string());
        let outcomes = probe_batch(
            probe_with(fake.clone()),
            owned(&["/w/roost/crates"]),
            cache.known_roots(),
        )
        .await;

        assert_eq!(outcomes[0].root.as_deref(), Some("/w/roost"));
        match &outcomes[0].value {
            ProbeValue::Reused(text) => assert_eq!(text, "2f +5 -0"),
            ProbeValue::Measured(_) => panic!("a known root must not be re-measured"),
        }
        // Only the cheap half ran.
        assert_eq!(fake.calls_of(REV_PARSE_ARGV), 1);
        assert_eq!(fake.calls_of(SHORTSTAT_ARGV), 0);
        assert_eq!(fake.calls_of(UNTRACKED_ARGV), 0);
    }

    #[tokio::test]
    async fn a_failing_redundant_probe_cannot_clobber_a_known_root() {
        // Same shape, but the repo would now fail to measure (unborn
        // HEAD / timeout). Because the root is known, nothing runs — and
        // even if it had, first-write-wins keeps the stored value.
        let mut fake = FakeGit::default().repo("/w/roost/sub", "/w/roost", "", b"");
        fake.unborn.insert("/w/roost".to_string());
        let fake = Arc::new(fake);

        let mut cache = MetricsCache::default();
        cache.begin_session(1);
        cache.store_root("/w/roost", "/w/roost", "4f +9 -2".to_string());
        let outcomes = probe_batch(
            probe_with(fake.clone()),
            owned(&["/w/roost/sub"]),
            cache.known_roots(),
        )
        .await;

        let ProbeValue::Reused(text) = &outcomes[0].value else {
            panic!("a known root must not be re-measured");
        };
        assert_eq!(text, "4f +9 -2");
        cache.store_root("/w/roost/sub", "/w/roost", text.clone());
        assert_eq!(cache.text_for_session(1, "/w/roost/sub"), Some("4f +9 -2"));
        assert_eq!(cache.text_for_session(1, "/w/roost"), Some("4f +9 -2"));
        assert_eq!(fake.calls_of(SHORTSTAT_ARGV), 0);
    }
}
