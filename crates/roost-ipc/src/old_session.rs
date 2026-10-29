//! Finding a session still running on a path this build no longer uses
//! (plan 083 D-A5) — the upgrade across M2, which moved the Linux session
//! socket off `XDG_RUNTIME_DIR` (`crate::paths`).
//!
//! A daemon started by an older build keeps listening where that build
//! put it. Nothing here attaches to it or stops it: every consumer only
//! *reports* what this finds — `start` refuses to run beside it, `roostctl
//! session status` and `doctor` name it, the UI turns it into a restart
//! prompt — and the one way to stop it is an explicit
//! `roostctl session stop --old-path <socket>`.
//!
//! # The gate, fail closed
//!
//! The production candidates include `/tmp/<ns>-<uid>/roost.sock`, which
//! on a developer's box is the real, running session. So the probe runs
//! only for a process that could be nothing but a user's own: no
//! [`SESSION_SOCKET_DIR_ENV`], no `ROOST_TEST_MODE` (set at all), and not a
//! debug build pointed at the shipped directory names. A test reaches it
//! one way only — a debug build's [`TEST_OLD_SESSION_DIR_ENV`], which
//! names the single candidate probed, whatever else is set.
//!
//! Retired one release after M2, with the bridge's fallback.

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::messages::ops;
use crate::paths::{SESSION_SOCKET_DIR_ENV, TEST_OLD_SESSION_DIR_ENV};
use crate::peer::current_euid;
use crate::socket_state::{self, classify_connect_error, is_socket, SocketState};
use crate::IpcClient;

/// How long each leg of a probe — the connect, then the
/// `session.identify` — may take. A listener that accepts and never
/// answers is reported as [`OldSession::Unidentified`], not waited on.
pub const PROBE_DEADLINE: Duration = Duration::from_secs(2);

/// Something answering on an old path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OldSession {
    /// A session that identified itself. Every field past `socket` is
    /// read leniently, so any generation's answer decodes.
    Found {
        socket: PathBuf,
        protocol_version: u32,
        pid: Option<u32>,
        session_id: Option<String>,
    },
    /// Something is there but could not be identified — a permission
    /// error, a deadline, an answer that is not a session's. Reported,
    /// never used for anything.
    Unidentified { socket: PathBuf, reason: String },
}

impl OldSession {
    pub fn socket(&self) -> &Path {
        match self {
            Self::Found { socket, .. } | Self::Unidentified { socket, .. } => socket,
        }
    }

    /// The one line every consumer reports this as.
    pub fn describe(&self) -> String {
        match self {
            Self::Found {
                socket,
                protocol_version,
                session_id,
                ..
            } => match session_id {
                Some(id) => format!(
                    "{} (session {id}, protocol {protocol_version})",
                    socket.display()
                ),
                None => format!("{} (protocol {protocol_version})", socket.display()),
            },
            Self::Unidentified { socket, reason } => {
                format!("{} (not identified: {reason})", socket.display())
            }
        }
    }
}

/// Whether a session may start at `new_socket` — `roost-session start`
/// and `roostctl session start` alike: refused, naming the fix, while
/// nothing answers there and a session is [`OldSession::Found`] on an
/// old path. A session already at `new_socket` is the start's own locks
/// to report.
pub async fn check_start(new_socket: &Path) -> Result<(), String> {
    if !socket_state::probe(new_socket, socket_state::PROBE_TIMEOUT)
        .await
        .safe_to_unlink()
    {
        return Ok(());
    }
    match discover(new_socket)
        .await
        .iter()
        .find(|old| matches!(old, OldSession::Found { .. }))
    {
        Some(old) => Err(format!(
            "a session is running at an old socket path, {}, which this build no longer \
             uses; stop it with 'roostctl session stop --old-path {}' and start again",
            old.describe(),
            old.socket().display()
        )),
        None => Ok(()),
    }
}

/// What the gate lets this process probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Candidates {
    /// Nothing, and why.
    Closed(&'static str),
    /// The one scratch socket a debug build's
    /// [`TEST_OLD_SESSION_DIR_ENV`] names.
    Test(PathBuf),
    /// The production candidates, in probe order.
    Production(Vec<PathBuf>),
}

impl Candidates {
    pub fn sockets(&self) -> &[PathBuf] {
        match self {
            Self::Closed(_) => &[],
            Self::Test(socket) => std::slice::from_ref(socket),
            Self::Production(sockets) => sockets,
        }
    }

