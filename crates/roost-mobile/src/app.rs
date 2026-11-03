//! `RoostApp`: plan 089 C5 in plain Rust. Saved hosts and their
//! connections, each host's workspace, control-plane ops, and attached
//! terminals, reported through callback sinks the FFI layer turns into
//! Dart streams.
//!
//! The app owns its tokio runtime and its terminal owner thread. Teardown
//! runs in dependency order — streams detached and host connections shut
//! down, their tasks given a moment to finish, the owner thread stopped,
//! the runtime last — so a `DETACH` and a clean close still reach each
//! session. Call the app from any thread but its own runtime's: the few
//! calls that wait (`app_paused`, a frame pull, teardown) block.

// The public methods take owned values on purpose: they are the Rust half
// of the FFI surface, whose generated bindings hand over owned strings.
#![allow(clippy::needless_pass_by_value)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use base64::Engine;
use roost_client::attach::{AttachRequest, AttachStream};
use roost_client::dial::{Dialer, UnixDialer};
use roost_client::host::{HostCallError, HostClient, HostClientConfig, HostEvent};
use roost_ssh::{HostKeyPolicy, TailscaleSshDialer};
use roost_ui_model::theme::Theme;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::handle::{self, Attachment, TerminalHandle};
use crate::live::Live;
use crate::owner::{OwnerConfig, TerminalId, TerminalOwner};
use crate::registry::{self, HostRecord, Target};
use crate::terminal::Geometry;
use crate::views::{self, HostStateView, HostView, WorkspaceView};

/// A Dart stream, as the FFI layer hands it in.
pub type Sink<T> = Box<dyn Fn(T) + Send + Sync>;
pub(crate) type SharedSink<T> = Arc<dyn Fn(T) + Send + Sync>;

/// How long a pause waits for its `tab.release_size` answers, and
/// teardown for its tasks: a phone going to the background gets a few
/// moments, not an open-ended wait.
const PAUSE_BUDGET: Duration = Duration::from_secs(2);
const TEARDOWN_BUDGET: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("no saved host {0}")]
    UnknownHost(String),
    #[error("host {0} is not connected")]
    NotConnected(String),
    #[error("{0}")]
    InvalidTarget(String),
    #[error("params are not a JSON object: {0}")]
    InvalidParams(String),
    #[error(transparent)]
    Call(#[from] HostCallError),
    #[error("{what}: {source}")]
    Io {
        what: &'static str,
        source: std::io::Error,
    },
}

/// Live resources, for leak checks. Readable after the app is gone.
#[derive(Debug, Default)]
pub struct Counters {
    hosts: Arc<AtomicUsize>,
    host_pumps: Arc<AtomicUsize>,
    pub(crate) attach_pumps: Arc<AtomicUsize>,
    pub(crate) attachments: Arc<AtomicUsize>,
    terminals: Arc<AtomicUsize>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LeakCounts {
    /// Host connection tasks (`HostClient`s).
    pub hosts: usize,
    /// Tasks forwarding a host's events.
    pub host_pumps: usize,
    /// Tasks forwarding an attach stream's events.
    pub attach_pumps: usize,
    /// Terminal handles' shared state.
    pub attachments: usize,
    /// Tab terminals on the owner thread.
    pub terminals: usize,
}

impl Counters {
    pub fn counts(&self) -> LeakCounts {
        let read = |count: &Arc<AtomicUsize>| count.load(Ordering::SeqCst);
        LeakCounts {
            hosts: read(&self.hosts),
            host_pumps: read(&self.host_pumps),
            attach_pumps: read(&self.attach_pumps),
            attachments: read(&self.attachments),
            terminals: read(&self.terminals),
        }
    }
}

/// One saved host and, while it is wanted, its connection.
struct HostSlot {
    record: HostRecord,
    state: HostStateView,
    /// Kept across connects, so trusting a key re-pins the dialer the
    /// connection already uses.
    ssh: Option<Arc<TailscaleSshDialer>>,
    conn: Option<Connection>,
    /// Moves with every connect and disconnect: a host pump that outlived
    /// its connection reports nothing.
    serial: u64,
    workspace_sink: Option<SharedSink<WorkspaceView>>,
}

struct Connection {
    client: HostClient,
    dialer: Arc<dyn Dialer>,
    _live: Live,
}

impl HostSlot {
    fn view(&self) -> HostView {
        let (waiting_tabs, working_tabs) = match (&self.conn, &self.state) {
            (Some(conn), HostStateView::Connected) => {
                views::agent_counts(&conn.client.mirror().read())
            }
            _ => (0, 0),
        };
        HostView {
            id: self.record.id.clone(),
            label: self.record.label.clone(),
            target: self.record.target.clone(),
            state: self.state.clone(),
            waiting_tabs,
            working_tabs,
        }
    }

