//! The bootstrap runtime (plan 039 §3.2–§3.4), driven end to end
//! against a fake `ssh` that really runs the remote commands.
//!
//! [`ssh_transport_test`](../ssh_transport_test.rs) drives the same
//! fixture in its `ok` mode, where the remote command is ignored and one
//! fixed program stands in for the far side. That is enough for a byte
//! pump. It is not enough here, because in this module the remote
//! command *is* the thing under test: a generated `/bin/sh -s` script, a
//! `tee` the binary is streamed into, an `exec` of whatever the probe
//! found. So the fixture's `run-remote` mode runs the actual argv, and
//! these tests assert on what it did to a filesystem.
//!
//! **Which makes hermeticity the whole game** (plan 039 §3.8, a panel
//! blocker). The scripts run on the machine running the test, so the
//! fixture is handed a session environment that makes this machine
//! invisible to them:
//!
//! * `HOME` is a tempdir, so `$HOME/.local/bin/roost-session` — the
//!   install destination and rung 1 of the ladder — is inside it.
//! * `PATH` is one directory this harness built: a fake `uname` that
//!   answers whatever the test wants, symlinks to the seven coreutils
//!   the generated scripts run, and **no `roost-session`**, so the
//!   ladder's `command -v` rung can never find a real one.
//! * `ROOST_BOOTSTRAP_FS_ROOT` is a jail directory, which the candidate
//!   ladder prefixes onto its absolute rungs — so a `/usr/bin/
//!   roost-session` probe reads `<jail>/usr/bin/roost-session`.
//!
//! Without those three the suite would answer differently on a Mac (real
//! `uname -s` is `Darwin`, which the probe classifies as unsupported)
//! than on a Linux box with the deb installed (a real
//! `/usr/bin/roost-session` on the ladder, and on `PATH`). With them it
//! is the same test everywhere, and the acceptance criterion is that it
//! passes on both.
//!
//! The far-side `roost-session` is a shell script. It can be, because
//! nothing here needs a terminal: the probe execs `<path> identify` and
//! reads one JSON line, `start` prints one readiness line, and
//! `client-bridge` is a newline-delimited JSON pipe. A script that
//! answers those three is a `roost-session` as far as this code can
//! tell, and it can be *scripted* — wrong build, too old to identify,
//! no session, a session that takes a few polls to finalize.
//!
//! Two rules from the transport suite carry over: every test gets its
//! own scratch parent and its own fixture configuration, both injected
//! rather than set in process-global environment, and nothing sleeps
//! waiting for a condition.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use roost_ipc::bootstrap::{
    asset_name, checksum_name, shell_quote, BootstrapError, BootstrapJob, BootstrapOptions,
    IdentityGate, InstallPhase, InstallSource, ProbeOutcome, RemoteArch, ResolvedSource,
    SourceOrigin,
};
use roost_ipc::messages::{SessionBinaryIdentity, SESSION_PROTOCOL_VERSION};
use roost_ipc::session_launch::Verdict;
use roost_ipc::ssh::{classify, ResolvedTransport, SshConfigPaths, SshTarget, SshTunnelOptions};
use sha2::{Digest, Sha256};

// ============================================================================
// What the client is, and what the far side answers
// ============================================================================

const CLIENT_VERSION: &str = "0.0.19";
const CLIENT_BUILD: &str = "ghostty-0123456789abcdef+snapshot.v1";
const OTHER_BUILD: &str = "ghostty-fedcba9876543210+snapshot.v1";

/// The triple this client demands of anything it installs.
fn expected() -> SessionBinaryIdentity {
    SessionBinaryIdentity {
        app_version: CLIENT_VERSION.to_string(),
        session_protocol: SESSION_PROTOCOL_VERSION,
        libghostty_build: CLIENT_BUILD.to_string(),
    }
}

/// A triple that is a `roost-session`, and is not this one.
fn stale() -> SessionBinaryIdentity {
    SessionBinaryIdentity {
        app_version: "0.0.17".to_string(),
        session_protocol: SESSION_PROTOCOL_VERSION,
        libghostty_build: OTHER_BUILD.to_string(),
    }
}

/// The `session.identify` result a serving fake answers with.
fn running_identity(app_version: &str, build: &str) -> String {
    serde_json::json!({
        "app_version": app_version,
        "session_protocol": SESSION_PROTOCOL_VERSION,
        "payload_kinds": ["ghostty-snapshot"],
        "libghostty_build": build,
        "session_id": "sess-fixture",
        "started_at": "2026-08-31T00:00:00Z",
    })
    .to_string()
}

// ============================================================================
// The fake `roost-session`
// ============================================================================

/// What a planted fake answers to `client-bridge`.
#[derive(Debug, Clone)]
enum Bridge {
    /// The far side's bridge found no session — the real one's exact
    /// stderr line and exit code, which is what
    /// `classify_ssh_failure` reads as `NoSession`.
    NoSession,
    /// A session is serving: `session.stop` is answered `ok`, and
    /// `session.identify` is answered with a running identity carrying
    /// `app_version` and `build`. After `alive_polls` identify calls it
    /// answers as [`Bridge::NoSession`] instead — which is how a delayed
    /// finalization is spelled. `None` never finalizes.
    Serving {
        app_version: String,
        build: String,
        alive_polls: Option<usize>,
    },
    /// The session answers, refusing with a specific wire code. The
    /// code is what `stop_over_the_wire` routes on, so a fake that
    /// hard-codes one is a fake that can only test one branch.
    Refuses { code: String, message: String },
    /// The bridge ran and never answered — a poll that eats its whole
    /// budget rather than failing.
    Hangs(u32),
    /// The bridge dies with a diagnostic nothing classifies.
    Fails(String),
}

impl Bridge {
    /// A session serving this client's own build.
    fn serving(alive_polls: Option<usize>) -> Self {
        Self::Serving {
            app_version: CLIENT_VERSION.to_string(),
            build: CLIENT_BUILD.to_string(),
            alive_polls,
        }
    }

    fn refuses(code: &str, message: &str) -> Self {
        Self::Refuses {
            code: code.to_string(),
            message: message.to_string(),
        }
    }
}

/// What a planted fake answers to `start`.
#[derive(Debug, Clone)]
enum Start {
    Ready(u32),
    /// The race `await_gone` exists to close, from the far side: the
    /// first start loses to a session that has not finished dying, the
    /// next one wins.
    AlreadyRunningThenReady {
        first: u32,
        then: u32,
    },
    /// The race that never resolves: every attempt loses. What makes
    /// accepting it safe is the post-start identify, and this is how
    /// that claim gets tested.
    AlwaysAlreadyRunning(u32),
    Errors(String),
}

/// A fake `roost-session`, as a shell script.
#[derive(Debug, Clone)]
struct Stub {
    /// Namespaces this stub's counter files, and makes two stubs in one
    /// test differ byte for byte.
    name: String,
    /// What `identify` prints, or `None` for a build older than the
    /// subcommand: a diagnostic on stderr and exit 2, which the probe
    /// reads as "present, unidentifiable".
    identify: Option<SessionBinaryIdentity>,
    /// Answer `identify` only while running from the staged temporary.
    ///
    /// The staged verify execs `<dest>.tmp.<pid>` and the post-commit
    /// re-verify execs `<dest>`, so a stub that branches on `$0` is a
    /// binary that passes the gate and then stops answering — which is
    /// the one shape that exercises the rollback.
    only_when_staged: bool,
    bridge: Bridge,
    start: Start,
}

impl Stub {
    fn new(name: &str, identify: Option<SessionBinaryIdentity>) -> Self {
        Self {
            name: name.to_string(),
            identify,
            only_when_staged: false,
            bridge: Bridge::NoSession,
            start: Start::Ready(4242),
        }
    }

    /// A binary that is exactly this client's build.
    fn matching(name: &str) -> Self {
        Self::new(name, Some(expected()))
    }

    /// A binary that is some other build.
    fn stale(name: &str) -> Self {
        Self::new(name, Some(stale()))
    }

    /// A binary too old to know `identify` at all.
    fn unidentifiable(name: &str) -> Self {
        Self::new(name, None)
    }

    /// A binary that identifies from the staging path and nowhere else.
    fn only_when_staged(mut self) -> Self {
        self.only_when_staged = true;
        self
    }

    fn bridge(mut self, bridge: Bridge) -> Self {
        self.bridge = bridge;
        self
    }

    fn start(mut self, start: Start) -> Self {
        self.start = start;
        self
    }

    /// The script, with its counter files rooted at `state`.
    fn script(&self, state: &Path) -> String {
        let mut out = String::from("#!/bin/sh\n");
        out.push_str(&format!("# fake roost-session: {}\n", self.name));
        out.push_str(&format!(
            "STATE={}\nSELF={}\n",
            shell_quote(&state.display().to_string()),
            shell_quote(&self.name)
        ));
        // A counter that survives across invocations, because every
        // remote step is a separate process. Prints the value *before*
        // the bump, so the first call reads zero.
        out.push_str(
            "bump() {\n  n=0\n  if [ -f \"$STATE/$SELF.$1\" ]; then read n < \"$STATE/$SELF.$1\"; \
             fi\n  printf '%s\\n' \"$((n + 1))\" > \"$STATE/$SELF.$1\"\n  printf '%s\\n' \"$n\"\n\
             }\n",
        );
        out.push_str("case \"${1:-}\" in\n");
        out.push_str(&format!("identify)\n{}  ;;\n", self.identify_body()));
        out.push_str(&format!("start)\n{}  ;;\n", self.start_body()));
        out.push_str(&format!("client-bridge)\n{}  ;;\n", self.bridge_body()));
        out.push_str(
            "*)\n  printf '%s: unknown subcommand\\n' \"$SELF\" >&2\n  exit 2\n  ;;\nesac\n",
        );
        out
    }

