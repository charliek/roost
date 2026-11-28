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
use roost_ipc::messages::ClipboardEffectTarget;
use roost_ssh::{HostKeyPolicy, TailscaleSshDialer};
use roost_ui_model::theme::Theme;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::frame::rgb;
use crate::handle::{self, Attachment, TerminalHandle};
use crate::live::Live;
use crate::owner::{OwnerConfig, TerminalEvent, TerminalId, TerminalOwner};
use crate::registry::{self, Appearance, HostRecord, MobileSettings, Target};
use crate::terminal::Geometry;
use crate::views::{self, HostStateView, HostView, NotificationView, WorkspaceView};

/// A Dart stream, as the FFI layer hands it in.
pub type Sink<T> = Box<dyn Fn(T) + Send + Sync>;
pub(crate) type SharedSink<T> = Arc<dyn Fn(T) + Send + Sync>;

/// How long a pause (or a disconnect) waits for its `tab.release_size`
/// answers, and teardown for its tasks: a phone going to the background
/// gets a few moments, not an open-ended wait.
const RELEASE_BUDGET: Duration = Duration::from_secs(2);
const TEARDOWN_BUDGET: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("no saved host {0}")]
    UnknownHost(String),
    #[error("host {0} is not connected")]
    NotConnected(String),
    #[error("the app is in the background")]
    Paused,
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

/// The saved settings plus what the platform says, and the brightness and
/// terminal theme they make: worked out here once, so the app never
/// recomputes them.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsState {
    pub settings: MobileSettings,
    pub system_dark: bool,
}

impl SettingsState {
    pub fn effective_dark(&self) -> bool {
        match self.settings.appearance {
            Appearance::Dark => true,
            Appearance::Light => false,
            Appearance::System => self.system_dark,
        }
    }

    pub fn effective_theme_name(&self) -> &str {
        if self.effective_dark() {
            &self.settings.dark_theme
        } else {
            &self.settings.light_theme
        }
    }
}

/// A bundled terminal theme, for a swatch. Colors are `0x00RRGGBB`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeSummary {
    pub name: String,
    pub light: bool,
    pub background: u32,
    pub foreground: u32,
    pub cursor: u32,
    /// The 16 ANSI colors.
    pub ansi: Vec<u32>,
}

