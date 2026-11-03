//! The mobile host client (`roost-client`'s `HostClient` and
//! `AttachStream`) against the real `roost-session` binary: the prologue
//! and mirror, control ops, the attach data plane, resume, reconnects
//! across a session restart, and the compatibility gate.
//!
//! Each daemon runs under `ROOST_TEST_MODE=1` with a private socket
//! directory, state directory, home and runtime dir under a short `/tmp`
//! root, built from an empty environment, so nothing here can reach a
//! developer's own session. Every wait is a condition against a scaled
//! deadline.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use roost_client::attach::{AttachEvent, AttachRequest, AttachStream, PayloadKind};
use roost_client::dial::{Dialer, UnixDialer};
use roost_client::host::{HostCallError, HostClient, HostClientConfig, HostEvent, HostState};
use roost_ipc::dataframe::InputTake;
use roost_ipc::messages::{AttachMode, SizeOwner, SESSION_PROTOCOL_VERSION};
use roost_ipc::paths::{
    SESSION_SOCKET_DIR_ENV, STATE_DIR_ENV, TEST_PASSWD_HOME_ENV, TEST_USER_RUNTIME_DIR_ENV,
};
use support::scaled;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc::UnboundedReceiver;

const CLIENT_ID: &str = "mobile-test-1";
const LABEL: &str = "test-phone";

/// A build string no session has, so the attach negotiates `vt`, whose
/// terminator a test can see without decoding a GHOSTSNP stream.
const SKEWED_BUILD: &str = "ghostty-0000000000000000+snapshot.v1";

fn budget() -> Duration {
    scaled(Duration::from_secs(30))
}

fn owner_only(path: PathBuf) -> PathBuf {
    std::fs::create_dir_all(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}

/// A short `/tmp` root, canonical because the daemon refuses a socket
/// directory under a symlinked ancestor.
fn root() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::Builder::new()
        .prefix("rm-mc-")
        .tempdir_in("/tmp")
        .expect("tempdir");
    let path = support::canonical(tmp.path());
    (tmp, path)
}

fn socket(root: &Path) -> PathBuf {
    root.join("ss").join("roost.sock")
}