    fn identify_body(&self) -> String {
        let silent = "  printf '%s: unknown subcommand: identify\\n' \"$SELF\" >&2\n  exit 2\n";
        let Some(identity) = &self.identify else {
            return silent.to_string();
        };
        let answer = format!(
            "  printf '%s\\n' {}\n  exit 0\n",
            shell_quote(&serde_json::to_string(identity).expect("serialize an identity"))
        );
        if !self.only_when_staged {
            return answer;
        }
        // `$0` is the path it was exec'd through: the staged temporary
        // for the pre-commit gate, the destination for the re-verify.
        format!("  case \"$0\" in\n  *.tmp.*)\n{answer}    ;;\n  *)\n{silent}    ;;\n  esac\n")
    }

    fn start_body(&self) -> String {
        match &self.start {
            Start::Ready(pid) => format!("  printf 'ready pid={pid}\\n'\n  exit 0\n"),
            Start::AlreadyRunningThenReady { first, then } => format!(
                "  n=$(bump start)\n  if [ \"$n\" -eq 0 ]; then\n    printf \
                 'already-running pid={first}\\n'\n  else\n    printf 'ready pid={then}\\n'\n  \
                 fi\n  exit 0\n"
            ),
            Start::AlwaysAlreadyRunning(pid) => {
                format!("  printf 'already-running pid={pid}\\n'\n  exit 0\n")
            }
            // **Exit 1, like the real binary.** `roost-session start`
            // writes `error: <reason>` to stdout and exits non-zero; a
            // stub that exited 0 let the client pass this test while
            // reading the verdict line only on the success path, so the
            // reason a real failure carries was thrown away and
            // `Verdict::Error` was unreachable.
            Start::Errors(reason) => format!(
                "  printf 'error: %s\\n' {}\n  exit 1\n",
                shell_quote(reason)
            ),
        }
    }

    fn bridge_body(&self) -> String {
        let no_session = "  printf 'client-bridge: no session\\n' >&2\n  exit 1\n";
        match &self.bridge {
            Bridge::NoSession => no_session.to_string(),
            Bridge::Fails(detail) => {
                format!("  printf '%s\\n' {} >&2\n  exit 1\n", shell_quote(detail))
            }
            // Bounded, so the orphan the kill leaves behind cannot
            // outlive the run by much.
            Bridge::Hangs(seconds) => format!("  exec sleep {seconds}\n"),
            Bridge::Refuses { code, message } => format!(
                "  read -r _request || _request=\n  printf '%s\\n' {}\n  exit 0\n",
                shell_quote(&format!(
                    r#"{{"id":"1","ok":false,"error":{{"code":"{code}","message":"{message}"}}}}"#
                ))
            ),
            Bridge::Serving {
                app_version,
                build,
                alive_polls,
            } => {
                let stop =
                    r#"{"id":"1","ok":true,"result":{"reaped":[],"killed":[],"abandoned":[]}}"#;
                let identify = format!(
                    r#"{{"id":"1","ok":true,"result":{}}}"#,
                    running_identity(app_version, build)
                );
                // Only the identify branch consumes a poll: the stop is
                // what starts the clock, not a tick of it.
                let finalize = match alive_polls {
                    Some(polls) => format!(
                        "    n=$(bump poll)\n    if [ \"$n\" -ge {polls} ]; then\n    \
                         {no_session}    fi\n"
                    ),
                    None => String::new(),
                };
                format!(
                    "  read -r request || request=\n  case \"$request\" in\n  *session.stop*)\n    \
                     printf '%s\\n' {stop}\n    exit 0\n    ;;\n  *)\n{finalize}    printf '%s\\n' \
                     {identify}\n    exit 0\n    ;;\n  esac\n",
                    stop = shell_quote(stop),
                    identify = shell_quote(&identify),
                )
            }
        }
    }
}

// ============================================================================
// Harness
// ============================================================================

/// The coreutils the generated scripts actually run. Symlinked into a
/// `PATH` of exactly one directory, so nothing else on this machine —
/// least of all a real `roost-session` — is reachable by name.
/// `cat` is the exception: no generated script runs it, but the tool
/// overrides this harness writes do — a stand-in `tee` has to drain
/// stdin somehow.
const TOOLS: &[&str] = &[
    "sh", "printf", "mkdir", "tee", "chmod", "mv", "rm", "sleep", "cat",
];

/// Where those symlinks are resolved from. Not `$PATH`: this harness is
/// about not inheriting the developer's environment.
const TOOL_DIRS: &[&str] = &["/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"];

/// A fake `ssh`, a fake far side, and the jail both live in.
struct Harness {
    _root: tempfile::TempDir,
    /// Scratch parent handed to [`SshTunnelOptions`] — the job's own
    /// directory is created under here.
    parent: PathBuf,
    log: PathBuf,
    ssh_bin: PathBuf,
    /// `ROOST_BOOTSTRAP_FS_ROOT`: what the ladder's absolute rungs are
    /// resolved against.
    jail: PathBuf,
    /// The far side's `$HOME`.
    home: PathBuf,
    /// The far side's entire `PATH`.
    stub_bin: PathBuf,
    /// Counter files for the planted fakes.
    state: PathBuf,
}

impl Harness {
    fn new() -> Self {
        Self::with_uname("Linux", "x86_64")
    }

    /// `os` and `machine` are what the far side's `uname -s` and
    /// `uname -m` answer.
    fn with_uname(os: &str, machine: &str) -> Self {
        // `/tmp` rather than `$TMPDIR` for `ssh_transport_test`'s
        // reason: a macOS per-user `$TMPDIR` is deep enough to overrun
        // `sun_path`, and these tests assert on a known directory.
        // A short prefix on purpose: the job's control socket lives at
        // `<root>/scratch/roost-ssh-bootstrap-<pid>-<nanos>-<n>/ctl`,
        // and `pick_socket_dir` refuses outright rather than falling
        // back when that overruns `sun_path`.
        let root = tempfile::Builder::new()
            .prefix("roost-bs-t")
            .tempdir_in("/tmp")
            .expect("scratch root");
        let parent = root.path().join("scratch");
        let jail = root.path().join("jail");
        let home = jail.join("home/fixture");
        let stub_bin = root.path().join("stub-bin");
        let state = root.path().join("state");
        for dir in [&parent, &jail, &home, &stub_bin, &state] {
            std::fs::create_dir_all(dir).expect("create a harness directory");
        }
        let log = root.path().join("invocations.log");
        std::fs::write(&log, b"").expect("invocation log");

        let harness = Self {
            _root: root,
            parent,
            log,
            ssh_bin: PathBuf::new(),
            jail,
            home,
            stub_bin,
            state,
        };
        harness.write_tools();
        harness.write_uname(&format!(
            "#!/bin/sh\ncase \"${{1:-}}\" in\n-s) printf '%s\\n' {os} ;;\n-m) printf '%s\\n' \
             {machine} ;;\n*) printf '%s\\n' {os} ;;\nesac\n",
            os = shell_quote(os),
            machine = shell_quote(machine),
        ));
        let session_env = harness.write_session_env();
        let ssh_bin = harness.write_ssh_wrapper(&session_env);
        Self { ssh_bin, ..harness }
    }

    /// Symlink the coreutils in. Anything missing is a broken machine,
    /// not a skipped test: the scripts under test run these.
    fn write_tools(&self) {
        for tool in TOOLS {
            std::os::unix::fs::symlink(real_tool(tool), self.stub_bin.join(tool))
                .expect("symlink a tool into the stub PATH");
        }
    }

    fn write_uname(&self, script: &str) {
        write_executable(&self.stub_bin.join("uname"), script.as_bytes());
    }

    /// Replace one tool with a script of the test's own — how a `tee`
    /// that dies part-way is arranged.
    fn override_tool(&self, tool: &str, script: &str) {
        let path = self.stub_bin.join(tool);
        let _ = std::fs::remove_file(&path);
        write_executable(&path, script.as_bytes());
    }

    /// A `chmod` that blocks until [`Harness::release_chmod`], then does
    /// its job.
    ///
    /// The block sits in the staged-verify phase, which is after the
    /// stream: the temporary is fully written and nothing will write to
    /// it again, so a caller that drops the install future here is
    /// cancelling with a staged file that is stable. Blocking `tee`
    /// instead would leave an orphaned *writer*, which can re-create the
    /// path after the next attempt swept it — a race in the fixture, not
    /// in the code under test.
    ///
    /// A released wait rather than a fixed `sleep`, because the orphan
    /// the cancellation leaves behind is what the next exec ends up
    /// waiting on: a sleeping one turns its whole duration into test
    /// wall time, and a duration short enough not to would be one the
    /// cancellation could race.
    fn block_chmod(&self) {
        self.override_tool(
            "chmod",
            &format!(
                "#!/bin/sh\nwaited=0\nwhile [ ! -f {state}/chmod.release ] && [ \"$waited\" -lt \
                 600 ]; do\n  sleep 0.1\n  waited=$((waited + 1))\ndone\nexec {chmod} \"$@\"\n",
                state = shell_quote(&self.state.display().to_string()),
                chmod = shell_quote(&real_tool("chmod").display().to_string()),
            ),
        );
    }

    /// Let every blocked (and future) `chmod` through.
    fn release_chmod(&self) {
        std::fs::write(self.state.join("chmod.release"), b"").expect("release chmod");
    }

    /// Make the far side's `uname` hang, so a probe exec runs out its
    /// budget. Bounded rather than infinite: the process this leaves
    /// behind is orphaned by the kill, and an orphan that never exits is
    /// a leak in every later test run.
    fn hang_uname(&self, seconds: u32) {
        self.write_uname(&format!("#!/bin/sh\nexec sleep {seconds}\n"));
    }

