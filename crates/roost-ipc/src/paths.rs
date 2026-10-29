//! Bundle profile + path resolution.
//!
//! `roost-ipc` is the canonical home of `BundleProfile`. `roost-common`
//! re-exports the type as a compatibility shim until the daemon
//! goes away in M7. The Swift companion at
//! `mac/Sources/Roost/BundleProfile.swift` mirrors this resolver
//! byte-for-byte **on macOS** — the only OS it runs on — and the two
//! implementations are tested in lockstep there. Off macOS this
//! resolver diverges by design: the Linux profile's app id collapses
//! onto the production `ai.stridelabs.Roost`, matching how its paths
//! already collapse.
//!
//! # Two locks
//!
//! A profile resolves **two** permanent lock paths, because the two
//! things a single instance must own move independently:
//!
//! * [`BundleProfile::socket_lock_path`] — beside the socket, follows
//!   `XDG_RUNTIME_DIR`. Guards the probe→unlink→bind sequence and the
//!   socket's lifetime.
//! * [`BundleProfile::state_lock_path`] — beside `state.json`, follows
//!   `ROOST_STATE_DIR`/`XDG_DATA_HOME`. Guards persistent state.
//!
//! Neither is legacy and neither replaces the other. The original
//! single lock lived beside the socket, which was right about the
//! socket and wrong about state: two processes sharing a `state_dir`
//! but not a runtime dir both started and wrote one `state.json`.
//! Simply moving that lock next to `state.json` would have traded the
//! bug for a worse one — two processes sharing a socket but not a
//! state dir would both bind, the second unlinking the first's socket,
//! and `roostctl` would address whichever bound last.
//!
//! Their **filenames deliberately differ**. `state_dir` can equal the
//! socket's directory (the HOME-less `/tmp/<label>` fallback below, or
//! `ROOST_STATE_DIR` pointed at the runtime dir): with one filename
//! they would be one file, and the second `flock` would contend with
//! the first — `flock` is per-open-file-description, so a process
//! would refuse to start against itself.
//!
//! # The session socket's own knobs
//!
//! The session profile alone answers to [`SESSION_SOCKET_DIR_ENV`] and
//! to two debug-only test knobs, [`TEST_PASSWD_HOME_ENV`] and
//! [`TEST_OLD_SESSION_DIR_ENV`], and it refuses a test-shaped process
//! the default socket path (see `session_socket`). The frozen Swift
//! mirror (`BundleProfile.swift`) honours none of them.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

#[cfg(not(target_os = "macos"))]
use anyhow::Context;

/// Profiles Roost ships or evaluates — the three UI variants plus the
/// headless host-session daemon. On macOS they coexist with distinct
/// paths. On Linux the established Mac/Linux profiles keep sharing the
/// production XDG paths, while the dev Iced profile uses its own
/// namespace so it can run beside a packaged install.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BundleProfileKind {
    /// The Swift `Roost.app`. App id: `ai.stridelabs.Roost`.
    Mac,
    /// The packaged Linux UI (`/usr/bin/roost`). Its app id is
    /// platform-resolved: `ai.stridelabs.Roost` on Linux, shared with
    /// the Mac profile that can never run there and that already
    /// shares the production path namespace; `ai.stridelabs.Roost.linux`
    /// on macOS, where the kind stays resolvable but nothing ships or
    /// launches it.
    Linux,
    /// The Rust + Iced build run from a dev tree (and the experimental
    /// macOS `Roost-Iced.app`). App id: `ai.stridelabs.Roost.iced`.
    /// Always isolated from the production Mac/Linux namespace,
    /// including on Linux.
    Iced,
    /// The headless `roost-session` daemon. App id:
    /// `ai.stridelabs.Roost.session`. Not a UI, and reachable through
    /// `roostctl`'s target ladder only explicitly — `--target session`
    /// or `--socket` (#475); it has no `ROOST_BUNDLE_PROFILE` spelling
    /// and auto-detect never probes it. Its directory names carry a
    /// `-dev`/`Dev` suffix in debug builds so a dev session can never
    /// collide with a real one.
    Session,
}

impl BundleProfileKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Mac => "mac",
            Self::Linux => "linux",
            Self::Iced => "iced",
            Self::Session => "session",
        }
    }
}

/// Test-only: points a **debug** build's session profile at the shipped
/// directory names (`release`) or its own (`dev`), so today's debug UI,
/// the debug session it restarts into, and a debug `roostctl` can all
/// reach a released `roost-session` (plan 072 D13).
const SESSION_DIR_NAMES_ENV: &str = "ROOST_TEST_SESSION_DIR_NAMES";

/// The session profile's `(app_label, linux_namespace)` pair.
///
/// Parameterized rather than a bare `#[cfg]` at the use site so every
/// cell is unit-testable in a single build. Debug builds get their own
/// directories — `roost-session` and a dev session must never share a
/// socket, a `state.json`, or a log (host-sessions architecture §8).
///
/// `requested` is [`SESSION_DIR_NAMES_ENV`]'s value, and only a debug
/// build honours it: a packaged binary keeps the shipped names whatever
/// it inherits. An unknown value is the build's own pair.
fn session_dir_names(debug_build: bool, requested: Option<&str>) -> (&'static str, &'static str) {
    if debug_build && requested != Some("release") {
        ("RoostSessionDev", "roost-session-dev")
    } else {
        ("RoostSession", "roost-session")
    }
}

/// This build's session directory names, override included.
fn build_session_dir_names() -> (&'static str, &'static str) {
    session_dir_names(
        cfg!(debug_assertions),
        std::env::var(SESSION_DIR_NAMES_ENV).ok().as_deref(),
    )
}

/// Names the session profile's socket **directory** itself: the socket is
/// `<dir>/roost.sock` and its lock `<dir>/roost.lock`, with no namespace
/// appended, so a debug and a release process handed one value share one
/// socket and one lock — the caller owns that isolation. Empty is unset;
/// a relative value is an error. Honoured on every platform, and by the
/// session profile only.
///
/// A cross-process contract like [`STATE_DIR_ENV`]: a launcher's spawned
/// session inherits it unchanged, and the test harness sets it on every
/// process that can reach a session. Frozen by a test.
pub const SESSION_SOCKET_DIR_ENV: &str = "ROOST_SESSION_SOCKET_DIR";

/// Test-only, and honoured by **debug** builds only, exactly like
/// [`SESSION_DIR_NAMES_ENV`]: an absolute directory standing in for this
/// user's passwd home. It moves no path; what it does is let a
/// test-shaped process resolve the session socket's default path at all
/// (see `session_socket`). Frozen by a test.
pub const TEST_PASSWD_HOME_ENV: &str = "ROOST_TEST_PASSWD_HOME";

/// Test-only, debug builds only: the one absolute directory whose
/// `roost.sock` a debug build may probe for a session on an old path,
/// instead of any real candidate. Frozen by a test.
pub const TEST_OLD_SESSION_DIR_ENV: &str = "ROOST_TEST_OLD_SESSION_DIR";

/// [`SESSION_SOCKET_DIR_ENV`]'s rule. Silent for unset and empty, like
/// `ROOST_STATE_DIR` and the XDG dirs; loud for a relative value, which
/// would otherwise bind a session socket against the process CWD.
fn session_socket_dir(raw: Option<&OsStr>) -> anyhow::Result<Option<PathBuf>> {
    let Some(raw) = raw.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let dir = PathBuf::from(raw);
    if !dir.is_absolute() {
        anyhow::bail!("{SESSION_SOCKET_DIR_ENV} must be an absolute path (got {raw:?})");
    }
    Ok(Some(dir))
}

/// A debug-only test knob's value: an absolute path in a debug build,
/// nothing otherwise. A value this drops reads as unset, which is safe
/// for both knobs because each only ever *widens* what a test-shaped
/// process may reach — dropped, it fails closed.
fn debug_test_path(debug_build: bool, raw: Option<&OsStr>) -> Option<PathBuf> {
    if !debug_build {
        return None;
    }
    let path = PathBuf::from(raw?);
    path.is_absolute().then_some(path)
}

/// One environment lookup. The readers below take one rather than call
/// `std::env` themselves, so a test can hand them a map keyed by the real
/// names and catch a reader that asks for the wrong one.
type GetEnv<'a> = &'a dyn Fn(&str) -> Option<std::ffi::OsString>;

fn process_env(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name)
}

/// [`TEST_OLD_SESSION_DIR_ENV`] as this build reads it.
pub fn test_old_session_dir() -> Option<PathBuf> {
    test_old_session_dir_from(&process_env)
}

fn test_old_session_dir_from(getenv: GetEnv) -> Option<PathBuf> {
    debug_test_path(
        cfg!(debug_assertions),
        getenv(TEST_OLD_SESSION_DIR_ENV).as_deref(),
    )
}

/// The shapes of process the session socket's default path is refused to
/// (`session_socket`), read once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SessionTestShape {
    /// `ROOST_TEST_MODE=1`, whatever the build profile.
    test_mode: bool,
    /// A debug build that [`SESSION_DIR_NAMES_ENV`] pointed at the
    /// shipped session's directory names.
    shipped_names_in_debug: bool,
}

impl SessionTestShape {
    fn new(debug_build: bool, test_mode: Option<&OsStr>, dir_names: Option<&str>) -> Self {
        Self {
            test_mode: test_mode.is_some_and(|value| value == "1"),
            shipped_names_in_debug: debug_build
                && session_dir_names(true, dir_names) == session_dir_names(false, None),
        }
    }

