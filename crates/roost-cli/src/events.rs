//! `roostctl events`, and the event stream both it and `wait` read.
//!
//! The stream is served by whichever socket owns the tabs: an in-process
//! UI socket, or — under `local-backend = session`, or when `--socket`
//! names one — a session socket. [`resolve`] reads that off `identify`,
//! and [`open`] is the one place both verbs subscribe: stream on one
//! connection, then the identity check (and `wait`'s `tab.list`) on a
//! second, because a pushed connection answers nothing more.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;

use roost_ipc::client::{EventFrame, EventStream};
use roost_ipc::messages::{
    ops, EventEnvelope, IdentifyResult, SessionIdentify, SessionIdentifyParams, TabListResult,
    WireTabRef, SESSION_STOPPING_EVENT, STREAM_ENDED_EVENT,
};
use roost_ipc::IpcClient;

use crate::error::CliError;
use crate::UiSocket;

/// How many times a subscribe whose two legs reached different processes
/// is retried before giving up.
const IDENTITY_RETRIES: usize = 3;

/// Where the stream is, and how to ask that socket who it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Source {
    pub socket: PathBuf,
    pub identity: Identity,
    /// `identify.ops` names `events.subscribe`, or the stream is the
    /// session's. `false` is the Mac app or an older server.
    pub serves_stream: bool,
}

/// The call on the second connection that names the process the ack's
/// `session_id` came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Identity {
    /// `session.identify.session_id`.
    Session,
    /// `identify.instance_id`.
    Ui,
}

/// The stream's source, from the `identify` the resolved UI socket gave.
///
/// A socket whose `identify` carries no `instance_id` is either a session
/// dialled through `--socket` or a server with no stream at all, and only
/// the first has anything else to be identified by.
pub(crate) fn resolve(ui_socket: &Path, identify: &IdentifyResult) -> Source {
    if let Some(session) = &identify.local_session_socket {
        return Source {
            socket: PathBuf::from(session),
            identity: Identity::Session,
            serves_stream: true,
        };
    }
    Source {
        socket: ui_socket.to_path_buf(),
        identity: match identify.instance_id {
            Some(_) => Identity::Ui,
            None => Identity::Session,
        },
        serves_stream: identify
            .ops
            .iter()
            .flatten()
            .any(|op| op == ops::EVENTS_SUBSCRIBE),
    }
}

/// A subscribed stream, and the request connection whose identity matched
/// its ack.
pub(crate) struct Legs {
    pub stream: EventStream,
    pub conn: IpcClient,
    /// `tab.list`, taken on `conn` after the subscribe, when asked for.
    pub tabs: Option<TabListResult>,
}

/// What [`open`] reached.
pub(crate) enum Opened {
    Legs(Box<Legs>),
    /// The stream was acked, and then the second connection failed — its
    /// dial, the identity call, or the snapshot. `acked` is the ack's
    /// `session_id`, the process the stream was on.
    Dropped {
        acked: String,
        error: CliError,
    },
}

/// The second connection, once the stream is acked.
enum Second {
    Matched(IpcClient, Option<TabListResult>),
    /// Named some other process, or none.
    Other(Option<String>),
}

/// Subscribe on one connection, then identify (and snapshot) on another.
///
/// The order is the fence: every commit after the subscribe's revision
/// arrives on the stream, so a `tab.list` taken after it can only be
/// newer, and a caller discards the batches that snapshot already holds.
/// Two legs that name different processes mean it restarted in between,
/// where revisions restart too; that is retried, and then refused.
pub(crate) async fn open(source: &Source, snapshot: bool) -> Result<Opened, CliError> {
    let mut named = Vec::new();
    for _ in 0..=IDENTITY_RETRIES {
        let stream = subscribe(&source.socket).await?;
        let acked = stream.session_id().to_string();
        match second(source, &acked, snapshot).await {
            Ok(Second::Matched(conn, tabs)) => {
                return Ok(Opened::Legs(Box::new(Legs { stream, conn, tabs })))
            }
            Ok(Second::Other(incarnation)) => named.push(format!(
                "{acked} then {}",
                incarnation.as_deref().unwrap_or("nothing")
            )),
            Err(error @ CliError::Connection(_)) => return Ok(Opened::Dropped { acked, error }),
            Err(refused) => return Err(refused),
        }
    }
    Err(CliError::Connection(format!(
        "{}: the stream and the second connection reached different processes on every \
         attempt ({}); the server keeps restarting",
        source.socket.display(),
        named.join(", ")
    )))
}