    /// The far side's environment, sourced by the fixture before it runs
    /// a remote command. This is the hermeticity contract.
    fn write_session_env(&self) -> PathBuf {
        let path = self._root.path().join("session.env");
        std::fs::write(
            &path,
            format!(
                "HOME={home}\nPATH={path}\nUSER={user}\nROOST_BOOTSTRAP_FS_ROOT={jail}\n\
                 export HOME PATH USER ROOST_BOOTSTRAP_FS_ROOT\n",
                home = shell_quote(&self.home.display().to_string()),
                path = shell_quote(&self.stub_bin.display().to_string()),
                user = shell_quote("fixture"),
                jail = shell_quote(&self.jail.display().to_string()),
            ),
        )
        .expect("write the session env");
        path
    }

    fn write_ssh_wrapper(&self, session_env: &Path) -> PathBuf {
        let path = self._root.path().join("ssh");
        write_executable(
            &path,
            format!(
                "#!/bin/sh\nFAKE_SSH_LOG={log}\nFAKE_SSH_MODE=run-remote\nFAKE_SSH_EXEC=true\n\
                 FAKE_SSH_SESSION_ENV={env}\nexport FAKE_SSH_LOG FAKE_SSH_MODE FAKE_SSH_EXEC \
                 FAKE_SSH_SESSION_ENV\nexec {fixture} \"$@\"\n",
                log = shell_quote(&self.log.display().to_string()),
                env = shell_quote(&session_env.display().to_string()),
                fixture = shell_quote(&fixture_path().display().to_string()),
            )
            .as_bytes(),
        );
        path
    }

    fn ssh_options(&self) -> SshTunnelOptions {
        SshTunnelOptions {
            // Neither file exists, so the generated config carries no
            // `Include`: a test must never read the developer's own
            // `~/.ssh/config`.
            config_paths: SshConfigPaths {
                user: None,
                system: None,
            },
            scratch_parents: vec![self.parent.clone()],
            ssh_bin: self.ssh_bin.clone(),
        }
    }

    fn options(&self) -> BootstrapOptions {
        BootstrapOptions {
            expected: expected(),
            asset_base: None,
            install_bin: None,
            // Injected empty on purpose: the sibling rung asks what
            // platform *this* build is for, so a test that let it
            // resolve would pass on Linux and skip on a Mac.
            sibling_bin: None,
            curl_bin: PathBuf::from("curl"),
            source: None,
            // The fixture's far side is this machine, so the absolute
            // rungs have to be redirected into the jail. See
            // `FS_ROOT_ENV`: the seam is gated on `ROOST_TEST_MODE=1`
            // in `from_env`, and set here directly rather than through
            // process-global environment every other test also reads.
            jail_fs_root: true,
        }
    }

    async fn job(&self, options: BootstrapOptions) -> BootstrapJob {
        BootstrapJob::open(&ssh_target("workbox"), &self.ssh_options(), options)
            .await
            .expect("open a bootstrap job")
    }

    /// A job pinned to the download rung, served by `server`. Forced
    /// rather than merely preferred: on a Linux runner an available
    /// sibling or override would win and the download would never run.
    async fn asset_job(&self, server: &AssetServer) -> BootstrapJob {
        let mut options = self.options();
        options.asset_base = Some(server.base());
        options.source = Some(InstallSource::Asset);
        self.job(options).await
    }

    // ---- the far side's filesystem -----------------------------------

    /// A remote path as this side can open it. `$HOME/…` lands in the
    /// fake home; an absolute path lands in the jail, exactly as
    /// `ROOST_BOOTSTRAP_FS_ROOT` makes it.
    fn local(&self, remote: &str) -> PathBuf {
        match remote.strip_prefix("$HOME/") {
            Some(rest) => self.home.join(rest),
            None => self
                .jail
                .join(remote.strip_prefix('/').expect("an absolute remote path")),
        }
    }

    /// The same path spelled the way the far side reports it — what a
    /// [`ProbeOutcome`] carries and what a `start` execs.
    fn remote(&self, remote: &str) -> String {
        self.local(remote).display().to_string()
    }

    /// Put a fake `roost-session` at a remote path.
    fn plant(&self, remote: &str, stub: &Stub) -> PathBuf {
        self.plant_bytes(remote, stub.script(&self.state).as_bytes())
    }

    /// The same, for bytes that are not a [`Stub`] — an incumbent
    /// install a failing job must leave byte for byte.
    fn plant_bytes(&self, remote: &str, contents: &[u8]) -> PathBuf {
        let path = self.local(remote);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("create the rung's dir");
        write_executable(&path, contents);
        path
    }

    /// A local file to install *from*, which is also a working fake.
    fn source_file(&self, stub: &Stub) -> PathBuf {
        let path = self._root.path().join(format!("source-{}", stub.name));
        write_executable(&path, stub.script(&self.state).as_bytes());
        path
    }

    /// Where an install lands, as this side can read it.
    fn dest(&self) -> PathBuf {
        self.local("$HOME/.local/bin/roost-session")
    }

    /// Every `roost-session.tmp.<pid>` still sitting beside the
    /// destination. Empty is the invariant on every path out of an
    /// install.
    fn staged(&self) -> Vec<PathBuf> {
        self.leftovers("roost-session.tmp.")
    }

    /// Every `roost-session.bak.<pid>` still sitting beside the
    /// destination — the incumbent's copy, which a finished install has
    /// either discarded or moved back.
    fn backups(&self) -> Vec<PathBuf> {
        self.leftovers("roost-session.bak.")
    }

    fn leftovers(&self, prefix: &str) -> Vec<PathBuf> {
        let dir = self.dest().parent().expect("a parent").to_path_buf();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut found: Vec<PathBuf> = entries
            .filter_map(|entry| {
                let entry = entry.ok()?;
                entry
                    .file_name()
                    .to_str()?
                    .starts_with(prefix)
                    .then(|| entry.path())
            })
            .collect();
        found.sort();
        found
    }

    // ---- the invocation log -------------------------------------------

    fn lines(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(&self.log)
            .expect("read the invocation log")
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| line.split('\t').map(str::to_string).collect())
            .collect()
    }

    /// Every invocation's argv, with the fixture's leading `pid=` field
    /// stripped off.
    fn invocations(&self) -> Vec<Vec<String>> {
        self.lines()
            .into_iter()
            .map(|fields| fields[1..].to_vec())
            .collect()
    }

    fn pids(&self) -> Vec<i32> {
        self.lines()
            .into_iter()
            .filter_map(|fields| fields[0].strip_prefix("pid=")?.parse().ok())
            .collect()
    }

    /// Every remote command run so far, in order.
    fn remote_commands(&self) -> Vec<String> {
        self.invocations()
            .into_iter()
            .filter(|argv| !is_master_exit(argv))
            .filter_map(|argv| argv.last().cloned())
            .collect()
    }

    fn ran(&self, needle: &str) -> usize {
        self.remote_commands()
            .iter()
            .filter(|command| command.contains(needle))
            .count()
    }

    /// Every scratch directory under the harness's parent.
    fn job_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&self.parent)
            .expect("read the scratch parent")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect();
        dirs.sort();
        dirs
    }
}

/// Where this machine keeps `tool`. Anything missing is a broken
/// machine, not a skipped test: the scripts under test run these.
fn real_tool(tool: &str) -> PathBuf {
    TOOL_DIRS
        .iter()
        .map(|dir| Path::new(dir).join(tool))
        .find(|candidate| candidate.exists())
        .unwrap_or_else(|| panic!("this machine has no {tool} in {TOOL_DIRS:?}"))
}

fn write_executable(path: &Path, contents: &[u8]) {
    std::fs::write(path, contents).expect("write a fixture file");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod a fixture file");
}

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/roosttest/fixtures/fake-ssh.sh")
        .canonicalize()
        .expect("the fake-ssh fixture must exist")
}

fn ssh_target(raw: &str) -> SshTarget {
    match classify(raw).expect("classify the target") {
        ResolvedTransport::Ssh(target) => target,
        other => panic!("expected an ssh target, got {other:?}"),
    }
}

fn is_master_exit(argv: &[String]) -> bool {
    argv.windows(2)
        .any(|pair| pair[0] == "-O" && pair[1] == "exit")
}

/// The `-S` control socket an invocation addressed.
fn ctl_of(argv: &[String]) -> Option<String> {
    argv.windows(2)
        .find(|pair| pair[0] == "-S")
        .map(|pair| pair[1].clone())
}

/// `kill(pid, 0)`: true while the pid is addressable, which includes an
/// unreaped zombie — so this doubles as a check that what was killed was
/// also reaped.
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 sends nothing; it only asks whether the pid can
    // be signalled.
    unsafe { libc::kill(pid, 0) == 0 }
}

async fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    // The crate's own knob, clamped up: a scale below 1 is a request to
    // run the *product* faster, never to give this poll less rope.
    let scale = roost_ipc::session_launch::timeout_scale().max(1.0);
    let deadline = std::time::Instant::now() + Duration::from_secs(20).mul_f64(scale);
    loop {
        if ready() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

// ============================================================================
// A release server on loopback
// ============================================================================

/// One canned response.
struct Route {
    status: &'static str,
    body: Vec<u8>,
    /// A `Content-Length` other than the body's real length — the only
    /// way to offer an asset too big to accept without moving 256 MiB
    /// through a test.
    content_length: Option<usize>,
    /// Send no `Content-Length` header at all, ending the body with the
    /// connection close.
    undeclared: bool,
}

impl Route {
    fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: "200 OK",
            body: body.into(),
            content_length: None,
            undeclared: false,
        }
    }

    fn claiming(mut self, length: usize) -> Self {
        self.content_length = Some(length);
        self
    }

    /// No `Content-Length` at all — the shape `--max-filesize` cannot
    /// bound, because there is no declared length for it to compare
    /// against.
    fn without_content_length(mut self) -> Self {
        self.undeclared = true;
        self
    }
}

/// The `_FeedServer` idea from `tools/roosttest/test_sparkle.py`, in
/// Rust: a loopback listener serving a fixed route table and recording
/// what was asked for, so a test can prove the wire was actually used.
struct AssetServer {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
}