    /// Whether an explicitly named socket may be probed — the gate that
    /// `roostctl session stop --old-path` re-probes through. Only one of
    /// these candidates is, compared through canonical directories so no
    /// spelling of another path gets past: a test-shaped process can name
    /// only its scratch candidate, and nothing a discovery here would
    /// never have looked at can be named at all.
    pub fn permit(&self, socket: &Path) -> Result<(), String> {
        if let Self::Closed(why) = self {
            return Err(format!("old session paths are not probed here: {why}"));
        }
        if self
            .sockets()
            .iter()
            .any(|candidate| same_socket(candidate, socket))
        {
            return Ok(());
        }
        let listed = self
            .sockets()
            .iter()
            .map(|candidate| candidate.display().to_string())
            .collect::<Vec<_>>();
        Err(match (self, listed.is_empty()) {
            (Self::Test(_), _) => format!(
                "only {} may be probed here ({TEST_OLD_SESSION_DIR_ENV})",
                listed.join(", ")
            ),
            (_, true) => "this build looks for no session on an old path".to_string(),
            (_, false) => format!(
                "{} is not a path an older build used; those are {}",
                socket.display(),
                listed.join(", ")
            ),
        })
    }
}

/// Whether two spellings name one socket: compared through their
/// directories' canonical paths, the socket itself need not exist.
pub fn same_socket(a: &Path, b: &Path) -> bool {
    canonical(a) == canonical(b)
}

/// Everything the gate reads, captured once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Gate {
    /// A non-empty [`SESSION_SOCKET_DIR_ENV`].
    pub socket_dir_set: bool,
    /// `ROOST_TEST_MODE` set to anything at all.
    pub test_mode: bool,
    /// A debug build pointed at the shipped session directory names.
    pub shipped_names_in_debug: bool,
    /// [`TEST_OLD_SESSION_DIR_ENV`], as a debug build honours it.
    pub test_old_session_dir: Option<PathBuf>,
    /// `XDG_RUNTIME_DIR`, when set and absolute.
    pub runtime_dir: Option<PathBuf>,
    pub uid: u32,
    pub namespace: String,
}

impl Gate {
    pub fn from_process() -> Self {
        Self::from_env(
            cfg!(debug_assertions),
            &|name| std::env::var_os(name),
            crate::paths::real_uid(),
        )
    }

    /// `debug_build` is a parameter so the safety matrix covers both
    /// build profiles from one build.
    pub(crate) fn from_env(
        debug_build: bool,
        getenv: &dyn Fn(&str) -> Option<std::ffi::OsString>,
        uid: u32,
    ) -> Self {
        let dir_names = crate::paths::requested_dir_names(getenv);
        let (_, namespace) = crate::paths::session_dir_names(debug_build, dir_names.as_deref());
        Self {
            socket_dir_set: getenv(SESSION_SOCKET_DIR_ENV).is_some_and(|value| !value.is_empty()),
            test_mode: getenv("ROOST_TEST_MODE").is_some(),
            shipped_names_in_debug: crate::paths::shipped_names_in_debug(
                debug_build,
                dir_names.as_deref(),
            ),
            test_old_session_dir: crate::paths::debug_test_path(
                debug_build,
                getenv(TEST_OLD_SESSION_DIR_ENV).as_deref(),
            ),
            runtime_dir: getenv("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .filter(|dir| dir.is_absolute()),
            uid,
            namespace: namespace.to_string(),
        }
    }

    pub fn candidates(&self) -> Candidates {
        if let Some(dir) = &self.test_old_session_dir {
            return Candidates::Test(dir.join("roost.sock"));
        }
        if self.socket_dir_set {
            return Candidates::Closed("ROOST_SESSION_SOCKET_DIR names this session's socket");
        }
        if self.test_mode {
            return Candidates::Closed("ROOST_TEST_MODE is set");
        }
        if self.shipped_names_in_debug {
            return Candidates::Closed(
                "a debug build on the shipped session names could reach this user's session",
            );
        }
        Candidates::Production(production_candidates(
            self.runtime_dir.as_deref(),
            self.uid,
            &self.namespace,
        ))
    }
}

/// Where a pre-M2 Linux build put the session socket, in the order that
/// build's resolver would have: the runtime dir it saw, the runtime dir
/// systemd gives this uid, and the `/tmp` fallback. macOS never moved its
/// session socket, so it has none.
fn production_candidates(runtime_dir: Option<&Path>, uid: u32, namespace: &str) -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        return Vec::new();
    }
    let mut sockets = Vec::new();
    if let Some(dir) = runtime_dir {
        sockets.push(dir.join(namespace).join("roost.sock"));
    }
    sockets.push(
        PathBuf::from(format!("/run/user/{uid}"))
            .join(namespace)
            .join("roost.sock"),
    );
    sockets.push(PathBuf::from(format!("/tmp/{namespace}-{uid}")).join("roost.sock"));
    sockets
}