async fn second(source: &Source, acked: &str, snapshot: bool) -> Result<Second, CliError> {
    let mut conn = crate::dial(&source.socket).await?;
    let incarnation = incarnation(&mut conn, source.identity).await?;
    // Before the snapshot, which another process could not fence — and
    // whose failure there would end the open instead of retrying it.
    if incarnation.as_deref() != Some(acked) {
        return Ok(Second::Other(incarnation));
    }
    let tabs = if snapshot {
        Some(crate::list_tabs(&mut conn).await?)
    } else {
        None
    };
    Ok(Second::Matched(conn, tabs))
}

async fn subscribe(socket: &Path) -> Result<EventStream, CliError> {
    Ok(crate::dial(socket).await?.subscribe_events().await?)
}

async fn incarnation(conn: &mut IpcClient, identity: Identity) -> Result<Option<String>, CliError> {
    Ok(match identity {
        Identity::Session => {
            let reply: SessionIdentify = conn
                .call(ops::SESSION_IDENTIFY, SessionIdentifyParams {})
                .await?;
            Some(reply.session_id)
        }
        Identity::Ui => crate::identify(conn).await?.instance_id,
    })
}

/// The tab a stream verb's `--tab` names. The stream is the local
/// workspace's (or the slot's, which bare ids already mean), so a host's
/// tab is refused rather than silently never matched.
pub(crate) fn local_tab(verb: &str, tab: WireTabRef) -> Result<i64, CliError> {
    match tab {
        WireTabRef::Local(id) => Ok(id),
        host @ WireTabRef::Host { .. } => Err(CliError::Usage(format!(
            "{verb} reads the local workspace's event stream, so it cannot watch the host \
             tab {host}; pass a bare tab id"
        ))),
    }
}

/// Whether an event is about `tab_id`. The catalog names a tab three ways:
/// `tab_id` (every per-tab event, and `active.changed`'s newly active tab),
/// the `tab` a `tab.opened` carries, and each of `tabs.reordered`'s
/// `tab_ids`.
pub(crate) fn names_tab(envelope: &EventEnvelope, tab_id: i64) -> bool {
    let is_tab = |id: &serde_json::Value| match id {
        serde_json::Value::String(id) => id.parse() == Ok(tab_id),
        serde_json::Value::Number(id) => id.as_i64() == Some(tab_id),
        _ => false,
    };
    let data = &envelope.data;
    data.get("tab_id").is_some_and(is_tab)
        || data
            .get("tab")
            .and_then(|tab| tab.get("id"))
            .is_some_and(is_tab)
        || data
            .get("tab_ids")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|ids| ids.iter().any(is_tab))
}

/// One line of `roostctl events`: an event envelope, with the revision of
/// the commit it rode in, or a terminal envelope, which has none.
#[derive(Serialize)]
struct Line<'a> {
    event: &'a str,
    data: &'a serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    revision: Option<u64>,
}

/// `roostctl events [--tab N]`.
pub(crate) async fn run(ui: &mut UiSocket<'_>, tab: Option<String>) -> Result<i32, CliError> {
    stream_to(ui, tab, &mut std::io::stdout()).await
}