    fn workspace(&self) -> WorkspaceView {
        self.conn
            .as_ref()
            .map(|conn| views::workspace(&conn.client.mirror().read()))
            .unwrap_or_default()
    }
}

pub(crate) struct Inner {
    data_dir: PathBuf,
    config: HostClientConfig,
    pub(crate) rt: tokio::runtime::Handle,
    pub(crate) owner: Arc<TerminalOwner>,
    hosts: Mutex<Vec<HostSlot>>,
    hosts_sink: Mutex<Option<SharedSink<Vec<HostView>>>>,
    attachments: Mutex<HashMap<TerminalId, Arc<Attachment>>>,
    pub(crate) counters: Arc<Counters>,
    pub(crate) closed: AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Inner {
    fn with_slot<T>(
        &self,
        host_id: &str,
        f: impl FnOnce(&mut HostSlot) -> T,
    ) -> Result<T, AppError> {
        let mut hosts = lock(&self.hosts);
        let slot = hosts
            .iter_mut()
            .find(|slot| slot.record.id == host_id)
            .ok_or_else(|| AppError::UnknownHost(host_id.to_string()))?;
        Ok(f(slot))
    }

    fn emit_hosts(&self) {
        let views: Vec<HostView> = lock(&self.hosts).iter().map(HostSlot::view).collect();
        let sink = lock(&self.hosts_sink).clone();
        if let Some(sink) = sink {
            sink(views);
        }
    }

    fn emit_workspace(&self, host_id: &str) {
        let Ok(Some((sink, view))) = self.with_slot(host_id, |slot| {
            slot.workspace_sink
                .clone()
                .map(|sink| (sink, slot.workspace()))
        }) else {
            return;
        };
        sink(view);
    }

    fn persist(&self) -> Result<(), AppError> {
        let records: Vec<HostRecord> = lock(&self.hosts)
            .iter()
            .map(|slot| slot.record.clone())
            .collect();
        registry::save_hosts(&self.data_dir, &records).map_err(|source| AppError::Io {
            what: "saving hosts.json",
            source,
        })
    }

    fn host_event(&self, host_id: &str, serial: u64, event: HostEvent) {
        let workspace_moved = self
            .with_slot(host_id, |slot| {
                if slot.serial != serial || slot.conn.is_none() {
                    return false;
                }
                match event {
                    HostEvent::State(state) => {
                        let connected = state == roost_client::host::HostState::Connected;
                        let remote = slot.ssh.as_ref().and_then(|ssh| ssh.last_remote_error());
                        let host = match Target::parse(&slot.record.target) {
                            Ok(Target::Ssh(target)) => target.host_port(),
                            _ => slot.record.label.clone(),
                        };
                        slot.state = views::host_state(state, remote, &host);
                        connected
                    }
                    HostEvent::Workspace => true,
                    HostEvent::Notification { .. } => false,
                }
            })
            .unwrap_or(false);
        if workspace_moved {
            self.emit_workspace(host_id);
        }
        self.emit_hosts();
    }

    /// A new stream for `tab_id` on `host_id`'s connection.
    pub(crate) fn spawn_stream(
        &self,
        host_id: &str,
        tab_id: i64,
        geometry: Geometry,
        take: bool,
    ) -> Result<
        (
            AttachStream,
            UnboundedReceiver<roost_client::attach::AttachEvent>,
        ),
        AppError,
    > {
        self.with_slot(host_id, |slot| {
            let conn = slot
                .conn
                .as_ref()
                .ok_or_else(|| AppError::NotConnected(host_id.to_string()))?;
            Ok(AttachStream::spawn(
                &self.rt,
                Arc::clone(&conn.dialer),
                &conn.client,
                AttachRequest {
                    tab_id,
                    cols: geometry.cols,
                    rows: geometry.rows,
                    cell_w_px: geometry.cell_w_px,
                    cell_h_px: geometry.cell_h_px,
                    take,
                },
            ))
        })?
    }

    pub(crate) fn forget_attachment(&self, id: TerminalId) {
        lock(&self.attachments).remove(&id);
    }

    fn call(
        &self,
        host_id: &str,
        op: &str,
        params: serde_json::Value,
    ) -> Result<
        futures::future::BoxFuture<'static, Result<serde_json::Value, HostCallError>>,
        AppError,
    > {
        self.with_slot(host_id, |slot| {
            slot.conn
                .as_ref()
                .map(|conn| conn.client.call(op, params))
                .ok_or_else(|| AppError::NotConnected(host_id.to_string()))
        })?
    }
}

/// The dialer a target needs. An SSH dialer is made once per host and
/// kept, so trusting a key later re-pins the one in use.
fn dialer_for(
    record: &HostRecord,
    ssh: &mut Option<Arc<TailscaleSshDialer>>,
) -> Result<Arc<dyn Dialer>, AppError> {
    match Target::parse(&record.target).map_err(AppError::InvalidTarget)? {
        Target::Unix(path) => Ok(Arc::new(UnixDialer(path))),
        Target::Ssh(target) => {
            let dialer = ssh.get_or_insert_with(|| {
                Arc::new(TailscaleSshDialer::new(
                    target,
                    HostKeyPolicy {
                        pinned: record.pinned_host_key.clone(),
                    },
                ))
            });
            Ok(Arc::clone(dialer) as Arc<dyn Dialer>)
        }
    }
}

async fn host_pump(
    inner: Weak<Inner>,
    host_id: String,
    serial: u64,
    mut events: UnboundedReceiver<HostEvent>,
    _live: Live,
) {
    while let Some(event) = events.recv().await {
        let Some(inner) = inner.upgrade() else {
            return;
        };
        inner.host_event(&host_id, serial, event);
    }
}

pub struct RoostApp {
    inner: Arc<Inner>,
    /// Dropped last, explicitly, in `Drop`.
    rt: Option<tokio::runtime::Runtime>,
}

impl RoostApp {
    /// Load the saved hosts and this install's `client_id` from
    /// `data_dir`, creating it. No host is dialed until [`Self::connect`].
    pub fn new(data_dir: impl Into<PathBuf>, device_label: String) -> Result<Self, AppError> {
        let data_dir = data_dir.into();
        let io = |what| move |source| AppError::Io { what, source };
        std::fs::create_dir_all(&data_dir).map_err(io("creating the data directory"))?;
        let client_id = registry::client_id(&data_dir).map_err(io("reading client_id"))?;
        let records = registry::load_hosts(&data_dir).map_err(io("reading hosts.json"))?;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("roost-mobile-rt")
            .enable_all()
            .build()
            .map_err(io("starting the runtime"))?;
        let owner = TerminalOwner::spawn(OwnerConfig {
            client_id: client_id.clone(),
            theme: Theme::default(),
        })
        .map_err(io("starting the terminal thread"))?;
        let counters = Arc::new(Counters {
            terminals: owner.live_counter(),
            ..Counters::default()
        });
        let label: String = device_label.trim().chars().take(64).collect();
        let inner = Arc::new(Inner {
            data_dir,
            config: HostClientConfig {
                client_id,
                label: if label.is_empty() {
                    "phone".into()
                } else {
                    label
                },
                libghostty_build: roost_vt::libghostty_build(),
            },
            rt: rt.handle().clone(),
            owner: Arc::new(owner),
            hosts: Mutex::new(
                records
                    .into_iter()
                    .map(|record| HostSlot {
                        record,
                        state: HostStateView::Disconnected,
                        ssh: None,
                        conn: None,
                        serial: 0,
                        workspace_sink: None,
                    })
                    .collect(),
            ),
            hosts_sink: Mutex::new(None),
            attachments: Mutex::new(HashMap::new()),
            counters,
            closed: AtomicBool::new(false),
        });
        Ok(Self {
            inner,
            rt: Some(rt),
        })
    }