impl AssetServer {
    fn new(routes: Vec<(String, Route)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback listener");
        let port = listener.local_addr().expect("local addr").port();
        let routes: HashMap<String, Route> = routes.into_iter().collect();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        {
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    let Ok(mut stream) = stream else { continue };
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                    // One read is enough: the request line — all this
                    // needs — is the first thing on the wire.
                    let mut buf = [0u8; 4096];
                    let read = stream.read(&mut buf).unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..read]).to_string();
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    requests.lock().expect("the request log").push(path.clone());

                    let response = match routes.get(&path) {
                        Some(route) => {
                            let mut bytes = if route.undeclared {
                                format!("HTTP/1.1 {}\r\nConnection: close\r\n\r\n", route.status)
                                    .into_bytes()
                            } else {
                                let length = route.content_length.unwrap_or(route.body.len());
                                format!(
                                    "HTTP/1.1 {}\r\nContent-Length: {length}\r\nConnection: \
                                     close\r\n\r\n",
                                    route.status
                                )
                                .into_bytes()
                            };
                            bytes.extend_from_slice(&route.body);
                            bytes
                        }
                        None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: \
                                  close\r\n\r\n"
                            .to_vec(),
                    };
                    let _ = stream.write_all(&response);
                    let _ = stream.flush();
                }
            });
        }

        Self {
            port,
            requests,
            stop,
        }
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("the request log").clone()
    }
}

impl Drop for AssetServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // One connection to wake the accept loop so the thread ends.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

/// The asset name a client of [`CLIENT_VERSION`] would fetch, and its
/// checksum sibling.
fn asset_paths(arch: RemoteArch) -> (String, String) {
    let asset = asset_name(CLIENT_VERSION, arch);
    let checksum = checksum_name(&asset);
    (asset, checksum)
}

/// A route table serving `body` as the asset and a correct `sha256sum`
/// record for it.
fn release_routes(body: &[u8]) -> Vec<(String, Route)> {
    let (asset, checksum) = asset_paths(RemoteArch::Amd64);
    vec![
        (format!("/{asset}"), Route::ok(body.to_vec())),
        (
            format!("/{checksum}"),
            Route::ok(format!("{}  {asset}\n", sha256_hex(body))),
        ),
    ]
}

// ============================================================================
// The probe
// ============================================================================

#[tokio::test]
async fn a_matching_binary_on_the_first_rung_reads_as_compatible() {
    let harness = Harness::new();
    harness.plant("$HOME/.local/bin/roost-session", &Stub::matching("hit"));
    let job = harness.job(harness.options()).await;

    let probe = job.probe().await.expect("probe");

    assert_eq!(probe.arch, RemoteArch::Amd64);
    assert_eq!(
        probe.outcome,
        ProbeOutcome::Compatible {
            path: harness.remote("$HOME/.local/bin/roost-session")
        }
    );
    job.close().await;
}

#[tokio::test]
async fn a_host_with_no_roost_session_anywhere_reads_as_missing() {
    let harness = Harness::new();
    let job = harness.job(harness.options()).await;

    let probe = job.probe().await.expect("probe");

    assert_eq!(probe.outcome, ProbeOutcome::Missing);
    assert!(probe.candidates.is_empty(), "{:?}", probe.candidates);
    job.close().await;
}

#[tokio::test]
async fn another_build_reads_as_a_mismatch_carrying_the_identity_it_read() {
    let harness = Harness::new();
    harness.plant("$HOME/.local/bin/roost-session", &Stub::stale("old"));
    let job = harness.job(harness.options()).await;

    let probe = job.probe().await.expect("probe");

    assert_eq!(
        probe.outcome,
        ProbeOutcome::Mismatch {
            path: harness.remote("$HOME/.local/bin/roost-session"),
            identity: Some(stale()),
        }
    );
    job.close().await;
}

/// A build older than the `identify` subcommand: present, and unable to
/// say who it is. The UX answer is the same offer, so the probe must
/// reach it rather than failing.
#[tokio::test]
async fn a_binary_that_will_not_identify_reads_as_a_mismatch_with_no_identity() {
    let harness = Harness::new();
    harness.plant(
        "$HOME/.local/bin/roost-session",
        &Stub::unidentifiable("ancient"),
    );
    let job = harness.job(harness.options()).await;

    let probe = job.probe().await.expect("probe");

    assert_eq!(
        probe.outcome,
        ProbeOutcome::Mismatch {
            path: harness.remote("$HOME/.local/bin/roost-session"),
            identity: None,
        }
    );
    job.close().await;
}

/// The anti-shadowing invariant (plan 039 §3.2). A stale binary on the
/// rung the transport execs *first* and a perfectly good one further
/// down: the verdict is about the rung that will actually run, because
/// calling the host compatible on the strength of a rung nothing execs
/// would offer no install while every attach kept failing.
#[tokio::test]
async fn a_stale_preferred_rung_outranks_a_compatible_one_below_it() {
    let harness = Harness::new();
    harness.plant(
        "$HOME/.local/bin/roost-session",
        &Stub::unidentifiable("shadow"),
    );
    harness.plant("/usr/bin/roost-session", &Stub::matching("deb"));
    let job = harness.job(harness.options()).await;

    let probe = job.probe().await.expect("probe");

    assert_eq!(
        probe.candidates,
        vec![
            harness.remote("$HOME/.local/bin/roost-session"),
            harness.remote("/usr/bin/roost-session"),
        ],
        "both rungs are found, in ladder order"
    );
    assert_eq!(
        probe.outcome,
        ProbeOutcome::Mismatch {
            path: harness.remote("$HOME/.local/bin/roost-session"),
            identity: None,
        },
        "the verdict names the rung the exec chain would run, so an install is offered"
    );
    job.close().await;
}

#[tokio::test]
async fn a_darwin_remote_is_refused_before_anything_is_offered() {
    let harness = Harness::with_uname("Darwin", "arm64");
    let job = harness.job(harness.options()).await;

    let error = job.probe().await.expect_err("a Mac host has no build");

    assert_eq!(error, BootstrapError::UnsupportedOs("Darwin".to_string()));
    assert!(error.message("workbox").contains("Linux only"));
    job.close().await;
}

#[tokio::test]
async fn an_architecture_with_no_build_is_refused_by_name() {
    let harness = Harness::with_uname("Linux", "ppc64le");
    let job = harness.job(harness.options()).await;

    let error = job.probe().await.expect_err("no ppc64le build exists");

    assert_eq!(
        error,
        BootstrapError::UnsupportedArch("ppc64le".to_string())
    );
    assert!(error.message("workbox").contains("ppc64le"));
    job.close().await;
}

// ============================================================================
// The install
// ============================================================================

#[tokio::test]
async fn an_install_streams_verifies_and_commits_the_binary() {
    let harness = Harness::new();
    let source = harness.source_file(&Stub::matching("payload"));
    let mut options = harness.options();
    options.install_bin = Some(source.clone());
    let job = harness.job(options).await;

    let resolved = job
        .resolve_source(RemoteArch::Amd64)
        .await
        .expect("resolve the override");
    assert_eq!(*resolved.origin(), SourceOrigin::Override);

    let installed = job.install(&resolved).await.expect("install");

    assert_eq!(
        installed.dest,
        harness.remote("$HOME/.local/bin/roost-session")
    );
    let dest = harness.dest();
    assert_eq!(
        std::fs::read(&dest).expect("read the installed binary"),
        std::fs::read(&source).expect("read the source"),
        "the bytes that landed are the bytes that were sent"
    );
    let mode = std::fs::metadata(&dest).expect("stat").permissions().mode();
    assert!(
        mode & 0o111 != 0,
        "the install must be executable: {mode:o}"
    );
    assert!(harness.staged().is_empty(), "{:?}", harness.staged());
    assert!(harness.backups().is_empty(), "{:?}", harness.backups());
    assert!(
        installed.path_warning.is_some(),
        "the fixture's PATH holds no roost-session, so the warning rides along"
    );
    job.close().await;
}

/// The incumbent is moved aside rather than overwritten, and put back
/// when the *post-commit* re-verify fails — the window the single `mv`
/// left open, in which the old bytes were already gone and the new ones
/// had not answered.
///
/// The stub identifies from the staging path and nowhere else, so it
/// passes the pre-commit gate, gets renamed into place, and then goes
/// silent: exactly the shape a dropped leg or a timeout after the `mv`
/// produces.
#[tokio::test]
async fn a_commit_that_lands_and_then_fails_identify_puts_the_incumbent_back() {
    let harness = Harness::new();
    let incumbent = b"#!/bin/sh\n# the install that must survive this\nexit 7\n".to_vec();
    let dest = harness.plant_bytes("$HOME/.local/bin/roost-session", &incumbent);

    let source = harness.source_file(&Stub::matching("two-faced").only_when_staged());
    let mut options = harness.options();
    options.install_bin = Some(source);
    let job = harness.job(options).await;
    let resolved = job
        .resolve_source(RemoteArch::Amd64)
        .await
        .expect("resolve");

    let error = job
        .install(&resolved)
        .await
        .expect_err("the committed file would not identify itself");

    match &error {
        BootstrapError::PostCommit { restored, .. } => {
            assert!(restored, "an incumbent was there, so it must be back")
        }
        other => panic!("{other:?}"),
    }
    // The copy says what happened rather than asserting an unchanged
    // install it could not vouch for.
    let message = error.message("workbox");
    assert!(message.contains("put back"), "{message}");

    assert_eq!(
        std::fs::read(&dest).expect("read the destination"),
        incumbent,
        "the previous install is back, byte for byte"
    );
    assert!(harness.staged().is_empty(), "{:?}", harness.staged());
    assert!(
        harness.backups().is_empty(),
        "the backup is consumed by the rollback: {:?}",
        harness.backups()
    );
    job.close().await;
}