async fn stream_to(
    ui: &mut UiSocket<'_>,
    tab: Option<String>,
    out: &mut impl Write,
) -> Result<i32, CliError> {
    let filter = tab
        .as_deref()
        .map(crate::parse_tab_flag)
        .transpose()?
        .map(|tab| local_tab("events", tab))
        .transpose()?;
    let identify = crate::identify(ui.client().await?).await?;
    // No poll fallback: a server without the stream refuses the subscribe
    // in its own words, and that refusal is the answer.
    let source = resolve(ui.socket_path(), &identify);
    let mut stream = match open(&source, false).await? {
        Opened::Legs(legs) => legs.stream,
        Opened::Dropped { error, .. } => return Err(error),
    };
    loop {
        match stream.next().await {
            Ok(Some(EventFrame::Batch(batch))) => {
                for envelope in &batch.events {
                    if filter.is_some_and(|tab| !names_tab(envelope, tab)) {
                        continue;
                    }
                    let line = Line {
                        event: &envelope.event,
                        data: &envelope.data,
                        revision: Some(batch.revision),
                    };
                    if !emit(out, &line)? {
                        return Ok(0);
                    }
                }
            }
            Ok(Some(EventFrame::Stopping(stopping))) => {
                return last_line(out, SESSION_STOPPING_EVENT, &stopping.reason)
            }
            Ok(Some(EventFrame::Ended(ended))) => {
                return last_line(out, STREAM_ENDED_EVENT, &ended.reason)
            }
            Ok(None) => {
                return Err(CliError::Connection(format!(
                    "{}: the event stream closed without saying why",
                    source.socket.display()
                )))
            }
            Err(error) => {
                return Err(CliError::Connection(format!(
                    "{}: {error}",
                    source.socket.display()
                )))
            }
        }
    }
}

/// The terminal envelope, which ends the stream on purpose: exit 0.
fn last_line(out: &mut impl Write, event: &str, reason: &str) -> Result<i32, CliError> {
    let data = serde_json::json!({ "reason": reason });
    emit(
        out,
        &Line {
            event,
            data: &data,
            revision: None,
        },
    )?;
    Ok(0)
}

/// Write one line and flush it. `false` when the reader has gone — `events
/// | head -n1` is a finished stream, not a failure.
fn emit(out: &mut impl Write, line: &Line<'_>) -> Result<bool, CliError> {
    let mut encoded = serde_json::to_vec(line)
        .map_err(|e| CliError::Failed(format!("encode an event line: {e}")))?;
    encoded.push(b'\n');
    match out.write_all(&encoded).and_then(|()| out.flush()) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(false),
        Err(e) => Err(CliError::Failed(format!("write an event line: {e}"))),
    }
}

/// A stand-in for the socket a stream verb reads: a tiny workspace of tab
/// states behind a real Unix socket, which serves `events.subscribe`,
/// `tab.list` and the rest over the same revisions, so the order a client
/// takes its two legs in shows up in what it sees.
#[cfg(test)]
pub(crate) mod fake {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use serde_json::{json, Value};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::mpsc;

    use roost_ipc::messages::{RawRequest, Response};