/// One candidate per real socket, and never this build's own: two
/// spellings of one directory (a runtime dir that is a symlink) probe
/// once.
fn dedupe(candidates: &[PathBuf], new_socket: &Path) -> Vec<PathBuf> {
    let mut seen = HashSet::from([canonical(new_socket)]);
    candidates
        .iter()
        .filter(|socket| seen.insert(canonical(socket)))
        .cloned()
        .collect()
}

/// The socket's path through its canonical directory; the socket itself
/// need not exist.
fn canonical(socket: &Path) -> PathBuf {
    match (socket.parent(), socket.file_name()) {
        (Some(dir), Some(name)) => std::fs::canonicalize(dir)
            .map(|dir| dir.join(name))
            .unwrap_or_else(|_| socket.to_path_buf()),
        _ => socket.to_path_buf(),
    }
}

/// Every session answering on an old path this process may probe, in
/// candidate order — all of them, not just the first. `new_socket` is the
/// path this build uses, never a candidate.
pub async fn discover(new_socket: &Path) -> Vec<OldSession> {
    discover_among(&Gate::from_process().candidates(), new_socket).await
}

/// [`discover`] over an explicit candidate set.
pub async fn discover_among(candidates: &Candidates, new_socket: &Path) -> Vec<OldSession> {
    let sockets = dedupe(candidates.sockets(), new_socket);
    let mut probes = tokio::task::JoinSet::new();
    for (index, socket) in sockets.into_iter().enumerate() {
        probes.spawn(async move { (index, probe_socket(&socket).await) });
    }
    let mut found = Vec::new();
    while let Some(joined) = probes.join_next().await {
        if let Ok((index, Some(session))) = joined {
            found.push((index, session));
        }
    }
    found.sort_by_key(|(index, _)| *index);
    found.into_iter().map(|(_, session)| session).collect()
}

/// Probe one socket: `None` when nothing is listening there, or when the
/// path fails the ownership screen.
pub async fn probe_socket(socket: &Path) -> Option<OldSession> {
    if !passes_screen(socket) {
        return None;
    }
    let unidentified = |reason: String| {
        Some(OldSession::Unidentified {
            socket: socket.to_path_buf(),
            reason,
        })
    };
    let mut client = match tokio::time::timeout(PROBE_DEADLINE, IpcClient::connect(socket)).await {
        Err(_) => return unidentified(format!("connect took over {}s", PROBE_DEADLINE.as_secs())),
        Ok(Ok(client)) => client,
        Ok(Err(crate::Error::Io(error))) => match classify_connect_error(&error) {
            SocketState::Missing | SocketState::Stale => return None,
            _ => return unidentified(error.to_string()),
        },
        Ok(Err(error)) => return unidentified(error.to_string()),
    };
    let answer = tokio::time::timeout(
        PROBE_DEADLINE,
        client.call_raw(ops::SESSION_IDENTIFY, serde_json::json!({})),
    )
    .await;
    match answer {
        Err(_) => unidentified(format!(
            "no {} answer within {}s",
            ops::SESSION_IDENTIFY,
            PROBE_DEADLINE.as_secs()
        )),
        Ok(Err(error)) => unidentified(format!("{}: {error}", ops::SESSION_IDENTIFY)),
        Ok(Ok(identity)) => Some(decode(socket, &identity)),
    }
}

/// Lenient on purpose: an old daemon's identity has fewer fields than
/// today's, and all a report needs is that it is a session and which.
fn decode(socket: &Path, identity: &serde_json::Value) -> OldSession {
    let Some(protocol_version) = identity
        .get("session_protocol")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
    else {
        return OldSession::Unidentified {
            socket: socket.to_path_buf(),
            reason: format!(
                "its {} answer names no session_protocol",
                ops::SESSION_IDENTIFY
            ),
        };
    };
    OldSession::Found {
        socket: socket.to_path_buf(),
        protocol_version,
        pid: identity
            .get("pid")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok()),
        session_id: identity
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string),
    }
}