/// The same failure on a host that had nothing installed: there is
/// nothing to put back, and the copy says so instead of promising a
/// restore that never happened.
#[tokio::test]
async fn the_same_failure_with_no_incumbent_says_there_was_nothing_to_restore() {
    let harness = Harness::new();
    let source = harness.source_file(&Stub::matching("two-faced").only_when_staged());
    let mut options = harness.options();
    options.install_bin = Some(source);
    let job = harness.job(options).await;
    let resolved = job
        .resolve_source(RemoteArch::Amd64)
        .await
        .expect("resolve");

    let error = job.install(&resolved).await.expect_err("no identity");

    match &error {
        BootstrapError::PostCommit { restored, .. } => {
            assert!(!restored, "there was no previous install to restore")
        }
        other => panic!("{other:?}"),
    }
    let message = error.message("workbox");
    assert!(message.contains("no previous install"), "{message}");
    assert!(harness.backups().is_empty(), "{:?}", harness.backups());
    job.close().await;
}

/// The load-bearing refusal (plan 039 §3.4): what is on the far side
/// before a bad install is what is on it after.
#[tokio::test]
async fn a_wrong_build_leaves_the_previous_install_byte_for_byte() {
    let harness = Harness::new();
    let incumbent = b"#!/bin/sh\n# the install that was already there\nexit 7\n".to_vec();
    let dest = harness.plant_bytes("$HOME/.local/bin/roost-session", &incumbent);

    let source = harness.source_file(&Stub::stale("wrong"));
    let mut options = harness.options();
    options.install_bin = Some(source);
    let job = harness.job(options).await;
    let resolved = job
        .resolve_source(RemoteArch::Amd64)
        .await
        .expect("resolve");

    let error = job
        .install(&resolved)
        .await
        .expect_err("a binary that is not this build must never be committed");

    assert!(matches!(error, BootstrapError::Verify(_)), "{error:?}");
    assert_eq!(
        std::fs::read(&dest).expect("read the destination"),
        incumbent,
        "the previous install is untouched"
    );
    assert!(harness.staged().is_empty(), "{:?}", harness.staged());
    assert_eq!(harness.ran("mv -- "), 0, "the commit never ran");
    job.close().await;
}

/// The same, one rung further along the trust chain: the checksum
/// matched, the download is exactly what the server published, and the
/// bytes are still not this build.
#[tokio::test]
async fn a_checksum_valid_but_wrong_build_still_leaves_the_destination_alone() {
    let harness = Harness::new();
    let incumbent = b"#!/bin/sh\n# still here afterwards\nexit 7\n".to_vec();
    let dest = harness.plant_bytes("$HOME/.local/bin/roost-session", &incumbent);

    let published = harness.source_file(&Stub::stale("published"));
    let body = std::fs::read(&published).expect("read the published body");
    let server = AssetServer::new(release_routes(&body));
    let job = harness.asset_job(&server).await;

    let resolved = job
        .resolve_source(RemoteArch::Amd64)
        .await
        .expect("the checksum matches, so the download resolves");
    let error = job.install(&resolved).await.expect_err("wrong build");

    assert!(matches!(error, BootstrapError::Verify(_)), "{error:?}");
    assert_eq!(
        std::fs::read(&dest).expect("read the destination"),
        incumbent
    );
    assert!(harness.staged().is_empty(), "{:?}", harness.staged());
    job.close().await;
}

/// A `tee` that writes a little and dies — the shape a full disk takes.
#[tokio::test]
async fn a_stream_that_dies_part_way_removes_the_staged_file() {
    let harness = Harness::new();
    let incumbent = b"#!/bin/sh\n# unchanged by a failed stream\nexit 7\n".to_vec();
    let dest = harness.plant_bytes("$HOME/.local/bin/roost-session", &incumbent);
    // argv is `tee -- <tmp>`, so the path is the second operand.
    harness.override_tool(
        "tee",
        "#!/bin/sh\nprintf 'a partial write' > \"$2\"\nprintf 'tee: no space left on device\\n' \
         >&2\nexit 1\n",
    );

    let source = harness.source_file(&Stub::matching("interrupted"));
    let mut options = harness.options();
    options.install_bin = Some(source);
    let job = harness.job(options).await;
    let resolved = job
        .resolve_source(RemoteArch::Amd64)
        .await
        .expect("resolve");

    let error = job.install(&resolved).await.expect_err("the stream died");

    assert!(
        matches!(
            error,
            BootstrapError::Install {
                phase: InstallPhase::Stream,
                ..
            }
        ),
        "{error:?}"
    );
    assert!(
        harness.staged().is_empty(),
        "the staged file must be cleaned up: {:?}",
        harness.staged()
    );
    assert_eq!(
        std::fs::read(&dest).expect("read the destination"),
        incumbent
    );
    job.close().await;
}

/// The other half of a broken stream: the *local* source stops being
/// readable half-way through the job. The far side sees a short write
/// and this side reports the read, not an EPIPE.
#[tokio::test]
async fn a_source_that_cannot_be_read_fails_the_stream_and_stages_nothing() {
    let harness = Harness::new();
    let source = harness.source_file(&Stub::matching("vanishing"));
    let mut options = harness.options();
    options.install_bin = Some(source.clone());
    let job = harness.job(options).await;
    let resolved = job
        .resolve_source(RemoteArch::Amd64)
        .await
        .expect("resolve");
    std::fs::remove_file(&source).expect("take the source away after it resolved");

    let error = job.install(&resolved).await.expect_err("unreadable source");

    match &error {
        BootstrapError::Install { phase, detail } => {
            assert_eq!(*phase, InstallPhase::Stream);
            assert!(detail.contains("opening"), "{detail}");
        }
        other => panic!("{other:?}"),
    }
    assert!(harness.staged().is_empty(), "{:?}", harness.staged());
    assert!(!harness.dest().exists(), "nothing was installed");
    job.close().await;
}

// ============================================================================
// The download rung
// ============================================================================

#[tokio::test]
async fn a_release_asset_is_downloaded_checksum_verified_and_installed() {
    let harness = Harness::new();
    let published = harness.source_file(&Stub::matching("release"));
    let body = std::fs::read(&published).expect("read the published body");
    let server = AssetServer::new(release_routes(&body));
    let job = harness.asset_job(&server).await;

    let resolved = job
        .resolve_source(RemoteArch::Amd64)
        .await
        .expect("download and verify");
    assert_eq!(
        *resolved.origin(),
        SourceOrigin::Asset {
            base: server.base(),
            overridden: true,
        }
    );
    assert!(
        resolved.describe().contains(&server.base()),
        "an overridden base is named as itself: {}",
        resolved.describe()
    );

    job.install(&resolved).await.expect("install the download");

    let (asset, checksum) = asset_paths(RemoteArch::Amd64);
    assert_eq!(
        server.requests(),
        vec![format!("/{asset}"), format!("/{checksum}")],
        "the asset and its checksum both came off the wire, in that order"
    );
    assert_eq!(
        std::fs::read(harness.dest()).expect("read the installed binary"),
        body
    );
    job.close().await;
}

#[tokio::test]
async fn a_checksum_that_does_not_match_sends_nothing() {
    let harness = Harness::new();
    let published = harness.source_file(&Stub::matching("mismatched"));
    let body = std::fs::read(&published).expect("read the published body");
    let (asset, checksum) = asset_paths(RemoteArch::Amd64);
    let server = AssetServer::new(vec![
        (format!("/{asset}"), Route::ok(body)),
        (
            format!("/{checksum}"),
            Route::ok(format!("{}  {asset}\n", "0".repeat(64))),
        ),
    ]);
    let job = harness.asset_job(&server).await;

    let error = job
        .resolve_source(RemoteArch::Amd64)
        .await
        .expect_err("the hash does not match");

    assert!(matches!(error, BootstrapError::Checksum(_)), "{error:?}");
    assert_eq!(harness.ran("tee -- "), 0, "nothing unverified is ever sent");
    assert!(!harness.dest().exists());
    job.close().await;
}

#[tokio::test]
async fn a_malformed_checksum_file_sends_nothing() {
    let harness = Harness::new();
    let published = harness.source_file(&Stub::matching("malformed"));
    let body = std::fs::read(&published).expect("read the published body");
    let (asset, checksum) = asset_paths(RemoteArch::Amd64);
    let server = AssetServer::new(vec![
        (format!("/{asset}"), Route::ok(body)),
        (
            format!("/{checksum}"),
            Route::ok("<!doctype html>\nnot a checksum at all\n"),
        ),
    ]);
    let job = harness.asset_job(&server).await;

    let error = job
        .resolve_source(RemoteArch::Amd64)
        .await
        .expect_err("that is not a sha256sum record");

    assert!(matches!(error, BootstrapError::Checksum(_)), "{error:?}");
    assert_eq!(harness.ran("tee -- "), 0);
    job.close().await;
}