    fn from_env(getenv: GetEnv) -> Self {
        let dir_names = getenv(SESSION_DIR_NAMES_ENV).and_then(|value| value.into_string().ok());
        Self::new(
            cfg!(debug_assertions),
            getenv("ROOST_TEST_MODE").as_deref(),
            dir_names.as_deref(),
        )
    }
}

/// The macOS session socket: no passwd home there (macOS keeps `$HOME`),
/// so the knob has nothing to stand in for and must not let a
/// test-shaped process past.
#[cfg(any(target_os = "macos", test))]
fn mac_session_socket(default: PathBuf, getenv: GetEnv) -> anyhow::Result<PathBuf> {
    session_socket(
        default,
        getenv(SESSION_SOCKET_DIR_ENV).as_deref(),
        None,
        SessionTestShape::from_env(getenv),
    )
}

/// The session socket: `<override>/roost.sock` when
/// [`SESSION_SOCKET_DIR_ENV`] is set, else `default` — which a
/// test-shaped process may only reach with a scratch passwd home in
/// hand.
///
/// Fail closed, not warn: a test-mode process (either build profile), or
/// a debug build pointed at the shipped names, that resolved the default
/// would be addressing this user's real session — the one a developer
/// runs the suite from inside of.
fn session_socket(
    default: PathBuf,
    socket_dir: Option<&OsStr>,
    passwd_home: Option<&Path>,
    shape: SessionTestShape,
) -> anyhow::Result<PathBuf> {
    if let Some(dir) = session_socket_dir(socket_dir)? {
        return Ok(dir.join("roost.sock"));
    }
    if passwd_home.is_none() {
        if shape.test_mode {
            anyhow::bail!(
                "ROOST_TEST_MODE=1 and {SESSION_SOCKET_DIR_ENV} is unset: a test-mode process \
                 must name its own session socket directory (or, in a debug build, \
                 {TEST_PASSWD_HOME_ENV}) rather than reach this user's session at {}",
                default.display()
            );
        }
        if shape.shipped_names_in_debug {
            anyhow::bail!(
                "{SESSION_DIR_NAMES_ENV}=release points this debug build at the shipped \
                 session's directories: set {SESSION_SOCKET_DIR_ENV} (or \
                 {TEST_PASSWD_HOME_ENV}) so it cannot reach this user's session at {}",
                default.display()
            );
        }
    }
    Ok(default)
}

/// Resolved paths for one bundle profile.
///
/// `socket_path` is the Unix-domain-socket path the UI binds and any
/// CLI dials. `state_dir` is the directory containing persistent
/// state (`state.json` post-M3; the legacy `roost.db` pre-M7).
/// `log_dir` is the directory containing `roost.log`.
#[derive(Clone, Debug)]
pub struct BundleProfile {
    pub kind: BundleProfileKind,
    /// Human-readable label used in path components on macOS, and
    /// reported by `identify` / `roostctl doctor` on every platform.
    /// `"Roost"` for Mac, `"Roost-linux"` for Linux, `"Roost-iced"` for
    /// Iced, and `"RoostSession"` (`"RoostSessionDev"` in debug builds)
    /// for Session. Linux itself uses the established `roost/` namespace
    /// for Mac and Linux, the isolated `roost-iced/` namespace for Iced,
    /// and `roost-session/` (`roost-session-dev/` in debug builds) for
    /// Session.
    pub app_label: &'static str,
    /// Reverse-DNS application identifier (`CFBundleIdentifier` on
    /// macOS, the desktop-entry id on Linux). Stable per kind except
    /// for Linux, which resolves per platform — `ai.stridelabs.Roost`
    /// on Linux (shared with the Mac profile, which never runs there
    /// and already shares the production path namespace) and
    /// `ai.stridelabs.Roost.linux` on macOS (where the side-by-side
    /// profiles must stay distinct).
    pub app_id: &'static str,
    pub socket_path: PathBuf,
    pub state_dir: PathBuf,
    pub log_dir: PathBuf,
}

impl BundleProfile {
    /// Resolve a profile by kind.
    pub fn for_kind(kind: BundleProfileKind) -> anyhow::Result<BundleProfile> {
        let (app_label, app_id) = match kind {
            BundleProfileKind::Mac => ("Roost", "ai.stridelabs.Roost"),
            BundleProfileKind::Linux => (
                "Roost-linux",
                if cfg!(target_os = "macos") {
                    "ai.stridelabs.Roost.linux"
                } else {
                    "ai.stridelabs.Roost"
                },
            ),
            BundleProfileKind::Iced => ("Roost-iced", "ai.stridelabs.Roost.iced"),
            BundleProfileKind::Session => {
                (build_session_dir_names().0, "ai.stridelabs.Roost.session")
            }
        };
        let (socket_path, state_dir, log_dir) = resolve_paths(kind, app_label)?;
        // Redirect ONLY the state dir when `ROOST_STATE_DIR` is set, so
        // tests (and side-by-side instances) get an isolated `state.json`
        // while the socket/lock/log stay on the default profile path — the
        // CLI + harness find the UI by the unchanged socket. See
        // `apply_state_dir_override` for the strict (absolute) policy.
        let state_dir = apply_state_dir_override(state_dir, std::env::var_os(STATE_DIR_ENV));
        Ok(BundleProfile {
            kind,
            app_label,
            app_id,
            socket_path,
            state_dir,
            log_dir,
        })
    }

    pub fn mac() -> anyhow::Result<BundleProfile> {
        Self::for_kind(BundleProfileKind::Mac)
    }

    pub fn linux() -> anyhow::Result<BundleProfile> {
        Self::for_kind(BundleProfileKind::Linux)
    }

    pub fn iced() -> anyhow::Result<BundleProfile> {
        Self::for_kind(BundleProfileKind::Iced)
    }

    pub fn session() -> anyhow::Result<BundleProfile> {
        Self::for_kind(BundleProfileKind::Session)
    }

    /// Pick a profile with `ROOST_BUNDLE_PROFILE` overriding the
    /// caller's default. Unknown values fall through to the default
    /// (with a warn) rather than failing — see
    /// [`kind_from_profile_env`]. The CLI's own parser
    /// (`target.rs`) deliberately hard-errors instead.
    pub fn resolve(default: BundleProfileKind) -> anyhow::Result<BundleProfile> {
        let value = std::env::var("ROOST_BUNDLE_PROFILE").ok();
        Self::for_kind(kind_from_profile_env(default, value.as_deref()))
    }

    /// SQLite database path inside `state_dir`. Daemon-only; deleted
    /// in M7 along with the daemon.
    pub fn db_path(&self) -> PathBuf {
        self.state_dir.join("roost.db")
    }

    /// `state.json` path inside `state_dir`. Introduced in M3.
    pub fn state_json_path(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }

    /// `roost.log` path inside `log_dir`.
    pub fn log_path(&self) -> PathBuf {
        self.log_dir.join("roost.log")
    }

    /// Where a host session stores files a client uploaded for one of
    /// its tabs (plan 047 §3.1). **Cache semantics**, honestly: the
    /// contents are reproducible from the client, disposable, and swept
    /// at every start and at every clean stop.
    ///
    /// macOS `~/Library/Caches/<label>/files`; Linux
    /// `$XDG_CACHE_HOME/<ns>/files`, default `~/.cache/<ns>/files`.
    /// Deliberately **not** the Mac state dir, whose path contains a
    /// space and so could never be pasted bare; and **not**
    /// `$XDG_RUNTIME_DIR`, which is tmpfs.
    ///
    /// When `ROOST_STATE_DIR` is honoured it is `<state_dir>/files`
    /// instead — the same condition [`apply_state_dir_override`]
    /// applies, so a spawned dev session's sweep-on-start stays inside
    /// the launcher's isolated tree rather than emptying the
    /// developer's real cache.
    pub fn files_dir(&self) -> anyhow::Result<PathBuf> {
        self.files_dir_with(std::env::var_os(STATE_DIR_ENV).as_deref())
    }

    /// [`Self::files_dir`] with the seam's value passed in, so the
    /// policy is testable without mutating process-global env — the
    /// same split [`apply_state_dir_override`] uses.
    fn files_dir_with(&self, state_dir_env: Option<&std::ffi::OsStr>) -> anyhow::Result<PathBuf> {
        if honoured_state_dir(state_dir_env).is_some() {
            return Ok(self.state_dir.join("files"));
        }
        Ok(resolve_cache_dir(self.kind, self.app_label)?.join("files"))
    }

    /// Where the file store goes when [`Self::files_dir`] resolves to a
    /// path that cannot be pasted bare (a space in `$HOME`, say).
    ///
    /// The socket's own `/tmp` fallback shape, per profile — never a
    /// single shared directory, because a debug and a release session on
    /// the same box would then sweep each other's live uploads at start.
    /// Every byte is in `[A-Za-z0-9._/-]` by construction.
    #[must_use]
    pub fn files_dir_fallback(&self) -> PathBuf {
        files_dir_fallback(self.kind, self.app_label)
    }

    /// flock guarding the IPC socket: the probe→unlink→bind sequence
    /// and the bound socket's lifetime. Lives next to the socket, so
    /// it follows `XDG_RUNTIME_DIR` (the session's, its
    /// [`SESSION_SOCKET_DIR_ENV`]) and a `ROOST_STATE_DIR` override
    /// never moves it.
    ///
    /// The filename stays `roost.lock` — it is the path external
    /// tooling (`tools/roosttest`, `docs/reference/paths.md`, muscle
    /// memory) already knows, and keeping it means a new binary still
    /// contends with an older one **when the runtime path agrees**.
    /// It cannot contend with an old binary whose `XDG_RUNTIME_DIR`
    /// differs, because it has no way to discover that path.
    pub fn socket_lock_path(&self) -> PathBuf {
        // `socket_path` always lives in a directory we control.
        match self.socket_path.parent() {
            Some(parent) => parent.join("roost.lock"),
            // Should never happen: BundleProfile::for_kind always
            // joins at least one component onto an absolute root.
            // Fall back to a leaf-style filename to avoid a panic.
            None => PathBuf::from("roost.lock"),
        }
    }