    pub fn client_id(&self) -> String {
        self.inner.config.client_id.clone()
    }

    /// Every saved host, now and after each change to any of them.
    pub fn hosts(&self, sink: Sink<Vec<HostView>>) {
        *lock(&self.inner.hosts_sink) = Some(Arc::from(sink));
        self.inner.emit_hosts();
    }

    /// Save a host; its id. `target` is `user@host[:port]` (Tailscale
    /// SSH) or `unix:/path/to/roost.sock`.
    pub fn add_host(&self, label: String, target: String) -> Result<String, AppError> {
        let target = target.trim().to_string();
        Target::parse(&target).map_err(AppError::InvalidTarget)?;
        let label = match label.trim() {
            "" => target.clone(),
            label => label.to_string(),
        };
        let id = registry::new_id();
        lock(&self.inner.hosts).push(HostSlot {
            record: HostRecord {
                id: id.clone(),
                label,
                target,
                pinned_host_key: None,
            },
            state: HostStateView::Disconnected,
            ssh: None,
            conn: None,
            serial: 0,
            workspace_sink: None,
        });
        self.inner.persist()?;
        self.inner.emit_hosts();
        Ok(id)
    }

    pub fn remove_host(&self, host_id: String) -> Result<(), AppError> {
        let removed = {
            let mut hosts = lock(&self.inner.hosts);
            let at = hosts
                .iter()
                .position(|slot| slot.record.id == host_id)
                .ok_or_else(|| AppError::UnknownHost(host_id.clone()))?;
            hosts.remove(at)
        };
        drop(removed);
        self.inner.persist()?;
        self.inner.emit_hosts();
        Ok(())
    }