#[tokio::test]
async fn a_checksum_naming_another_file_sends_nothing() {
    let harness = Harness::new();
    let published = harness.source_file(&Stub::matching("misnamed"));
    let body = std::fs::read(&published).expect("read the published body");
    let (asset, checksum) = asset_paths(RemoteArch::Amd64);
    let server = AssetServer::new(vec![
        (format!("/{asset}"), Route::ok(body.clone())),
        (
            format!("/{checksum}"),
            Route::ok(format!("{}  some-other-artifact\n", sha256_hex(&body))),
        ),
    ]);
    let job = harness.asset_job(&server).await;

    let error = job
        .resolve_source(RemoteArch::Amd64)
        .await
        .expect_err("a hash for bytes nobody downloaded");

    match &error {
        BootstrapError::Checksum(detail) => {
            assert!(detail.contains("some-other-artifact"), "{detail}")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(harness.ran("tee -- "), 0);
    job.close().await;
}

/// The cap breach, offered the way a real one arrives: a
/// `Content-Length` past the limit, which `curl --max-filesize` refuses
/// before a byte of body is read.
#[tokio::test]
async fn an_asset_past_the_size_cap_is_refused() {
    let harness = Harness::new();
    let (asset, checksum) = asset_paths(RemoteArch::Amd64);
    let server = AssetServer::new(vec![
        (
            format!("/{asset}"),
            Route::ok(b"far smaller than it claims".to_vec()).claiming(300 * 1024 * 1024),
        ),
        (format!("/{checksum}"), Route::ok("unused")),
    ]);
    let job = harness.asset_job(&server).await;

    let error = job
        .resolve_source(RemoteArch::Amd64)
        .await
        .expect_err("300 MiB is past the cap");

    assert!(matches!(error, BootstrapError::Download(_)), "{error:?}");
    assert_eq!(harness.ran("tee -- "), 0);
    assert!(!harness.dest().exists());
    job.close().await;
}

/// The cap breach `--max-filesize` **cannot** catch: a response that
/// declares no length at all. Nothing but this side's own counting copy
/// bounds it, and without that it is bounded by `--max-time` and the
/// disk.
///
/// Aimed at the `.sha256` sibling rather than the asset, because its cap
/// is 4 KiB and the asset's is 256 MiB — the same code path, four
/// orders of magnitude cheaper to prove.
#[tokio::test]
async fn a_response_with_no_declared_length_is_still_capped() {
    let harness = Harness::new();
    let published = harness.source_file(&Stub::matching("undeclared"));
    let body = std::fs::read(&published).expect("read the published body");
    let (asset, checksum) = asset_paths(RemoteArch::Amd64);
    let server = AssetServer::new(vec![
        (format!("/{asset}"), Route::ok(body)),
        (
            format!("/{checksum}"),
            // Well past CHECKSUM_MAX_BYTES, and the server never says
            // how long it is.
            Route::ok(vec![b'x'; 32 * 1024]).without_content_length(),
        ),
    ]);
    let job = harness.asset_job(&server).await;

    let error = job
        .resolve_source(RemoteArch::Amd64)
        .await
        .expect_err("an unbounded body must be cut off");

    assert!(matches!(error, BootstrapError::Download(_)), "{error:?}");
    assert_eq!(harness.ran("tee -- "), 0, "nothing was sent");
    job.close().await;
}

#[tokio::test]
async fn a_missing_curl_is_classified_rather_than_panicked_on() {
    let harness = Harness::new();
    let mut options = harness.options();
    options.asset_base = Some("https://example.invalid/download".to_string());
    options.source = Some(InstallSource::Asset);
    options.curl_bin = PathBuf::from("/nonexistent/bin/curl");
    let job = harness.job(options).await;

    let error = job
        .resolve_source(RemoteArch::Amd64)
        .await
        .expect_err("no curl, no download");

    match &error {
        BootstrapError::Download(detail) => assert!(detail.contains("curl"), "{detail}"),
        other => panic!("{other:?}"),
    }
    job.close().await;
}

/// The seam that lets a fixture serve over plain HTTP must not become a
/// way to downgrade a real download, so the exception is loopback only —
/// and it is enforced before anything is fetched.
#[tokio::test]
async fn a_plaintext_asset_base_that_is_not_loopback_is_refused_before_any_fetch() {
    let harness = Harness::new();
    let mut options = harness.options();
    options.asset_base = Some("http://downloads.example.com/roost".to_string());
    options.source = Some(InstallSource::Asset);
    // Any fetch attempt would have to exec this, and there is nothing
    // there — so a refusal that reads `curl` in its copy is a refusal
    // that happened too late.
    options.curl_bin = PathBuf::from("/nonexistent/bin/curl");
    let job = harness.job(options).await;

    let error = job
        .resolve_source(RemoteArch::Amd64)
        .await
        .expect_err("plain http off loopback");

    match &error {
        BootstrapError::Download(detail) => {
            assert!(detail.contains("https"), "{detail}");
            assert!(
                !detail.contains("curl"),
                "refused before any fetch: {detail}"
            );
        }
        other => panic!("{other:?}"),
    }
    job.close().await;
}

// ============================================================================
// Stop, await, start
// ============================================================================

/// The job asked for the session to be gone and it is gone. Reading
/// that as a failure would turn "already stopped" into a dead end.
#[tokio::test]
async fn no_session_on_the_stop_reads_as_already_stopped() {
    let harness = Harness::new();
    harness.plant(
        "$HOME/.local/bin/roost-session",
        &Stub::matching("inert").bridge(Bridge::NoSession),
    );
    let job = harness.job(harness.options()).await;

    job.stop_over_the_wire()
        .await
        .expect("nothing to stop is a stop that succeeded");

    job.close().await;
}

/// A session that answers the stop with a refusal — `shutting-down`,
/// most often — is not a failed stop either. Whether the process
/// actually went is [`BootstrapJob::await_gone`]'s question, and it does
/// not care who asked it to go.
#[tokio::test]
async fn a_session_that_declines_the_stop_is_left_to_await_gone() {
    let harness = Harness::new();
    harness.plant(
        "$HOME/.local/bin/roost-session",
        &Stub::matching("declining").bridge(Bridge::refuses(
            "shutting-down",
            "the session is already shutting down",
        )),
    );
    let job = harness.job(harness.options()).await;

    job.stop_over_the_wire()
        .await
        .expect("a refusal is not a transport failure");

    job.close().await;
}

#[tokio::test]
async fn a_hard_stop_failure_is_classified() {
    let harness = Harness::new();
    harness.plant(
        "$HOME/.local/bin/roost-session",
        &Stub::matching("wedged").bridge(Bridge::Fails(
            "kex_exchange_identification: Connection closed by remote host".to_string(),
        )),
    );
    let job = harness.job(harness.options()).await;

    let error = job
        .stop_over_the_wire()
        .await
        .expect_err("a bridge that dies is not a stop");

    match &error {
        BootstrapError::Stop(detail) => assert!(!detail.is_empty(), "{detail}"),
        other => panic!("{other:?}"),
    }
    assert!(error.message("workbox").contains("roostctl session stop"));
    job.close().await;
}

/// Only `shutting-down` means "the stop you asked for is already
/// happening". Every other refusal is a refusal, and reading them all as
/// consent made the job wait out the whole await-gone budget before
/// reporting a timeout — instead of the reason the session gave in its
/// first sentence.
#[tokio::test]
async fn the_stop_accepts_only_the_shutting_down_code() {
    for (code, message, accepted) in [
        (
            "shutting-down",
            "the session is already shutting down",
            true,
        ),
        ("connect-required", "connect first", false),
        ("internal", "the dispatcher fell over", false),
        ("a-code-from-the-future", "who knows", false),
    ] {
        let harness = Harness::new();
        harness.plant(
            "$HOME/.local/bin/roost-session",
            &Stub::matching("refusing").bridge(Bridge::refuses(code, message)),
        );
        let job = harness.job(harness.options()).await;

        let result = job.stop_over_the_wire().await;

        if accepted {
            result.unwrap_or_else(|error| panic!("{code} must be accepted: {error:?}"));
        } else {
            match result.expect_err(code) {
                BootstrapError::Stop(detail) => {
                    assert!(detail.contains(message), "{code}: {detail}")
                }
                other => panic!("{code}: {other:?}"),
            }
        }
        job.close().await;
    }
}

#[tokio::test]
async fn await_gone_returns_as_soon_as_the_far_side_reports_no_session() {
    let harness = Harness::new();
    harness.plant(
        "$HOME/.local/bin/roost-session",
        &Stub::matching("gone").bridge(Bridge::NoSession),
    );
    let job = harness.job(harness.options()).await;

    job.await_gone().await.expect("already gone");

    assert_eq!(
        harness.ran("client-bridge"),
        1,
        "one poll answered the question"
    );
    job.close().await;
}

/// `session.stop` replies before the process unlinks its socket. The
/// poll is what turns that gap into a wait rather than a race the next
/// `start` loses.
#[tokio::test]
async fn await_gone_waits_out_a_delayed_finalization() {
    let harness = Harness::new();
    harness.plant(
        "$HOME/.local/bin/roost-session",
        &Stub::matching("dying").bridge(Bridge::serving(Some(2))),
    );
    let job = harness.job(harness.options()).await;

    job.stop_over_the_wire()
        .await
        .expect("the stop is accepted");
    job.await_gone()
        .await
        .expect("it finalizes on the third poll");

    assert!(
        harness.ran("client-bridge") >= 4,
        "the stop plus three polls: {}",
        harness.ran("client-bridge")
    );
    job.close().await;
}

/// The budget bounds the **loop**, not each poll. Checking the deadline
/// only after a complete `bridge_call` meant one hung poll could eat the
/// whole budget and a second full poll would still start behind it — so
/// the wait ran to ~2× while the error reported 1×.
#[tokio::test]
async fn await_gone_stays_inside_its_budget_when_a_poll_hangs() {
    let harness = Harness::new();
    harness.plant(
        "$HOME/.local/bin/roost-session",
        // Never answers, and outlasts the budget several times over.
        &Stub::matching("silent").bridge(Bridge::Hangs(10)),
    );
    let job = harness.job(harness.options()).await;

    let budget = Duration::from_secs(1);
    let started = std::time::Instant::now();
    let error = job
        .await_gone_within(budget)
        .await
        .expect_err("nothing ever said the session was gone");
    let elapsed = started.elapsed();

    match &error {
        BootstrapError::Stop(detail) => assert!(detail.contains("1s"), "{detail}"),
        other => panic!("{other:?}"),
    }
    assert!(
        elapsed < budget * 2,
        "the wait ran {elapsed:?} against a {budget:?} budget — one hung poll must not buy a \
         second one"
    );
    job.close().await;
}

/// The far side's half of the same race: a loser of the socket-lock
/// race answers `already-running` once, and the retry wins.
#[tokio::test]
async fn already_running_is_retried_and_then_succeeds() {
    let harness = Harness::new();
    let path = harness.remote("$HOME/.local/bin/roost-session");
    harness.plant(
        "$HOME/.local/bin/roost-session",
        &Stub::matching("racing").start(Start::AlreadyRunningThenReady {
            first: 4242,
            then: 4343,
        }),
    );
    let job = harness.job(harness.options()).await;

    let verdict = job.start(&path).await.expect("start");

    assert_eq!(verdict, Verdict::Ready(4343));
    assert!(harness.ran(" start") >= 2, "the first attempt was retried");
    job.close().await;
}

#[tokio::test]
async fn a_start_that_reports_an_error_is_classified() {
    let harness = Harness::new();
    let path = harness.remote("$HOME/.local/bin/roost-session");
    harness.plant(
        "$HOME/.local/bin/roost-session",
        &Stub::matching("broken").start(Start::Errors("the profile directory is read-only".into())),
    );
    let job = harness.job(harness.options()).await;

    let error = job.start(&path).await.expect_err("it said error:");

    match &error {
        BootstrapError::Start(detail) => assert!(detail.contains("read-only"), "{detail}"),
        other => panic!("{other:?}"),
    }
    job.close().await;
}

/// The deb case: the only `roost-session` is at `/usr/bin`, and a start
/// through a `~/.local/bin` that does not exist would fail forever.
#[tokio::test]
async fn a_start_only_flow_execs_the_path_the_probe_resolved() {
    let harness = Harness::new();
    harness.plant(
        "/usr/bin/roost-session",
        &Stub::matching("deb")
            .start(Start::Ready(1234))
            .bridge(Bridge::serving(None)),
    );
    let job = harness.job(harness.options()).await;

    let probe = job.probe().await.expect("probe");
    let ProbeOutcome::Compatible { path } = probe.outcome.clone() else {
        panic!("expected a compatible deb install, got {:?}", probe.outcome);
    };
    assert_eq!(path, harness.remote("/usr/bin/roost-session"));

    let verdict = job.start(&path).await.expect("start");
    assert_eq!(verdict, Verdict::Ready(1234));

    let started: Vec<String> = harness
        .remote_commands()
        .into_iter()
        .filter(|command| command.contains(" start"))
        .collect();
    assert_eq!(started.len(), 1, "{started:?}");
    assert!(started[0].contains(&path), "{started:?}");
    assert!(
        !started[0].contains(".local/bin"),
        "the ladder must not be re-walked at start time: {started:?}"
    );

    // Nothing was installed, so the bar is the attach gate.
    let identity = job
        .post_start_identify(IdentityGate::Existing)
        .await
        .expect("the session that came up is this build");
    assert_eq!(identity.libghostty_build, CLIENT_BUILD);
    job.close().await;
}

/// Started the right file, and the wrong session answered — which is
/// what `already-running` masks and what the on-disk checks cannot see.
#[tokio::test]
async fn post_start_identify_rejects_a_session_whose_build_differs() {
    let harness = Harness::new();
    harness.plant(
        "$HOME/.local/bin/roost-session",
        &Stub::matching("impostor").bridge(Bridge::Serving {
            app_version: CLIENT_VERSION.to_string(),
            build: OTHER_BUILD.to_string(),
            alive_polls: None,
        }),
    );
    let job = harness.job(harness.options()).await;

    let error = job
        .post_start_identify(IdentityGate::Existing)
        .await
        .expect_err("a session on another build is not a success");

    match &error {
        BootstrapError::Start(detail) => assert!(detail.contains(OTHER_BUILD), "{detail}"),
        other => panic!("{other:?}"),
    }
    job.close().await;
}

/// **The upgrade that quietly did not happen.**
///
/// `already-running` is accepted as a success once the retry budget
/// expires, so a start that never wins the socket leaves the *old*
/// process serving. When the two builds differ only in `app_version` —
/// two releases with no ghostty bump between them, the common case —
/// the attach gate cannot see it. After an install the whole triple is
/// the bar, and this is the failure that bar exists for.
///
/// The start-only flow keeps the looser gate on purpose: nothing was
/// installed, so "a session this client can talk to" is the right
/// question, and it is the same one `roost-iced` asks at attach.
#[tokio::test]
async fn an_already_running_older_session_fails_the_post_install_gate_and_passes_the_attach_one() {
    let harness = Harness::new();
    harness.plant(
        "$HOME/.local/bin/roost-session",
        &Stub::matching("stubborn")
            .start(Start::AlwaysAlreadyRunning(7))
            .bridge(Bridge::Serving {
                // Same protocol, same ghostty build, older release.
                app_version: "0.0.18".to_string(),
                build: CLIENT_BUILD.to_string(),
                alive_polls: None,
            }),
    );
    let path = harness.remote("$HOME/.local/bin/roost-session");
    let job = harness.job(harness.options()).await;

    // Retried past the budget and then accepted — which is what makes
    // the identity check below the only thing standing between this and
    // a reported-successful upgrade.
    let verdict = job
        .start(&path)
        .await
        .expect("already-running is not a failure");
    assert_eq!(verdict, Verdict::AlreadyRunning(Some(7)));
    assert!(
        harness.ran(" start") >= 2,
        "it must have been retried: {}",
        harness.ran(" start")
    );

    let error = job
        .post_start_identify(IdentityGate::Installed)
        .await
        .expect_err("the old session is still the one serving");
    match &error {
        BootstrapError::Start(detail) => {
            assert!(detail.contains("0.0.18"), "{detail}");
            assert!(detail.contains(CLIENT_VERSION), "{detail}");
        }
        other => panic!("{other:?}"),
    }

    // The same session, judged as a start-only flow: protocol and build
    // match, so this client can talk to it, and that is the whole
    // question when nothing was installed.
    let identity = job
        .post_start_identify(IdentityGate::Existing)
        .await
        .expect("the attach gate is protocol + build");
    assert_eq!(identity.app_version, "0.0.18");
    job.close().await;
}

// ============================================================================
// Job hygiene
// ============================================================================

/// Every exec goes over the job's own private master — never the
/// tunnel's, whose wedged state is the very thing a bootstrap is most
/// often digging a user out of.
#[tokio::test]
async fn every_exec_runs_over_the_jobs_own_control_socket() {
    let harness = Harness::new();
    let source = harness.source_file(&Stub::matching("mux"));
    let mut options = harness.options();
    options.install_bin = Some(source);
    let job = harness.job(options).await;

    job.probe().await.expect("probe");
    let resolved = job
        .resolve_source(RemoteArch::Amd64)
        .await
        .expect("resolve");
    job.install(&resolved).await.expect("install");

    let ctls: Vec<String> = harness
        .invocations()
        .iter()
        .map(|argv| ctl_of(argv).unwrap_or_else(|| panic!("every exec addresses a -S: {argv:?}")))
        .collect();
    assert!(ctls.len() > 5, "{ctls:?}");
    let first = ctls[0].clone();
    assert!(
        ctls.iter().all(|ctl| *ctl == first),
        "one master for the whole job: {ctls:?}"
    );

    let dir = Path::new(&first).parent().expect("the job's own directory");
    assert_eq!(dir.parent(), Some(harness.parent.as_path()));
    let name = dir
        .file_name()
        .and_then(|name| name.to_str())
        .expect("a directory name");
    assert!(
        name.starts_with("roost-ssh-bootstrap-"),
        "a job's directory is named like a one-shot, never like a tunnel's, so the host sweep \
         cannot reclaim it mid-install: {name}"
    );
    job.close().await;
}

#[tokio::test]
async fn closing_a_job_exits_its_master_while_the_socket_is_still_there() {
    let harness = Harness::new();
    harness.plant("$HOME/.local/bin/roost-session", &Stub::matching("closing"));
    let job = harness.job(harness.options()).await;
    job.probe().await.expect("probe");
    assert_eq!(harness.job_dirs().len(), 1);

    job.close().await;

    let last = harness
        .lines()
        .last()
        .cloned()
        .expect("at least one invocation");
    assert!(is_master_exit(&last[1..]), "{last:?}");
    assert!(
        last.contains(&"ctl-exists=1".to_string()),
        "the exit must run before the socket goes, or nothing can address the master: {last:?}"
    );
    assert!(harness.job_dirs().is_empty(), "{:?}", harness.job_dirs());

    // Idempotent: a second close has nothing left to do.
    job.close().await;
    assert_eq!(
        harness
            .invocations()
            .iter()
            .filter(|argv| is_master_exit(argv))
            .count(),
        1
    );
}

#[tokio::test]
async fn a_failed_job_still_takes_its_scratch_directory_with_it() {
    let harness = Harness::with_uname("Darwin", "arm64");
    let job = harness.job(harness.options()).await;
    job.probe().await.expect_err("a Mac remote");

    job.close().await;

    assert!(harness.job_dirs().is_empty(), "{:?}", harness.job_dirs());
    assert!(
        harness
            .invocations()
            .iter()
            .any(|argv| is_master_exit(argv)),
        "the master is exited on the failure path too"
    );
}

/// **What cancelling an install does and does not guarantee.**
///
/// It does *not* run the remote cleanup: `Drop` has no runtime to await
/// a round trip on, and a spawned teardown reliably never runs (the
/// transport's own `blocking_exit_master` says so) — while a blocking
/// remote exec in `Drop` would freeze a runtime thread on a host that
/// may be exactly the wedged one being bootstrapped. So the temporary
/// survives the cancellation.
///
/// What it *does* guarantee is that the leak self-heals: the name is one
/// this side generated, so `prepare_script` recognizes it as ours and
/// sweeps it on the next attempt. This test pins both halves.
#[tokio::test]
async fn a_cancelled_install_leaves_a_temporary_that_the_next_one_sweeps() {
    let harness = Harness::new();
    // Blocks the staged-verify phase, so the cancellation below lands
    // with the temporary fully written and nothing still writing to it.
    harness.block_chmod();
    let source = harness.source_file(&Stub::matching("interrupted"));
    let mut options = harness.options();
    options.install_bin = Some(source.clone());
    let job = harness.job(options).await;
    let resolved = job
        .resolve_source(RemoteArch::Amd64)
        .await
        .expect("resolve");

    // Drive the install until the temporary is fully streamed — a fixed
    // sleep would cancel before the prepare on a loaded machine — and
    // then drop the future, which is the cancellation.
    let sent = std::fs::metadata(&source).expect("stat the source").len();
    {
        let install = job.install(&resolved);
        tokio::pin!(install);
        let scale = roost_ipc::session_launch::timeout_scale().max(1.0);
        let deadline = std::time::Instant::now() + Duration::from_secs(20).mul_f64(scale);
        loop {
            tokio::select! {
                finished = &mut install => {
                    panic!("the blocked chmod must keep the install in flight: {finished:?}")
                }
                _ = tokio::time::sleep(Duration::from_millis(20)) => {
                    let staged = harness.staged();
                    if staged.len() == 1
                        && std::fs::metadata(&staged[0]).is_ok_and(|meta| meta.len() == sent)
                    {
                        break;
                    }
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for the staged file to be written"
            );
        }
    }

    // The orphan the cancellation left behind is still sitting on the
    // block; let it go before the retry, which would otherwise queue
    // behind it.
    harness.release_chmod();

    let leaked = harness.staged();
    assert_eq!(leaked.len(), 1, "{leaked:?}");
    assert!(!harness.dest().exists(), "nothing was committed");

    // The next attempt reclaims it, and lands.
    let installed = job.install(&resolved).await.expect("the retry installs");
    assert_eq!(
        installed.dest,
        harness.remote("$HOME/.local/bin/roost-session")
    );
    assert!(
        harness.staged().is_empty(),
        "the stale temporary must have been swept: {:?}",
        harness.staged()
    );
    assert_eq!(
        std::fs::read(harness.dest()).expect("read the install"),
        std::fs::read(&source).expect("read the source")
    );
    job.close().await;
}

/// A `close()` future dropped part-way through its teardown used to
/// leave the job marked closed with nothing torn down: the master lived
/// until `ControlPersist` and the scratch directory leaked forever,
/// because `parse_scratch_dir_name` refuses these names and no sweep
/// ever reclaims them. `Drop` finishes what the cancelled close started.
#[tokio::test]
async fn a_cancelled_close_leaves_drop_still_owing_the_teardown() {
    let harness = Harness::new();
    harness.plant("$HOME/.local/bin/roost-session", &Stub::matching("cut"));
    let dir = {
        let job = harness.job(harness.options()).await;
        // Opens the master, so the control socket exists and `close()`
        // has real work to do.
        job.probe().await.expect("probe");
        assert_eq!(harness.job_dirs().len(), 1);

        // Zero budget: `timeout` polls the inner future once — long
        // enough to enter the teardown — and then drops it.
        let cancelled = tokio::time::timeout(Duration::ZERO, job.close()).await;
        assert!(cancelled.is_err(), "the close must not have finished");
        assert_eq!(
            harness.job_dirs().len(),
            1,
            "the cancelled close got nowhere near removing the directory"
        );
        harness.job_dirs()[0].clone()
    };

    assert!(!dir.exists(), "{} outlived its job", dir.display());
    assert!(
        harness
            .invocations()
            .iter()
            .any(|argv| is_master_exit(argv)),
        "Drop still owes the `-O exit` a cancelled close never sent"
    );
}

/// Not closing is not an excuse to leak: `Drop` still exits the master
/// and removes the directory, blocking because it has no runtime left to
/// await on.
#[tokio::test]
async fn dropping_a_job_still_exits_the_master_and_removes_the_directory() {
    let harness = Harness::new();
    {
        let job = harness.job(harness.options()).await;
        job.probe().await.expect("probe");
        assert_eq!(harness.job_dirs().len(), 1);
    }

    assert!(harness.job_dirs().is_empty(), "{:?}", harness.job_dirs());
    let last = harness
        .lines()
        .last()
        .cloned()
        .expect("at least one invocation");
    assert!(is_master_exit(&last[1..]), "{last:?}");
    assert!(last.contains(&"ctl-exists=1".to_string()), "{last:?}");
}

/// A far side that never answers is the case every budget exists for.
/// The exec is killed and reaped and the job reports a classified
/// timeout — the alternative is a UI that hangs on a wedged host with no
/// way back.
#[tokio::test]
async fn a_remote_step_that_never_answers_is_killed_reaped_and_classified() {
    let harness = Harness::new();
    // Longer than the probe's own budget, and bounded so the orphan the
    // kill leaves behind cannot outlive the test run by much.
    harness.hang_uname(45);
    let job = harness.job(harness.options()).await;

    let error = job.probe().await.expect_err("nothing answered");

    match &error {
        BootstrapError::Probe(detail) => {
            assert!(detail.contains("did not finish within"), "{detail}")
        }
        other => panic!("{other:?}"),
    }
    let pids = harness.pids();
    assert!(!pids.is_empty());
    wait_for("the timed-out ssh child to be reaped", || {
        pids.iter().all(|pid| !alive(*pid))
    })
    .await;
    job.close().await;
}

// ============================================================================
// Hermeticity
// ============================================================================

/// The acceptance criterion of plan 039 §3.8, asserted rather than
/// assumed. This test would fail on a Mac if the fixture leaked the real
/// `uname` (the probe would classify `Darwin` as unsupported), and it
/// would fail on a deb-installed Linux box if the jail leaked (a real
/// `/usr/bin/roost-session` would answer, and the candidate list would
/// not be empty).
#[tokio::test]
async fn the_far_side_sees_the_fixture_and_never_this_machine() {
    let harness = Harness::new();
    let job = harness.job(harness.options()).await;

    let probe = job.probe().await.expect("probe");

    assert_eq!(
        probe.arch,
        RemoteArch::Amd64,
        "the arch comes from the fixture's uname, whatever this machine is"
    );
    assert_eq!(
        probe.outcome,
        ProbeOutcome::Missing,
        "an empty jail and a scrubbed PATH mean nothing is installed over there"
    );

    // And the reverse: a binary planted in the jail *is* found at the
    // absolute rung, which is what proves the prefix is what the ladder
    // resolved rather than the real `/usr/bin`.
    harness.plant("/usr/bin/roost-session", &Stub::matching("jailed"));
    let probe = job.probe().await.expect("probe again");
    assert_eq!(
        probe.outcome,
        ProbeOutcome::Compatible {
            path: harness.remote("/usr/bin/roost-session")
        }
    );
    assert!(
        harness
            .remote("/usr/bin/roost-session")
            .starts_with(harness.jail.to_str().expect("a utf-8 jail path")),
        "the rung the probe reported is inside the jail"
    );

    // No candidate may name a path outside the fixture at all — a real
    // `/usr/bin/roost-session` or a `command -v` hit on the developer's
    // own `PATH` would show up here and nowhere else.
    let jail = harness.jail.to_str().expect("a utf-8 jail path");
    let home = harness.home.to_str().expect("a utf-8 home path");
    for candidate in &probe.candidates {
        assert!(
            candidate.starts_with(jail) || candidate.starts_with(home),
            "{candidate} is outside the fixture — this machine leaked into the probe"
        );
    }
    job.close().await;
}

/// The other half of the hermeticity claim, and the half that cannot
/// pass by coincidence.
///
/// `Linux` + `x86_64` is what an ordinary CI runner answers anyway, so
/// a fixture that leaked the real `uname` would still satisfy the test
/// above on such a box. This one asks for `aarch64` — which is wrong on
/// an x86_64 runner and right nowhere but through the fake — and its
/// sibling above asks for `x86_64`, which is wrong on this arm64 Mac.
/// Between them, a leak fails on every host either test runs on.
#[tokio::test]
async fn the_probe_reports_the_fixtures_uname_and_not_this_machines() {
    let harness = Harness::with_uname("Linux", "aarch64");
    let job = harness.job(harness.options()).await;

    let probe = job.probe().await.expect("probe");

    assert_eq!(
        probe.arch,
        RemoteArch::Arm64,
        "the arch came from the fixture, not from this machine"
    );
    job.close().await;
}

/// The whole ladder in one pass: probe a stale install, replace it, and
/// start what landed — the upgrade order plan 039 §3.4 pins, minus the
/// stop it does not need because nothing is running.
#[tokio::test]
async fn a_mismatched_install_is_replaced_and_then_started() {
    let harness = Harness::new();
    harness.plant(
        "$HOME/.local/bin/roost-session",
        &Stub::stale("outgoing").bridge(Bridge::NoSession),
    );
    let replacement = Stub::matching("incoming")
        .start(Start::Ready(9001))
        .bridge(Bridge::serving(None));
    let source = harness.source_file(&replacement);
    let mut options = harness.options();
    options.install_bin = Some(source.clone());
    let job = harness.job(options).await;

    let probe = job.probe().await.expect("probe");
    assert!(
        matches!(probe.outcome, ProbeOutcome::Mismatch { .. }),
        "{:?}",
        probe.outcome
    );

    let resolved: ResolvedSource = job.resolve_source(probe.arch).await.expect("resolve");
    let installed = job.install(&resolved).await.expect("install");
    // Nothing was running, so the action matrix skips the stop and the
    // await-gone that follows it and goes straight to the start.
    let verdict = job.start(&installed.dest).await.expect("start");
    assert_eq!(verdict, Verdict::Ready(9001));
    // An install happened, so the whole triple is required: the point of
    // the job was to make *this* build the one serving.
    job.post_start_identify(IdentityGate::Installed)
        .await
        .expect("the session that came up is this build");

    assert_eq!(
        std::fs::read(harness.dest()).expect("read the installed binary"),
        std::fs::read(&source).expect("read the source"),
        "the stale binary was replaced by the streamed one"
    );
    assert!(harness.staged().is_empty(), "{:?}", harness.staged());
    job.close().await;
}