    /// flock guarding persistent state. Lives next to `state.json`, so
    /// it follows `ROOST_STATE_DIR` — two UIs pointed at one state dir
    /// contend even when their runtime dirs differ.
    ///
    /// Named differently from [`Self::socket_lock_path`] on purpose:
    /// see the module docs on why one shared filename would make a
    /// process refuse to start against itself.
    ///
    /// The trade-off this locks in: `ROOST_STATE_DIR`-isolated sessions
    /// each get their own state lock, so collisions that used to be
    /// loud go silent. That is what isolation means; the socket lock is
    /// what still catches a genuine double instance on one socket.
    pub fn state_lock_path(&self) -> PathBuf {
        self.state_dir.join("state.lock")
    }
}

/// Map `ROOST_BUNDLE_PROFILE` onto a kind, keeping the UI-side policy:
/// an unrecognized value falls through to the caller's default rather
/// than refusing to launch. It warns first — a stale value (a `gtk`
/// left over from before the profile was renamed to `linux`) silently
/// changes which namespace the process owns, and that has to be
/// observable in the log.
///
/// `session` is deliberately absent: it is not a UI, so pointing a UI
/// process at the session namespace is always a mistake. HS-1 defines
/// how a session gets addressed.
fn kind_from_profile_env(default: BundleProfileKind, value: Option<&str>) -> BundleProfileKind {
    match value.map(str::trim) {
        Some("mac") => BundleProfileKind::Mac,
        Some("linux") => BundleProfileKind::Linux,
        Some("iced") => BundleProfileKind::Iced,
        // Empty (including the launchd-inherited empty-env case) is not
        // a mistake — only a non-empty value the parser doesn't know is.
        Some(other) if !other.is_empty() => {
            tracing::warn!(
                value = other,
                default = default.as_str(),
                "unrecognized ROOST_BUNDLE_PROFILE; falling back to the default profile"
            );
            default
        }
        _ => default,
    }
}

/// The raw `ROOST_BUNDLE_PROFILE` value when it is set, non-empty, and
/// not a recognized profile slug. The UI resolves its profile *before*
/// it can initialize logging (the log directory comes from the
/// profile), so the warn inside [`kind_from_profile_env`] is discarded
/// at the moment it matters most — the UI calls this again once its
/// subscriber is installed and logs the stale value itself.
pub fn unrecognized_profile_env() -> Option<String> {
    let raw = std::env::var("ROOST_BUNDLE_PROFILE").ok()?;
    let value = raw.trim();
    match value {
        "" | "mac" | "linux" | "iced" => None,
        other => Some(other.to_string()),
    }
}

/// The state-dir isolation seam.
///
/// A cross-process contract, not just a local read: every profile
/// resolution here honours it, and
/// [`crate::session_launch::spawn_and_read_verdict`] *writes* a derived
/// value onto the `roost-session` it spawns. Frozen by a test.
pub const STATE_DIR_ENV: &str = "ROOST_STATE_DIR";

/// The one rule for a `ROOST_STATE_DIR` value: a non-empty, absolute
/// path is honoured, anything else is not.
///
/// Silent — the warn belongs to [`apply_state_dir_override`], which is
/// where a user's ignored value actually costs them something. Shared
/// so [`derived_session_state_dir`] cannot drift from the resolver it
/// has to agree with.
fn honoured_state_dir(raw: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let raw = raw.filter(|v| !v.is_empty())?;
    let path = PathBuf::from(raw);
    path.is_absolute().then_some(path)
}

/// Apply a `ROOST_STATE_DIR` override to the resolved state dir. The env
/// value is passed in (not read here) so the policy is unit-testable
/// without mutating process-global env. Redirects **only** the state dir;
/// the caller leaves socket/lock/log untouched.
///
/// Validation follows the strict `valid_home`/XDG style (**absolute** +
/// non-empty), NOT the permissive `ROOST_CONFIG` policy: a relative state
/// dir would resolve against the process CWD — nondeterministic, and a
/// likely way to scribble state somewhere unexpected. A set-but-invalid
/// value (non-empty + non-absolute) is ignored with a warn; empty/unset
/// falls back silently (mirrors the HOME handling in `resolve_paths`).
/// Existence isn't checked — like the default dir, it's created on first
/// write. KEEP IN SYNC with `BundleProfile.swift`'s override.
fn apply_state_dir_override(default: PathBuf, raw: Option<std::ffi::OsString>) -> PathBuf {
    let Some(raw) = raw.filter(|v| !v.is_empty()) else {
        return default;
    };
    match honoured_state_dir(Some(&raw)) {
        Some(path) => path,
        None => {
            tracing::warn!(
                value = ?raw,
                "ROOST_STATE_DIR ignored: not an absolute path; using default state dir"
            );
            default
        }
    }
}

/// The state dir a **spawned** `roost-session` gets when its launcher is
/// itself running under the seam: `Some(<value>/session)` for exactly
/// the values [`apply_state_dir_override`] honours, `None` otherwise.
///
/// A daemon that simply inherited the value would resolve the
/// *launcher's* state dir, find the launcher's `state.lock` held, and
/// refuse to start — the seam colliding with itself
/// ([#397](https://github.com/charliek/roost/issues/397)). Deriving a
/// directory **inside** the launcher's own state dir inherits the
/// isolation instead of the collision, and the daemon stays addressable
/// because the socket never moves with this variable. Nested, not
/// beside: whoever wipes the launcher's state dir wipes the session's
/// with it, so anything that clears one has to account for the other.
///
/// Pure (the value is a parameter, not a read) for the same reason
/// `apply_state_dir_override` is: the policy is testable without
/// mutating process-global env. Borrowed rather than owned, as
/// [`crate::session_launch::locate_session_binary`]'s env parameters
/// are — clippy refuses an `Option<OsString>` this never consumes.
pub fn derived_session_state_dir(raw: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    honoured_state_dir(raw).map(|dir| dir.join("session"))
}

#[cfg(target_os = "macos")]
fn resolve_paths(
    kind: BundleProfileKind,
    app_label: &str,
) -> anyhow::Result<(PathBuf, PathBuf, PathBuf)> {
    resolve_paths_mac(kind, app_label, &process_env)
}

/// The macOS path contract, over an injected environment so its literal
/// output is pinnable whatever the test process itself carries.
#[cfg(target_os = "macos")]
fn resolve_paths_mac(
    kind: BundleProfileKind,
    app_label: &str,
    getenv: GetEnv,
) -> anyhow::Result<(PathBuf, PathBuf, PathBuf)> {
    let (socket, state, log) = mac_default_paths(app_label, getenv("HOME").as_deref());
    if kind != BundleProfileKind::Session {
        return Ok((socket, state, log));
    }
    Ok((mac_session_socket(socket, getenv)?, state, log))
}

#[cfg(target_os = "macos")]
fn mac_default_paths(app_label: &str, home: Option<&OsStr>) -> (PathBuf, PathBuf, PathBuf) {
    // Sandboxed launchd processes can inherit `HOME=""` (set but
    // empty) or no HOME at all. Mirror the Swift companion's
    // `/tmp/<appLabel>/...` fallback rather than erroring — the
    // alternative is the process refusing to launch at all in that
    // environment. The two implementations are tested against each
    // other to stay in lockstep.
    if let Some(raw) = home {
        let home = PathBuf::from(raw);
        if !home.as_os_str().is_empty() && home.is_absolute() {
            let socket = home
                .join("Library/Caches")
                .join(app_label)
                .join("roost.sock");
            let state = home.join("Library/Application Support").join(app_label);
            let log = home.join("Library/Logs").join(app_label);
            return (socket, state, log);
        }
    }
    let tmp = PathBuf::from("/tmp").join(app_label);
    (tmp.join("roost.sock"), tmp.clone(), tmp)
}

#[cfg(target_os = "macos")]
fn files_dir_fallback(_kind: BundleProfileKind, app_label: &str) -> PathBuf {
    PathBuf::from("/tmp").join(app_label).join("files")
}

#[cfg(target_os = "macos")]
fn resolve_cache_dir(_kind: BundleProfileKind, app_label: &str) -> anyhow::Result<PathBuf> {
    Ok(resolve_cache_dir_mac(
        app_label,
        std::env::var_os("HOME").as_deref(),
    ))
}

/// The Mac cache root: the same directory the socket already lives in,
/// with the same HOME-less `/tmp/<label>` fallback `resolve_paths`
/// applies — a sandboxed launchd process can inherit `HOME=""`.
#[cfg(target_os = "macos")]
fn resolve_cache_dir_mac(app_label: &str, home: Option<&std::ffi::OsStr>) -> PathBuf {
    if let Some(raw) = home {
        let home = PathBuf::from(raw);
        if !home.as_os_str().is_empty() && home.is_absolute() {
            return home.join("Library/Caches").join(app_label);
        }
    }
    PathBuf::from("/tmp").join(app_label)
}

