//! The mobile app layer (`roost-mobile`'s `RoostApp` and
//! `TerminalHandle`) against the real `roost-session` binary: a saved
//! `unix:` host connected and mirrored, a tab attached and typed into
//! through the client terminal, control-plane writes and ops, the pause
//! and resume lifecycle, a data-plane resume, and teardown leaving
//! nothing running.
//!
//! The app owns its runtime, so these are plain threads waiting on
//! conditions against a scaled deadline. Each daemon runs under
//! `ROOST_TEST_MODE=1` with private directories under a short `/tmp`
//! root and an empty environment, so nothing here reaches a developer's
//! own session.

mod support;

use std::fmt::Debug;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use roost_ipc::paths::{
    SESSION_SOCKET_DIR_ENV, STATE_DIR_ENV, TEST_PASSWD_HOME_ENV, TEST_USER_RUNTIME_DIR_ENV,
};
use roost_mobile::app::{AppError, LeakCounts, RoostApp, Sink};
use roost_mobile::frame::{decode, flags, row_text};
use roost_mobile::owner::TerminalEvent;
use roost_mobile::terminal::{AttachState, Geometry};
use roost_mobile::views::{HostStateView, HostView, WorkspaceView};
use roost_mobile::TerminalHandle;
use support::scaled;

const GEOMETRY: Geometry = Geometry {
    cols: 80,
    rows: 24,
    cell_w_px: 9,
    cell_h_px: 18,
};

fn budget() -> Duration {
    scaled(Duration::from_secs(30))
}