    /// Whether a hook runs before the request is answered or after.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Phase {
        Before,
        After,
    }

    type Hook = Box<dyn FnMut(&mut World, &str, Phase) + Send>;
    type Answer = (
        Result<Value, (String, String)>,
        Option<mpsc::UnboundedReceiver<Push>>,
    );

    enum Push {
        Line(String),
        Close,
    }

    /// What a hook can have the fake do with a request instead of
    /// answering it.
    pub(crate) enum Instead {
        /// Close the connection without a reply.
        HangUp,
        /// Refuse it with this code and message.
        Refuse(&'static str, &'static str),
    }

    pub(crate) struct World {
        pub revision: u64,
        /// Tab id → state, all in project 1.
        pub tabs: BTreeMap<i64, &'static str>,
        /// `identify`'s whole result.
        pub identify: Value,
        /// What the subscribe ack names.
        pub ack_id: String,
        /// `session.identify.session_id`.
        pub session_id: String,
        /// Every tab's viewport.
        pub dump: String,
        /// How many revisions `tab.list` reports behind the workspace's.
        pub snapshot_behind: u64,
        /// Set by a [`Phase::Before`] hook: the request it ran for is not
        /// answered.
        pub instead: Option<Instead>,
        /// Every request, as `(connection, op)`.
        pub log: Vec<(u64, String)>,
        subscribers: Vec<mpsc::UnboundedSender<Push>>,
        hook: Option<Hook>,
    }

    impl World {
        /// One commit, pushed to every subscriber.
        pub fn commit(&mut self, events: &Value) {
            self.revision += 1;
            let batch = json!({ "revision": self.revision, "events": events });
            self.push(&batch.to_string());
        }

        /// A raw line to every subscriber.
        pub fn push(&mut self, line: &str) {
            self.subscribers
                .retain(|subscriber| subscriber.send(Push::Line(line.to_string())).is_ok());
        }

        pub fn set_state(&mut self, tab: i64, state: &'static str) {
            self.tabs.insert(tab, state);
            self.commit(&json!([{
                "event": "tab.state_changed",
                "data": { "tab_id": tab.to_string(), "state": state },
            }]));
        }

        pub fn retitle(&mut self, tab: i64) {
            self.commit(&json!([{
                "event": "tab.title_changed",
                "data": { "tab_id": tab.to_string(), "title": "t" },
            }]));
        }

        /// End every stream: `last` (if any), then the close.
        pub fn end_streams(&mut self, last: Option<&Value>) {
            for subscriber in self.subscribers.drain(..) {
                if let Some(last) = last {
                    let _ = subscriber.send(Push::Line(last.to_string()));
                }
                let _ = subscriber.send(Push::Close);
            }
        }

        pub fn ops(&self) -> Vec<&str> {
            self.log.iter().map(|(_, op)| op.as_str()).collect()
        }

        pub fn count(&self, op: &str) -> usize {
            self.log.iter().filter(|(_, logged)| logged == op).count()
        }

        /// `None` hangs up.
        fn handle(&mut self, conn: u64, op: &str) -> Option<Answer> {
            self.log.push((conn, op.to_string()));
            let mut hook = self.hook.take();
            if let Some(hook) = hook.as_mut() {
                hook(self, op, Phase::Before);
            }
            let answered = match self.instead.take() {
                Some(Instead::HangUp) => None,
                Some(Instead::Refuse(code, message)) => {
                    Some((Err((code.into(), message.into())), None))
                }
                None => Some(self.answer(op)),
            };
            if let Some(hook) = hook.as_mut() {
                hook(self, op, Phase::After);
            }
            self.hook = hook;
            answered
        }

        fn answer(&mut self, op: &str) -> Answer {
            let reply = match op {
                "identify" => Ok(self.identify.clone()),
                "session.identify" => Ok(json!({
                    "app_version": "0", "session_protocol": 6, "payload_kinds": [],
                    "libghostty_build": "x", "session_id": self.session_id, "started_at": "t",
                })),
                "events.subscribe" => {
                    let serves = self.identify["ops"]
                        .as_array()
                        .is_some_and(|ops| ops.iter().any(|op| op == "events.subscribe"));
                    if !serves {
                        Err(("not-implemented".into(), "no stream here".into()))
                    } else if !self.identify["local_backend_switch"].is_null() {
                        Err((
                            roost_ipc::codes::BUSY.into(),
                            roost_ipc::local_route::SWITCH_BUSY_MESSAGE.into(),
                        ))
                    } else {
                        let (tx, rx) = mpsc::unbounded_channel();
                        self.subscribers.push(tx);
                        let ack = json!({ "revision": self.revision, "session_id": self.ack_id });
                        return (Ok(ack), Some(rx));
                    }
                }
                "tab.list" => Ok(json!({
                    "projects": [{
                        "id": "1", "name": "p", "cwd": "/", "position": 0, "created_at": 0,
                        "tabs": self.tabs.iter().map(|(id, state)| json!({
                            "id": id.to_string(), "project_id": "1", "title": "t", "cwd": "/",
                            "state": state, "has_notification": false, "is_active": false,
                            "user_titled": false, "position": 0, "created_at": 0,
                            "last_active": 0, "hook_active": false,
                        })).collect::<Vec<_>>(),
                    }],
                    "revision": self.revision - self.snapshot_behind,
                })),
                "tab.dump" => Ok(json!({ "cols": 80, "rows": 1, "rows_text": [self.dump] })),
                other => Err(("unknown-op".into(), format!("not faked: {other}"))),
            };
            (reply, None)
        }
    }

    pub(crate) struct Fake {
        pub socket: PathBuf,
        world: Arc<Mutex<World>>,
    }

    pub(crate) fn identify(ops: &[&str], instance_id: Option<&str>) -> Value {
        let mut identify = json!({
            "socket_path": "/fake", "pid": 1, "active_project_id": "1", "active_tab_id": "7",
            "app_label": "Roost", "app_id": "test", "ui_version": "0", "protocol_version": 1,
        });
        if !ops.is_empty() {
            identify["ops"] = json!(ops);
        }
        if let Some(instance_id) = instance_id {
            identify["instance_id"] = json!(instance_id);
        }
        identify
    }

    impl Fake {
        /// An in-process UI socket: `instance_id` and the stream.
        pub fn ui(tag: &str) -> Self {
            Self::start(
                tag,
                identify(&["identify", "events.subscribe", "tab.list"], Some("ui-1")),
                "ui-1",
            )
        }

        /// A session socket: the stream, `session.identify`, no `instance_id`.
        pub fn session(tag: &str) -> Self {
            let ops = [
                "identify",
                "session.identify",
                "events.subscribe",
                "tab.list",
            ];
            Self::start(tag, identify(&ops, None), "session-1")
        }

        /// The Mac app, or an older Roost: no `ops`, no stream.
        pub fn without_stream(tag: &str) -> Self {
            Self::start(tag, identify(&[], None), "none")
        }

        fn start(tag: &str, identify: Value, id: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "roostctl-stream-{tag}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("a scratch dir");
            let socket = dir.join("s.sock");
            let _ = std::fs::remove_file(&socket);
            let world = Arc::new(Mutex::new(World {
                revision: 40,
                tabs: BTreeMap::from([(7, "running")]),
                identify,
                ack_id: id.to_string(),
                session_id: id.to_string(),
                dump: String::new(),
                snapshot_behind: 0,
                instead: None,
                log: Vec::new(),
                subscribers: Vec::new(),
                hook: None,
            }));
            let listener = tokio::net::UnixListener::bind(&socket).expect("bind the fake");
            let shared = world.clone();
            tokio::spawn(async move {
                let mut conn = 0;
                while let Ok((stream, _)) = listener.accept().await {
                    conn += 1;
                    tokio::spawn(serve(conn, stream, shared.clone()));
                }
            });
            Self { socket, world }
        }

        pub fn with<R>(&self, f: impl FnOnce(&mut World) -> R) -> R {
            f(&mut self.world.lock().unwrap())
        }

        pub fn hook(&self, hook: impl FnMut(&mut World, &str, Phase) + Send + 'static) {
            self.with(|world| world.hook = Some(Box::new(hook)));
        }

        /// Runs `change` once, before `op`'s `nth` request (counting from 1)
        /// is answered — or after, with [`Phase::After`]. Adds to the hooks
        /// already set rather than replacing them.
        pub fn on(
            &self,
            op: &'static str,
            nth: usize,
            phase: Phase,
            change: impl FnOnce(&mut World) + Send + 'static,
        ) {
            let mut change = Some(change);
            let mut earlier = self.with(|world| world.hook.take());
            self.hook(move |world, seen, at| {
                if let Some(earlier) = earlier.as_mut() {
                    earlier(world, seen, at);
                }
                if seen == op && at == phase && world.count(op) == nth {
                    if let Some(change) = change.take() {
                        change(world);
                    }
                }
            });
        }

        pub fn socket(&self) -> String {
            self.socket.display().to_string()
        }
    }

    async fn serve(conn: u64, stream: tokio::net::UnixStream, world: Arc<Mutex<World>>) {
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let request: RawRequest = serde_json::from_str(&line).expect("a request frame");
            let Some((reply, pushes)) = world.lock().unwrap().handle(conn, &request.op) else {
                return;
            };
            let response = match reply {
                Ok(result) => Response::ok(request.id, result),
                Err((code, message)) => Response::err(request.id, code, message),
            };
            let mut frame = serde_json::to_vec(&response).unwrap();
            frame.push(b'\n');
            if write.write_all(&frame).await.is_err() {
                return;
            }
            let Some(mut pushes) = pushes else { continue };
            while let Some(Push::Line(line)) = pushes.recv().await {
                if write
                    .write_all(format!("{line}\n").as_bytes())
                    .await
                    .is_err()
                {
                    return;
                }
            }
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{Fake, Instead, Phase};
    use super::*;
    use roost_ipc::target::TargetSelector;
    use serde_json::json;

    async fn events(
        fake: &Fake,
        tab: Option<&str>,
    ) -> (Result<i32, CliError>, Vec<serde_json::Value>) {
        let selector = TargetSelector {
            socket_override: Some(fake.socket.clone()),
            kind_override: None,
        };
        let mut out = Vec::new();
        let mut ui = UiSocket::new(&selector);
        let run = stream_to(&mut ui, tab.map(str::to_string), &mut out);
        let exit = tokio::time::timeout(std::time::Duration::from_secs(10), run)
            .await
            .expect("events never ended");
        let lines = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).expect("one JSON document per line"))
            .collect();
        (exit, lines)
    }

    fn stopping() -> serde_json::Value {
        json!({ "event": "session.stopping", "data": { "reason": "stop" } })
    }

    /// Two commits, then the session stops: one naming tab 7 twice and
    /// tab 9 once, one naming no tab at all.
    fn two_commits_then_stop(fake: &Fake) {
        fake.on("events.subscribe", 1, Phase::After, |world| {
            world.commit(&json!([
                { "event": "tab.state_changed", "data": { "tab_id": "7", "state": "idle" } },
                { "event": "tab.state_changed", "data": { "tab_id": "9", "state": "idle" } },
                { "event": "tab.title_changed", "data": { "tab_id": "7", "title": "x" } },
            ]));
            world.commit(
                &json!([{ "event": "projects.reordered", "data": { "project_ids": ["1"] } }]),
            );
            world.end_streams(Some(&stopping()));
        });
    }

    #[tokio::test]
    async fn each_event_is_one_line_with_its_commits_revision_until_the_stop() {
        let fake = Fake::ui("lines");
        two_commits_then_stop(&fake);
        let (exit, lines) = events(&fake, None).await;
        assert_eq!(exit, Ok(0));
        let shape: Vec<(&str, Option<u64>)> = lines
            .iter()
            .map(|line| (line["event"].as_str().unwrap(), line["revision"].as_u64()))
            .collect();
        assert_eq!(
            shape,
            [
                ("tab.state_changed", Some(41)),
                ("tab.state_changed", Some(41)),
                ("tab.title_changed", Some(41)),
                ("projects.reordered", Some(42)),
                ("session.stopping", None),
            ]
        );
        assert_eq!(lines[4], stopping());
        assert_eq!(lines[1]["data"], json!({ "tab_id": "9", "state": "idle" }));
    }

    #[tokio::test]
    async fn tab_keeps_only_the_events_that_name_that_tab() {
        let fake = Fake::ui("filter");
        two_commits_then_stop(&fake);
        let (exit, lines) = events(&fake, Some("7")).await;
        assert_eq!(exit, Ok(0));
        let kept: Vec<&str> = lines
            .iter()
            .map(|line| line["event"].as_str().unwrap())
            .collect();
        assert_eq!(
            kept,
            ["tab.state_changed", "tab.title_changed", "session.stopping"]
        );
        assert!(
            lines[..2].iter().all(|line| line["data"]["tab_id"] == "7"),
            "{lines:?}"
        );
    }

    #[tokio::test]
    async fn tab_keeps_a_reorder_that_lists_that_tab() {
        let fake = Fake::ui("filter-reorder");
        fake.on("events.subscribe", 1, Phase::After, |world| {
            world.commit(&json!([
                { "event": "tabs.reordered", "data": { "project_id": "1", "tab_ids": ["9", "7"] } },
            ]));
            world.commit(&json!([
                { "event": "tabs.reordered", "data": { "project_id": "2", "tab_ids": ["70", "9"] } },
            ]));
            world.end_streams(Some(&stopping()));
        });
        let (exit, lines) = events(&fake, Some("7")).await;
        assert_eq!(exit, Ok(0));
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0]["event"], "tabs.reordered");
        assert_eq!(lines[0]["data"]["tab_ids"], json!(["9", "7"]));
        assert_eq!(lines[1], stopping());
    }

    #[test]
    fn a_tab_is_named_by_its_tab_id_the_tab_an_open_carries_or_a_reorders_list() {
        let envelope = |event: &str, data| EventEnvelope {
            event: event.into(),
            data,
        };
        assert!(names_tab(
            &envelope("tab.closed", json!({ "tab_id": "7" })),
            7
        ));
        assert!(names_tab(
            &envelope("tab.opened", json!({ "tab": { "id": "7" } })),
            7
        ));
        assert!(!names_tab(
            &envelope("tab.closed", json!({ "tab_id": "70" })),
            7
        ));
        assert!(!names_tab(
            &envelope("project.deleted", json!({ "project_id": "7" })),
            7
        ));
        assert!(names_tab(
            &envelope(
                "active.changed",
                json!({ "project_id": "1", "tab_id": "7" })
            ),
            7
        ));
        assert!(names_tab(
            &envelope(
                "tabs.reordered",
                json!({ "project_id": "1", "tab_ids": ["9", "7"] })
            ),
            7
        ));
        assert!(!names_tab(
            &envelope(
                "tabs.reordered",
                json!({ "project_id": "7", "tab_ids": ["70", "9"] })
            ),
            7
        ));
    }

    #[tokio::test]
    async fn a_stream_ended_by_a_backend_switch_is_the_last_line_and_exits_0() {
        let fake = Fake::ui("ended");
        fake.on("events.subscribe", 1, Phase::After, |world| {
            world.retitle(7);
            world.end_streams(Some(&json!({
                "event": "stream.ended", "data": { "reason": "backend-switch" },
            })));
        });
        let (exit, lines) = events(&fake, None).await;
        assert_eq!(exit, Ok(0));
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(
            lines[1],
            json!({ "event": "stream.ended", "data": { "reason": "backend-switch" } })
        );
    }

    #[tokio::test]
    async fn a_stream_that_drops_or_skips_a_revision_is_a_connection_failure() {
        let dropped = Fake::ui("drop");
        dropped.on("events.subscribe", 1, Phase::After, |world| {
            world.retitle(7);
            world.end_streams(None);
        });
        let (exit, lines) = events(&dropped, None).await;
        assert!(matches!(exit, Err(CliError::Connection(_))), "{exit:?}");
        assert_eq!(lines.len(), 1, "the commit before the drop still printed");

        let gapped = Fake::ui("gap");
        gapped.on("events.subscribe", 1, Phase::After, |world| {
            world.push(&json!({ "revision": world.revision + 2, "events": [] }).to_string());
        });
        let (exit, _) = events(&gapped, None).await;
        let Err(CliError::Connection(message)) = exit else {
            panic!("{exit:?}")
        };
        assert!(message.contains("skipped a revision"), "{message}");
    }

    #[tokio::test]
    async fn a_server_without_the_stream_refuses_in_its_own_words() {
        let fake = Fake::without_stream("refused");
        let (exit, lines) = events(&fake, None).await;
        assert_eq!(
            exit,
            Err(CliError::Server {
                code: "not-implemented".into(),
                message: "no stream here".into()
            })
        );
        assert!(lines.is_empty());
        assert_eq!(
            fake.with(|world| world.ops().join(" ")),
            "identify events.subscribe"
        );
    }

    #[tokio::test]
    async fn under_a_session_backend_the_stream_is_the_sessions() {
        let session = Fake::session("session-side");
        two_commits_then_stop(&session);
        let ui = Fake::ui("ui-side");
        ui.with(|world| {
            world.identify["local_session_socket"] = json!(session.socket());
        });
        let (exit, lines) = events(&ui, Some("7")).await;
        assert_eq!(exit, Ok(0));
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(ui.with(|world| world.ops().join(" ")), "identify");
        assert_eq!(
            session.with(|world| world.ops().join(" ")),
            "events.subscribe session.identify"
        );
    }

    #[tokio::test]
    async fn a_host_tab_is_refused_before_anything_is_dialled() {
        let fake = Fake::ui("host-tab");
        let (exit, _) = events(&fake, Some("h1.7")).await;
        assert!(matches!(exit, Err(CliError::Usage(_))), "{exit:?}");
        assert!(fake.with(|world| world.log.is_empty()));
    }

    #[tokio::test]
    async fn legs_from_two_processes_are_retried_three_times_then_refused() {
        let fake = Fake::ui("identity");
        fake.with(|world| world.ack_id = "ui-0".into());
        let source = resolve(
            &fake.socket,
            &serde_json::from_value(fake.with(|w| w.identify.clone())).unwrap(),
        );
        let Err(CliError::Connection(message)) = open(&source, false).await.map(|_| ()) else {
            panic!("the identity never matched")
        };
        assert!(message.contains("different processes"), "{message}");
        assert_eq!(fake.with(|world| world.count("events.subscribe")), 4);
        assert_eq!(fake.with(|world| world.count("identify")), 4);
    }

    /// The identity is compared before the snapshot is asked for, so a
    /// process that hangs up on that snapshot is retried like any other
    /// mismatch rather than ending the open.
    #[tokio::test]
    async fn a_mismatched_second_leg_is_never_asked_for_a_snapshot() {
        let fake = Fake::ui("identity-before-list");
        fake.with(|world| world.ack_id = "ui-0".into());
        fake.hook(|world, op, phase| {
            if op == "tab.list" && phase == Phase::Before {
                world.instead = Some(Instead::HangUp);
            }
        });
        let source = resolve(
            &fake.socket,
            &serde_json::from_value(fake.with(|w| w.identify.clone())).unwrap(),
        );
        let message = match open(&source, true).await {
            Err(CliError::Connection(message)) => message,
            Ok(Opened::Dropped { error, .. }) => panic!("ended on the hung-up snapshot: {error:?}"),
            Ok(Opened::Legs(_)) => panic!("the identity never matched, and still opened"),
            Err(error) => panic!("{error:?}"),
        };
        assert!(message.contains("different processes"), "{message}");
        assert_eq!(fake.with(|world| world.count("events.subscribe")), 4);
        assert_eq!(fake.with(|world| world.count("tab.list")), 0);
    }

    #[tokio::test]
    async fn a_restart_between_the_legs_is_retried_against_the_new_process() {
        let fake = Fake::session("restart");
        fake.with(|world| world.ack_id = "session-0".into());
        fake.on("session.identify", 1, Phase::After, |world| {
            world.ack_id = world.session_id.clone();
        });
        let source = resolve(
            &fake.socket,
            &serde_json::from_value(fake.with(|w| w.identify.clone())).unwrap(),
        );
        assert_eq!(source.identity, Identity::Session);
        let Ok(Opened::Legs(legs)) = open(&source, true).await else {
            panic!("the second attempt matches");
        };
        assert_eq!(legs.stream.session_id(), "session-1");
        assert_eq!(
            fake.with(|world| world.ops().join(" ")),
            "events.subscribe session.identify events.subscribe session.identify tab.list",
            "no snapshot is asked of the attempt that reached another process"
        );
    }

    #[test]
    fn the_source_is_the_session_the_ui_names_else_the_socket_that_streams() {
        let identify = |value: serde_json::Value| -> IdentifyResult {
            let mut base = super::fake::identify(&[], None);
            base.as_object_mut()
                .unwrap()
                .extend(value.as_object().unwrap().clone());
            serde_json::from_value(base).unwrap()
        };
        let ui = Path::new("/ui.sock");
        assert_eq!(
            resolve(
                ui,
                &identify(json!({ "local_session_socket": "/s.sock", "ops": ["identify"] }))
            ),
            Source {
                socket: "/s.sock".into(),
                identity: Identity::Session,
                serves_stream: true
            }
        );
        assert_eq!(
            resolve(
                ui,
                &identify(json!({ "ops": ["events.subscribe"], "instance_id": "a" }))
            ),
            Source {
                socket: ui.into(),
                identity: Identity::Ui,
                serves_stream: true
            }
        );
        assert_eq!(
            resolve(
                ui,
                &identify(json!({ "ops": ["events.subscribe", "session.identify"] }))
            ),
            Source {
                socket: ui.into(),
                identity: Identity::Session,
                serves_stream: true
            }
        );
        assert_eq!(
            resolve(ui, &identify(json!({}))),
            Source {
                socket: ui.into(),
                identity: Identity::Session,
                serves_stream: false
            }
        );
        assert!(
            !resolve(
                ui,
                &identify(json!({ "ops": ["tab.list"], "instance_id": "a" }))
            )
            .serves_stream
        );
    }
}