/// `roost-session start --foreground` from `exe` over `root`, keeping
/// its layout in `root/<state>`, once it says ready.
async fn start(root: &Path, exe: &Path, state: &str) -> Child {
    let home = owner_only(root.join("home"));
    let sockets = owner_only(root.join("ss"));
    let runtime = owner_only(root.join("rt"));
    let state = owner_only(root.join(state));
    let mut command = Command::new(exe);
    command
        .args(["start", "--foreground"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("SHELL", "/bin/sh")
        .env("ROOST_SHELL_FEATURES", "")
        .env("ROOST_TEST_MODE", "1")
        .env("HOME", &home)
        .env(TEST_PASSWD_HOME_ENV, &home)
        .env(SESSION_SOCKET_DIR_ENV, &sockets)
        .env(STATE_DIR_ENV, &state)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env(TEST_USER_RUNTIME_DIR_ENV, &runtime)
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("xstate"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .current_dir(&home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(scale) = std::env::var_os("ROOST_TEST_TIMEOUT_SCALE") {
        command.env("ROOST_TEST_TIMEOUT_SCALE", scale);
    }
    let mut child = command.spawn().expect("spawn roost-session start");
    let mut verdict = String::new();
    tokio::time::timeout(
        budget(),
        BufReader::new(child.stdout.take().expect("stdout")).read_line(&mut verdict),
    )
    .await
    .expect("a verdict within the budget")
    .expect("read the verdict");
    assert!(verdict.starts_with("ready pid="), "{verdict}");
    child
}

fn session_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_roost-session"))
}

async fn within<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(budget(), future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

struct Host {
    client: HostClient,
    events: UnboundedReceiver<HostEvent>,
    dialer: Arc<dyn Dialer>,
}

impl Host {
    fn spawn(root: &Path, build: &str) -> Host {
        let dialer: Arc<dyn Dialer> = Arc::new(UnixDialer(socket(root)));
        let (client, events) = HostClient::spawn(
            &tokio::runtime::Handle::current(),
            Arc::clone(&dialer),
            HostClientConfig {
                client_id: CLIENT_ID.into(),
                label: LABEL.into(),
                libghostty_build: build.into(),
            },
        );
        Host {
            client,
            events,
            dialer,
        }
    }

    /// Read events until a state `want` accepts; every state seen on the
    /// way is in the panic message.
    async fn until_state(&mut self, what: &str, want: impl Fn(&HostState) -> bool) -> HostState {
        let mut seen = Vec::new();
        let found = tokio::time::timeout(budget(), async {
            loop {
                match self.events.recv().await.expect("the host task is alive") {
                    HostEvent::State(state) if want(&state) => return state,
                    HostEvent::State(state) => seen.push(state),
                    _ => {}
                }
            }
        })
        .await;
        found.unwrap_or_else(|_| panic!("timed out waiting for {what}; states were {seen:?}"))
    }

    async fn connected(&mut self) {
        self.until_state("Connected", |state| *state == HostState::Connected)
            .await;
    }

    /// Wait until the mirror satisfies `want`, re-checking on every event.
    async fn until_mirror(
        &mut self,
        what: &str,
        want: impl Fn(&roost_client::mirror::HostMirror) -> bool,
    ) {
        let mirror = self.client.mirror();
        within(what, async {
            while !want(&mirror.read()) {
                self.events.recv().await.expect("the host task is alive");
            }
        })
        .await;
    }

    fn tab_ids(&self) -> Vec<i64> {
        self.client
            .mirror()
            .read()
            .tabs()
            .map(|tab| tab.id)
            .collect()
    }

    fn attach(&self, tab_id: i64, take: bool) -> (AttachStream, UnboundedReceiver<AttachEvent>) {
        AttachStream::spawn(
            &tokio::runtime::Handle::current(),
            Arc::clone(&self.dialer),
            &self.client,
            AttachRequest {
                tab_id,
                cols: 80,
                rows: 24,
                cell_w_px: 9,
                cell_h_px: 18,
                take,
            },
        )
    }

    async fn call(&self, op: &str, params: serde_json::Value) -> serde_json::Value {
        let result = within(op, self.client.call(op, params)).await;
        result.unwrap_or_else(|error| panic!("{op}: {error}"))
    }
}

fn me() -> Option<SizeOwner> {
    Some(SizeOwner {
        client_id: CLIENT_ID.into(),
        label: LABEL.into(),
    })
}

/// The next attach event `want` accepts, returning every event before it.
async fn until_attach(
    events: &mut UnboundedReceiver<AttachEvent>,
    what: &str,
    want: impl Fn(&AttachEvent) -> bool,
) -> (AttachEvent, Vec<AttachEvent>) {
    let mut before = Vec::new();
    let found = tokio::time::timeout(budget(), async {
        loop {
            let event = events.recv().await.expect("the attach task is alive");
            if want(&event) {
                return event;
            }
            before.push(event);
        }
    })
    .await;
    match found {
        Ok(event) => (event, before),
        Err(_) => panic!("timed out waiting for {what}; saw {before:?}"),
    }
}

/// Collect `Pty` bytes until they contain `needle`. Any other event on
/// the way fails the test: output was expected, not a reconnect.
async fn until_output(events: &mut UnboundedReceiver<AttachEvent>, needle: &str) {
    let mut output = Vec::new();
    let found = tokio::time::timeout(budget(), async {
        loop {
            match events.recv().await.expect("the attach task is alive") {
                AttachEvent::Pty(bytes) => {
                    output.extend_from_slice(&bytes);
                    if String::from_utf8_lossy(&output).contains(needle) {
                        return;
                    }
                }
                AttachEvent::Snap(_) | AttachEvent::Grid { .. } => {}
                other => panic!("expected output, got {other:?}"),
            }
        }
    })
    .await;
    if found.is_err() {
        panic!(
            "timed out waiting for {needle:?}; output was {:?}",
            String::from_utf8_lossy(&output)
        );
    }
}

/// Attach and read through the `vt` payload to its terminator, then tell
/// the stream the terminal holds it all.
async fn attach_hydrated(
    host: &Host,
    tab_id: i64,
) -> (AttachStream, UnboundedReceiver<AttachEvent>) {
    let (stream, mut events) = host.attach(tab_id, true);
    let (accepted, _) = until_attach(&mut events, "Accepted", |event| {
        matches!(event, AttachEvent::Accepted { .. })
    })
    .await;
    assert!(
        matches!(
            accepted,
            AttachEvent::Accepted {
                kind: PayloadKind::Vt,
                mode: AttachMode::Snapshot,
                ..
            }
        ),
        "{accepted:?}"
    );
    until_attach(&mut events, "the vt terminator", |event| {
        *event == AttachEvent::Snap(Vec::new())
    })
    .await;
    stream.hydrated();
    (stream, events)
}

fn seed_tab(host: &Host) -> i64 {
    let ids = host.tab_ids();
    assert_eq!(ids.len(), 1, "a fresh session seeds one tab: {ids:?}");
    ids[0]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connect_mirrors_the_seed_tab_and_follows_an_op_through_events() {
    let (_tmp, root) = root();
    let _daemon = start(&root, &session_binary(), "state").await;
    let mut host = Host::spawn(&root, &roost_vt::libghostty_build());
    host.connected().await;

    let facts = host.client.session().expect("facts while connected");
    assert!(!facts.session_id.is_empty());
    assert!(
        facts.payload_kinds.iter().any(|kind| kind == "vt"),
        "{facts:?}"
    );
    let project_id = {
        let mirror = host.client.mirror();
        let mirror = mirror.read();
        assert_eq!(mirror.projects.len(), 1, "the seed project");
        assert_eq!(mirror.projects[0].tabs.len(), 1, "the seed tab");
        mirror.projects[0].id
    };

    let opened = host
        .call(
            "tab.open",
            serde_json::json!({
                "project_id": project_id.to_string(),
                "cwd": root.to_string_lossy(),
                "argv": ["/bin/sh"],
            }),
        )
        .await;
    let tab_id: i64 = opened["tab"]["id"]
        .as_str()
        .expect("a string-wrapped tab id")
        .parse()
        .unwrap();
    host.until_mirror("the opened tab in the mirror", |mirror| {
        mirror.tab(tab_id).is_some()
    })
    .await;

    let refused = within(
        "a refused op",
        host.client
            .call("tab.close", serde_json::json!({"tab_id": "999999"})),
    )
    .await;
    assert!(
        matches!(&refused, Err(HostCallError::Server { code, .. }) if code == "not-found"),
        "{refused:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attach_snapshots_echoes_input_and_follows_a_focused_resize() {
    let (_tmp, root) = root();
    let _daemon = start(&root, &session_binary(), "state").await;
    let mut host = Host::spawn(&root, &roost_vt::libghostty_build());
    host.connected().await;
    let tab_id = seed_tab(&host);

    let (stream, mut events) = host.attach(tab_id, true);
    let (accepted, _) = until_attach(&mut events, "Accepted", |event| {
        matches!(event, AttachEvent::Accepted { .. })
    })
    .await;
    assert_eq!(
        accepted,
        AttachEvent::Accepted {
            kind: PayloadKind::GhosttySnapshot,
            mode: AttachMode::Snapshot,
            cols: 80,
            rows: 24,
            cell_w_px: 9,
            cell_h_px: 18,
            owner: me(),
        }
    );
    let (snap, _) = until_attach(
        &mut events,
        "snapshot bytes",
        |event| matches!(event, AttachEvent::Snap(bytes) if !bytes.is_empty()),
    )
    .await;
    assert!(matches!(snap, AttachEvent::Snap(_)));

    stream.input(b"echo hi-$((6*7))\r".to_vec(), InputTake::Before);
    until_output(&mut events, "hi-42").await;

    stream.resize(100, 30, 9, 18, true);
    let (grid, _) = until_attach(&mut events, "the GRID of the resize", |event| {
        matches!(event, AttachEvent::Grid { cols: 100, .. })
    })
    .await;
    assert_eq!(
        grid,
        AttachEvent::Grid {
            cols: 100,
            rows: 30,
            cell_w_px: 9,
            cell_h_px: 18,
            owner: me(),
        }
    );

    stream.detach();
    until_attach(&mut events, "Ended", |event| {
        matches!(event, AttachEvent::Ended { .. })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_data_connection_resumes_where_it_stopped() {
    let (_tmp, root) = root();
    let _daemon = start(&root, &session_binary(), "state").await;
    let mut host = Host::spawn(&root, SKEWED_BUILD);
    host.connected().await;
    let tab_id = seed_tab(&host);

    let (stream, mut events) = attach_hydrated(&host, tab_id).await;
    stream.input(b"echo one-$((1+1))\r".to_vec(), InputTake::Before);
    until_output(&mut events, "one-2").await;

    host.call(
        "tab.drop_data_conns",
        serde_json::json!({"tab_id": tab_id.to_string()}),
    )
    .await;
    until_attach(&mut events, "Reconnecting", |event| {
        matches!(event, AttachEvent::Reconnecting { .. })
    })
    .await;
    let (accepted, before) = until_attach(&mut events, "the re-attach", |event| {
        matches!(event, AttachEvent::Accepted { .. })
    })
    .await;
    assert!(before.is_empty(), "nothing between: {before:?}");
    assert!(
        matches!(
            accepted,
            AttachEvent::Accepted {
                mode: AttachMode::Resume,
                kind: PayloadKind::Vt,
                ..
            }
        ),
        "{accepted:?}"
    );

    stream.input(b"echo two-$((2+2))\r".to_vec(), InputTake::Before);
    until_output(&mut events, "two-4").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_killed_session_is_redialed_and_the_attach_snapshots_its_successor() {
    let (_tmp, root) = root();
    let mut first = start(&root, &session_binary(), "state-a").await;
    let mut host = Host::spawn(&root, SKEWED_BUILD);
    host.connected().await;
    let tab_id = seed_tab(&host);
    let old_session = host.client.session().expect("facts").session_id;
    let (stream, mut events) = attach_hydrated(&host, tab_id).await;

    first.start_kill().expect("kill the first session");
    within("the first session to exit", first.wait())
        .await
        .expect("reap");
    host.until_state("Retrying", |state| {
        matches!(state, HostState::Retrying { .. })
    })
    .await;
    until_attach(&mut events, "Reconnecting", |event| {
        matches!(event, AttachEvent::Reconnecting { .. })
    })
    .await;

    // A fresh layout, so its seed tab has the id the attach names.
    let _second = start(&root, &session_binary(), "state-b").await;
    host.connected().await;
    let new_session = host.client.session().expect("facts").session_id;
    assert_ne!(new_session, old_session);
    assert_eq!(
        seed_tab(&host),
        tab_id,
        "the precondition: the seed tab's id again"
    );

    let (accepted, before) = until_attach(&mut events, "the re-attach", |event| {
        matches!(event, AttachEvent::Accepted { .. })
    })
    .await;
    assert!(
        before
            .iter()
            .all(|event| matches!(event, AttachEvent::Reconnecting { .. })),
        "{before:?}"
    );
    assert!(
        matches!(
            accepted,
            AttachEvent::Accepted {
                mode: AttachMode::Snapshot,
                ..
            }
        ),
        "another session's stream cannot be resumed: {accepted:?}"
    );
    stream.input(b"echo three-$((1+2))\r".to_vec(), InputTake::Before);
    until_output(&mut events, "three-3").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_session_ends_the_attach_and_the_host_reconnects_to_its_restart() {
    let (_tmp, root) = root();
    let mut first = start(&root, &session_binary(), "state").await;
    let mut host = Host::spawn(&root, &roost_vt::libghostty_build());
    host.connected().await;
    let tab_id = seed_tab(&host);
    let old_session = host.client.session().expect("facts").session_id;
    let (_stream, mut events) = host.attach(tab_id, true);
    until_attach(&mut events, "Accepted", |event| {
        matches!(event, AttachEvent::Accepted { .. })
    })
    .await;

    host.call("session.stop", serde_json::json!({})).await;
    let (ended, _) = until_attach(&mut events, "Ended", |event| {
        matches!(event, AttachEvent::Ended { .. })
    })
    .await;
    assert!(
        matches!(&ended, AttachEvent::Ended { reason } if reason.contains("shutting down")),
        "{ended:?}"
    );
    within("the stopped session to exit", first.wait())
        .await
        .expect("reap");
    host.until_state("Retrying", |state| {
        matches!(state, HostState::Retrying { .. })
    })
    .await;

    let _second = start(&root, &session_binary(), "state").await;
    host.connected().await;
    let facts = host.client.session().expect("facts");
    assert_ne!(facts.session_id, old_session);
    host.until_mirror("the restored tab", |mirror| mirror.tabs().count() == 1)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_on_another_protocol_fails_the_host_naming_both_versions() {
    let (_tmp, root) = root();
    let bin = owner_only(root.join("bin"));
    let exe = bin.join("roost-session");
    std::fs::hard_link(session_binary(), &exe).expect("link the binary");
    let theirs = SESSION_PROTOCOL_VERSION + 1;
    std::fs::write(
        bin.join("roost-session.test-identity"),
        format!(r#"{{"session_protocol": {theirs}}}"#),
    )
    .expect("plant the identity sidecar");
    let _daemon = start(&root, &exe, "state").await;

    let mut host = Host::spawn(&root, &roost_vt::libghostty_build());
    let failed = host
        .until_state("Failed", |state| matches!(state, HostState::Failed { .. }))
        .await;
    let HostState::Failed { reason } = failed else {
        unreachable!()
    };
    assert!(
        reason.contains(&format!("speaks {SESSION_PROTOCOL_VERSION}"))
            && reason.contains(&format!("speaks {theirs}")),
        "{reason}"
    );
    assert_eq!(host.client.session(), None);
    assert_eq!(
        host.client.call("tab.list", serde_json::json!({})).await,
        Err(HostCallError::NotConnected)
    );

    host.client.reconnect_now();
    host.until_state("Connecting", |state| *state == HostState::Connecting)
        .await;
    host.until_state("Failed again", |state| {
        matches!(state, HostState::Failed { .. })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn release_size_on_the_control_connection_hands_the_size_on() {
    let (_tmp, root) = root();
    let _daemon = start(&root, &session_binary(), "state").await;
    let mut host = Host::spawn(&root, &roost_vt::libghostty_build());
    host.connected().await;
    let tab_id = seed_tab(&host);

    let (_stream, mut events) = host.attach(tab_id, true);
    let (accepted, _) = until_attach(&mut events, "Accepted", |event| {
        matches!(event, AttachEvent::Accepted { .. })
    })
    .await;
    assert!(
        matches!(&accepted, AttachEvent::Accepted { owner, .. } if *owner == me()),
        "{accepted:?}"
    );

    host.call(
        "tab.release_size",
        serde_json::json!({"tab_id": tab_id.to_string()}),
    )
    .await;
    until_attach(&mut events, "a GRID naming no owner", |event| {
        matches!(event, AttachEvent::Grid { owner: None, .. })
    })
    .await;
}