/// The environment Linux path resolution reads, captured once so the
/// resolver below can be pure. Same philosophy as
/// [`apply_state_dir_override`]: the policy is unit-testable — and its
/// literal output pinnable — without a test mutating process-global env
/// that every other test in the binary also reads.
#[cfg(not(target_os = "macos"))]
#[derive(Debug, Default, Clone)]
struct LinuxPathEnv {
    home: Option<std::ffi::OsString>,
    runtime_dir: Option<std::ffi::OsString>,
    data_home: Option<std::ffi::OsString>,
    state_home: Option<std::ffi::OsString>,
    cache_home: Option<std::ffi::OsString>,
    /// [`SESSION_SOCKET_DIR_ENV`], raw: validated where the session
    /// socket is resolved, so a bad value fails the session profile and
    /// nothing else.
    session_socket_dir: Option<std::ffi::OsString>,
    /// The home the session socket's default path is resolved for. Only
    /// [`TEST_PASSWD_HOME_ENV`] fills it today, and it moves no path.
    passwd_home: Option<PathBuf>,
    session_test_shape: SessionTestShape,
}

#[cfg(not(target_os = "macos"))]
impl LinuxPathEnv {
    fn from_process() -> Self {
        Self::from_env(&process_env)
    }

    fn from_env(getenv: GetEnv) -> Self {
        Self {
            home: getenv("HOME"),
            runtime_dir: getenv("XDG_RUNTIME_DIR"),
            data_home: getenv("XDG_DATA_HOME"),
            state_home: getenv("XDG_STATE_HOME"),
            cache_home: getenv("XDG_CACHE_HOME"),
            session_socket_dir: getenv(SESSION_SOCKET_DIR_ENV),
            passwd_home: debug_test_path(
                cfg!(debug_assertions),
                getenv(TEST_PASSWD_HOME_ENV).as_deref(),
            ),
            session_test_shape: SessionTestShape::from_env(getenv),
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn resolve_paths(
    kind: BundleProfileKind,
    _app_label: &str,
) -> anyhow::Result<(PathBuf, PathBuf, PathBuf)> {
    resolve_paths_linux(kind, &LinuxPathEnv::from_process(), libc_getuid())
}

/// The Linux path contract. **Byte-stable**: these are the paths an
/// installed Roost already owns, so a rename of the profile kind must
/// never move them. Pinned by literal-string golden tests below.
#[cfg(not(target_os = "macos"))]
fn resolve_paths_linux(
    kind: BundleProfileKind,
    env: &LinuxPathEnv,
    uid: u32,
) -> anyhow::Result<(PathBuf, PathBuf, PathBuf)> {
    let namespace = linux_namespace(kind);
    let socket = match valid_dir(env.runtime_dir.as_deref()) {
        Some(dir) => dir.join(namespace).join("roost.sock"),
        None => PathBuf::from(format!("/tmp/{namespace}-{uid}")).join("roost.sock"),
    };
    let socket = if kind == BundleProfileKind::Session {
        session_socket(
            socket,
            env.session_socket_dir.as_deref(),
            env.passwd_home.as_deref(),
            env.session_test_shape,
        )?
    } else {
        socket
    };
    let state = match valid_dir(env.data_home.as_deref()) {
        Some(dir) => dir.join(namespace),
        None => valid_home(env.home.as_deref())?
            .join(".local/share")
            .join(namespace),
    };
    let log = match valid_dir(env.state_home.as_deref()) {
        Some(dir) => dir.join(namespace),
        None => valid_home(env.home.as_deref())?
            .join(".local/state")
            .join(namespace),
    };
    Ok((socket, state, log))
}

/// The XDG directory name every Linux path for a profile hangs off.
#[cfg(not(target_os = "macos"))]
fn linux_namespace(kind: BundleProfileKind) -> &'static str {
    match kind {
        BundleProfileKind::Mac | BundleProfileKind::Linux => "roost",
        BundleProfileKind::Iced => "roost-iced",
        BundleProfileKind::Session => build_session_dir_names().1,
    }
}

#[cfg(not(target_os = "macos"))]
fn files_dir_fallback(kind: BundleProfileKind, _app_label: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/{}-{}", linux_namespace(kind), libc_getuid())).join("files")
}

#[cfg(not(target_os = "macos"))]
fn resolve_cache_dir(kind: BundleProfileKind, _app_label: &str) -> anyhow::Result<PathBuf> {
    resolve_cache_dir_linux(kind, &LinuxPathEnv::from_process())
}

/// The Linux cache root, the fourth XDG base dir this resolver reads.
/// Same rule as the other three: a set-but-unusable `XDG_CACHE_HOME` is
/// ignored, and `$HOME` is only required when it is what's left.
#[cfg(not(target_os = "macos"))]
fn resolve_cache_dir_linux(kind: BundleProfileKind, env: &LinuxPathEnv) -> anyhow::Result<PathBuf> {
    let namespace = linux_namespace(kind);
    Ok(match valid_dir(env.cache_home.as_deref()) {
        Some(dir) => dir.join(namespace),
        None => valid_home(env.home.as_deref())?
            .join(".cache")
            .join(namespace),
    })
}

/// Validate `$HOME` as non-empty and absolute. The raw env value would
/// silently be `""` or a relative path in a misconfigured launchd /
/// container env, producing unusable paths like `.local/share/roost`
/// (no leading slash).
#[cfg(not(target_os = "macos"))]
fn valid_home(raw: Option<&std::ffi::OsStr>) -> anyhow::Result<PathBuf> {
    let raw = raw.context("$HOME not set")?;
    let p = PathBuf::from(raw);
    if p.as_os_str().is_empty() || !p.is_absolute() {
        anyhow::bail!("$HOME is not an absolute non-empty path (got {:?})", raw);
    }
    Ok(p)
}

/// An XDG base dir counts only when it is set, non-empty and absolute —
/// the spec's own rule, and what keeps a stray relative value from
/// scattering state against the process CWD.
#[cfg(not(target_os = "macos"))]
fn valid_dir(raw: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let p = PathBuf::from(raw?);
    (!p.as_os_str().is_empty() && p.is_absolute()).then_some(p)
}

/// The **real** uid, deliberately not the effective one
/// ([`crate::current_euid`]): this names the `/tmp/<ns>-<uid>` fallback
/// namespace, whose bytes are pinned by the golden tests below.
#[cfg(not(target_os = "macos"))]
fn libc_getuid() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: `getuid` reads process-global state, takes no
        // arguments and cannot fail; there is no unsafe precondition to
        // uphold.
        unsafe { libc::getuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_ids_are_stable() {
        let mac = BundleProfile::mac().expect("mac profile");
        let linux = BundleProfile::linux().expect("linux profile");
        let iced = BundleProfile::iced().expect("iced profile");
        assert_eq!(mac.app_id, "ai.stridelabs.Roost");
        #[cfg(target_os = "macos")]
        assert_eq!(linux.app_id, "ai.stridelabs.Roost.linux");
        #[cfg(not(target_os = "macos"))]
        assert_eq!(linux.app_id, "ai.stridelabs.Roost");
        assert_eq!(iced.app_id, "ai.stridelabs.Roost.iced");
        assert_eq!(mac.app_label, "Roost");
        // `'static` per kind, so this is the label `identify` and
        // `roostctl doctor` report on Linux too.
        assert_eq!(linux.app_label, "Roost-linux");
        assert_eq!(iced.app_label, "Roost-iced");

        let session = BundleProfile::session().expect("session profile");
        assert_eq!(session.app_id, "ai.stridelabs.Roost.session");
        assert_eq!(session.app_label, build_session_dir_names().0);
    }

    #[test]
    fn session_dir_names_split_dev_from_prod() {
        assert_eq!(
            session_dir_names(false, None),
            ("RoostSession", "roost-session"),
            "the shipped session profile owns these exact directory names"
        );
        assert_eq!(
            session_dir_names(true, None),
            ("RoostSessionDev", "roost-session-dev"),
            "a debug session must never land in the shipped session's directories"
        );
    }

    #[test]
    fn the_dir_names_override_moves_a_debug_build_and_never_a_packaged_one() {
        const SHIPPED: (&str, &str) = ("RoostSession", "roost-session");
        const DEV: (&str, &str) = ("RoostSessionDev", "roost-session-dev");
        for (debug_build, requested, want) in [
            (true, Some("release"), SHIPPED),
            (true, Some("dev"), DEV),
            (true, Some("Release"), DEV),
            (false, Some("dev"), SHIPPED),
            (false, Some("release"), SHIPPED),
        ] {
            assert_eq!(
                session_dir_names(debug_build, requested),
                want,
                "debug_build={debug_build} requested={requested:?}"
            );
        }
        assert_eq!(SESSION_DIR_NAMES_ENV, "ROOST_TEST_SESSION_DIR_NAMES");
    }

    /// Cross-process contracts: a launcher's spawned session inherits the
    /// override, the harness sets all three on the processes it starts,
    /// and `tools/roosttest/ui.py` spells them again.
    #[test]
    fn the_session_socket_env_names_are_frozen() {
        assert_eq!(SESSION_SOCKET_DIR_ENV, "ROOST_SESSION_SOCKET_DIR");
        assert_eq!(TEST_PASSWD_HOME_ENV, "ROOST_TEST_PASSWD_HOME");
        assert_eq!(TEST_OLD_SESSION_DIR_ENV, "ROOST_TEST_OLD_SESSION_DIR");
    }

    const SAFE_DEFAULT: &str = "/run/user/1000/roost-session-dev/roost.sock";

    /// `session_socket` the way a process resolves it: the passwd knob
    /// reduced to what this build honours, the shape read off the same
    /// raw values the process would have.
    fn resolve_session_socket(
        debug_build: bool,
        test_mode: Option<&str>,
        dir_names: Option<&str>,
        socket_dir: Option<&str>,
        passwd_home: Option<&str>,
    ) -> anyhow::Result<PathBuf> {
        let passwd_home = debug_test_path(debug_build, passwd_home.map(OsStr::new));
        session_socket(
            PathBuf::from(SAFE_DEFAULT),
            socket_dir.map(OsStr::new),
            passwd_home.as_deref(),
            SessionTestShape::new(debug_build, test_mode.map(OsStr::new), dir_names),
        )
    }

    #[test]
    fn the_override_names_the_socket_directory_itself() {
        let socket = resolve_session_socket(true, None, None, Some("/tmp/roost-ss"), None)
            .expect("an absolute override resolves");
        assert_eq!(socket, PathBuf::from("/tmp/roost-ss/roost.sock"));
    }

    /// No namespace is appended, so the debug and the shipped directory
    /// names under one override land on one socket — the property that
    /// lets a lane hand a debug UI and a released daemon the same value.
    #[test]
    fn debug_and_release_names_share_one_socket_under_one_override() {
        let override_dir = Some(std::ffi::OsStr::new("/tmp/roost-ss"));
        let resolved: Vec<PathBuf> = [
            session_dir_names(true, None),
            session_dir_names(false, None),
        ]
        .into_iter()
        .map(|(_, namespace)| {
            let default = PathBuf::from(format!("/run/user/1000/{namespace}/roost.sock"));
            session_socket(default, override_dir, None, SessionTestShape::default())
                .expect("resolve")
        })
        .collect();
        assert_eq!(resolved[0], resolved[1]);
        assert_eq!(resolved[0], PathBuf::from("/tmp/roost-ss/roost.sock"));
    }

    #[test]
    fn an_empty_override_is_unset_and_a_relative_one_is_an_error() {
        assert_eq!(
            resolve_session_socket(true, None, None, Some(""), None).expect("empty is unset"),
            PathBuf::from(SAFE_DEFAULT)
        );
        let error = resolve_session_socket(true, None, None, Some("relative/dir"), None)
            .expect_err("a relative override is refused");
        assert!(
            format!("{error:#}").contains(SESSION_SOCKET_DIR_ENV),
            "{error:#}"
        );
    }

    /// Plan 083 D-A6's two fail-closed guards over every knob they read:
    /// `(debug build, ROOST_TEST_MODE, DIR_NAMES, override, passwd knob)`.
    /// The scratch passwd home is honoured by a debug build only, so the
    /// release rows that carry one still refuse.
    #[test]
    fn a_test_shaped_process_never_reaches_the_default_session_socket() {
        type Knobs = (
            bool,
            Option<&'static str>,
            Option<&'static str>,
            Option<&'static str>,
            Option<&'static str>,
        );
        const TEST: Option<&str> = Some("1");
        const SHIPPED: Option<&str> = Some("release");
        const OVR: Option<&str> = Some("/tmp/roost-ss");
        const HOME: Option<&str> = Some("/tmp/scratch-home");
        const BAD_HOME: Option<&str> = Some("relative-home");
        let resolve = |(debug, test_mode, names, socket_dir, home): Knobs| {
            resolve_session_socket(debug, test_mode, names, socket_dir, home)
        };

        let to_the_override: [Knobs; 4] = [
            (true, TEST, None, OVR, None),
            (false, TEST, None, OVR, None),
            (true, None, SHIPPED, OVR, None),
            (true, TEST, SHIPPED, OVR, HOME),
        ];
        for knobs in to_the_override {
            let socket = resolve(knobs).unwrap_or_else(|e| panic!("{knobs:?}: {e:#}"));
            assert_eq!(
                socket,
                PathBuf::from("/tmp/roost-ss/roost.sock"),
                "{knobs:?}"
            );
        }

        let to_the_default: [Knobs; 8] = [
            (true, TEST, None, None, HOME),
            (true, None, SHIPPED, None, HOME),
            (false, None, SHIPPED, None, HOME),
            (false, None, SHIPPED, None, None),
            (true, None, Some("dev"), None, None),
            (true, Some("0"), None, None, None),
            (true, None, None, None, None),
            (false, None, None, None, None),
        ];
        for knobs in to_the_default {
            let socket = resolve(knobs).unwrap_or_else(|e| panic!("{knobs:?}: {e:#}"));
            assert_eq!(socket, PathBuf::from(SAFE_DEFAULT), "{knobs:?}");
        }

        let refused: [(Knobs, &str); 7] = [
            ((true, TEST, None, None, None), "ROOST_TEST_MODE=1"),
            ((false, TEST, None, None, None), "ROOST_TEST_MODE=1"),
            ((false, TEST, None, None, HOME), "ROOST_TEST_MODE=1"),
            ((true, TEST, None, None, BAD_HOME), "ROOST_TEST_MODE=1"),
            ((true, TEST, SHIPPED, None, None), "ROOST_TEST_MODE=1"),
            ((true, None, SHIPPED, None, None), SESSION_DIR_NAMES_ENV),
            ((true, None, SHIPPED, None, BAD_HOME), SESSION_DIR_NAMES_ENV),
        ];
        for (knobs, reason) in refused {
            let Err(error) = resolve(knobs) else {
                panic!("{knobs:?} must be refused");
            };
            let text = format!("{error:#}");
            assert!(text.contains(reason), "{knobs:?}: {text}");
            assert!(text.contains(SESSION_SOCKET_DIR_ENV), "{knobs:?}: {text}");
        }
    }

    /// A lookup over `pairs` alone, keyed by the literal variable names, so
    /// a reader that asked for any other name would see nothing.
    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<std::ffi::OsString> {
        let map: std::collections::HashMap<String, std::ffi::OsString> = pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.into()))
            .collect();
        move |name| map.get(name).cloned()
    }