/// `roost-dark` first, the other dark themes after it, then the light ones.
fn theme_summaries() -> Vec<ThemeSummary> {
    let summary = |name: String, light: bool| {
        let theme = Theme::load_bundled(&name);
        ThemeSummary {
            light,
            background: rgb(theme.background),
            foreground: rgb(theme.foreground),
            cursor: rgb(theme.cursor),
            ansi: theme.palette[..16].iter().copied().map(rgb).collect(),
            name,
        }
    };
    let mut dark = Theme::bundled_names();
    dark.sort_by_key(|name| name != registry::DEFAULT_DARK_THEME);
    dark.into_iter()
        .map(|name| summary(name, false))
        .chain(
            Theme::bundled_light_names()
                .into_iter()
                .map(|name| summary(name, true)),
        )
        .collect()
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
    /// Held from a saved-host change through its write, so concurrent
    /// changes reach `hosts.json` in the order they were made.
    registry: Mutex<()>,
    hosts_sink: Mutex<Option<SharedSink<Vec<HostView>>>>,
    notifications_sink: Mutex<Option<SharedSink<NotificationView>>>,
    attachments: Mutex<HashMap<TerminalId, Arc<Attachment>>>,
    pub(crate) counters: Arc<Counters>,
    pub(crate) closed: AtomicBool,
    /// Between `app_paused` and `app_resumed`: attaches are refused. Set
    /// and read under the `attachments` lock.
    paused: AtomicBool,
    /// Held for a whole settings change, through its write and its theme
    /// send, so changes reach the file and the owner in the order made.
    settings_io: Mutex<()>,
    /// Never held across a write or a send.
    settings: Mutex<SettingsState>,
    themes: Vec<ThemeSummary>,
    /// Runs just before a settings write, to hold one partway.
    #[cfg(test)]
    before_settings_save: Mutex<Option<Box<dyn Fn() + Send>>>,
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

    /// Change the saved hosts and write them, as one step.
    fn change_hosts<T>(
        &self,
        change: impl FnOnce(&mut Vec<HostSlot>) -> Result<T, AppError>,
    ) -> Result<T, AppError> {
        let _registry = lock(&self.registry);
        let (changed, records) = {
            let mut hosts = lock(&self.hosts);
            let changed = change(&mut hosts)?;
            let records: Vec<HostRecord> = hosts.iter().map(|slot| slot.record.clone()).collect();
            (changed, records)
        };
        registry::save_hosts(&self.data_dir, &records).map_err(|source| AppError::Io {
            what: "saving hosts.json",
            source,
        })?;
        Ok(changed)
    }

    /// Change the settings as one step: saved before the change is
    /// visible, then the owner told of a new terminal theme.
    fn change_settings<E>(
        &self,
        change: impl FnOnce(&mut SettingsState),
        save: impl FnOnce(&MobileSettings) -> Result<(), E>,
    ) -> Result<SettingsState, E> {
        let _io = lock(&self.settings_io);
        let current = lock(&self.settings).clone();
        let mut next = current.clone();
        change(&mut next);
        next.settings = next.settings.sanitize();
        save(&next.settings)?;
        *lock(&self.settings) = next.clone();
        if next.effective_theme_name() != current.effective_theme_name() {
            self.owner
                .set_theme(Theme::load_bundled(next.effective_theme_name()));
        }
        Ok(next)
    }

    fn attachments_of(&self, host_id: Option<&str>) -> Vec<Arc<Attachment>> {
        lock(&self.attachments)
            .values()
            .filter(|attachment| host_id.is_none_or(|host_id| attachment.host_id == host_id))
            .cloned()
            .collect()
    }

    /// Give back the size of every one of `attachments` still streaming
    /// (`tab.release_size`, waited for briefly), then detach them all,
    /// each handle reporting `Ended` with `reason`.
    fn release_and_end(&self, attachments: Vec<Arc<Attachment>>, reason: &str) {
        let releases: Vec<_> = attachments
            .iter()
            .filter(|attachment| attachment.streaming())
            .filter_map(|attachment| {
                let params = serde_json::json!({"tab_id": attachment.tab_id.to_string()});
                self.call(&attachment.host_id, "tab.release_size", params)
                    .ok()
            })
            .collect();
        if !releases.is_empty() {
            // Inside the runtime: the timer needs its context to be built.
            let released = self.rt.block_on(async {
                tokio::time::timeout(RELEASE_BUDGET, futures::future::join_all(releases)).await
            });
            if released.is_err() {
                tracing::warn!(%reason, "tab.release_size did not answer in time");
            }
        }
        for attachment in attachments {
            attachment.end(reason);
        }
    }

    fn host_event(&self, host_id: &str, serial: u64, event: HostEvent) {
        if let HostEvent::Clipboard {
            tab_id,
            target,
            text,
        } = event
        {
            self.clipboard_write(host_id, serial, tab_id, target, text);
            return;
        }
        let mut notification = None;
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
                    HostEvent::Notification {
                        tab_id,
                        title,
                        body,
                    } => {
                        notification = Some(NotificationView {
                            host_id: host_id.to_string(),
                            tab_id,
                            title,
                            body,
                        });
                        false
                    }
                    HostEvent::Clipboard { .. } => false,
                }
            })
            .unwrap_or(false);
        if let Some(notification) = notification {
            let sink = lock(&self.notifications_sink).clone();
            if let Some(sink) = sink {
                sink(notification);
            }
            return;
        }
        if workspace_moved {
            self.emit_workspace(host_id);
        }
        self.emit_hosts();
    }

    /// A program's clipboard write lands only on a terminal of that tab that
    /// is streaming, which is one on screen: `ipc.md` leaves it to each
    /// client to apply only the tab it is viewing, so a write from a tab in
    /// the background, or one this device merely mirrors, is dropped. The
    /// phone has no selection clipboard, so a write aimed there is too.
    fn clipboard_write(
        &self,
        host_id: &str,
        serial: u64,
        tab_id: i64,
        target: ClipboardEffectTarget,
        text: String,
    ) {
        if target != ClipboardEffectTarget::System {
            return;
        }
        let current = self
            .with_slot(host_id, |slot| slot.serial == serial && slot.conn.is_some())
            .unwrap_or(false);
        if !current {
            return;
        }
        let viewing = self
            .attachments_of(Some(host_id))
            .into_iter()
            .find(|attachment| attachment.tab_id == tab_id && attachment.streaming());
        if let Some(attachment) = viewing {
            attachment.emit(TerminalEvent::Clipboard { text });
        }
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
    /// Load the saved hosts, settings and this install's `client_id` from
    /// `data_dir`, creating it. `system_dark`: the platform is in dark mode
    /// now, so the first tab opens in the right theme. No host is dialed
    /// until [`Self::connect`].
    pub fn new(
        data_dir: impl Into<PathBuf>,
        device_label: String,
        system_dark: bool,
    ) -> Result<Self, AppError> {
        let data_dir = data_dir.into();
        let io = |what| move |source| AppError::Io { what, source };
        std::fs::create_dir_all(&data_dir).map_err(io("creating the data directory"))?;
        let client_id = registry::client_id(&data_dir).map_err(io("reading client_id"))?;
        let records = registry::load_hosts(&data_dir).map_err(io("reading hosts.json"))?;
        let settings = SettingsState {
            settings: registry::load_settings(&data_dir).map_err(io("reading settings.json"))?,
            system_dark,
        };
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("roost-mobile-rt")
            .enable_all()
            .build()
            .map_err(io("starting the runtime"))?;
        let owner = TerminalOwner::spawn(OwnerConfig {
            client_id: client_id.clone(),
            theme: Theme::load_bundled(settings.effective_theme_name()),
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
            registry: Mutex::new(()),
            hosts_sink: Mutex::new(None),
            notifications_sink: Mutex::new(None),
            attachments: Mutex::new(HashMap::new()),
            counters,
            closed: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            settings_io: Mutex::new(()),
            settings: Mutex::new(settings),
            themes: theme_summaries(),
            #[cfg(test)]
            before_settings_save: Mutex::new(None),
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
        self.inner.change_hosts(|hosts| {
            hosts.push(HostSlot {
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
            Ok(())
        })?;
        self.inner.emit_hosts();
        Ok(id)
    }

    /// Forget a saved host. Its attached terminals give back their size
    /// and end ("removed"), then its connection closes.
    pub fn remove_host(&self, host_id: String) -> Result<(), AppError> {
        self.inner.with_slot(&host_id, |_| ())?;
        self.inner
            .release_and_end(self.inner.attachments_of(Some(&host_id)), "removed");
        let removed = self.inner.change_hosts(|hosts| {
            let at = hosts
                .iter()
                .position(|slot| slot.record.id == host_id)
                .ok_or_else(|| AppError::UnknownHost(host_id.clone()))?;
            Ok(hosts.remove(at))
        })?;
        drop(removed);
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

    /// Drop the host's connection. Its attached terminals give back
    /// their size and end ("disconnected") first.
    pub fn disconnect(&self, host_id: String) -> Result<(), AppError> {
        self.inner.with_slot(&host_id, |_| ())?;
        self.inner
            .release_and_end(self.inner.attachments_of(Some(&host_id)), "disconnected");
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
        self.inner.change_hosts(|hosts| {
            let slot = hosts
                .iter_mut()
                .find(|slot| slot.record.id == host_id)
                .ok_or_else(|| AppError::UnknownHost(host_id.clone()))?;
            slot.record.pinned_host_key = Some(fingerprint.clone());
            if let Some(ssh) = &slot.ssh {
                ssh.set_host_key_policy(HostKeyPolicy {
                    pinned: Some(fingerprint),
                });
            }
            if let Some(conn) = &slot.conn {
                conn.client.reconnect_now();
            }
            Ok(())
        })?;
        self.inner.emit_hosts();
        Ok(())
    }

    /// `notification.fired` from every connected host — live only, never
    /// replayed after a reconnect. A later sink replaces this one.
    pub fn notifications(&self, sink: Sink<NotificationView>) {
        *lock(&self.inner.notifications_sink) = Some(Arc::from(sink));
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
        // Opened and registered under the lock a pause collects through: a
        // pause either ends this attachment or comes first and refuses it.
        let mut attachments = lock(&self.inner.attachments);
        if self.inner.paused.load(Ordering::SeqCst) {
            return Err(AppError::Paused);
        }
        let attachment = handle::open(&self.inner, host_id, tab_id, geometry, take)?;
        attachments.insert(attachment.id(), Arc::clone(&attachment));
        drop(attachments);
        Ok(TerminalHandle::new(attachment))
    }

    /// The app went to the background: give back the size of every
    /// attached tab (`tab.release_size`), then detach them. Their handles
    /// report `Ended` ("paused"); Dart re-attaches the visible tab on
    /// resume. Host connections and their mirrors stay.
    /// Attaches are refused until [`Self::app_resumed`].
    pub fn app_paused(&self) {
        let attachments = {
            let attachments = lock(&self.inner.attachments);
            self.inner.paused.store(true, Ordering::SeqCst);
            attachments.values().cloned().collect()
        };
        self.inner.release_and_end(attachments, "paused");
    }

    /// Back in the foreground: every connected host retries now rather
    /// than at its next backoff step.
    pub fn app_resumed(&self) {
        {
            let _attachments = lock(&self.inner.attachments);
            self.inner.paused.store(false, Ordering::SeqCst);
        }
        for slot in lock(&self.inner.hosts).iter() {
            if let Some(conn) = &slot.conn {
                conn.client.reconnect_now();
            }
        }
    }

    pub fn settings(&self) -> SettingsState {
        lock(&self.inner.settings).clone()
    }

    /// Save `settings` (sanitized) and, if the terminal theme in effect
    /// changed, send it to every tab. An error leaves the settings as
    /// they were.
    pub fn update_settings(&self, settings: MobileSettings) -> Result<SettingsState, AppError> {
        let inner = &self.inner;
        inner.change_settings(
            |state| state.settings = settings,
            |settings| {
                #[cfg(test)]
                if let Some(hook) = &*lock(&inner.before_settings_save) {
                    hook();
                }
                registry::save_settings(&inner.data_dir, settings).map_err(|source| AppError::Io {
                    what: "saving settings.json",
                    source,
                })
            },
        )
    }

    /// The platform went dark or light: nothing is saved, but under
    /// `Appearance::System` the terminal theme follows.
    pub fn set_system_dark(&self, dark: bool) -> SettingsState {
        let Ok(state) = self.inner.change_settings(
            |state| state.system_dark = dark,
            |_| Ok::<(), std::convert::Infallible>(()),
        );
        state
    }

    /// Every bundled terminal theme, `roost-dark` first, then the other
    /// dark ones, then the light ones.
    pub fn terminal_themes(&self) -> Vec<ThemeSummary> {
        self.inner.themes.clone()
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

#[cfg(test)]
mod tests {
    use roost_vt::ColorRgb;

    use super::*;
    use crate::registry::ReadableText;

    fn background(name: &str) -> ColorRgb {
        Theme::load_bundled(name).background
    }

    fn settings_with(change: impl FnOnce(&mut MobileSettings)) -> MobileSettings {
        let mut settings = MobileSettings::default();
        change(&mut settings);
        settings
    }

    #[test]
    fn the_effective_brightness_and_theme_follow_appearance_and_platform() {
        let cases = [
            (Appearance::Dark, false, true),
            (Appearance::Dark, true, true),
            (Appearance::Light, false, false),
            (Appearance::Light, true, false),
            (Appearance::System, false, false),
            (Appearance::System, true, true),
        ];
        for (appearance, system_dark, dark) in cases {
            let state = SettingsState {
                settings: settings_with(|s| {
                    s.appearance = appearance;
                    s.dark_theme = "Catppuccin Mocha".into();
                    s.light_theme = "GitHub Light Default".into();
                }),
                system_dark,
            };
            assert_eq!(state.effective_dark(), dark, "{appearance:?} {system_dark}");
            assert_eq!(
                state.effective_theme_name(),
                if dark {
                    "Catppuccin Mocha"
                } else {
                    "GitHub Light Default"
                },
                "{appearance:?} {system_dark}"
            );
        }
    }

    #[test]
    fn terminal_themes_list_roost_dark_first_then_dark_then_light() {
        let dir = tempfile::tempdir().expect("tempdir");
        let app = RoostApp::new(dir.path(), "phone".into(), true).expect("app");
        let themes = app.terminal_themes();
        let names: Vec<&str> = themes.iter().map(|t| t.name.as_str()).collect();
        let mut dark = Theme::bundled_names();
        dark.retain(|name| name != "roost-dark");
        let want: Vec<String> = std::iter::once("roost-dark".to_string())
            .chain(dark)
            .chain(Theme::bundled_light_names())
            .collect();
        assert_eq!(names, want);
        assert!(
            themes.iter().all(|t| t.ansi.len() == 16),
            "16 ANSI colors each"
        );
        assert!(themes
            .iter()
            .all(|t| t.light == Theme::bundled_light_names().contains(&t.name)));
        let latte = themes
            .iter()
            .find(|t| t.name == "Catppuccin Latte")
            .expect("Latte");
        assert_eq!(
            (
                latte.background,
                latte.foreground,
                latte.cursor,
                latte.ansi[1]
            ),
            (0x00ef_f1f5, 0x004c_4f69, 0x00dc_8a78, 0x00d2_0f39)
        );
    }

    #[test]
    fn only_a_change_of_the_theme_in_effect_sends_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let app = RoostApp::new(dir.path(), "phone".into(), true).expect("app");
        let sent = || app.inner.owner.sent_themes();

        app.update_settings(settings_with(|s| s.terminal_font_size_wide = 18.0))
            .expect("size");
        app.update_settings(settings_with(|s| {
            s.terminal_font_size_wide = 18.0;
            s.light_theme = "GitHub Light Default".into();
        }))
        .expect("a light theme while dark");
        assert_eq!(sent(), vec![], "the theme in effect did not change");

        let light = app
            .update_settings(settings_with(|s| {
                s.appearance = Appearance::Light;
                s.light_theme = "github light default".into();
            }))
            .expect("light");
        assert_eq!(light.effective_theme_name(), "GitHub Light Default");
        assert_eq!(sent(), vec![background("GitHub Light Default")]);

        app.update_settings(settings_with(|s| {
            s.appearance = Appearance::Light;
            s.light_theme = "GitHub Light Default".into();
            s.readable_text = ReadableText::On;
        }))
        .expect("same theme");
        app.set_system_dark(false);
        assert_eq!(sent(), vec![background("GitHub Light Default")]);
    }

    #[test]
    fn the_platform_brightness_moves_the_theme_only_under_system() {
        let dir = tempfile::tempdir().expect("tempdir");
        let app = RoostApp::new(dir.path(), "phone".into(), true).expect("app");
        assert_eq!(
            app.set_system_dark(false).effective_theme_name(),
            "roost-dark"
        );
        assert_eq!(
            app.inner.owner.sent_themes(),
            vec![],
            "Dark ignores the platform"
        );

        app.update_settings(settings_with(|s| s.appearance = Appearance::System))
            .expect("system");
        assert_eq!(
            app.inner.owner.sent_themes(),
            vec![background("Catppuccin Latte")]
        );
        let dark = app.set_system_dark(true);
        assert_eq!(dark.effective_theme_name(), "roost-dark");
        assert_eq!(
            app.inner.owner.sent_themes(),
            vec![background("Catppuccin Latte"), background("roost-dark")]
        );
    }

    #[test]
    fn a_failed_save_changes_nothing_and_sends_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let app = RoostApp::new(dir.path(), "phone".into(), true).expect("app");
        let before = app.settings();
        std::fs::create_dir_all(dir.path().join("settings.json").join("in-the-way"))
            .expect("a directory where the file goes");
        let error = app
            .update_settings(settings_with(|s| s.appearance = Appearance::Light))
            .expect_err("the save fails");
        assert!(
            error.to_string().contains("saving settings.json"),
            "{error}"
        );
        assert_eq!(app.settings(), before);
        assert_eq!(app.inner.owner.sent_themes(), vec![]);
    }

    /// A's update is held in its write while B's platform change arrives:
    /// B waits for A, so the snapshot and the theme sends follow the
    /// order the two were made in.
    #[test]
    fn a_platform_change_waits_for_an_update_in_flight() {
        let dir = tempfile::tempdir().expect("tempdir");
        let app = &RoostApp::new(dir.path(), "phone".into(), true).expect("app");
        app.update_settings(settings_with(|s| s.appearance = Appearance::System))
            .expect("system, dark platform");
        assert_eq!(app.inner.owner.sent_themes(), vec![]);

        let (entered, in_save) = std::sync::mpsc::channel();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let held = Mutex::new(held);
        *lock(&app.inner.before_settings_save) = Some(Box::new(move || {
            let _ = entered.send(());
            let _ = lock(&held).recv();
        }));

        std::thread::scope(|scope| {
            let a = scope.spawn(|| {
                app.update_settings(settings_with(|s| {
                    s.appearance = Appearance::System;
                    s.dark_theme = "Catppuccin Mocha".into();
                }))
            });
            in_save.recv().expect("A is in its write");
            let (b_calling, b_called) = std::sync::mpsc::channel();
            let (b_done, b_finished) = std::sync::mpsc::channel();
            let b = scope.spawn(move || {
                let _ = b_calling.send(());
                let state = app.set_system_dark(false);
                let _ = b_done.send(());
                state
            });
            b_called.recv().expect("B is about to call");
            // B is in its call now; a grace period for it to overtake A if
            // nothing holds it back.
            let b_overtook = b_finished.recv_timeout(Duration::from_millis(200)).is_ok();
            release.send(()).expect("release A");
            a.join().expect("A").expect("A saved");
            let b = b.join().expect("B");
            assert!(!b_overtook, "B finished while A was still writing");
            assert_eq!(b.effective_theme_name(), "Catppuccin Latte");
        });

        let state = app.settings();
        assert_eq!(state.settings.dark_theme, "Catppuccin Mocha");
        assert!(!state.system_dark);
        assert_eq!(
            app.inner.owner.sent_themes(),
            vec![
                background("Catppuccin Mocha"),
                background("Catppuccin Latte")
            ]
        );
        assert_eq!(
            registry::load_settings(dir.path())
                .expect("load")
                .dark_theme,
            "Catppuccin Mocha"
        );
    }

    #[test]
    fn a_corrupt_settings_file_still_starts_the_app() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("settings.json"), "{ nope").expect("corrupt");
        let app = RoostApp::new(dir.path(), "phone".into(), true).expect("app");
        assert_eq!(app.settings().settings, MobileSettings::default());
        assert!(dir.path().join("settings.json.bad").exists());
    }

    #[test]
    fn an_unreadable_settings_file_fails_the_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("settings.json")).expect("in the way");
        let error = RoostApp::new(dir.path(), "phone".into(), true)
            .err()
            .expect("a read error");
        assert!(
            error.to_string().contains("reading settings.json"),
            "{error}"
        );
    }

    #[test]
    fn every_setting_survives_a_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let chosen = MobileSettings {
            terminal_font_size_compact: 9.5,
            terminal_font_size_wide: 17.0,
            ui_text_scale: 1.35,
            appearance: Appearance::Light,
            dark_theme: "Catppuccin Mocha".into(),
            light_theme: "GitHub Light Default".into(),
            readable_text: ReadableText::Off,
            bright_faint: true,
            ..MobileSettings::default()
        };
        {
            let app = RoostApp::new(dir.path(), "phone".into(), true).expect("app");
            let saved = app.update_settings(chosen.clone()).expect("save");
            assert_eq!(saved.settings, chosen);
        }
        let app = RoostApp::new(dir.path(), "phone".into(), false).expect("app again");
        assert_eq!(
            app.settings(),
            SettingsState {
                settings: chosen,
                system_dark: false,
            }
        );
    }

    /// Concurrent changes, each saved as made: once a change returns,
    /// the file holds it, and no older list written afterwards takes it
    /// back out.
    #[test]
    fn concurrent_host_changes_all_reach_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let app = RoostApp::new(dir.path(), "phone".into(), true).expect("app");
        let saved = |id: &str| {
            registry::load_hosts(dir.path())
                .expect("a whole file")
                .into_iter()
                .find(|record| record.id == id)
        };
        let ids: Vec<String> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|worker| {
                    let (app, saved) = (&app, &saved);
                    scope.spawn(move || {
                        (0..10)
                            .map(|n| {
                                let id = app
                                    .add_host(
                                        format!("h{worker}-{n}"),
                                        format!("u@host{worker}x{n}"),
                                    )
                                    .expect("add");
                                assert!(saved(&id).is_some(), "an add that returned is saved");
                                app.trust_host_key(id.clone(), format!("SHA256:{worker}-{n}"))
                                    .expect("trust");
                                assert!(
                                    saved(&id)
                                        .is_some_and(|record| record.pinned_host_key.is_some()),
                                    "a trust that returned is saved"
                                );
                                id
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|worker| worker.join().expect("worker"))
                .collect()
        });
        drop(app);
        let all = registry::load_hosts(dir.path()).expect("a whole file");
        assert_eq!(all.len(), ids.len());
        assert!(all.iter().all(|record| record.pinned_host_key.is_some()));
    }
}