fn owner_only(path: PathBuf) -> PathBuf {
    std::fs::create_dir_all(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn root() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::Builder::new()
        .prefix("rm-ma-")
        .tempdir_in("/tmp")
        .expect("tempdir");
    let path = support::canonical(tmp.path());
    (tmp, path)
}

fn socket(root: &Path) -> PathBuf {
    root.join("ss").join("roost.sock")
}

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A shell prompt row: `$ ` from `/bin/sh` on Linux, `host:~ user$` on
/// macOS, whose login shell reads `/etc/bashrc`'s prompt.
fn is_prompt(row: &str) -> bool {
    row.trim_end().ends_with('$')
}

/// `roost-session start --foreground` over `root`, once it says ready.
fn start(root: &Path) -> Daemon {
    let home = owner_only(root.join("home"));
    let mut command = Command::new(env!("CARGO_BIN_EXE_roost-session"));
    command
        .args(["start", "--foreground"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("SHELL", "/bin/sh")
        .env("ROOST_SHELL_FEATURES", "")
        .env("ROOST_TEST_MODE", "1")
        .env("HOME", &home)
        .env(TEST_PASSWD_HOME_ENV, &home)
        .env(SESSION_SOCKET_DIR_ENV, owner_only(root.join("ss")))
        .env(STATE_DIR_ENV, owner_only(root.join("state")))
        .env("XDG_RUNTIME_DIR", owner_only(root.join("rt")))
        .env(TEST_USER_RUNTIME_DIR_ENV, root.join("rt"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("xstate"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .current_dir(&home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(scale) = std::env::var_os("ROOST_TEST_TIMEOUT_SCALE") {
        command.env("ROOST_TEST_TIMEOUT_SCALE", scale);
    }
    let mut child = command.spawn().expect("spawn roost-session start");
    let stdout = child.stdout.take().expect("stdout");
    let daemon = Daemon(child);
    let (said, verdict) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = said.send(line);
    });
    let line = verdict
        .recv_timeout(budget())
        .expect("a verdict within the budget");
    assert!(line.starts_with("ready pid="), "{line}");
    daemon
}

/// Everything a sink was handed, waited on by condition.
struct Recorder<T> {
    seen: Arc<(Mutex<Vec<T>>, Condvar)>,
}

impl<T: Clone + Debug + Send + 'static> Recorder<T> {
    fn new() -> Self {
        Self {
            seen: Arc::default(),
        }
    }

    fn sink(&self) -> Sink<T> {
        let seen = Arc::clone(&self.seen);
        Box::new(move |value| {
            seen.0.lock().unwrap().push(value);
            seen.1.notify_all();
        })
    }

    fn len(&self) -> usize {
        self.seen.0.lock().unwrap().len()
    }

    fn since(&self, from: usize) -> Vec<T> {
        self.seen.0.lock().unwrap()[from..].to_vec()
    }

    /// The first value from `from` on that `want` accepts, and its index.
    fn wait_from(&self, from: usize, what: &str, want: impl Fn(&T) -> bool) -> (usize, T) {
        let deadline = Instant::now() + budget();
        let mut seen = self.seen.0.lock().unwrap();
        loop {
            if let Some((at, value)) = seen
                .iter()
                .enumerate()
                .skip(from)
                .find(|(_, value)| want(value))
            {
                return (at, value.clone());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "timed out waiting for {what}; saw {:?}",
                &seen[from.min(seen.len())..]
            );
            seen = self.seen.1.wait_timeout(seen, left).unwrap().0;
        }
    }

    fn wait(&self, what: &str, want: impl Fn(&T) -> bool) -> T {
        self.wait_from(0, what, want).1
    }
}

/// A painter's row cache, fed by pulled frames.
#[derive(Default)]
struct Screen {
    rows: Vec<String>,
    full_frames: usize,
}

impl Screen {
    fn pull(&mut self, handle: &TerminalHandle) {
        let bytes = handle.take_frame();
        if bytes.is_empty() {
            return;
        }
        let frame = decode(&bytes).expect("a v1 frame");
        let rows = usize::from(frame.header.rows);
        if frame.header.flags & flags::FULL != 0 {
            self.full_frames += 1;
            self.rows = vec![String::new(); rows];
        }
        self.rows.resize(rows, String::new());
        for row in &frame.rows {
            self.rows[usize::from(row.index)] = row_text(row, frame.header.cols);
        }
    }

    fn wait(&mut self, handle: &TerminalHandle, what: &str, want: impl Fn(&[String]) -> bool) {
        let deadline = Instant::now() + budget();
        loop {
            self.pull(handle);
            if want(&self.rows) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; the screen was {:#?}",
                self.rows
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn has_line(&self, line: &str) -> bool {
        self.rows.iter().any(|row| row.trim() == line)
    }
}

fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + budget();
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    futures::executor::block_on(future)
}

fn is_status(event: &TerminalEvent, want: AttachState) -> bool {
    matches!(event, TerminalEvent::Status { state, .. } if *state == want)
}

/// A connected app with one `unix:` host, and that host's id.
fn connected_app(
    root: &Path,
    dir: &str,
    label: &str,
) -> (RoostApp, String, Recorder<Vec<HostView>>) {
    let app = RoostApp::new(root.join(dir), label.into()).expect("app");
    let hosts = Recorder::new();
    app.hosts(hosts.sink());
    let host = app
        .add_host("dev".into(), format!("unix:{}", socket(root).display()))
        .expect("add_host");
    app.connect(host.clone()).expect("connect");
    hosts.wait("Connected", |views| {
        views
            .iter()
            .any(|view| view.id == host && view.state == HostStateView::Connected)
    });
    (app, host, hosts)
}

/// The workspace sink, and the seed tab's (project, tab) ids.
fn seed_tab(app: &RoostApp, host: &str) -> (Recorder<WorkspaceView>, i64, i64) {
    let workspace = Recorder::new();
    app.workspace(host.into(), workspace.sink())
        .expect("workspace");
    let view = workspace.wait("the seed tab", |view| {
        view.projects
            .iter()
            .map(|project| project.tabs.len())
            .sum::<usize>()
            == 1
    });
    (workspace, view.projects[0].id, view.projects[0].tabs[0].id)
}

fn attach(
    app: &RoostApp,
    host: &str,
    tab: i64,
    take: bool,
) -> (TerminalHandle, Recorder<TerminalEvent>) {
    let handle = app
        .attach(host.into(), tab, GEOMETRY, take)
        .expect("attach");
    let events = Recorder::new();
    handle.events(events.sink());
    (handle, events)
}

#[test]
fn a_unix_host_mirrors_attaches_and_takes_typing_writes_and_ops() {
    let (_tmp, root) = root();
    let _daemon = start(&root);
    let (app, host, hosts) = connected_app(&root, "app", "Test Phone");
    let (workspace, project, tab) = seed_tab(&app, &host);

    let handle = app
        .attach(host.clone(), tab, GEOMETRY, true)
        .expect("attach");
    let mut screen = Screen::default();
    screen.wait(&handle, "the shell prompt", |rows| {
        rows.iter().any(|row| is_prompt(row))
    });
    assert_eq!(screen.rows.len(), 24);
    // Dart sets its sink after `attach` returns: what the terminal said
    // meanwhile waits for it.
    let events = Recorder::new();
    handle.events(events.sink());
    events.wait("Live", |event| is_status(event, AttachState::Live));
    events.wait("our size", |event| {
        matches!(event, TerminalEvent::Owner { is_me: true, label: Some(label), cols: 80, rows: 24 }
            if label == "Test Phone")
    });
    events.wait("FrameReady", |event| *event == TerminalEvent::FrameReady);

    handle.text("echo mobile-$((40+2))\r".into());
    screen.wait(&handle, "mobile-42", |rows| {
        rows.iter().any(|row| row.trim() == "mobile-42")
    });

    block_on(app.send_text(host.clone(), tab, "echo via-$((1+1))\r".into(), false))
        .expect("tab.write");
    screen.wait(&handle, "via-2", |rows| {
        rows.iter().any(|row| row.trim() == "via-2")
    });

    let opened = block_on(app.op(
        host.clone(),
        "tab.open".into(),
        serde_json::json!({"project_id": project.to_string(), "argv": ["/bin/sh"]}).to_string(),
    ))
    .expect("tab.open");
    let opened: serde_json::Value = serde_json::from_str(&opened).expect("a JSON result");
    let new_tab: i64 = opened["tab"]["id"]
        .as_str()
        .expect("an id")
        .parse()
        .unwrap();
    workspace.wait("the opened tab", |view| {
        view.projects
            .iter()
            .flat_map(|project| &project.tabs)
            .any(|tab| tab.id == new_tab)
    });
    let notifications = Recorder::new();
    app.notifications(notifications.sink());
    block_on(
        app.op(
            host.clone(),
            "notification.create".into(),
            serde_json::json!({"tab_id": tab.to_string(), "title": "Build", "body": "passed"})
                .to_string(),
        ),
    )
    .expect("notification.create");
    let fired = notifications.wait("the notification", |_| true);
    assert_eq!(
        (
            fired.host_id.as_str(),
            fired.tab_id,
            fired.title.as_str(),
            fired.body.as_str()
        ),
        (host.as_str(), tab, "Build", "passed")
    );

    let refused = block_on(app.op(
        host.clone(),
        "tab.close".into(),
        r#"{"tab_id":"999999"}"#.into(),
    ));
    assert!(refused.is_err(), "{refused:?}");
    assert!(
        hosts.len() > 1,
        "the host list was re-sent as things changed"
    );

    let counters = app.leak_counters();
    assert_eq!(
        app.leak_counts(),
        LeakCounts {
            hosts: 1,
            host_pumps: 1,
            attach_pumps: 1,
            attachments: 1,
            terminals: 1,
        }
    );
    drop(handle);
    drop(app);
    assert_eq!(counters.counts(), LeakCounts::default());
}

#[test]
fn a_dropped_data_connection_resumes_without_a_new_snapshot() {
    let (_tmp, root) = root();
    let _daemon = start(&root);
    let (app, host, _hosts) = connected_app(&root, "app", "Test Phone");
    let (_workspace, _, tab) = seed_tab(&app, &host);
    let (handle, events) = attach(&app, &host, tab, true);
    let mut screen = Screen::default();
    handle.text("echo one-$((1+1))\r".into());
    screen.wait(&handle, "one-2", |rows| {
        rows.iter().any(|row| row.trim() == "one-2")
    });
    let full_before = screen.full_frames;

    let from = events.len();
    block_on(app.op(
        host.clone(),
        "tab.drop_data_conns".into(),
        serde_json::json!({"tab_id": tab.to_string()}).to_string(),
    ))
    .expect("tab.drop_data_conns");
    let (dropped, _) = events.wait_from(from, "Reconnecting", |event| {
        is_status(event, AttachState::Reconnecting)
    });
    events.wait_from(dropped, "Live again", |event| {
        is_status(event, AttachState::Live)
    });
    let statuses: Vec<TerminalEvent> = events
        .since(from)
        .into_iter()
        .filter(|event| matches!(event, TerminalEvent::Status { .. }))
        .collect();
    assert!(
        !statuses
            .iter()
            .any(|event| is_status(event, AttachState::Attaching)),
        "a resume, not a fresh snapshot: {statuses:?}"
    );

    handle.text("echo two-$((2+2))\r".into());
    screen.wait(&handle, "two-4", |rows| {
        rows.iter().any(|row| row.trim() == "two-4")
    });
    assert!(
        screen.has_line("one-2"),
        "the screen before the drop is kept"
    );
    assert_eq!(
        screen.full_frames, full_before,
        "no new terminal was swapped in"
    );

    // Dart can still hold a handle when the app goes: the app's teardown
    // still stops everything behind it, and the handle goes inert.
    let counters = app.leak_counters();
    drop(app);
    assert_eq!(
        counters.counts(),
        LeakCounts {
            attachments: 1,
            ..LeakCounts::default()
        }
    );
    assert!(handle.take_frame().is_empty());
    handle.text("ignored".into());
    drop(handle);
    assert_eq!(counters.counts(), LeakCounts::default());
}

#[test]
fn a_pause_releases_the_size_and_detaches_and_a_resume_attaches_again() {
    let (_tmp, root) = root();
    let _daemon = start(&root);
    let (phone, host, _) = connected_app(&root, "phone", "Phone A");
    let (watcher, watcher_host, _) = connected_app(&root, "watcher", "Watcher");
    let (_workspace, _, tab) = seed_tab(&phone, &host);

    let (handle, events) = attach(&phone, &host, tab, true);
    events.wait("our size", |event| {
        matches!(event, TerminalEvent::Owner { is_me: true, .. })
    });
    let (watching, watched) = attach(&watcher, &watcher_host, tab, false);
    watched.wait("the phone's size", |event| {
        matches!(event, TerminalEvent::Owner { is_me: false, label: Some(label), .. }
            if label == "Phone A")
    });

    phone.app_paused();
    events.wait("Ended: paused", |event| {
        matches!(event, TerminalEvent::Status { state: AttachState::Ended, message: Some(why) }
            if why == "paused")
    });
    watched.wait("the size handed on", |event| {
        matches!(event, TerminalEvent::Owner { is_me: true, .. })
    });
    drop(handle);

    // A released client is no candidate, not even through an attach it
    // opens afterwards, until it takes the size again: with the watcher
    // gone, the size goes to nobody rather than back to the phone.
    phone.app_resumed();
    let (quiet, quiet_events) = attach(&phone, &host, tab, false);
    quiet_events.wait("Live", |event| is_status(event, AttachState::Live));
    drop(watching);
    quiet_events.wait("the size owned by nobody", |event| {
        matches!(event, TerminalEvent::Owner { label: None, .. })
    });
    drop(quiet);

    let (again, again_events) = attach(&phone, &host, tab, true);
    again_events.wait("our size again", |event| {
        matches!(event, TerminalEvent::Owner { is_me: true, .. })
    });
    let mut screen = Screen::default();
    again.text("echo back-$((3+3))\r".into());
    screen.wait(&again, "back-6", |rows| {
        rows.iter().any(|row| row.trim() == "back-6")
    });
}

#[test]
fn saved_hosts_and_the_client_id_survive_a_restart() {
    let (_tmp, root) = root();
    let dir = root.join("app");
    let (first_id, unix, ssh) = {
        let app = RoostApp::new(&dir, "Phone".into()).expect("app");
        let unix = app
            .add_host("dev".into(), "unix:/nonexistent/roost.sock".into())
            .expect("a unix host");
        let ssh = app
            .add_host("".into(), "charlie@popos".into())
            .expect("an ssh host");
        let refused = app.add_host("bad".into(), "popos".into());
        assert!(
            refused
                .as_ref()
                .is_err_and(|error| error.to_string().contains("user@host")),
            "{:?}",
            refused.map(|_| ())
        );
        app.trust_host_key(ssh.clone(), "SHA256:abc".into())
            .expect("trust");
        (app.client_id(), unix, ssh)
    };

    let app = RoostApp::new(&dir, "Phone".into()).expect("app again");
    assert_eq!(app.client_id(), first_id);
    let hosts = Recorder::new();
    app.hosts(hosts.sink());
    let views = hosts.wait("the saved hosts", |views| views.len() == 2);
    assert_eq!(
        views
            .iter()
            .map(|view| (view.id.as_str(), view.label.as_str(), view.target.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (unix.as_str(), "dev", "unix:/nonexistent/roost.sock"),
            (ssh.as_str(), "charlie@popos", "charlie@popos"),
        ]
    );
    let saved = std::fs::read_to_string(dir.join("hosts.json")).expect("hosts.json");
    assert!(saved.contains("SHA256:abc"), "{saved}");

    app.connect(unix.clone()).expect("connect");
    hosts.wait("a host with no session retrying", |views| {
        views
            .iter()
            .any(|view| view.id == unix && matches!(view.state, HostStateView::Retrying { .. }))
    });
    app.remove_host(unix).expect("remove");
    let counters = app.leak_counters();
    drop(app);
    assert_eq!(counters.counts(), LeakCounts::default());
    let saved = std::fs::read_to_string(dir.join("hosts.json")).expect("hosts.json");
    assert!(!saved.contains("nonexistent"), "{saved}");
}

#[test]
fn disconnecting_or_removing_a_host_ends_its_terminals_and_hands_the_size_on() {
    let (_tmp, root) = root();
    let _daemon = start(&root);
    let (phone, host, _) = connected_app(&root, "phone", "Phone A");
    let (watcher, watcher_host, _) = connected_app(&root, "watcher", "Watcher");
    let (_workspace, _, tab) = seed_tab(&phone, &host);
    let (_watching, watched) = attach(&watcher, &watcher_host, tab, false);

    for (verb, reason) in [("disconnect", "disconnected"), ("remove", "removed")] {
        let before = watched.len();
        let (handle, events) = attach(&phone, &host, tab, true);
        events.wait("our size", |event| {
            matches!(event, TerminalEvent::Owner { is_me: true, .. })
        });
        watched.wait_from(before, "the phone's size", |event| {
            matches!(event, TerminalEvent::Owner { is_me: false, label: Some(label), .. }
                if label == "Phone A")
        });

        let from = watched.len();
        match verb {
            "disconnect" => phone.disconnect(host.clone()).expect("disconnect"),
            _ => phone.remove_host(host.clone()).expect("remove"),
        }
        events.wait(reason, |event| {
            matches!(event, TerminalEvent::Status { state: AttachState::Ended, message: Some(why) }
                if why == reason)
        });
        watched.wait_from(from, "the size handed on", |event| {
            matches!(event, TerminalEvent::Owner { is_me: true, .. })
        });
        wait_until(&format!("{verb}: the stream detached"), || {
            phone.leak_counts().attach_pumps == 0
        });
        drop(handle);
        if verb == "disconnect" {
            phone.connect(host.clone()).expect("reconnect");
        }
    }
}

#[test]
fn input_typed_just_before_a_close_still_reaches_the_tab() {
    let (_tmp, root) = root();
    let _daemon = start(&root);
    let (app, host, _) = connected_app(&root, "app", "Phone");
    let (_workspace, _, tab) = seed_tab(&app, &host);

    let (handle, _events) = attach(&app, &host, tab, true);
    let mut screen = Screen::default();
    screen.wait(&handle, "the shell prompt", |rows| {
        rows.iter().any(|row| is_prompt(row))
    });
    // Queued for the owner thread, then closed at once: the close must
    // not drop the stream out from under the queued input.
    handle.text("echo closed-$((2+3))\r".into());
    handle.close();
    drop(handle);

    let (again, _) = attach(&app, &host, tab, true);
    let mut screen = Screen::default();
    screen.wait(&again, "closed-5", |rows| {
        rows.iter().any(|row| row.trim() == "closed-5")
    });
}

#[test]
fn an_attach_while_paused_is_refused_until_resume() {
    let (_tmp, root) = root();
    let _daemon = start(&root);
    let (app, host, _) = connected_app(&root, "app", "Phone");
    let (_workspace, _, tab) = seed_tab(&app, &host);

    app.app_paused();
    let refused = app.attach(host.clone(), tab, GEOMETRY, true);
    assert!(
        matches!(refused, Err(AppError::Paused)),
        "an attach in the background must not take the size"
    );
    assert_eq!(app.leak_counts().attachments, 0);

    app.app_resumed();
    let (handle, events) = attach(&app, &host, tab, true);
    events.wait("Live", |event| is_status(event, AttachState::Live));
    drop(handle);
}