    /// The production readers, not a hand-built shape: each knob comes
    /// from its own variable, and the debug-only ones only in a debug
    /// build — this build's own flag.
    #[test]
    fn the_session_readers_take_each_knob_from_its_own_variable() {
        let shaped = SessionTestShape::from_env(&env_of(&[
            ("ROOST_TEST_MODE", "1"),
            ("ROOST_TEST_SESSION_DIR_NAMES", "release"),
        ]));
        assert_eq!(
            shaped,
            SessionTestShape {
                test_mode: true,
                shipped_names_in_debug: cfg!(debug_assertions),
            }
        );
        assert_eq!(
            SessionTestShape::from_env(&env_of(&[])),
            SessionTestShape::default()
        );

        let old = env_of(&[("ROOST_TEST_OLD_SESSION_DIR", "/tmp/old-session")]);
        assert_eq!(
            test_old_session_dir_from(&old),
            cfg!(debug_assertions).then(|| PathBuf::from("/tmp/old-session"))
        );

        let default = PathBuf::from(SAFE_DEFAULT);
        let refused = mac_session_socket(default.clone(), &env_of(&[("ROOST_TEST_MODE", "1")]))
            .expect_err("a test-mode resolve with no override is refused");
        assert!(
            format!("{refused:#}").contains(SESSION_SOCKET_DIR_ENV),
            "{refused:#}"
        );
        let overridden = env_of(&[
            ("ROOST_TEST_MODE", "1"),
            ("ROOST_SESSION_SOCKET_DIR", "/tmp/roost-ss"),
        ]);
        assert_eq!(
            mac_session_socket(default, &overridden).expect("resolve"),
            PathBuf::from("/tmp/roost-ss/roost.sock")
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_linux_reader_takes_each_path_from_its_own_variable() {
        let env = LinuxPathEnv::from_env(&env_of(&[
            ("HOME", "/home/tester"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("XDG_DATA_HOME", "/data"),
            ("XDG_STATE_HOME", "/state"),
            ("XDG_CACHE_HOME", "/cache"),
            ("ROOST_SESSION_SOCKET_DIR", "/tmp/roost-ss"),
            ("ROOST_TEST_PASSWD_HOME", "/tmp/scratch-home"),
            ("ROOST_TEST_MODE", "1"),
        ]));
        assert_eq!(env.home.as_deref(), Some(OsStr::new("/home/tester")));
        assert_eq!(
            env.runtime_dir.as_deref(),
            Some(OsStr::new("/run/user/1000"))
        );
        assert_eq!(env.data_home.as_deref(), Some(OsStr::new("/data")));
        assert_eq!(env.state_home.as_deref(), Some(OsStr::new("/state")));
        assert_eq!(env.cache_home.as_deref(), Some(OsStr::new("/cache")));
        assert_eq!(
            env.session_socket_dir.as_deref(),
            Some(OsStr::new("/tmp/roost-ss"))
        );
        assert_eq!(
            env.passwd_home,
            cfg!(debug_assertions).then(|| PathBuf::from("/tmp/scratch-home"))
        );
        assert!(env.session_test_shape.test_mode);

        // End to end through the same reader: test mode alone is refused.
        let bare = LinuxPathEnv::from_env(&env_of(&[
            ("HOME", "/home/tester"),
            ("ROOST_TEST_MODE", "1"),
        ]));
        let error = resolve_paths_linux(BundleProfileKind::Session, &bare, 1000)
            .expect_err("a test-mode session resolve with no override is refused");
        assert!(
            format!("{error:#}").contains(SESSION_SOCKET_DIR_ENV),
            "{error:#}"
        );
    }

    #[test]
    fn the_test_path_knobs_are_absolute_and_debug_only() {
        let knob =
            |debug_build, raw: Option<&str>| debug_test_path(debug_build, raw.map(OsStr::new));
        assert_eq!(
            knob(true, Some("/tmp/scratch")),
            Some(PathBuf::from("/tmp/scratch"))
        );
        for raw in [None, Some(""), Some("relative/scratch")] {
            assert_eq!(knob(true, raw), None, "{raw:?}");
        }
        assert_eq!(
            knob(false, Some("/tmp/scratch")),
            None,
            "a packaged build ignores it"
        );
    }

    #[test]
    fn session_profile_slug_round_trips_but_is_not_a_profile_env_value() {
        assert_eq!(BundleProfileKind::Session.as_str(), "session");
        // A UI pointed at the session namespace is always a mistake, so
        // the env parser leaves the caller's default in place.
        assert_eq!(
            kind_from_profile_env(BundleProfileKind::Iced, Some("session")),
            BundleProfileKind::Iced
        );
    }

    #[test]
    fn socket_lock_is_next_to_the_socket() {
        let p = BundleProfile::mac().expect("mac profile");
        assert_eq!(
            p.socket_lock_path().parent(),
            p.socket_path.parent(),
            "the socket lock must share a directory with the socket it guards"
        );
    }

    #[test]
    fn state_lock_is_next_to_state_json() {
        let p = BundleProfile::mac().expect("mac profile");
        assert_eq!(
            p.state_lock_path().parent(),
            p.state_json_path().parent(),
            "the state lock must share a directory with the state it guards"
        );
    }

    /// The R1 guard. When `state_dir` collapses onto the socket's
    /// directory — the HOME-less `/tmp/<label>` fallback, or a
    /// `ROOST_STATE_DIR` aimed at the runtime dir — one shared
    /// filename would make the two locks one file, and the second
    /// `flock` would contend with the first (per-open-file-description
    /// semantics). Distinct filenames are what keeps that impossible.
    #[test]
    fn the_two_lock_filenames_differ_even_when_the_directories_collide() {
        let p = BundleProfile::mac().expect("mac profile");
        let collapsed = BundleProfile {
            state_dir: p
                .socket_path
                .parent()
                .expect("socket has a parent")
                .to_path_buf(),
            ..p.clone()
        };
        assert_eq!(
            collapsed.socket_lock_path().parent(),
            collapsed.state_lock_path().parent(),
            "this test is only meaningful with both locks in one directory"
        );
        assert_ne!(
            collapsed.socket_lock_path(),
            collapsed.state_lock_path(),
            "the two locks must never resolve to the same file"
        );
    }

    #[test]
    fn profile_env_parser_accepts_all_targets_and_preserves_fallback_policy() {
        assert_eq!(
            kind_from_profile_env(BundleProfileKind::Mac, Some(" iced ")),
            BundleProfileKind::Iced
        );
        assert_eq!(
            kind_from_profile_env(BundleProfileKind::Iced, Some("linux")),
            BundleProfileKind::Linux
        );
        assert_eq!(
            kind_from_profile_env(BundleProfileKind::Iced, Some("mac")),
            BundleProfileKind::Mac
        );
        assert_eq!(
            kind_from_profile_env(BundleProfileKind::Linux, Some("unknown")),
            BundleProfileKind::Linux
        );
        assert_eq!(
            kind_from_profile_env(BundleProfileKind::Iced, None),
            BundleProfileKind::Iced
        );
        assert_eq!(
            kind_from_profile_env(BundleProfileKind::Iced, Some("  ")),
            BundleProfileKind::Iced
        );
    }

    /// A `ROOST_BUNDLE_PROFILE=gtk` left over from before the rename is
    /// no longer a recognized value. The UI-side policy is deliberately
    /// forgiving — it keeps the caller's default rather than refusing to
    /// launch (the `warn!` in `kind_from_profile_env` is what makes the
    /// namespace change observable). The CLI side hard-errors instead;
    /// that divergence is pinned in `target.rs`.
    #[test]
    fn a_stale_gtk_profile_value_falls_through_to_the_default() {
        assert_eq!(
            kind_from_profile_env(BundleProfileKind::Linux, Some("gtk")),
            BundleProfileKind::Linux
        );
        assert_eq!(
            kind_from_profile_env(BundleProfileKind::Iced, Some("gtk")),
            BundleProfileKind::Iced
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn all_profile_paths_are_distinct_on_mac() {
        let mac = BundleProfile::mac().expect("mac profile");
        let linux = BundleProfile::linux().expect("linux profile");
        let iced = BundleProfile::iced().expect("iced profile");
        assert_ne!(mac.socket_path, linux.socket_path);
        assert_ne!(mac.socket_path, iced.socket_path);
        assert_ne!(linux.socket_path, iced.socket_path);
        assert_ne!(mac.state_dir, linux.state_dir);
        assert_ne!(mac.state_dir, iced.state_dir);
        assert_ne!(linux.state_dir, iced.state_dir);
        assert_ne!(mac.log_dir, linux.log_dir);
        assert_ne!(mac.log_dir, iced.log_dir);
        assert_ne!(linux.log_dir, iced.log_dir);
        assert!(mac.socket_path.to_string_lossy().contains("/Roost/"));
        assert!(linux
            .socket_path
            .to_string_lossy()
            .contains("/Roost-linux/"));
        assert!(iced.socket_path.to_string_lossy().contains("/Roost-iced/"));
        // Nothing ships the Linux kind on macOS, but keeping it total
        // and distinct means the enum needs no cfg-holes: macOS keys a
        // running instance by app id, so the ids must differ too.
        assert_ne!(mac.app_id, linux.app_id);
        assert_ne!(mac.app_id, iced.app_id);
        assert_ne!(linux.app_id, iced.app_id);

        let session = BundleProfile::session().expect("session profile");
        for ui in [&mac, &linux, &iced] {
            assert_ne!(session.socket_path, ui.socket_path);
            assert_ne!(session.state_dir, ui.state_dir);
            assert_ne!(session.log_dir, ui.log_dir);
            assert_ne!(session.app_id, ui.app_id);
        }
    }

    /// The macOS half of the session path contract, pinned literally over
    /// an injected environment — an override or `HOME` the test process
    /// happens to carry must not move it. Mirrored by `RoostTests.swift`'s
    /// `bundleProfileSession*` tests.
    #[cfg(target_os = "macos")]
    #[test]
    fn session_paths_live_under_their_own_mac_library_dirs() {
        let label = build_session_dir_names().0;
        let home = env_of(&[("HOME", "/Users/tester")]);
        let (socket, state, log) =
            resolve_paths_mac(BundleProfileKind::Session, label, &home).expect("resolve");
        assert_eq!(
            socket,
            PathBuf::from(format!("/Users/tester/Library/Caches/{label}/roost.sock"))
        );
        assert_eq!(
            state,
            PathBuf::from(format!("/Users/tester/Library/Application Support/{label}"))
        );
        assert_eq!(
            log,
            PathBuf::from(format!("/Users/tester/Library/Logs/{label}"))
        );

        for homeless in [env_of(&[]), env_of(&[("HOME", "")])] {
            let (socket, state, log) =
                resolve_paths_mac(BundleProfileKind::Session, label, &homeless).expect("resolve");
            let tmp = PathBuf::from("/tmp").join(label);
            assert_eq!(socket, tmp.join("roost.sock"));
            assert_eq!(state, tmp);
            assert_eq!(log, tmp);
        }

        // The override moves the session socket and nothing else, and the
        // UI profiles never read it.
        let overridden = env_of(&[
            ("HOME", "/Users/tester"),
            ("ROOST_SESSION_SOCKET_DIR", "/tmp/roost-ss"),
        ]);
        let (socket, moved_state, _) =
            resolve_paths_mac(BundleProfileKind::Session, label, &overridden).expect("resolve");
        assert_eq!(socket, PathBuf::from("/tmp/roost-ss/roost.sock"));
        assert_eq!(moved_state, state);
        let (iced, _, _) =
            resolve_paths_mac(BundleProfileKind::Iced, "Roost-iced", &overridden).expect("resolve");
        assert_eq!(
            iced,
            PathBuf::from("/Users/tester/Library/Caches/Roost-iced/roost.sock")
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn linux_keeps_iced_distinct_from_collapsed_production_profiles() {
        let mac = BundleProfile::mac().expect("mac profile");
        let linux = BundleProfile::linux().expect("linux profile");
        let iced = BundleProfile::iced().expect("iced profile");
        assert_eq!(mac.socket_path, linux.socket_path);
        assert_eq!(mac.state_dir, linux.state_dir);
        assert_eq!(mac.log_dir, linux.log_dir);
        assert_ne!(linux.socket_path, iced.socket_path);
        assert_ne!(linux.state_dir, iced.state_dir);
        assert_ne!(linux.log_dir, iced.log_dir);
        assert!(iced.socket_path.to_string_lossy().contains("roost-iced"));
        assert_eq!(linux.app_id, "ai.stridelabs.Roost");
        assert_ne!(linux.app_id, iced.app_id);
    }

    // ---------------------------------------------------------------
    // Golden Linux paths — the on-disk upgrade contract.
    //
    // These are literal strings on purpose. The `Linux` kind used to be
    // called `Gtk`, and an installed Roost owns exactly these paths:
    // comparing two profiles against each other would still pass if
    // BOTH moved, so the contract is pinned against recorded values
    // instead. Driven through the pure `resolve_paths_linux` so no test
    // has to mutate process-global env.
    // ---------------------------------------------------------------

    #[cfg(not(target_os = "macos"))]
    fn golden_env() -> LinuxPathEnv {
        LinuxPathEnv {
            home: Some("/home/tester".into()),
            runtime_dir: Some("/run/user/1000".into()),
            data_home: Some("/home/tester/.local/share".into()),
            state_home: Some("/home/tester/.local/state".into()),
            cache_home: Some("/home/tester/.cache".into()),
            ..LinuxPathEnv::default()
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn golden_linux_paths_under_explicit_xdg_dirs() {
        let (socket, state, log) =
            resolve_paths_linux(BundleProfileKind::Linux, &golden_env(), 1000).expect("resolve");
        assert_eq!(socket, PathBuf::from("/run/user/1000/roost/roost.sock"));
        assert_eq!(state, PathBuf::from("/home/tester/.local/share/roost"));
        assert_eq!(log, PathBuf::from("/home/tester/.local/state/roost"));

        let profile = BundleProfile {
            kind: BundleProfileKind::Linux,
            app_label: "Roost-linux",
            app_id: "ai.stridelabs.Roost",
            socket_path: socket,
            state_dir: state,
            log_dir: log,
        };
        assert_eq!(
            profile.socket_lock_path(),
            PathBuf::from("/run/user/1000/roost/roost.lock")
        );
        assert_eq!(
            profile.state_lock_path(),
            PathBuf::from("/home/tester/.local/share/roost/state.lock")
        );
        assert_eq!(
            profile.state_json_path(),
            PathBuf::from("/home/tester/.local/share/roost/state.json")
        );
        assert_eq!(
            profile.log_path(),
            PathBuf::from("/home/tester/.local/state/roost/roost.log")
        );
        // The app id an installed desktop entry / launcher pin sees.
        assert_eq!(
            BundleProfile::linux().expect("linux profile").app_id,
            "ai.stridelabs.Roost"
        );
    }

    /// With no `XDG_RUNTIME_DIR` the socket falls back to
    /// `/tmp/roost-<uid>` — the path `roostctl` and the docs both name.
    /// State and log still come from `$HOME`.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn golden_linux_paths_without_a_runtime_dir() {
        let env = LinuxPathEnv {
            home: Some("/home/tester".into()),
            ..LinuxPathEnv::default()
        };
        let (socket, state, log) =
            resolve_paths_linux(BundleProfileKind::Linux, &env, 1000).expect("resolve");
        assert_eq!(socket, PathBuf::from("/tmp/roost-1000/roost.sock"));
        assert_eq!(state, PathBuf::from("/home/tester/.local/share/roost"));
        assert_eq!(log, PathBuf::from("/home/tester/.local/state/roost"));
    }

    /// A set-but-unusable XDG value (empty or relative) is ignored, not
    /// honored — the same rule the shipped resolver has always applied.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn golden_linux_paths_ignore_unusable_xdg_values() {
        let env = LinuxPathEnv {
            home: Some("/home/tester".into()),
            runtime_dir: Some("".into()),
            data_home: Some("relative/share".into()),
            state_home: Some("".into()),
            cache_home: Some("relative/cache".into()),
            ..LinuxPathEnv::default()
        };
        let (socket, state, log) =
            resolve_paths_linux(BundleProfileKind::Linux, &env, 1000).expect("resolve");
        assert_eq!(socket, PathBuf::from("/tmp/roost-1000/roost.sock"));
        assert_eq!(state, PathBuf::from("/home/tester/.local/share/roost"));
        assert_eq!(log, PathBuf::from("/home/tester/.local/state/roost"));
        assert_eq!(
            resolve_cache_dir_linux(BundleProfileKind::Linux, &env).expect("resolve"),
            PathBuf::from("/home/tester/.cache/roost")
        );
    }

    /// `ROOST_STATE_DIR` moves `state.json` and its lock and nothing
    /// else. Composed exactly as `for_kind` composes them.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn golden_linux_paths_under_a_state_dir_override() {
        let (socket, state, log) =
            resolve_paths_linux(BundleProfileKind::Linux, &golden_env(), 1000).expect("resolve");
        let profile = BundleProfile {
            kind: BundleProfileKind::Linux,
            app_label: "Roost-linux",
            app_id: "ai.stridelabs.Roost",
            socket_path: socket,
            state_dir: apply_state_dir_override(state, Some("/tmp/roost-isolated-state".into())),
            log_dir: log,
        };
        assert_eq!(
            profile.state_json_path(),
            PathBuf::from("/tmp/roost-isolated-state/state.json")
        );
        assert_eq!(
            profile.state_lock_path(),
            PathBuf::from("/tmp/roost-isolated-state/state.lock")
        );
        assert_eq!(
            profile.socket_path,
            PathBuf::from("/run/user/1000/roost/roost.sock")
        );
        assert_eq!(
            profile.socket_lock_path(),
            PathBuf::from("/run/user/1000/roost/roost.lock")
        );
        assert_eq!(
            profile.log_path(),
            PathBuf::from("/home/tester/.local/state/roost/roost.log")
        );
    }

    /// The dev Iced profile keeps its own namespace under the same env —
    /// the other half of the contract, and what lets a dev build run
    /// beside a packaged install.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn golden_iced_paths_stay_in_their_own_namespace() {
        let (socket, state, log) =
            resolve_paths_linux(BundleProfileKind::Iced, &golden_env(), 1000).expect("resolve");
        assert_eq!(
            socket,
            PathBuf::from("/run/user/1000/roost-iced/roost.sock")
        );
        assert_eq!(state, PathBuf::from("/home/tester/.local/share/roost-iced"));
        assert_eq!(log, PathBuf::from("/home/tester/.local/state/roost-iced"));
    }

    /// The headless session daemon gets a third namespace, so it never
    /// shares a socket, `state.json` or log with either UI.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn golden_session_paths_stay_in_their_own_namespace() {
        let ns = build_session_dir_names().1;
        let (socket, state, log) =
            resolve_paths_linux(BundleProfileKind::Session, &golden_env(), 1000).expect("resolve");
        assert_eq!(
            socket,
            PathBuf::from(format!("/run/user/1000/{ns}/roost.sock"))
        );
        assert_eq!(
            state,
            PathBuf::from(format!("/home/tester/.local/share/{ns}"))
        );
        assert_eq!(
            log,
            PathBuf::from(format!("/home/tester/.local/state/{ns}"))
        );

        let iced =
            resolve_paths_linux(BundleProfileKind::Iced, &golden_env(), 1000).expect("resolve");
        let linux =
            resolve_paths_linux(BundleProfileKind::Linux, &golden_env(), 1000).expect("resolve");
        assert_ne!(socket, iced.0);
        assert_ne!(socket, linux.0);
    }

    /// With no `XDG_RUNTIME_DIR` the session socket falls back to
    /// `/tmp/<namespace>-<uid>`, exactly as the UI profiles do.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn golden_session_paths_without_a_runtime_dir() {
        let ns = build_session_dir_names().1;
        let env = LinuxPathEnv {
            home: Some("/home/tester".into()),
            ..LinuxPathEnv::default()
        };
        let (socket, state, log) =
            resolve_paths_linux(BundleProfileKind::Session, &env, 1000).expect("resolve");
        assert_eq!(socket, PathBuf::from(format!("/tmp/{ns}-1000/roost.sock")));
        assert_eq!(
            state,
            PathBuf::from(format!("/home/tester/.local/share/{ns}"))
        );
        assert_eq!(
            log,
            PathBuf::from(format!("/home/tester/.local/state/{ns}"))
        );
    }

    /// `ROOST_SESSION_SOCKET_DIR` moves the session's socket and its lock
    /// and nothing else; the UI profiles never read it, not even to
    /// refuse a bad value.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn golden_session_socket_under_the_override() {
        let ns = build_session_dir_names().1;
        let env = LinuxPathEnv {
            session_socket_dir: Some("/tmp/roost-ss".into()),
            ..golden_env()
        };
        let (socket, state, log) =
            resolve_paths_linux(BundleProfileKind::Session, &env, 1000).expect("resolve");
        assert_eq!(socket, PathBuf::from("/tmp/roost-ss/roost.sock"));
        assert_eq!(
            state,
            PathBuf::from(format!("/home/tester/.local/share/{ns}"))
        );
        assert_eq!(
            log,
            PathBuf::from(format!("/home/tester/.local/state/{ns}"))
        );
        let profile = BundleProfile {
            kind: BundleProfileKind::Session,
            app_label: build_session_dir_names().0,
            app_id: "ai.stridelabs.Roost.session",
            socket_path: socket,
            state_dir: state,
            log_dir: log,
        };
        assert_eq!(
            profile.socket_lock_path(),
            PathBuf::from("/tmp/roost-ss/roost.lock")
        );

        for kind in [BundleProfileKind::Linux, BundleProfileKind::Iced] {
            let namespace = linux_namespace(kind);
            for raw in ["/tmp/roost-ss", "relative/dir"] {
                let env = LinuxPathEnv {
                    session_socket_dir: Some(raw.into()),
                    ..golden_env()
                };
                let (socket, _, _) = resolve_paths_linux(kind, &env, 1000).expect("resolve");
                assert_eq!(
                    socket,
                    PathBuf::from(format!("/run/user/1000/{namespace}/roost.sock")),
                    "{kind:?} {raw:?}"
                );
            }
        }
    }

    /// The guards sit on the session socket alone: a test-mode UI still
    /// resolves its own profile.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_test_mode_resolve_without_the_override_fails_the_session_profile_only() {
        let env = LinuxPathEnv {
            session_test_shape: SessionTestShape {
                test_mode: true,
                shipped_names_in_debug: false,
            },
            ..golden_env()
        };
        let error = resolve_paths_linux(BundleProfileKind::Session, &env, 1000)
            .expect_err("a test-mode session resolve with no override is refused");
        assert!(
            format!("{error:#}").contains(SESSION_SOCKET_DIR_ENV),
            "{error:#}"
        );
        assert!(resolve_paths_linux(BundleProfileKind::Iced, &env, 1000).is_ok());

        let scratch = LinuxPathEnv {
            passwd_home: Some("/tmp/scratch-home".into()),
            ..env
        };
        let (socket, _, _) =
            resolve_paths_linux(BundleProfileKind::Session, &scratch, 1000).expect("resolve");
        let ns = build_session_dir_names().1;
        assert_eq!(
            socket,
            PathBuf::from(format!("/run/user/1000/{ns}/roost.sock")),
            "the scratch home moves no path"
        );
    }

    /// The upload store's Linux path contract (plan 047 §3.1), pinned
    /// literally like every other path above it.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn golden_linux_files_dirs_hang_off_the_cache_root() {
        let ns = build_session_dir_names().1;
        let session_want = format!("/home/tester/.cache/{ns}/files");
        for (kind, want) in [
            (BundleProfileKind::Linux, "/home/tester/.cache/roost/files"),
            (
                BundleProfileKind::Iced,
                "/home/tester/.cache/roost-iced/files",
            ),
            (BundleProfileKind::Session, session_want.as_str()),
        ] {
            let dir = resolve_cache_dir_linux(kind, &golden_env())
                .expect("resolve")
                .join("files");
            assert_eq!(dir, PathBuf::from(want));
            // Never the runtime dir: it is tmpfs, and an upload has to
            // outlive the memory a session can spare for it.
            assert!(!dir.starts_with("/run/user"));
        }

        // No `XDG_CACHE_HOME` → the spec's own default under `$HOME`.
        let env = LinuxPathEnv {
            home: Some("/home/tester".into()),
            ..LinuxPathEnv::default()
        };
        assert_eq!(
            resolve_cache_dir_linux(BundleProfileKind::Session, &env)
                .expect("resolve")
                .join("files"),
            PathBuf::from(format!("/home/tester/.cache/{ns}/files"))
        );

        // And `$HOME` is required only when it is what's left.
        let homeless = LinuxPathEnv::default();
        assert!(resolve_cache_dir_linux(BundleProfileKind::Session, &homeless).is_err());
    }