    /// Start connecting, or retry now if already connecting.
    pub fn connect(&self, host_id: String) -> Result<(), AppError> {
        let inner = &self.inner;
        inner.with_slot(&host_id, |slot| {
            if let Some(conn) = &slot.conn {
                conn.client.reconnect_now();
                return;
            }
            let dialer = match dialer_for(&slot.record, &mut slot.ssh) {
                Ok(dialer) => dialer,
                Err(error) => {
                    slot.state = HostStateView::Failed {
                        reason: error.to_string(),
                    };
                    return;
                }
            };
            let (client, events) =
                HostClient::spawn(&inner.rt, Arc::clone(&dialer), inner.config.clone());
            slot.serial += 1;
            slot.state = HostStateView::Connecting;
            slot.conn = Some(Connection {
                client,
                dialer,
                _live: Live::new(&inner.counters.hosts),
            });
            inner.rt.spawn(host_pump(
                Arc::downgrade(inner),
                host_id.clone(),
                slot.serial,
                events,
                Live::new(&inner.counters.host_pumps),
            ));
        })?;
        inner.emit_hosts();
        Ok(())
    }

    /// Drop the host's connection. Its attached terminals end.
    pub fn disconnect(&self, host_id: String) -> Result<(), AppError> {
        let conn = self.inner.with_slot(&host_id, |slot| {
            slot.serial += 1;
            slot.state = HostStateView::Disconnected;
            slot.conn.take()
        })?;
        drop(conn);
        self.inner.emit_hosts();
        Ok(())
    }

    /// Pin the host key the user just accepted — the fingerprint a
    /// `NeedsTrust` state showed — and retry with it.
    pub fn trust_host_key(&self, host_id: String, fingerprint: String) -> Result<(), AppError> {
        self.inner.with_slot(&host_id, |slot| {
            slot.record.pinned_host_key = Some(fingerprint.clone());
            if let Some(ssh) = &slot.ssh {
                ssh.set_host_key_policy(HostKeyPolicy {
                    pinned: Some(fingerprint),
                });
            }
            if let Some(conn) = &slot.conn {
                conn.client.reconnect_now();
            }
        })?;
        self.inner.persist()?;
        self.inner.emit_hosts();
        Ok(())
    }

    /// The host's projects and tabs, now and after every change. One
    /// sink per host; a later one replaces it.
    pub fn workspace(&self, host_id: String, sink: Sink<WorkspaceView>) -> Result<(), AppError> {
        self.inner
            .with_slot(&host_id, |slot| slot.workspace_sink = Some(Arc::from(sink)))?;
        self.inner.emit_workspace(&host_id);
        Ok(())
    }

    /// Run a control-plane op on the host; its result as JSON.
    pub async fn op(
        &self,
        host_id: String,
        op: String,
        params_json: String,
    ) -> Result<String, AppError> {
        let params = match params_json.trim() {
            "" => serde_json::json!({}),
            json => serde_json::from_str::<serde_json::Value>(json)
                .ok()
                .filter(serde_json::Value::is_object)
                .ok_or_else(|| AppError::InvalidParams(json.to_string()))?,
        };
        let result = self.inner.call(&host_id, &op, params)?.await?;
        Ok(result.to_string())
    }