/// A socket this uid made, in a directory only this uid can change —
/// checked without following a link, before anything is dialed.
fn passes_screen(socket: &Path) -> bool {
    let euid = current_euid();
    let Ok(meta) = std::fs::symlink_metadata(socket) else {
        return false;
    };
    if !is_socket(meta.file_type()) || meta.uid() != euid {
        return false;
    }
    let Some(dir) = socket.parent() else {
        return false;
    };
    std::fs::symlink_metadata(dir)
        .is_ok_and(|dir| dir.is_dir() && dir.uid() == euid && dir.mode() & 0o022 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    /// A scratch directory shaped like a session's: `0700`, ours.
    fn scratch_dir(parent: &Path, name: &str) -> PathBuf {
        let dir = parent.join(name);
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn short_tempdir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("r083-old-")
            .tempdir_in("/tmp")
            .unwrap()
    }

    /// A scratch sentinel answering `session.identify` with `result`.
    fn sentinel(socket: &Path, result: serde_json::Value) -> tokio::task::JoinHandle<()> {
        let listener = UnixListener::bind(socket).unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let result = result.clone();
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut lines = BufReader::new(read).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                        let reply = serde_json::json!({
                            "id": request["id"], "ok": true, "result": result,
                        });
                        let mut bytes = serde_json::to_vec(&reply).unwrap();
                        bytes.push(b'\n');
                        if write.write_all(&bytes).await.is_err() {
                            return;
                        }
                    }
                });
            }
        })
    }

    fn old_identity(session_id: &str) -> serde_json::Value {
        serde_json::json!({
            "app_version": "0.0.19", "session_protocol": 2, "payload_kinds": ["ghostty-snapshot"],
            "libghostty_build": "gb-old", "session_id": session_id, "started_at": "t",
        })
    }

    #[tokio::test]
    async fn an_answering_session_is_found_with_what_it_said() {
        let root = short_tempdir();
        let dir = scratch_dir(root.path(), "old");
        let socket = dir.join("roost.sock");
        let _server = sentinel(&socket, old_identity("old-1"));
        assert_eq!(
            probe_socket(&socket).await,
            Some(OldSession::Found {
                socket: socket.clone(),
                protocol_version: 2,
                pid: None,
                session_id: Some("old-1".into()),
            })
        );
    }

    #[tokio::test]
    async fn an_identity_with_no_protocol_is_unidentified() {
        let root = short_tempdir();
        let dir = scratch_dir(root.path(), "old");
        let socket = dir.join("roost.sock");
        let _server = sentinel(&socket, serde_json::json!({"app_version": "x"}));
        assert!(matches!(
            probe_socket(&socket).await,
            Some(OldSession::Unidentified { .. })
        ));
    }

    #[tokio::test]
    async fn nothing_listening_and_a_stale_socket_are_skipped() {
        let root = short_tempdir();
        let dir = scratch_dir(root.path(), "old");
        let socket = dir.join("roost.sock");
        assert_eq!(probe_socket(&socket).await, None, "missing");
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        assert_eq!(probe_socket(&socket).await, None, "refused");
    }

    /// Plan 083 A-3: a listener that accepts and never answers is
    /// reported within the deadline, not waited on.
    #[tokio::test]
    async fn a_listener_that_never_answers_is_unidentified_within_the_deadline() {
        let root = short_tempdir();
        let dir = scratch_dir(root.path(), "old");
        let socket = dir.join("roost.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let _held = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        let started = std::time::Instant::now();
        let probed = probe_socket(&socket).await;
        assert!(
            matches!(&probed, Some(OldSession::Unidentified { reason, .. }) if reason.contains("within")),
            "{probed:?}"
        );
        assert!(
            started.elapsed() < PROBE_DEADLINE * 2,
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_candidate_in_a_group_writable_dir_is_skipped() {
        let root = short_tempdir();
        let dir = scratch_dir(root.path(), "old");
        let socket = dir.join("roost.sock");
        let _server = sentinel(&socket, old_identity("old-1"));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o770)).unwrap();
        assert_eq!(probe_socket(&socket).await, None);
    }

    #[tokio::test]
    async fn a_symlink_where_the_socket_belongs_is_skipped() {
        let root = short_tempdir();
        let real = scratch_dir(root.path(), "real");
        let socket = real.join("roost.sock");
        let _server = sentinel(&socket, old_identity("old-1"));
        let linked = scratch_dir(root.path(), "linked");
        std::os::unix::fs::symlink(&socket, linked.join("roost.sock")).unwrap();
        assert_eq!(probe_socket(&linked.join("roost.sock")).await, None);
    }

    /// Another uid's socket cannot be made without root, so the screen's
    /// uid rule is pinned on the metadata it reads: this uid's socket in
    /// this uid's `0700` dir passes, and the same socket in a dir another
    /// uid could write does not (above).
    #[tokio::test]
    async fn a_candidate_of_this_uid_in_its_own_dir_passes_the_screen() {
        let root = short_tempdir();
        let dir = scratch_dir(root.path(), "old");
        let socket = dir.join("roost.sock");
        let _server = sentinel(&socket, old_identity("old-1"));
        assert!(passes_screen(&socket));
        assert!(!passes_screen(&dir), "a directory is not a socket");
    }

    /// Plan 083 A-3: two old daemons are both listed, in candidate order.
    #[tokio::test]
    async fn every_answering_candidate_is_listed() {
        let root = short_tempdir();
        let first = scratch_dir(root.path(), "a").join("roost.sock");
        let missing = scratch_dir(root.path(), "b").join("roost.sock");
        let second = scratch_dir(root.path(), "c").join("roost.sock");
        let _one = sentinel(&first, old_identity("old-a"));
        let _two = sentinel(&second, old_identity("old-c"));
        let found = discover_among(
            &Candidates::Production(vec![first.clone(), missing, second.clone()]),
            &root.path().join("new/roost.sock"),
        )
        .await;
        let sockets: Vec<&Path> = found.iter().map(OldSession::socket).collect();
        assert_eq!(sockets, vec![first.as_path(), second.as_path()]);
    }

    /// A runtime dir that is a symlink names the same socket twice; it is
    /// probed once — and this build's own socket never.
    #[tokio::test]
    async fn two_spellings_of_one_socket_probe_once_and_the_new_path_never() {
        let root = short_tempdir();
        let runtime = scratch_dir(root.path(), "rt");
        let socket = scratch_dir(&runtime, "ns").join("roost.sock");
        let _server = sentinel(&socket, old_identity("old-1"));
        let link = root.path().join("rt-link");
        std::os::unix::fs::symlink(&runtime, &link).unwrap();
        let found = discover_among(
            &Candidates::Production(vec![link.join("ns/roost.sock"), socket.clone()]),
            &root.path().join("new/roost.sock"),
        )
        .await;
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].socket(),
            link.join("ns/roost.sock"),
            "the first spelling"
        );

        let found = discover_among(&Candidates::Production(vec![socket.clone()]), &socket).await;
        assert!(
            found.is_empty(),
            "the new path is not an old one: {found:?}"
        );
    }

    #[test]
    fn production_candidates_follow_the_pre_m2_resolver() {
        let candidates =
            production_candidates(Some(Path::new("/run/user/1000")), 1000, "roost-session");
        if cfg!(target_os = "macos") {
            assert!(candidates.is_empty());
            return;
        }
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/run/user/1000/roost-session/roost.sock"),
                PathBuf::from("/run/user/1000/roost-session/roost.sock"),
                PathBuf::from("/tmp/roost-session-1000/roost.sock"),
            ]
        );
        assert_eq!(
            production_candidates(None, 1000, "roost-session-dev"),
            vec![
                PathBuf::from("/run/user/1000/roost-session-dev/roost.sock"),
                PathBuf::from("/tmp/roost-session-dev-1000/roost.sock"),
            ]
        );
    }

    #[test]
    fn the_explicit_socket_permit_follows_the_gate() {
        let only = PathBuf::from("/tmp/scratch-old/roost.sock");
        assert!(Candidates::Test(only.clone()).permit(&only).is_ok());
        assert!(Candidates::Test(only)
            .permit(Path::new("/tmp/roost-session-1000/roost.sock"))
            .is_err());
        assert!(Candidates::Closed("test")
            .permit(Path::new("/tmp/x/roost.sock"))
            .is_err());
    }

    /// Plan 083 A2 review: an explicit socket must be one the gate's
    /// discovery would have looked at — nothing at all when it lists
    /// none — however it is spelled.
    #[test]
    fn only_a_candidate_may_be_named_whatever_its_spelling() {
        let root = short_tempdir();
        let real = scratch_dir(root.path(), "real");
        let candidate = real.join("roost.sock");
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let production = Candidates::Production(vec![candidate.clone()]);
        for spelling in [
            candidate.clone(),
            real.join("../real/roost.sock"),
            link.join("roost.sock"),
        ] {
            assert!(
                production.permit(&spelling).is_ok(),
                "{}",
                spelling.display()
            );
            assert!(Candidates::Test(candidate.clone())
                .permit(&spelling)
                .is_ok());
        }
        let elsewhere = scratch_dir(root.path(), "elsewhere").join("roost.sock");
        assert!(production.permit(&elsewhere).is_err());
        assert!(Candidates::Production(Vec::new())
            .permit(&candidate)
            .is_err());
    }
}