    /// The macOS half. `~/Library/Caches` and not
    /// `~/Library/Application Support`: the state dir has a space in
    /// it, and every byte of this path has to survive being pasted
    /// bare into an agent's composer.
    #[cfg(target_os = "macos")]
    #[test]
    fn golden_mac_files_dirs_hang_off_library_caches() {
        let home = std::ffi::OsString::from("/Users/tester");
        assert_eq!(
            resolve_cache_dir_mac("Roost", Some(&home)).join("files"),
            PathBuf::from("/Users/tester/Library/Caches/Roost/files")
        );
        let label = build_session_dir_names().0;
        let session = resolve_cache_dir_mac(label, Some(&home)).join("files");
        assert_eq!(
            session,
            PathBuf::from(format!("/Users/tester/Library/Caches/{label}/files"))
        );
        assert!(!session.to_string_lossy().contains(' '));

        // The same HOME-less fallback the socket takes.
        for raw in [None, Some(std::ffi::OsStr::new(""))] {
            assert_eq!(
                resolve_cache_dir_mac("Roost", raw).join("files"),
                PathBuf::from("/tmp/Roost/files")
            );
        }
    }

    /// The fallback is the one path the session never re-checks, so it
    /// has to be bare-pasteable by construction — and per profile, or a
    /// debug and a release session would sweep each other at start.
    #[test]
    fn the_files_dir_fallback_is_pasteable_and_per_profile() {
        let session = BundleProfile::session().expect("session profile");
        let iced = BundleProfile::iced().expect("iced profile");
        for profile in [&session, &iced] {
            let dir = profile.files_dir_fallback();
            let text = dir.to_str().expect("ascii");
            assert!(dir.is_absolute() && text.starts_with("/tmp/"), "{text}");
            assert!(dir.ends_with("files"), "{text}");
            assert!(
                text.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b)),
                "{text}"
            );
        }
        assert_ne!(session.files_dir_fallback(), iced.files_dir_fallback());
    }

    /// The isolation rule: a honoured `ROOST_STATE_DIR` moves the
    /// upload store inside it, so a spawned dev session's sweep-on-start
    /// cannot empty the developer's real cache. A value the resolver
    /// ignores leaves the store on the cache path, exactly as the state
    /// dir stays put.
    #[test]
    fn files_dir_follows_a_honoured_state_dir_override() {
        let base = BundleProfile::session().expect("session profile");
        let isolated = BundleProfile {
            state_dir: apply_state_dir_override(
                base.state_dir.clone(),
                Some("/tmp/roost-isolated-state".into()),
            ),
            ..base.clone()
        };
        assert_eq!(
            isolated
                .files_dir_with(Some(std::ffi::OsStr::new("/tmp/roost-isolated-state")))
                .expect("files dir"),
            PathBuf::from("/tmp/roost-isolated-state/files")
        );

        for raw in [None, Some(""), Some("relative/state")] {
            let dir = base
                .files_dir_with(raw.map(std::ffi::OsStr::new))
                .expect("files dir");
            assert!(
                !dir.starts_with(&base.state_dir),
                "{raw:?} must leave the store on the cache path, got {}",
                dir.display()
            );
            assert!(dir.ends_with("files"));
        }
    }

    /// A missing / unusable `$HOME` is only fatal when it is actually
    /// needed — with both XDG dirs set, resolution still succeeds.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn linux_resolution_needs_home_only_when_xdg_is_absent() {
        let env = LinuxPathEnv {
            home: None,
            ..golden_env()
        };
        assert!(resolve_paths_linux(BundleProfileKind::Linux, &env, 1000).is_ok());

        let bare = LinuxPathEnv {
            home: Some("relative/home".into()),
            ..LinuxPathEnv::default()
        };
        assert!(resolve_paths_linux(BundleProfileKind::Linux, &bare, 1000).is_err());
    }

    // ROOST_STATE_DIR override policy (pure helper — no env mutation).
    // Mirrored by BundleProfileTests.swift; kept in lockstep.

    #[test]
    fn state_dir_override_absolute_replaces() {
        let got = apply_state_dir_override(
            PathBuf::from("/default/state"),
            Some("/tmp/throwaway".into()),
        );
        assert_eq!(got, PathBuf::from("/tmp/throwaway"));
    }

    #[test]
    fn state_dir_override_unset_keeps_default() {
        let default = PathBuf::from("/default/state");
        assert_eq!(apply_state_dir_override(default.clone(), None), default);
    }

    #[test]
    fn state_dir_override_empty_keeps_default() {
        let default = PathBuf::from("/default/state");
        assert_eq!(
            apply_state_dir_override(default.clone(), Some("".into())),
            default
        );
    }

    #[test]
    fn state_dir_override_relative_keeps_default() {
        let default = PathBuf::from("/default/state");
        assert_eq!(
            apply_state_dir_override(default.clone(), Some("relative/state".into())),
            default
        );
    }

    #[test]
    fn state_dir_override_moves_state_and_its_lock_only() {
        // The lockstep invariant: redirecting state_dir moves
        // `state.json` AND the lock that guards it, and leaves the
        // socket, the socket lock, and the log byte-identical. The
        // state lock moving is the whole point — two UIs on one state
        // dir must contend even when their runtime dirs differ.
        let base = BundleProfile::linux().expect("linux profile");
        let overridden = BundleProfile {
            state_dir: apply_state_dir_override(
                base.state_dir.clone(),
                Some("/tmp/roost-isolated-state".into()),
            ),
            ..base.clone()
        };
        assert_eq!(
            overridden.state_json_path(),
            PathBuf::from("/tmp/roost-isolated-state/state.json")
        );
        assert_eq!(
            overridden.state_lock_path(),
            PathBuf::from("/tmp/roost-isolated-state/state.lock")
        );
        assert_ne!(overridden.state_lock_path(), base.state_lock_path());
        assert_eq!(overridden.socket_path, base.socket_path);
        assert_eq!(overridden.socket_lock_path(), base.socket_lock_path());
        assert_eq!(overridden.log_path(), base.log_path());
    }

    // The derived state dir a launcher hands a spawned session (#397).
    // The spawn itself is exercised over a real process in
    // `tests/session_launch_state_dir_test.rs`, which has to stay in a
    // binary of its own because it forks — see that file's header for
    // the listener it raced when it did not.

    /// The seam's name is a cross-process contract: a launcher sets it
    /// on the session it spawns, every profile resolution reads it, and
    /// the harness passes it to both. Nothing renames it alone.
    #[test]
    fn the_state_dir_env_name_is_frozen() {
        assert_eq!(STATE_DIR_ENV, "ROOST_STATE_DIR");
    }

    #[test]
    fn a_spawned_session_derives_a_dir_nested_in_an_absolute_seam() {
        assert_eq!(
            derived_session_state_dir(Some(std::ffi::OsStr::new("/tmp/throwaway"))),
            Some(PathBuf::from("/tmp/throwaway/session"))
        );
    }

    #[test]
    fn a_spawned_session_derives_nothing_from_a_value_the_resolver_ignores() {
        for raw in [None, Some(""), Some("relative/state")] {
            let raw = raw.map(std::ffi::OsStr::new);
            assert_eq!(derived_session_state_dir(raw), None, "{raw:?}");
        }
    }

    /// The rule is *the same* rule, not a second copy of it: a value
    /// derives a session dir exactly when it also redirects the
    /// launcher's own state dir.
    #[test]
    fn the_derivation_honours_exactly_what_the_override_honours() {
        let default = PathBuf::from("/default/state");
        for raw in [
            None,
            Some(""),
            Some("relative/state"),
            Some("/tmp/throwaway"),
        ] {
            let raw = raw.map(std::ffi::OsStr::new);
            let redirected =
                apply_state_dir_override(default.clone(), raw.map(|v| v.to_os_string())) != default;
            assert_eq!(
                derived_session_state_dir(raw).is_some(),
                redirected,
                "{raw:?}"
            );
        }
    }
}