    /// Type `text` into a tab over the control plane (`tab.write`), which
    /// never takes the tab's size. `paste` frames it as a paste:
    /// bracketed if the program asked for it, as an attached terminal of
    /// the tab knows — and, with none attached, bracketed anyway, since
    /// the compose box's programs (agent TUIs, shells) all ask.
    pub async fn send_text(
        &self,
        host_id: String,
        tab_id: i64,
        text: String,
        paste: bool,
    ) -> Result<(), AppError> {
        let bytes = if paste {
            let attached = lock(&self.inner.attachments)
                .values()
                .find(|attachment| attachment.host_id == host_id && attachment.tab_id == tab_id)
                .map(|attachment| attachment.id());
            attached
                .and_then(|id| self.inner.owner.encode_paste(id, text.clone()))
                .unwrap_or_else(|| crate::keys::paste_bytes(&text, true))
        } else {
            text.into_bytes()
        };
        if bytes.is_empty() {
            return Ok(());
        }
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        let params = serde_json::json!({"tab_id": tab_id.to_string(), "data": data});
        self.inner.call(&host_id, "tab.write", params)?.await?;
        Ok(())
    }

    pub async fn clear_notification(&self, host_id: String, tab_id: i64) -> Result<(), AppError> {
        let params = serde_json::json!({"tab_id": tab_id.to_string()});
        self.inner
            .call(&host_id, "tab.clear_notification", params)?
            .await?;
        Ok(())
    }

    /// Attach a terminal to a tab. `take`: the user chose this tab, so it
    /// takes the tab's size. The host must be connected (or connecting).
    pub fn attach(
        &self,
        host_id: String,
        tab_id: i64,
        geometry: Geometry,
        take: bool,
    ) -> Result<TerminalHandle, AppError> {
        let attachment = handle::open(&self.inner, host_id, tab_id, geometry, take)?;
        lock(&self.inner.attachments).insert(attachment.id(), Arc::clone(&attachment));
        Ok(TerminalHandle::new(attachment))
    }

    /// The app went to the background: give back the size of every
    /// attached tab (`tab.release_size`), then detach them. Their handles
    /// report `Ended` ("paused"); Dart re-attaches the visible tab on
    /// resume. Host connections and their mirrors stay.
    pub fn app_paused(&self) {
        let attachments: Vec<Arc<Attachment>> =
            lock(&self.inner.attachments).values().cloned().collect();
        let releases: Vec<_> = attachments
            .iter()
            .filter(|attachment| attachment.streaming())
            .filter_map(|attachment| {
                let params = serde_json::json!({"tab_id": attachment.tab_id.to_string()});
                self.inner
                    .call(&attachment.host_id, "tab.release_size", params)
                    .ok()
            })
            .collect();
        // Inside the runtime: the timer needs its context to be built.
        let released = self.inner.rt.block_on(async {
            tokio::time::timeout(PAUSE_BUDGET, futures::future::join_all(releases)).await
        });
        if released.is_err() {
            tracing::warn!("tab.release_size did not answer before the pause budget");
        }
        for attachment in attachments {
            attachment.end("paused");
        }
    }

    /// Back in the foreground: every connected host retries now rather
    /// than at its next backoff step.
    pub fn app_resumed(&self) {
        for slot in lock(&self.inner.hosts).iter() {
            if let Some(conn) = &slot.conn {
                conn.client.reconnect_now();
            }
        }
    }

    pub fn leak_counts(&self) -> LeakCounts {
        self.inner.counters.counts()
    }

    /// The counters, to read after the app is dropped.
    pub fn leak_counters(&self) -> Arc<Counters> {
        Arc::clone(&self.inner.counters)
    }
}

impl Drop for RoostApp {
    fn drop(&mut self) {
        let inner = &self.inner;
        inner.closed.store(true, Ordering::SeqCst);
        let attachments: Vec<Arc<Attachment>> =
            lock(&inner.attachments).drain().map(|(_, a)| a).collect();
        for attachment in &attachments {
            attachment.detach();
        }
        let conns: Vec<Connection> = lock(&inner.hosts)
            .iter_mut()
            .filter_map(|slot| slot.conn.take())
            .collect();
        drop(conns);
        // On a thread of some runtime, waiting here would stall it; the
        // runtime shutdown below drops whatever is left instead.
        if tokio::runtime::Handle::try_current().is_err() {
            let deadline = Instant::now() + TEARDOWN_BUDGET;
            while Instant::now() < deadline {
                let counts = inner.counters.counts();
                if counts.attach_pumps == 0 && counts.host_pumps == 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        inner.owner.shutdown();
        if let Some(rt) = self.rt.take() {
            if tokio::runtime::Handle::try_current().is_ok() {
                rt.shutdown_background();
            } else {
                rt.shutdown_timeout(TEARDOWN_BUDGET);
            }
        }
    }
}
