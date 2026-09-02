//! In-process workspace state.
//!
//! Daemon-removal refactor M3 — rewritten from
//! `crates/roost-core/src/state.rs`. Differences vs the legacy
//! daemon original:
//!
//! * **Storage**: an in-memory `BTreeMap` instead of the SQLite
//!   `Store`. State is persisted to `state.json` (atomic write +
//!   one-level backup) via `store_json::persist_state`.
//! * **Types**: `roost_ipc::messages::{Tab, Project, TabState}`
//!   instead of the legacy `roost_proto::v1::*` types.
//! * **Events**: a typed `WorkspaceEvent` enum is emitted on a
//!   `tokio::sync::broadcast` channel. The IPC server's
//!   `events.subscribe` op converts these into
//!   `roost_ipc::messages::EventEnvelope`s (see `crate::event_push`),
//!   on a host-session socket.
//! * **Session layout**: the workspace persists each project's tab
//!   layout (title + cwd + position) plus the active selection, so a
//!   relaunch re-opens the prior tabs as fresh shells in their saved
//!   directories. Live state (process, scrollback) is not restored.
//!   `open()` loads the layout into a one-shot `restore_layout` the UI
//!   bootstrap drains via `take_restore_layout`; it is kept out of the
//!   live `tabs` map (those are the re-opened fresh shells).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use roost_ipc::agent::{
    self, AgentLifecycle, AgentTabState, AttentionEffect, OwnershipAction, TabAgentReportParams,
    SOURCE_LEGACY, SOURCE_MANUAL,
};
use roost_ipc::messages::{Project, Tab, TabState};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::warn;

use crate::persistence::{persist_state, read_state, HostSnapshot, ProjectSnapshot, SnapshotFile};

/// How many events the broadcast channel buffers per subscriber.
/// Subscribers that fall behind get a `Lagged` and resync via
/// `tab.list`.
pub(crate) const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Narrowest the sidebar may be dragged, in logical points.
pub const SIDEBAR_MIN_WIDTH: f64 = 160.0;
/// Widest the sidebar may be dragged, in logical points.
pub const SIDEBAR_MAX_WIDTH: f64 = 400.0;
/// Sidebar width before the user has ever dragged it.
pub const SIDEBAR_DEFAULT_WIDTH: f64 = 220.0;

/// The one place the sidebar-width semantics live, shared by the
/// setter and the `state.json` restore path so they can't diverge:
/// a non-finite width is no width at all, anything else clamps into
/// [`SIDEBAR_MIN_WIDTH`]..=[`SIDEBAR_MAX_WIDTH`].
fn normalize_sidebar_width(width: f64) -> Option<f64> {
    width
        .is_finite()
        .then(|| width.clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH))
}

#[derive(Clone, Debug)]
struct ProjectRow {
    id: i64,
    name: String,
    cwd: String,
    position: i32,
    created_at: i64,
}

#[derive(Clone, Debug)]
struct TabRow {
    id: i64,
    project_id: i64,
    title: String,
    cwd: String,
    /// The three agent axes. `Tab.state` and `Tab.hook_active` are
    /// derived from this on the way out (`wire_tab`), never stored
    /// beside it.
    ///
    /// **Not persisted.** `TabSnapshot` stays `{title, cwd, position,
    /// user_titled}`; run state has never been in `state.json` and this
    /// plan does not put it there (plan 002 §2.6).
    agent: AgentTabState,
    has_notification: bool,
    user_titled: bool,
    position: i32,
    created_at: i64,
    last_active: i64,
}

struct Inner {
    projects: BTreeMap<i64, ProjectRow>,
    tabs: BTreeMap<i64, TabRow>,
    /// Last selected live tab per project. This is runtime navigation state,
    /// not a second workspace authority: every write happens alongside the
    /// global active selection under this same lock. Only the globally-active
    /// project position is persisted today, matching the existing restoration
    /// contract; other projects rebuild their preference as tabs are restored.
    active_tabs_by_project: BTreeMap<i64, i64>,
    next_id: i64,
    active_project_id: i64,
    active_tab_id: i64,
    /// Whether the sidebar is collapsed (hidden). UI-set via
    /// `set_sidebar_collapsed`; persisted so a relaunch restores it
    /// (Rust UI adapter (Iced) parity with the Mac UI's
    /// `RoostSidebarVisible`).
    sidebar_collapsed: bool,
    /// The sidebar's width in logical points. UI-set via
    /// `set_sidebar_width`; persisted so a relaunch restores it
    /// (Rust UI adapter (Iced) parity with the Mac UI's
    /// `RoostSidebarWidth`).
    sidebar_width: f64,
    /// Saved hosts, carried opaquely from `state.json` back into every
    /// rewrite. The workspace neither reads nor mutates them — HS-2
    /// owns the mutation API and the `HostSnapshot.id` ↔ `HostId`
    /// mapping — but they must live here, because a snapshot rebuilt
    /// without them silently erases the user's hosts on the first
    /// ordinary write-through.
    hosts: Vec<HostSnapshot>,
    /// Whether the UI window currently has focus. Half of the
    /// notification-suppression predicate (plan §3.5); reported by the
    /// UI via [`Workspace::set_window_focused`]. Never persisted — focus
    /// is a property of the running session, not of the layout.
    window_focused: bool,
    /// Monotonic commit counter, bumped each time a persistable
    /// snapshot is taken (under this lock). Tags each snapshot so
    /// `persist()` can drop stale out-of-order writes (#80).
    persist_seq: u64,
    /// Monotonic in-process revision for every committed transition.
    revision: u64,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            projects: BTreeMap::new(),
            tabs: BTreeMap::new(),
            active_tabs_by_project: BTreeMap::new(),
            next_id: 0,
            active_project_id: 0,
            active_tab_id: 0,
            sidebar_collapsed: false,
            sidebar_width: SIDEBAR_DEFAULT_WIDTH,
            hosts: Vec::new(),
            // Focused until a UI says otherwise: a headless or IPC-only
            // workspace never reports focus, and the alternative default
            // would leave the active tab permanently "unseen" — silently
            // routing every notification the opposite way from what a
            // real window would.
            window_focused: true,
            persist_seq: 0,
            revision: 0,
        }
    }
}

/// A persisted project's tab layout, surfaced to the UI bootstrap.
/// These are descriptors (cwd + title), not live tabs — the UI
/// re-opens them as fresh shells via the normal open path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreLayout {
    pub projects: Vec<RestoreProject>,
    /// Project to re-select (`0` = no preference → first project).
    pub active_project_id: i64,
    /// Position of the active tab within the active project.
    pub active_tab_position: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreProject {
    pub project_id: i64,
    /// Tabs in display (position) order.
    pub tabs: Vec<RestoreTab>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreTab {
    pub cwd: String,
    pub title: String,
    /// Whether the saved title was a manual user rename (Cmd+R /
    /// `tab.set_title`). When true, the restore path re-asserts
    /// the `user_titled` lock so a post-relaunch `cd` doesn't
    /// silently re-derive the title from cwd. See `set_tab_cwd`
    /// for the gate. Persisted in `TabSnapshot.user_titled`.
    pub user_titled: bool,
}

/// What a [`WorkspaceEvent::TabEffect`] carries, in process.
///
/// The wire shape is flat — an `effect` name beside optional `data` and
/// `target` fields (plan 037 §3.6) — but in process the payload belongs
/// to the one variant that has one, so a bell cannot be built carrying
/// clipboard text. `crate::event_push` flattens this into the wire form,
/// the same boundary that already does every other event's projection.
///
/// The clipboard text is user content: it goes on the wire and into no
/// log line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "effect", rename_all = "kebab-case")]
pub enum TabEffectKind {
    Bell,
    ClipboardWrite {
        text: String,
        target: roost_ipc::messages::ClipboardEffectTarget,
    },
}

/// Workspace event channel. The server-push relay
/// (`crate::event_push`) converts these to wire-format
/// `EventEnvelope`s; its serializer is a total match over this enum, so
/// a new variant added here is a compile error until it has a wire name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum WorkspaceEvent {
    TabOpened(Tab),
    TabClosed {
        #[serde(with = "roost_ipc::messages::string_int64")]
        tab_id: i64,
    },
    TabStateChanged {
        #[serde(with = "roost_ipc::messages::string_int64")]
        tab_id: i64,
        state: TabState,
    },
    TabTitleChanged {
        #[serde(with = "roost_ipc::messages::string_int64")]
        tab_id: i64,
        title: String,
    },
    TabCwdChanged {
        #[serde(with = "roost_ipc::messages::string_int64")]
        tab_id: i64,
        cwd: String,
    },
    TabNotification {
        #[serde(with = "roost_ipc::messages::string_int64")]
        tab_id: i64,
        has_pending: bool,
    },
    ProjectCreated(Project),
    ProjectRenamed {
        #[serde(with = "roost_ipc::messages::string_int64")]
        project_id: i64,
        name: String,
    },
    ProjectDeleted {
        #[serde(with = "roost_ipc::messages::string_int64")]
        project_id: i64,
    },
    ActiveChanged {
        #[serde(with = "roost_ipc::messages::string_int64")]
        project_id: i64,
        #[serde(with = "roost_ipc::messages::string_int64")]
        tab_id: i64,
    },
    HookActiveChanged {
        #[serde(with = "roost_ipc::messages::string_int64")]
        tab_id: i64,
        active: bool,
    },
    /// The full agent record after an accepted report or shell mark;
    /// see [`roost_ipc::messages::AgentReportChangedEvent`].
    AgentChanged {
        #[serde(with = "roost_ipc::messages::string_int64")]
        tab_id: i64,
        agent: AgentTabState,
    },
    NotificationFired {
        #[serde(with = "roost_ipc::messages::string_int64")]
        tab_id: i64,
        title: String,
        body: String,
    },
    /// Fired after `reorder_tabs`. `tab_ids` is the post-reorder
    /// display order — the supplied prefix followed by any
    /// unlisted siblings in their prior position order. Mirrors
    /// the Mac side's `Workspace.Event.tabsReordered`.
    TabsReordered {
        #[serde(with = "roost_ipc::messages::string_int64")]
        project_id: i64,
        #[serde(with = "roost_ipc::messages::vec_string_int64")]
        tab_ids: Vec<i64>,
    },
    /// One client-local effect a server-VT tab produced — a bell or an
    /// OSC 52 clipboard write (plan 037 §3.6). Transient like
    /// [`Self::NotificationFired`]: it is not workspace state, it is
    /// something that happened, and it rides the commit stream so an
    /// attached client sees it in order with everything else. Minted
    /// only by `tab_task`, so a UI's own workspace never emits one.
    TabEffect {
        #[serde(with = "roost_ipc::messages::string_int64")]
        tab_id: i64,
        effect: TabEffectKind,
    },
    /// Fired after `reorder_projects`. `project_ids` is the
    /// post-reorder sidebar order.
    ProjectsReordered {
        #[serde(with = "roost_ipc::messages::vec_string_int64")]
        project_ids: Vec<i64>,
    },
    /// Full-state recovery snapshot. Minted by the event bridge
    /// (`events::subscribe`) when the broadcast channel reports
    /// `Lagged`, so the UI reconciles against ground truth instead
    /// of applying deltas on top of a diverged base. Each `Project`
    /// carries its live tabs; the active tab is the one with
    /// `is_active == true`.
    Resync(Vec<Project>),
}

/// One commit's worth of workspace events, tagged under the
/// authoritative state lock and delivered as a single message.
///
/// The batch — not the individual event — is the unit of delivery, so a
/// subscriber's gap check ("did I skip a revision?") is sufficient: it
/// can never observe half of a commit. Every commit publishes exactly
/// one of these, **including a commit that produced no events**, so the
/// revision stream has no unexplained holes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionedWorkspaceEvent {
    pub revision: u64,
    pub events: Vec<WorkspaceEvent>,
}

pub struct Workspace {
    inner: Mutex<Inner>,
    /// Workspace event channel. Mutators publish on this **while
    /// still holding `inner`**, so broadcast order matches commit
    /// order: a fast subscriber must never observe an event sequence
    /// that contradicts the committed state (e.g. `TabClosed` before
    /// the `TabOpened` of the same tab when two mutators race). #80.
    /// `broadcast::Sender::send` is synchronous and non-blocking — it
    /// wakes receivers but never runs them inline — so holding the
    /// std `Mutex` across it cannot deadlock. Durability
    /// (`persist`) deliberately runs *after* the lock drops.
    events: broadcast::Sender<WorkspaceEvent>,
    versioned_events: broadcast::Sender<VersionedWorkspaceEvent>,
    /// Where to write the `state.json` file. `None` means the
    /// in-memory variant (used by tests).
    state_path: Option<PathBuf>,
    /// Guards `state.json` writes and tracks the highest commit seq
    /// already persisted. `persist()` serializes on this and skips
    /// any snapshot older than what's on disk, so a slow earlier
    /// commit can't clobber a newer one when writes race. The seq is
    /// assigned under `inner`, so it reflects commit order (#80).
    persist_guard: Mutex<u64>,
    /// One-shot tab layout loaded from `state.json` at `open` time,
    /// awaiting hydration by the UI bootstrap (`take_restore_layout`).
    /// `None` for the in-memory variant and after it's taken. Kept
    /// out of `inner.tabs` — the live tabs are the fresh shells the
    /// UI re-opens from these descriptors.
    restore_layout: Mutex<Option<RestoreLayout>>,
    /// Set by `flush()` on clean exit, *after* it writes the final
    /// layout. Once set, `persist()` is a no-op so a teardown-induced
    /// PTY-exit cascade (the window closing kills its shells) can't
    /// race in and overwrite the flushed layout with an empty one.
    /// Lock-free because `persist()` runs after the `inner` lock drops
    /// — it can't read a field guarded by that lock.
    shutting_down: AtomicBool,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("project {0} not found")]
    ProjectNotFound(i64),
    #[error("tab {0} not found")]
    TabNotFound(i64),
    #[error("tab {tab_id} does not belong to project {project_id}")]
    TabProjectMismatch { project_id: i64, tab_id: i64 },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde_json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("host {0} not found")]
    HostNotFound(String),
    #[error("host label must not be empty")]
    HostLabelEmpty,
    #[error("host label \"local\" is reserved")]
    HostLabelReserved,
    #[error("a host named {0:?} already exists")]
    HostLabelTaken(String),
    /// An app-side invariant did not hold, so the reply would have been
    /// a lie. Answered rather than papered over: today the only source
    /// is `host.status` finding the sidebar's cached bands out of step
    /// with the saved-host list they are built from, where the
    /// alternative is a silently truncated list.
    #[error("{0}")]
    Inconsistent(String),
}

/// Repair persisted project positions that collide, in place.
///
/// **Projects only, deliberately.** A persisted *tab* collision needs no
/// repair: `TabSnapshot` carries no id, and the bootstrap re-opens every
/// restore descriptor through `open_tab` into an empty project, so tab
/// positions are freshly allocated on each launch and a tie self-heals.
/// Project rows are the ones that keep their persisted position.
///
/// Live project rows load their `position` verbatim, so a tie written
/// by a pre-#80 build (which allocated from `len()`) survives every
/// relaunch. Walk the projects in display order — `(position, id)`,
/// the same comparator the sidebar sorts by — and push each one to at
/// least `previous + 1`. The mirror of the Swift
/// `normalizedProjectPositions`; keep the two in step.
///
/// The seed is `None`, not `-1`, so the walk is a true no-op on
/// already-unique input **including negative positions**: `position`
/// deserializes as a plain `i32` with no non-negativity check, and a
/// `-1` seed would rewrite a perfectly ordered `[-5, -3]` to `[0, 1]`.
///
/// Gaps are preserved — `[0, 5, 5]` becomes `[0, 5, 6]`, not
/// `[0, 1, 2]`. Positions are sparse by design; the invariant is
/// uniqueness within a parent, never density, so densifying would
/// rewrite rows that were never wrong.
///
/// `i32::MAX` saturates rather than panicking, matching
/// `next_project_position`: a workspace that reached it is corrupt
/// input, and degrading to a tie beats failing to launch. The ceiling
/// is therefore a tie-degradation boundary, **not** a uniqueness
/// guarantee — repairing `[i32::MAX - 1, i32::MAX - 1]` yields
/// `[i32::MAX - 1, i32::MAX]`, and the next `create_project` clamps
/// onto that `i32::MAX` (plan 004 §3.1).
///
/// Everything above holds **below the ceiling**; at it, the display
/// order is not preserved either. Clamping collapses a row onto
/// `i32::MAX`, and the `(position, id)` comparator then re-breaks that
/// *new* tie by id, which can let a row overtake a sibling:
/// `B@(MAX-1, id2), C@(MAX-1, id3), A@(MAX, id1)` displays `B, C, A`
/// and repairs to `B@MAX-1, C@MAX, A@MAX` — which displays `B, A, C`.
/// Corrupt input degrading to a reshuffle is the accepted cost of not
/// failing to launch; the guarantee is for positions a real workspace
/// can reach.
fn normalize_project_positions(projects: &mut [ProjectSnapshot]) {
    // Sort borrows of the rows, not the slice: the caller's order is
    // the order `RestoreLayout` (and hence the bootstrap) walks, and
    // the order rows are inserted into the id-keyed `projects` map.
    // `state.json` is user-editable, so two rows can share an `id` and
    // collapse last-write-wins — reordering the slice here would
    // silently change which one survives, and Swift's
    // `normalizedProjectPositions` returns file order to match.
    let mut order: Vec<&mut ProjectSnapshot> = projects.iter_mut().collect();
    order.sort_by_key(|p| (p.position, p.id));
    let mut previous: Option<i32> = None;
    for p in order {
        let position = match previous {
            None => p.position,
            Some(prev) => prev.saturating_add(1).max(p.position),
        };
        p.position = position;
        previous = Some(position);
    }
}

/// A saved host's label, checked before it is added: non-empty, unique
/// among the existing saved hosts case-insensitively, and not `local` —
/// that word is the sidebar's reserved header for the in-process
/// workspace, so a saved host claiming it would be indistinguishable
/// from the local section (host-sessions plan §3.1).
fn validate_host_label(existing: &[HostSnapshot], label: &str) -> Result<(), WorkspaceError> {
    if label.is_empty() {
        return Err(WorkspaceError::HostLabelEmpty);
    }
    // Unicode folding, not `eq_ignore_ascii_case`: "Éclair" and "éclair"
    // must collide, or the sidebar shows two visually identical headers.
    //
    // Both directions, because `to_lowercase` is not full case folding:
    // "straße" lowercases to itself while "STRASSE" lowercases to
    // "strasse", so a lowercase-only rule saves both — and the sidebar
    // then draws the header "STRASSE" twice, which is the collision this
    // rule exists to prevent. The uppercase comparison IS that display
    // form, so it catches the pair a lowercase one misses; the lowercase
    // comparison still catches the everyday "Pop-OS" / "pop-os". Either
    // form colliding is a collision.
    let lower = label.to_lowercase();
    let upper = label.to_uppercase();
    if lower == "local" || upper == "LOCAL" {
        return Err(WorkspaceError::HostLabelReserved);
    }
    if existing
        .iter()
        .any(|h| h.label.to_lowercase() == lower || h.label.to_uppercase() == upper)
    {
        return Err(WorkspaceError::HostLabelTaken(label.to_string()));
    }
    Ok(())
}

impl Workspace {
    /// Construct an empty in-memory workspace. Used by tests.
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (versioned_tx, _versioned_rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            inner: Mutex::new(Inner::default()),
            events: tx,
            versioned_events: versioned_tx,
            state_path: None,
            persist_guard: Mutex::new(0),
            restore_layout: Mutex::new(None),
            shutting_down: AtomicBool::new(false),
        }
    }

    /// Construct a workspace backed by `state_path`. Loads the file
    /// if present; corrupt or absent → empty workspace (warn-log).
    pub fn open(state_path: PathBuf) -> Self {
        let mut snapshot = match read_state(&state_path) {
            Ok(Some(s)) => s,
            Ok(None) => SnapshotFile::default(),
            Err(err) => {
                warn!(
                    path = %state_path.display(),
                    ?err,
                    "state.json failed to load; starting empty"
                );
                SnapshotFile::default()
            }
        };
        // Before anything reads a position: a pre-#80 file can hold
        // colliding project positions, and those survive the load.
        normalize_project_positions(&mut snapshot.projects);
        let (tx, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (versioned_tx, _versioned_rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let mut inner = Inner {
            next_id: snapshot.next_id.max(1),
            sidebar_collapsed: snapshot.sidebar_collapsed,
            sidebar_width: normalize_sidebar_width(snapshot.sidebar_width)
                .unwrap_or(SIDEBAR_DEFAULT_WIDTH),
            hosts: std::mem::take(&mut snapshot.hosts),
            ..Inner::default()
        };

        // Build the one-shot restore layout (tab descriptors) BEFORE
        // moving the projects into `inner`. These are NOT inserted as
        // live tabs — the UI bootstrap re-opens them as fresh shells
        // via `take_restore_layout` + the normal open path.
        let restore = RestoreLayout {
            active_project_id: snapshot.active_project_id,
            active_tab_position: snapshot.active_tab_position,
            projects: snapshot
                .projects
                .iter()
                .map(|p| {
                    let mut tabs: Vec<(i32, RestoreTab)> = p
                        .tabs
                        .iter()
                        .map(|t| {
                            (
                                t.position,
                                RestoreTab {
                                    cwd: t.cwd.clone(),
                                    title: t.title.clone(),
                                    user_titled: t.user_titled,
                                },
                            )
                        })
                        .collect();
                    tabs.sort_by_key(|(pos, _)| *pos);
                    RestoreProject {
                        project_id: p.id,
                        tabs: tabs.into_iter().map(|(_, t)| t).collect(),
                    }
                })
                .collect(),
        };

        for p in snapshot.projects {
            inner.projects.insert(
                p.id,
                ProjectRow {
                    id: p.id,
                    name: p.name,
                    cwd: p.cwd,
                    position: p.position,
                    created_at: p.created_at,
                },
            );
        }
        Self {
            inner: Mutex::new(inner),
            events: tx,
            versioned_events: versioned_tx,
            state_path: Some(state_path),
            persist_guard: Mutex::new(0),
            restore_layout: Mutex::new(Some(restore)),
            shutting_down: AtomicBool::new(false),
        }
    }

    /// Take the one-shot tab layout loaded from `state.json` at
    /// `open` time. Returns `None` for the in-memory variant and on
    /// every call after the first. The UI bootstrap calls this once
    /// to re-open each project's saved tabs as fresh shells.
    pub fn take_restore_layout(&self) -> Option<RestoreLayout> {
        self.restore_layout.lock().unwrap().take()
    }

    /// The sidebar's persisted collapsed state. The UI reads this at
    /// startup to restore the user's hide/show choice (Rust UI adapter
    /// (Iced) parity with the Mac UI's `RoostSidebarVisible`).
    pub fn sidebar_collapsed(&self) -> bool {
        self.inner.lock().unwrap().sidebar_collapsed
    }

    /// Record the sidebar's collapsed state and persist it. Emits no
    /// event — the UI that toggled already flipped its own widget; this
    /// only writes the choice through so a relaunch restores it. A no-op
    /// (no write) when unchanged, so re-toggling to the same state can't
    /// churn `state.json`.
    pub fn set_sidebar_collapsed(&self, collapsed: bool) {
        let mut inner = self.inner.lock().unwrap();
        if inner.sidebar_collapsed == collapsed {
            return;
        }
        inner.sidebar_collapsed = collapsed;
        self.commit(inner, Vec::new(), Persist::Write);
    }

    /// The sidebar's persisted width in logical points. The UI reads
    /// this at startup to restore the user's drag (Rust UI adapter
    /// (Iced) parity with the Mac UI's `RoostSidebarWidth`).
    pub fn sidebar_width(&self) -> f64 {
        self.inner.lock().unwrap().sidebar_width
    }

    /// Record the sidebar's width and persist it, clamped into
    /// [`SIDEBAR_MIN_WIDTH`]..=[`SIDEBAR_MAX_WIDTH`]. Emits no event —
    /// the UI that dragged already resized its own widget; this only
    /// writes the choice through so a relaunch restores it. A no-op (no
    /// write) when the normalized width is unchanged, so a drag that
    /// settles on the clamp can't churn `state.json`, and a non-finite
    /// width is ignored outright.
    pub fn set_sidebar_width(&self, width: f64) {
        let Some(width) = normalize_sidebar_width(width) else {
            return;
        };
        let mut inner = self.inner.lock().unwrap();
        if inner.sidebar_width == width {
            return;
        }
        inner.sidebar_width = width;
        self.commit(inner, Vec::new(), Persist::Write);
    }

    /// Report the UI window's focus state. Half of the notification
    /// suppression predicate (plan §3.5); the other half — which tab is
    /// active — the workspace already owns, so [`raise_attention`] can
    /// decide suppression atomically instead of the UI re-deriving it
    /// after the fact.
    ///
    /// [`raise_attention`]: Workspace::raise_attention
    pub fn set_window_focused(&self, focused: bool) {
        self.inner.lock().unwrap().window_focused = focused;
    }

    /// The client's saved host sessions, in the order the sidebar lists
    /// them. Purely a registry read — connecting to one is HS-2's
    /// `HostConn`, not this crate.
    pub fn hosts(&self) -> Vec<HostSnapshot> {
        self.inner.lock().unwrap().hosts.clone()
    }

    /// Save a new host, minting its stable id. Rejects the label per
    /// [`validate_host_label`]; `target` is carried opaquely (a socket
    /// path or, later, an SSH destination — HS-3) and is not validated
    /// here, since only actually dialing it (`roostctl host add
    /// --verify`, or the UI's "Add & Connect") can tell whether it is
    /// reachable.
    pub fn add_host(&self, label: &str, target: &str) -> Result<HostSnapshot, WorkspaceError> {
        // Trimmed before validation AND storage, so "  " cannot pass the
        // non-empty check and " local " cannot dodge the reserved one.
        let label = label.trim();
        let mut inner = self.inner.lock().unwrap();
        validate_host_label(&inner.hosts, label)?;
        let host = HostSnapshot {
            id: mint_host_id(&inner.hosts),
            label: label.to_string(),
            target: target.to_string(),
            last_connected: None,
        };
        inner.hosts.push(host.clone());
        self.commit(inner, Vec::new(), Persist::Write);
        Ok(host)
    }

    /// Would this label be accepted? The registry's own rules, asked
    /// without saving.
    ///
    /// The Add Host dialog checks here before it spends a round trip
    /// dialing the socket, so "local is reserved" is answered
    /// immediately rather than after a five-second timeout on a name
    /// that was never going to be saved. [`Self::add_host`] re-validates
    /// under the lock, so this is a courtesy and never the enforcement —
    /// which is also why it is the same function underneath: a second
    /// spelling of the rules would drift.
    pub fn check_host_label(&self, label: &str) -> Result<(), WorkspaceError> {
        let inner = self.inner.lock().unwrap();
        validate_host_label(&inner.hosts, label.trim())
    }

    /// Forget a saved host by its stable id. Does not touch a live
    /// connection — HostConn owns disconnecting before a remove reaches
    /// here (the UI only offers Remove while disconnected, §3.1).
    pub fn remove_host(&self, id: &str) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let before = inner.hosts.len();
        inner.hosts.retain(|h| h.id != id);
        if inner.hosts.len() == before {
            return Err(WorkspaceError::HostNotFound(id.to_string()));
        }
        self.commit(inner, Vec::new(), Persist::Write);
        Ok(())
    }

    /// Stamp a saved host's `last_connected` with the current time
    /// (RFC3339 UTC), for a successful connect. Owned by whoever mints
    /// the connection (HostConn, or `roostctl host add --verify`) —
    /// this accessor only persists the stamp.
    pub fn touch_host_connected(&self, id: &str) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let host = inner
            .hosts
            .iter_mut()
            .find(|h| h.id == id)
            .ok_or_else(|| WorkspaceError::HostNotFound(id.to_string()))?;
        host.last_connected = Some(rfc3339(std::time::SystemTime::now()));
        self.commit(inner, Vec::new(), Persist::Write);
        Ok(())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<WorkspaceEvent> {
        self.events.subscribe()
    }

    /// Subscribe to events carrying their authoritative commit revision.
    pub fn subscribe_versioned(&self) -> broadcast::Receiver<VersionedWorkspaceEvent> {
        self.versioned_events.subscribe()
    }

    /// Snapshot of the workspace as it appears on the wire.
    pub fn snapshot(&self) -> Vec<Project> {
        let inner = self.inner.lock().unwrap();
        self.snapshot_locked(&inner)
    }

    /// The revision of the most recent commit.
    ///
    /// For a caller that only needs the fence, not the state — the event
    /// push reads it right after subscribing, where snapshotting every
    /// project would be pure waste.
    pub fn revision(&self) -> u64 {
        self.inner.lock().unwrap().revision
    }

    /// Full ordered snapshot and revision captured under one lock.
    pub fn snapshot_with_revision(&self) -> (u64, Vec<Project>) {
        let inner = self.inner.lock().unwrap();
        (inner.revision, self.snapshot_locked(&inner))
    }

    #[cfg(feature = "facade")]
    pub(crate) fn snapshot_parts(&self) -> (u64, Vec<Project>, i64, i64, bool) {
        let inner = self.inner.lock().unwrap();
        (
            inner.revision,
            self.snapshot_locked(&inner),
            inner.active_project_id,
            inner.active_tab_id,
            inner.sidebar_collapsed,
        )
    }

    fn snapshot_locked(&self, inner: &Inner) -> Vec<Project> {
        let mut out: Vec<Project> = inner
            .projects
            .values()
            .map(|p| Project {
                id: p.id,
                name: p.name.clone(),
                cwd: p.cwd.clone(),
                position: p.position,
                created_at: p.created_at,
                tabs: inner
                    .tabs
                    .values()
                    .filter(|t| t.project_id == p.id)
                    .map(|t| self.to_wire_tab(t, inner))
                    .collect(),
            })
            .collect();
        out.sort_by_key(|p| (p.position, p.id));
        for p in &mut out {
            p.tabs.sort_by_key(|t| (t.position, t.id));
        }
        out
    }

    /// Build a `Resync` event carrying the current full snapshot.
    /// The event bridge sends this on broadcast `Lagged`.
    pub fn resync_event(&self) -> WorkspaceEvent {
        WorkspaceEvent::Resync(self.snapshot())
    }

    pub fn active(&self) -> (i64, i64) {
        let inner = self.inner.lock().unwrap();
        (inner.active_project_id, inner.active_tab_id)
    }

    /// Return the project's last selected live tab, falling back to its first
    /// tab in display order. UI adapters use this for project switching so the
    /// selection policy remains authoritative and toolkit-neutral.
    pub fn preferred_tab(&self, project_id: i64) -> Option<i64> {
        self.inner.lock().unwrap().preferred_tab(project_id)
    }

    /// Ensure a default project exists; return its id. Used by
    /// `tab.open` when the client passes `project_id = 0`.
    pub fn ensure_default_project(&self, cwd: &str) -> i64 {
        let mut inner = self.inner.lock().unwrap();
        if let Some(p) = inner.projects.values().next() {
            let id = p.id;
            let mut events = Vec::new();
            if inner.active_project_id == 0 {
                inner.active_project_id = id;
                events.push(WorkspaceEvent::ActiveChanged {
                    project_id: inner.active_project_id,
                    tab_id: inner.active_tab_id,
                });
            }
            // No inline write here (as before): the only mutation is
            // the active selection, which `flush()` captures on exit.
            self.commit(inner, events, Persist::Skip);
            return id;
        }
        let id = inner.alloc_id();
        let position = inner.next_project_position();
        let now = unix_now();
        inner.projects.insert(
            id,
            ProjectRow {
                id,
                name: "Default".into(),
                cwd: cwd.to_string(),
                position,
                created_at: now,
            },
        );
        inner.active_project_id = id;
        let project = Project {
            id,
            name: "Default".into(),
            cwd: cwd.to_string(),
            position,
            created_at: now,
            tabs: vec![],
        };
        let events = vec![
            WorkspaceEvent::ProjectCreated(project),
            WorkspaceEvent::ActiveChanged {
                project_id: id,
                tab_id: 0,
            },
        ];
        self.commit(inner, events, Persist::Write);
        id
    }

    pub fn create_project(&self, name: &str, cwd: &str) -> Result<Project, WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let id = inner.alloc_id();
        let position = inner.next_project_position();
        let chosen_name = if name.is_empty() {
            format!("Untitled {}", inner.projects.len() + 1)
        } else {
            name.to_string()
        };
        let row = ProjectRow {
            id,
            name: chosen_name,
            cwd: cwd.to_string(),
            position,
            created_at: unix_now(),
        };
        inner.projects.insert(id, row.clone());

        let project = Project {
            id: row.id,
            name: row.name,
            cwd: row.cwd,
            position: row.position,
            created_at: row.created_at,
            tabs: vec![],
        };
        self.commit(
            inner,
            vec![WorkspaceEvent::ProjectCreated(project.clone())],
            Persist::Write,
        );
        Ok(project)
    }

    pub fn rename_project(&self, project_id: i64, name: &str) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .projects
            .get_mut(&project_id)
            .ok_or(WorkspaceError::ProjectNotFound(project_id))?;
        row.name = name.to_string();
        self.commit(
            inner,
            vec![WorkspaceEvent::ProjectRenamed {
                project_id,
                name: name.to_string(),
            }],
            Persist::Write,
        );
        Ok(())
    }

    /// Delete a project. Cascades to its tabs (per-tab
    /// `TabClosed` events emitted first, then `ProjectDeleted`,
    /// then `ActiveChanged` if the selection moved). PTY cleanup
    /// is the caller's responsibility — the workspace doesn't own
    /// a `PtySupervisor` reference.
    pub fn delete_project(&self, project_id: i64) -> Result<Vec<i64>, WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.projects.contains_key(&project_id) {
            return Err(WorkspaceError::ProjectNotFound(project_id));
        }
        let tab_ids: Vec<i64> = inner
            .tabs
            .values()
            .filter(|t| t.project_id == project_id)
            .map(|t| t.id)
            .collect();
        for tid in &tab_ids {
            inner.tabs.remove(tid);
        }
        inner.projects.remove(&project_id);
        inner.active_tabs_by_project.remove(&project_id);

        // Adjust active selection if it points at the deleted
        // project or one of its tabs.
        let mut active_changed = false;
        if inner.active_project_id == project_id || tab_ids.contains(&inner.active_tab_id) {
            let fallback_project = inner.projects.keys().next().copied().unwrap_or(0);
            let fallback_tab = inner.preferred_tab(fallback_project).unwrap_or(0);
            inner.active_project_id = fallback_project;
            inner.active_tab_id = fallback_tab;
            active_changed = true;
        }
        let active = if active_changed {
            Some((inner.active_project_id, inner.active_tab_id))
        } else {
            None
        };

        // Commit order: TabClosed* → ProjectDeleted → ActiveChanged.
        let mut events: Vec<WorkspaceEvent> = tab_ids
            .iter()
            .map(|tid| WorkspaceEvent::TabClosed { tab_id: *tid })
            .collect();
        events.push(WorkspaceEvent::ProjectDeleted { project_id });
        if let Some((pid, tid)) = active {
            events.push(WorkspaceEvent::ActiveChanged {
                project_id: pid,
                tab_id: tid,
            });
        }
        self.commit(inner, events, Persist::Write);
        Ok(tab_ids)
    }

    /// Open a new tab in `project_id`. Returns the wire-format
    /// `Tab`. Caller spawns the PTY.
    ///
    /// An empty `cwd` is resolved here, not stored verbatim:
    /// requested → the project's cwd → `$HOME` → `/`. Every path
    /// (`ops::TAB_OPEN`, the facade, `LocalClient::open_tab`) reaches
    /// this method, so title derivation below sees the resolved
    /// value too.
    pub fn open_tab(&self, project_id: i64, cwd: &str, title: &str) -> Result<Tab, WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let project_cwd = match inner.projects.get(&project_id) {
            Some(p) => p.cwd.clone(),
            None => return Err(WorkspaceError::ProjectNotFound(project_id)),
        };
        let cwd: String = if !cwd.is_empty() {
            cwd.to_string()
        } else if !project_cwd.is_empty() {
            project_cwd
        } else {
            std::env::var("HOME").unwrap_or_else(|_| "/".into())
        };
        let id = inner.alloc_id();
        let now = unix_now();
        let position = inner.next_tab_position(project_id);
        let derived_title = if title.is_empty() {
            derive_title(&cwd)
        } else {
            title.to_string()
        };
        let row = TabRow {
            id,
            project_id,
            title: derived_title.clone(),
            cwd,
            agent: AgentTabState::default(),
            has_notification: false,
            // Always start with user_titled=false. The caller-
            // supplied `title` is a placeholder (e.g. UI's
            // "roost-mac N" / CLI's "roostctl" default) that
            // shell-side OSC 0/1/2 emissions should be allowed to
            // overwrite. Only an explicit user rename via
            // `set_tab_title` flips this to true. The previous
            // `!title.is_empty()` policy locked every newly-opened
            // tab to its placeholder, preventing shell prompts
            // like `👻 /tmp` from ever appearing in the tab bar.
            // Mirrors the Mac fix in `mac/Sources/Roost/Workspace.swift`.
            user_titled: false,
            position,
            created_at: now,
            last_active: now,
        };
        inner.tabs.insert(id, row.clone());
        // New tabs steal the active selection.
        inner.active_project_id = project_id;
        inner.active_tab_id = id;
        inner.active_tabs_by_project.insert(project_id, id);

        let tab = self.to_wire_tab(&row, &inner);
        self.commit(
            inner,
            vec![
                WorkspaceEvent::TabOpened(tab.clone()),
                WorkspaceEvent::ActiveChanged {
                    project_id,
                    tab_id: id,
                },
            ],
            Persist::Write,
        );
        Ok(tab)
    }

    /// Close a tab. If it was the project's **last** tab, the
    /// project is closed too (mirrors `delete_project`'s cascade) so
    /// a project can never linger with zero live tabs. The event
    /// order in that case is `TabClosed → ProjectDeleted →
    /// ActiveChanged`, matching `delete_project`; both UIs already
    /// converge on `ProjectDeleted` (remove the sidebar row, pick a
    /// fallback project, or close the window when none remain).
    pub fn close_tab(&self, tab_id: i64) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .tabs
            .remove(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        let project_id = row.project_id;

        // Last tab in the project? Cascade-close the project. Inlined
        // rather than calling `delete_project` so the event order is
        // exactly TabClosed → ProjectDeleted → ActiveChanged (the tab
        // is already removed; `delete_project` would re-emit it).
        let project_emptied = inner.projects.contains_key(&project_id)
            && !inner.tabs.values().any(|t| t.project_id == project_id);
        if project_emptied {
            inner.projects.remove(&project_id);
            inner.active_tabs_by_project.remove(&project_id);
        } else if inner.active_tabs_by_project.get(&project_id) == Some(&tab_id) {
            let replacement = inner.first_tab(project_id);
            if let Some(replacement) = replacement {
                inner.active_tabs_by_project.insert(project_id, replacement);
            } else {
                inner.active_tabs_by_project.remove(&project_id);
            }
        }

        // Reassign the active selection if it pointed at the closed
        // tab (or, when the project went away, at that project).
        let mut changed = false;
        if inner.active_tab_id == tab_id
            || (project_emptied && inner.active_project_id == project_id)
        {
            let next = if project_emptied {
                // Project gone: fall back to another project's tab.
                let fallback_project = inner.projects.keys().next().copied().unwrap_or(0);
                let fallback_tab = inner.preferred_tab(fallback_project).unwrap_or(0);
                (fallback_project, fallback_tab)
            } else {
                // Project survives: fall back to a sibling tab, else
                // any tab anywhere.
                inner
                    .preferred_tab(project_id)
                    .and_then(|tab_id| inner.tabs.get(&tab_id))
                    .or_else(|| inner.tabs.values().next())
                    .map(|t| (t.project_id, t.id))
                    .unwrap_or((project_id, 0))
            };
            inner.active_project_id = next.0;
            inner.active_tab_id = next.1;
            if next.1 != 0 {
                inner.active_tabs_by_project.insert(next.0, next.1);
            }
            changed = true;
        }
        let active = if changed {
            Some((inner.active_project_id, inner.active_tab_id))
        } else {
            None
        };

        // Commit order: TabClosed → ProjectDeleted? → ActiveChanged?.
        let mut events = vec![WorkspaceEvent::TabClosed { tab_id }];
        if project_emptied {
            events.push(WorkspaceEvent::ProjectDeleted { project_id });
        }
        if let Some((pid, tid)) = active {
            events.push(WorkspaceEvent::ActiveChanged {
                project_id: pid,
                tab_id: tid,
            });
        }
        self.commit(inner, events, Persist::Write);
        Ok(())
    }

    pub fn set_tab_title(&self, tab_id: i64, title: &str) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .tabs
            .get_mut(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        row.title = title.to_string();
        row.user_titled = true;
        self.commit(
            inner,
            vec![WorkspaceEvent::TabTitleChanged {
                tab_id,
                title: title.to_string(),
            }],
            Persist::Write,
        );
        Ok(())
    }

    /// OSC 0/1/2 paths set the title only if the user hasn't
    /// manually renamed the tab.
    pub fn set_tab_title_from_osc(&self, tab_id: i64, title: &str) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .tabs
            .get_mut(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        if row.user_titled {
            return Ok(());
        }
        row.title = title.to_string();
        // Shell-driven (OSC titles fire per prompt) but the write is
        // cheap now (no fsync until flush()), so write through.
        self.commit(
            inner,
            vec![WorkspaceEvent::TabTitleChanged {
                tab_id,
                title: title.to_string(),
            }],
            Persist::Write,
        );
        Ok(())
    }

    pub fn set_tab_cwd(&self, tab_id: i64, cwd: &str) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .tabs
            .get_mut(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        let cwd_owned = cwd.to_string();
        row.cwd = cwd_owned.clone();
        // Shell-driven (OSC 7 fires per `cd`) but write-through: each
        // change lands in the page cache without an fsync, so a `cd`
        // loop is cheap and the latest cwd is always on disk.
        let mut events = vec![WorkspaceEvent::TabCwdChanged {
            tab_id,
            cwd: cwd_owned,
        }];
        // Re-derive title from cwd when the user hasn't explicitly
        // renamed (mirrors `set_tab_title_from_osc`'s `user_titled`
        // gate). Lets the title follow cwd on shells without
        // integration (Apple /bin/bash 3.2, --norc bash). On integrated
        // shells, the next prompt's OSC 0 refines this basename to the
        // tilde-abbreviated path via `set_tab_title_from_osc` —
        // latest-wins. Event order is cwd-then-title (cause-then-effect).
        if !row.user_titled {
            let new_title = derive_title(cwd);
            if row.title != new_title {
                row.title = new_title.clone();
                events.push(WorkspaceEvent::TabTitleChanged {
                    tab_id,
                    title: new_title,
                });
            }
        }
        self.commit(inner, events, Persist::Write);
        Ok(())
    }

    /// Raise a tab's attention — pending badge, inbox row, and desktop
    /// banner — as **one transaction**. Returns whether it was delivered.
    ///
    /// Every gate lives here, under the same lock as the mutation,
    /// because each is a race otherwise. Reading the agent gate and then
    /// committing lets a concurrent `tab.agent_report` claim the tab in
    /// between; leaving focus to the UI's event drain re-evaluates it
    /// against a selection the user may have changed since, which both
    /// loses notifications and resurrects them retroactively.
    ///
    /// A suppressed raise is dropped **entirely** — no pending bit, no
    /// events. That is what makes plan §3.5's "switching away afterwards
    /// does not retroactively produce a badge" true by construction
    /// rather than by a UI write-back.
    pub fn raise_attention(
        &self,
        tab_id: i64,
        title: &str,
        body: &str,
        source: AttentionSource,
    ) -> Result<bool, WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let focus_suppressed = inner.attention_suppressed_by_focus(tab_id);
        // A 'lookup' error rather than a silent drop: hook tools racing
        // a tab close need to tell "gone" from "suppressed".
        let row = inner
            .tabs
            .get_mut(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        // Only *raw* OSC is gated on ownership. A structured
        // `notification.create` / `roostctl notify` is never suppressed
        // this way (plan §3.4) — the agent's own adapter is what emits
        // those, so gating them would mute the very source it protects.
        if source == AttentionSource::RawOsc && agent::suppress_raw_osc(&row.agent) {
            tracing::debug!(tab_id, "raw OSC notification suppressed (agent owns tab)");
            return Ok(false);
        }
        if focus_suppressed {
            return Ok(false);
        }
        row.has_notification = true;
        // Notification state isn't in the persisted snapshot — emit only.
        self.commit(
            inner,
            vec![
                WorkspaceEvent::TabNotification {
                    tab_id,
                    has_pending: true,
                },
                WorkspaceEvent::NotificationFired {
                    tab_id,
                    title: title.to_string(),
                    body: body.to_string(),
                },
            ],
            Persist::Skip,
        );
        Ok(true)
    }

    /// Publish one client-local tab effect (plan 037 §3.6) — a bell or
    /// an OSC 52 clipboard write a server-VT tab produced.
    ///
    /// A commit like any other, for the reason the events stream's
    /// whole loss-detection protocol depends on: every batch a
    /// subscriber sees carries the next revision, so an effect that
    /// rode outside the sequence would either need a revision it
    /// cannot have or a second delivery channel with no ordering
    /// against the state changes around it. `Persist::Skip` — nothing
    /// here is in the snapshot.
    ///
    /// Not gated on the tab still existing: the tab task is the only
    /// caller and it emits from inside its own tab's pipeline —
    /// `pub(crate)` so that stays true. That single caller is
    /// `server-vt`-only, so without the feature this is genuinely dead
    /// rather than merely unreferenced — which the iced lane's
    /// `-D warnings` lint of a featureless `roost-engine` catches.
    #[cfg_attr(not(feature = "server-vt"), allow(dead_code))]
    pub(crate) fn publish_tab_effect(&self, tab_id: i64, effect: TabEffectKind) {
        let inner = self.inner.lock().unwrap();
        // The tab task outlives its workspace row on purpose (it keeps
        // ingesting descendant output past the exit deadline), so a late
        // BEL or OSC 52 can arrive here after `tab.closed` committed. An
        // effect for a tab a subscriber has already watched close is
        // noise at best and a mis-attribution at worst — checked under
        // the same lock as the commit, so close-then-effect cannot
        // interleave the other way around.
        if !inner.tabs.contains_key(&tab_id) {
            tracing::debug!(tab_id, "dropping a tab effect for a closed tab");
            return;
        }
        self.commit(
            inner,
            vec![WorkspaceEvent::TabEffect { tab_id, effect }],
            Persist::Skip,
        );
    }

    /// `tab.agent_report` — the one op every agent adapter writes
    /// through. Session scoping, ownership supersede, and the patch
    /// semantics all live in [`agent::apply_report`]; this method owns
    /// only the mutation and the event fan-out.
    ///
    /// Returns the post-report tab plus whether the report was accepted
    /// (false = dropped on an ownership mismatch, tab unchanged).
    pub fn agent_report(
        &self,
        report: &TabAgentReportParams,
    ) -> Result<(bool, Tab), WorkspaceError> {
        self.apply_agent_reports(report, None)
    }

    /// Legacy `tab.set_state`, re-expressed on the agent axis per plan
    /// §3.7. Each value claims ownership as `manual` (an empty session
    /// id) so the user's override supersedes a live agent — that is
    /// what taking the wheel means.
    ///
    /// `none` additionally releases: "no state" most closely means "no
    /// agent owns this tab", so derivation falls back to the shell axis
    /// (`none` at a prompt, `running` under a live foreground process).
    /// It claims *before* releasing because a bare release is scoped to
    /// the current owner — claiming first is the only path that can
    /// take the tab from someone else, and it keeps the matching rule
    /// inside `agent::apply_report` rather than duplicated here.
    pub fn set_tab_state(&self, tab_id: i64, state: TabState) -> Result<(), WorkspaceError> {
        let lifecycle = match state {
            TabState::Running => AgentLifecycle::Working,
            TabState::NeedsInput => AgentLifecycle::Waiting,
            TabState::Idle => AgentLifecycle::Finished,
            TabState::None => AgentLifecycle::Inactive,
        };
        let claim = TabAgentReportParams::sessionless(
            tab_id,
            SOURCE_MANUAL,
            OwnershipAction::Claim,
            Some(lifecycle),
        );
        let release = (state == TabState::None).then(|| {
            TabAgentReportParams::sessionless(tab_id, SOURCE_MANUAL, OwnershipAction::Release, None)
        });
        self.apply_agent_reports(&claim, release.as_ref())
            .map(|_| ())
    }

    /// Deprecated `tab.set_hook_active` alias (plan §3.6): claim or
    /// release as `legacy` with an empty session id. Release is scoped
    /// like any other, so it cannot revoke a real agent's ownership.
    pub fn set_tab_hook_active(&self, tab_id: i64, active: bool) -> Result<(), WorkspaceError> {
        let action = if active {
            OwnershipAction::Claim
        } else {
            OwnershipAction::Release
        };
        let report = TabAgentReportParams::sessionless(tab_id, SOURCE_LEGACY, action, None);
        self.apply_agent_reports(&report, None).map(|_| ())
    }

    /// OSC 133 prompt/command mark → the **shell** axis, ungated.
    ///
    /// The old `hook_active` gate is gone: the axes are independent, so
    /// there is nothing to suppress — derivation decides which one
    /// shows. `A`/`B`/`D` additionally drop the lifecycle to `inactive`
    /// (keeping ownership as a label), which is the failsafe against a
    /// killed agent muting the tab forever. See [`agent::apply_shell_mark`].
    pub fn apply_shell_mark(&self, tab_id: i64, body: &str) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .tabs
            .get_mut(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        let Some(next) = agent::apply_shell_mark(&row.agent, body) else {
            return Ok(()); // undefined mark body — no change
        };
        let events = replace_agent(row, next);
        // Run state isn't in the persisted snapshot — emit only.
        self.commit(inner, events, Persist::Skip);
        Ok(())
    }

    /// The tab's PTY was replaced: the shell that hosted any agent is
    /// gone, so both axes and ownership reset.
    ///
    /// Stated as a rule about the PTY rather than about closing —
    /// closing drops the whole row, so it needs no help. #170's
    /// hard-restart keeps the row and is this call.
    pub fn pty_replaced(&self, tab_id: i64) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .tabs
            .get_mut(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        let events = replace_agent(row, AgentTabState::default());
        self.commit(inner, events, Persist::Skip);
        Ok(())
    }

    /// Apply `first` and then `then` under one lock, emitting the
    /// derived-state deltas once for the net result.
    ///
    /// The second slot exists for exactly one caller — `set-state none`,
    /// a claim followed by a release. Applying the pair under one lock
    /// keeps it atomic and stops the intermediate claim from reaching
    /// subscribers as a real state.
    fn apply_agent_reports(
        &self,
        first: &TabAgentReportParams,
        then: Option<&TabAgentReportParams>,
    ) -> Result<(bool, Tab), WorkspaceError> {
        let tab_id = first.tab_id;
        let now = unix_now();
        let mut inner = self.inner.lock().unwrap();
        let is_active = inner.active_tab_id == tab_id;
        let focus_suppressed = inner.attention_suppressed_by_focus(tab_id);
        let row = inner
            .tabs
            .get_mut(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;

        let mut next = row.agent.clone();
        let mut accepted = false;
        let mut attention = AttentionEffect::Unchanged;
        for report in std::iter::once(first).chain(then) {
            let outcome = agent::apply_report(&next, report, now);
            if !outcome.accepted {
                continue;
            }
            accepted = true;
            next = outcome.state;
            if outcome.attention != AttentionEffect::Unchanged {
                attention = outcome.attention;
            }
        }

        let mut events = replace_agent(row, next);
        match attention {
            // Same focus predicate as `raise_attention`, applied here
            // rather than by delegating because the report's own
            // mutation must stay in this transaction: a Claude
            // notification for the tab you are looking at is suppressed
            // exactly like any other, but its lifecycle change is not.
            //
            // `severity` stops here in v1 by design: policy B (plan
            // §3.5) suppresses on focus alone, and "failed overrides
            // suppression" is a later slice. It stays readable on the
            // report itself rather than being plumbed through an event
            // nobody reads yet.
            AttentionEffect::Set {
                title,
                body,
                severity: _,
            } if !focus_suppressed => {
                row.has_notification = true;
                events.push(WorkspaceEvent::TabNotification {
                    tab_id,
                    has_pending: true,
                });
                events.push(WorkspaceEvent::NotificationFired {
                    tab_id,
                    title,
                    body,
                });
            }
            AttentionEffect::Set { .. } => {}
            AttentionEffect::Clear => {
                row.has_notification = false;
                events.push(WorkspaceEvent::TabNotification {
                    tab_id,
                    has_pending: false,
                });
            }
            AttentionEffect::Unchanged => {}
        }
        let tab = wire_tab(row, is_active);
        // Run state isn't in the persisted snapshot — emit only.
        self.commit(inner, events, Persist::Skip);
        Ok((accepted, tab))
    }

    pub fn set_tab_has_notification(
        &self,
        tab_id: i64,
        has_pending: bool,
    ) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let row = inner
            .tabs
            .get_mut(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        row.has_notification = has_pending;
        // Notification flag isn't in the persisted snapshot — emit only.
        self.commit(
            inner,
            vec![WorkspaceEvent::TabNotification {
                tab_id,
                has_pending,
            }],
            Persist::Skip,
        );
        Ok(())
    }

    pub fn focus_tab(&self, tab_id: i64) -> Result<(i64, i64), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let (prev, now) = inner.select_tab(tab_id)?;
        // Persist the active selection so it survives a relaunch
        // (restored by position). Skip when unchanged — focusing the
        // already-active tab shouldn't churn the file. The
        // acknowledgement below never moves this: the notification flag
        // is not in the persisted snapshot.
        let persist = if prev != now {
            Persist::Write
        } else {
            Persist::Skip
        };
        let mut events = vec![WorkspaceEvent::ActiveChanged {
            project_id: now.0,
            tab_id: now.1,
        }];
        // Focusing a tab acknowledges its notification (`select_tab`
        // above already refused an unknown tab). Conditional because
        // `TabNotification` is an edge — emitting it on every focus
        // would announce a change that did not happen.
        if let Some(row) = inner.tabs.get_mut(&tab_id) {
            if row.has_notification {
                row.has_notification = false;
                events.push(WorkspaceEvent::TabNotification {
                    tab_id,
                    has_pending: false,
                });
            }
        }
        self.commit(inner, events, persist);
        Ok(prev)
    }

    /// An attached client stating what it is looking at (host sessions,
    /// HS-3's `session.set_focus`).
    ///
    /// `Some(tab)` means "my window is focused and this session's tab is
    /// the one on screen": the session takes both halves of the
    /// suppression predicate from the client — the window flag *and* the
    /// selection, which moves through the same `select_tab` the
    /// ordinary focus path uses so the session's own active tab and its
    /// persisted selection agree with what the user is actually looking
    /// at. It deliberately stops short of the rest of [`Self::focus_tab`]:
    /// saying what you are looking at does not acknowledge the tab's
    /// notification — the attached client sends `tab.clear_notification`
    /// for that. `None` means nothing here is
    /// being looked at (the window lost focus, or the selection moved
    /// elsewhere) and moves only the flag: the selection is where the
    /// client left it, and forgetting it would re-open the tab that the
    /// next reconnect restores to.
    ///
    /// Atomic in the sense the caller needs: an unknown tab is refused
    /// with **nothing** applied, so a client that names a tab which just
    /// closed does not silently flip the session to "focused" against
    /// whatever tab happened to be active.
    pub fn set_client_focus(&self, focused_tab_id: Option<i64>) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        let Some(tab_id) = focused_tab_id else {
            inner.window_focused = false;
            return Ok(());
        };
        // Before the flag moves: a refusal must leave the session
        // exactly as it was.
        let (prev, now) = inner.select_tab(tab_id)?;
        inner.window_focused = true;
        if prev == now {
            // The selection did not move, so there is nothing to
            // announce and nothing to write — and announcing anyway
            // would make every window-focus change a workspace commit
            // that every attached client re-renders on. The flag itself
            // is never persisted and no event carries it.
            return Ok(());
        }
        self.commit(
            inner,
            vec![WorkspaceEvent::ActiveChanged {
                project_id: now.0,
                tab_id: now.1,
            }],
            Persist::Write,
        );
        Ok(())
    }

    /// The attached client is gone: nobody is looking at this session.
    ///
    /// Called when the interactive lease turns over and when the last
    /// connection holding it closes. Only the flag moves — the selection
    /// stays where the departed client left it, so a reconnect restores
    /// the same tab. Without this, one `session.set_focus` would outlive
    /// the client that sent it and mute a tab for whoever comes next,
    /// which is the exact bug the op exists to fix.
    pub fn release_client_focus(&self) {
        self.inner.lock().unwrap().window_focused = false;
    }

    pub fn reorder_tabs(&self, project_id: i64, tab_ids: &[i64]) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.projects.contains_key(&project_id) {
            return Err(WorkspaceError::ProjectNotFound(project_id));
        }
        // Validate all referenced tabs exist and belong to the project.
        for tid in tab_ids {
            let row = inner
                .tabs
                .get(tid)
                .ok_or(WorkspaceError::TabNotFound(*tid))?;
            if row.project_id != project_id {
                return Err(WorkspaceError::TabProjectMismatch {
                    project_id,
                    tab_id: *tid,
                });
            }
        }
        // Reassign positions in the order given, then keep any
        // tabs not listed at their relative trailing positions.
        let mut next_pos = 0i32;
        for tid in tab_ids {
            if let Some(row) = inner.tabs.get_mut(tid) {
                row.position = next_pos;
                next_pos += 1;
            }
        }
        // Tabs in the project that were not listed: append in
        // their existing order.
        let mut unlisted: Vec<i64> = inner
            .tabs
            .values()
            .filter(|t| t.project_id == project_id && !tab_ids.contains(&t.id))
            .map(|t| t.id)
            .collect();
        // `(position, id)`, not position alone: the loop below assigns
        // `position = next_pos++` from this order, so a tie must not be
        // left to the container. `BTreeMap` + a stable sort already
        // yields ascending id here — state the rule rather than inherit
        // it (#262).
        unlisted.sort_by_key(|tid| (inner.tabs.get(tid).map(|r| r.position).unwrap_or(0), *tid));
        for tid in &unlisted {
            if let Some(row) = inner.tabs.get_mut(tid) {
                row.position = next_pos;
                next_pos += 1;
            }
        }
        // Compute the full post-reorder order for the event
        // payload: supplied prefix + sorted unlisted (matches
        // Mac's `Workspace.tabsReordered` payload shape).
        let final_order: Vec<i64> = tab_ids.iter().copied().chain(unlisted).collect();
        self.commit(
            inner,
            vec![WorkspaceEvent::TabsReordered {
                project_id,
                tab_ids: final_order,
            }],
            Persist::Write,
        );
        Ok(())
    }

    pub fn reorder_projects(&self, project_ids: &[i64]) -> Result<(), WorkspaceError> {
        let mut inner = self.inner.lock().unwrap();
        for pid in project_ids {
            if !inner.projects.contains_key(pid) {
                return Err(WorkspaceError::ProjectNotFound(*pid));
            }
        }
        let mut next_pos = 0i32;
        for pid in project_ids {
            if let Some(row) = inner.projects.get_mut(pid) {
                row.position = next_pos;
                next_pos += 1;
            }
        }
        let mut unlisted: Vec<i64> = inner
            .projects
            .values()
            .filter(|p| !project_ids.contains(&p.id))
            .map(|p| p.id)
            .collect();
        unlisted.sort_by_key(|pid| {
            (
                inner.projects.get(pid).map(|r| r.position).unwrap_or(0),
                *pid,
            )
        });
        for pid in &unlisted {
            if let Some(row) = inner.projects.get_mut(pid) {
                row.position = next_pos;
                next_pos += 1;
            }
        }
        let final_order: Vec<i64> = project_ids.iter().copied().chain(unlisted).collect();
        self.commit(
            inner,
            vec![WorkspaceEvent::ProjectsReordered {
                project_ids: final_order,
            }],
            Persist::Write,
        );
        Ok(())
    }

    pub fn tab(&self, tab_id: i64) -> Result<Tab, WorkspaceError> {
        let inner = self.inner.lock().unwrap();
        let is_active = inner.active_tab_id == tab_id;
        let row = inner
            .tabs
            .get(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?;
        Ok(wire_tab(row, is_active))
    }

    fn to_wire_tab(&self, row: &TabRow, inner: &Inner) -> Tab {
        wire_tab(row, inner.active_tab_id == row.id)
    }

    /// Persist `snapshot` to `state.json`, tagged with its commit
    /// `seq`. Runs synchronously on the caller's thread (the inner
    /// lock is already released; writes are small atomic renames).
    /// `sync` forces an `fsync` (the clean-exit `flush()` path);
    /// during the session it's `false` — write-through into the page
    /// cache, no disk barrier. `persist_guard` serializes concurrent
    /// writers and drops any snapshot older than the newest already
    /// on disk, so a slow earlier commit can never clobber a newer
    /// one (#80).
    fn persist(&self, seq: u64, snapshot: &SnapshotFile, sync: bool) {
        // Frozen by `flush()` on clean exit: ignore any later write so
        // a teardown cascade can't overwrite the flushed layout.
        if self.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        let Some(path) = self.state_path.clone() else {
            return; // in-memory variant; no persistence
        };
        let mut last = self.persist_guard.lock().unwrap();
        if seq <= *last {
            return; // a newer commit already persisted; this write is stale
        }
        if let Err(err) = persist_state(&path, snapshot, sync) {
            warn!(?err, "failed to persist state.json");
        }
        // Advance past this seq even on write failure: an older
        // snapshot must never win, and there is no retry of `seq`.
        *last = seq;
    }

    /// Persist the current layout with `fsync` and then freeze further
    /// persistence. Call once on a clean exit (each UI wires it into
    /// its app-quit hook). The `fsync` re-asserts physical durability
    /// at quit time — belt-and-suspenders, since the session's
    /// write-through already left the latest layout in the page cache,
    /// readable by a relaunch even without it. Setting `shutting_down`
    /// *after* the write means `flush`'s own `persist` isn't blocked
    /// while every subsequent one is, so a teardown-induced PTY-exit
    /// cascade can't clobber the flushed layout. Idempotent: a second
    /// call is a no-op (the freeze short-circuits its `persist`).
    pub fn flush(&self) {
        let (snapshot, seq) = {
            let mut inner = self.inner.lock().unwrap();
            inner.snapshot_for_persist()
        };
        self.persist(seq, &snapshot, true);
        self.shutting_down.store(true, Ordering::Relaxed);
    }

    /// Centralize the mutate → emit → persist tail shared by every
    /// mutator (#80). Snapshots **under the lock** when `persist` is
    /// `Persist::Write` (so the seq reflects commit order), then sends
    /// every event **while still holding the lock** (broadcast order
    /// matches commit order — a fast subscriber can't observe a
    /// contradicting sequence) — per-event on `events`, and as one
    /// [`VersionedWorkspaceEvent`] batch on `versioned_events`, sent
    /// even when the batch is empty so the revision stream stays
    /// gapless — and only after dropping the lock does
    /// it write to disk (no I/O under the lock). `Persist::Skip` is
    /// for state that isn't part of the persisted snapshot (tab
    /// run-state, notification flags) — emit only.
    fn commit(
        &self,
        mut inner: MutexGuard<'_, Inner>,
        events: Vec<WorkspaceEvent>,
        persist: Persist,
    ) {
        inner.revision = inner.revision.saturating_add(1);
        let revision = inner.revision;
        let to_write = match persist {
            Persist::Skip => None,
            Persist::Write => Some(inner.snapshot_for_persist()),
        };
        for ev in &events {
            let _ = self.events.send(ev.clone());
        }
        let _ = self
            .versioned_events
            .send(VersionedWorkspaceEvent { revision, events });
        drop(inner);
        if let Some((snapshot, seq)) = to_write {
            self.persist(seq, &snapshot, false);
        }
    }
}

/// Where an attention-raise came from. The only behavioral difference
/// is the agent gate: raw OSC is dropped while a live agent owns the tab
/// (plan §3.4), a structured `notification.create` / `roostctl notify`
/// never is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttentionSource {
    RawOsc,
    Structured,
}

/// Whether a `commit()` should write `state.json`. `Write` for layout
/// changes (projects/tabs/order/active selection); `Skip` for state
/// that isn't in the persisted snapshot (the agent axes, notification
/// flags) — those emit an event but never touch disk.
#[derive(Clone, Copy)]
enum Persist {
    Skip,
    Write,
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
}

/// The active selection: `(project id, tab id)` — the pair
/// [`Workspace::active`] answers with and [`Workspace::focus_tab`]
/// returns the previous value of.
type Selection = (i64, i64);

impl Inner {
    /// Move the active selection onto one tab, reporting where it was
    /// and where it landed. The mutation half of [`Workspace::focus_tab`],
    /// split out so [`Workspace::set_client_focus`] applies the same
    /// selection move inside its own transaction rather than taking the
    /// lock twice around it.
    ///
    /// Validation comes first and mutates nothing on the way out: an
    /// unknown tab leaves the selection exactly as it was.
    fn select_tab(&mut self, tab_id: i64) -> Result<(Selection, Selection), WorkspaceError> {
        let row = self
            .tabs
            .get(&tab_id)
            .ok_or(WorkspaceError::TabNotFound(tab_id))?
            .clone();
        let prev = (self.active_project_id, self.active_tab_id);
        self.active_project_id = row.project_id;
        self.active_tab_id = row.id;
        self.active_tabs_by_project.insert(row.project_id, row.id);
        Ok((prev, (row.project_id, row.id)))
    }

    /// Plan §3.5's suppression predicate: a notification for the tab the
    /// user is actively looking at is considered seen, so it raises no
    /// banner, no badge, and no inbox row.
    fn attention_suppressed_by_focus(&self, tab_id: i64) -> bool {
        self.window_focused && self.active_tab_id == tab_id
    }

    fn alloc_id(&mut self) -> i64 {
        self.next_id = self.next_id.max(1) + 1;
        self.next_id
    }

    /// Next free project position: `max(position) + 1`, or 0 when
    /// empty. `len()` would collide after a delete-then-create
    /// because positions are sparse, not dense (#80).
    ///
    /// Saturating: positions decode straight from a user-editable
    /// `state.json`, so a plain `m + 1` at `i32::MAX` panics at launch
    /// in debug. A workspace that reached `i32::MAX` is corrupt input,
    /// not a supported state — degrade it to a tie instead.
    fn next_project_position(&self) -> i32 {
        self.projects
            .values()
            .map(|p| p.position)
            .max()
            .map_or(0, |m| m.saturating_add(1))
    }

    /// Next free tab position within `project_id`: `max(position) + 1`,
    /// or 0 when the project has no tabs. See `next_project_position`.
    fn next_tab_position(&self, project_id: i64) -> i32 {
        self.tabs
            .values()
            .filter(|t| t.project_id == project_id)
            .map(|t| t.position)
            .max()
            .map_or(0, |m| m.saturating_add(1))
    }

    /// A project's tabs in display order: `position`, with `id`
    /// breaking ties. Returns rows, not snapshots, because
    /// `TabSnapshot` carries no id — sorting after the map could only
    /// key on `position` and would leave ties to `BTreeMap` iteration
    /// order: correct today, but by accident of the container rather
    /// than by statement.
    fn tabs_in_display_order(&self, project_id: i64) -> Vec<&TabRow> {
        let mut rows: Vec<&TabRow> = self
            .tabs
            .values()
            .filter(|t| t.project_id == project_id)
            .collect();
        rows.sort_by_key(|t| (t.position, t.id));
        rows
    }

    fn first_tab(&self, project_id: i64) -> Option<i64> {
        self.tabs_in_display_order(project_id)
            .first()
            .map(|tab| tab.id)
    }

    fn preferred_tab(&self, project_id: i64) -> Option<i64> {
        self.active_tabs_by_project
            .get(&project_id)
            .copied()
            .filter(|tab_id| {
                self.tabs
                    .get(tab_id)
                    .is_some_and(|tab| tab.project_id == project_id)
            })
            .or_else(|| self.first_tab(project_id))
    }

    /// Snapshot the persistable state plus a fresh commit sequence.
    /// The seq is assigned here — under the `inner` lock the caller
    /// holds — so it strictly reflects commit order; `persist()` uses
    /// it to drop stale out-of-order writes (#80). Each project
    /// carries its tab layout (title + cwd + position) so a relaunch
    /// can re-open the tabs in their saved directories.
    fn snapshot_for_persist(&mut self) -> (SnapshotFile, u64) {
        use crate::persistence::{ProjectSnapshot, TabSnapshot};
        self.persist_seq += 1;
        // Active tab restored by its DENSE index within the active
        // project's display-ordered tabs — not the raw `position`
        // field, which goes sparse after a mid-project close and
        // wouldn't match the re-opened tabs' contiguous 0..n indices
        // on restore (the UI selects the nth tab). #95 review.
        let active_tab_position = self
            .tabs
            .get(&self.active_tab_id)
            .map(|active| {
                self.tabs_in_display_order(active.project_id)
                    .into_iter()
                    .position(|t| t.id == active.id)
                    .unwrap_or(0) as i32
            })
            .unwrap_or(0);
        let snapshot = SnapshotFile {
            next_id: self.next_id,
            active_project_id: self.active_project_id,
            active_tab_position,
            sidebar_collapsed: self.sidebar_collapsed,
            sidebar_width: self.sidebar_width,
            hosts: self.hosts.clone(),
            projects: self
                .projects
                .values()
                .map(|p| {
                    let tabs: Vec<TabSnapshot> = self
                        .tabs_in_display_order(p.id)
                        .into_iter()
                        .map(|t| TabSnapshot {
                            title: t.title.clone(),
                            cwd: t.cwd.clone(),
                            position: t.position,
                            user_titled: t.user_titled,
                        })
                        .collect();
                    ProjectSnapshot {
                        id: p.id,
                        name: p.name.clone(),
                        cwd: p.cwd.clone(),
                        position: p.position,
                        created_at: p.created_at,
                        tabs,
                    }
                })
                .collect(),
        };
        (snapshot, self.persist_seq)
    }
}

fn wire_tab(row: &TabRow, is_active: bool) -> Tab {
    Tab {
        id: row.id,
        project_id: row.project_id,
        title: row.title.clone(),
        cwd: row.cwd.clone(),
        state: agent::effective(&row.agent),
        has_notification: row.has_notification,
        is_active,
        user_titled: row.user_titled,
        position: row.position,
        created_at: row.created_at,
        last_active: row.last_active,
        hook_active: agent::is_live(&row.agent),
        shell_state: row.agent.shell,
        agent_lifecycle: row.agent.lifecycle,
        ownership: row.agent.ownership.clone(),
    }
}

/// Swap a tab's agent record and return the events the swap implies.
///
/// `AgentChanged` carries the full record; `TabStateChanged` and
/// `HookActiveChanged` carry its two derived projections and each fires
/// only when its own projection moved. An identical record emits
/// nothing — repeated prompt marks are the common case and should not
/// churn the UI.
fn replace_agent(row: &mut TabRow, next: AgentTabState) -> Vec<WorkspaceEvent> {
    if row.agent == next {
        return Vec::new();
    }
    let tab_id = row.id;
    let prev = std::mem::replace(&mut row.agent, next);
    let mut events = Vec::with_capacity(3);
    let state = agent::effective(&row.agent);
    if state != agent::effective(&prev) {
        events.push(WorkspaceEvent::TabStateChanged { tab_id, state });
    }
    let active = agent::is_live(&row.agent);
    if active != agent::is_live(&prev) {
        events.push(WorkspaceEvent::HookActiveChanged { tab_id, active });
    }
    events.push(WorkspaceEvent::AgentChanged {
        tab_id,
        agent: row.agent.clone(),
    });
    events
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A saved host's stable id: 64 bits of OS entropy as 16 lowercase hex
/// characters. Not a credential (unlike a session lease) — it only has
/// to not collide with another saved host — so half the width
/// `roost-session`'s bearer tokens use is plenty. Far past birthday risk
/// for a hand-curated list, but a collision would be silent and
/// permanent — two entries answering one id — so the loop buys
/// certainty for one extra comparison per add.
fn mint_host_id(existing: &[HostSnapshot]) -> String {
    loop {
        let id = random_hex(8);
        if !existing.iter().any(|h| h.id == id) {
            return id;
        }
    }
}

/// `n` random bytes from the OS entropy source, rendered as lowercase
/// hex. Shared by [`random_host_id`] and `ipc::random_hex_128` — only
/// the byte width and the security expectations riding on it differ
/// per call site.
pub(crate) fn random_hex(n: usize) -> String {
    use std::fmt::Write as _;
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).expect("the OS random source must be available");
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// A `SystemTime` as `YYYY-MM-DDTHH:MM:SSZ`. Hand-rolled rather than
/// pulling in a date-time crate for two timestamps — mirrors
/// `roost_session::identity::rfc3339_utc` exactly, which this crate
/// cannot depend on (`roost-session` depends on `roost-engine`, not the
/// other way).
///
/// Public because the iced UI stamps `host.status`'s `armed_at` with it
/// and has no other way to format a time.
pub fn rfc3339(at: std::time::SystemTime) -> String {
    let secs = at
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let time_of_day = secs.rem_euclid(86_400);
    let (hour, minute, second) = (
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60,
    );
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Days-since-epoch to `(year, month, day)` — Howard Hinnant's
/// `civil_from_days`, the standard closed-form for this (avoids a
/// leap-year loop). Duplicated from `roost_session::identity` for the
/// same dependency-direction reason as [`rfc3339`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

fn derive_title(cwd: &str) -> String {
    if cwd.is_empty() {
        return "shell".into();
    }
    // Special-case "/": Path::file_name() returns None for the root,
    // and the prior "shell" fallback diverges from the Swift twin
    // (NSString.lastPathComponent("/") returns "/") and from what
    // shell integration's __roost_title would emit ("/"). Return "/"
    // explicitly to keep both UIs in lockstep and avoid the surprising
    // "you're at root, but the tab title says 'shell'" UX on
    // un-integrated shells.
    if cwd == "/" {
        return "/".into();
    }
    std::path::Path::new(cwd)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "shell".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use roost_ipc::agent::{AttentionOp, ShellState};

    #[test]
    fn open_tab_emits_tab_opened() {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let mut rx = ws.subscribe();
        let _ = ws.open_tab(pid, "/", "").unwrap();
        // Two events fire: TabOpened + ActiveChanged. Pull both.
        let _first = rx.try_recv().expect("event one");
        let _second = rx.try_recv().expect("event two");
    }

    #[test]
    fn close_tab_falls_back_to_sibling() {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let t1 = ws.open_tab(pid, "/", "one").unwrap().id;
        let _t2 = ws.open_tab(pid, "/", "two").unwrap().id;
        let (apid_before, atid_before) = ws.active();
        assert_eq!(apid_before, pid);
        ws.close_tab(atid_before).unwrap();
        let (apid_after, atid_after) = ws.active();
        assert_eq!(apid_after, pid);
        // The remaining tab is now active. It's the one we did not close.
        assert_ne!(atid_after, atid_before);
        assert_eq!(atid_after, t1);
    }

    #[test]
    fn preferred_tab_is_authoritative_per_project_and_repairs_after_close() {
        let ws = Workspace::new();
        let first_project = ws.create_project("first", "").unwrap().id;
        let first = ws.open_tab(first_project, "/", "one").unwrap().id;
        let second = ws.open_tab(first_project, "/", "two").unwrap().id;
        let other_project = ws.create_project("other", "").unwrap().id;
        let other = ws.open_tab(other_project, "/", "other").unwrap().id;

        assert_eq!(ws.preferred_tab(first_project), Some(second));
        ws.focus_tab(first).unwrap();
        ws.focus_tab(other).unwrap();
        assert_eq!(ws.preferred_tab(first_project), Some(first));

        ws.close_tab(first).unwrap();
        assert_eq!(ws.preferred_tab(first_project), Some(second));
        ws.delete_project(first_project).unwrap();
        assert_eq!(ws.preferred_tab(first_project), None);
    }

    #[test]
    fn closing_active_preferred_tab_uses_one_display_ordered_replacement() {
        let ws = Workspace::new();
        let project = ws.create_project("project", "").unwrap().id;
        let first = ws.open_tab(project, "/", "first").unwrap().id;
        let middle = ws.open_tab(project, "/", "middle").unwrap().id;
        let last = ws.open_tab(project, "/", "last").unwrap().id;
        ws.reorder_tabs(project, &[last, middle, first]).unwrap();
        ws.focus_tab(middle).unwrap();

        ws.close_tab(middle).unwrap();
        assert_eq!(ws.active(), (project, last));
        assert_eq!(ws.preferred_tab(project), Some(last));
    }

    #[test]
    fn close_last_tab_deletes_project() {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let t = ws.open_tab(pid, "/", "only").unwrap().id;
        let mut rx = ws.subscribe();
        ws.close_tab(t).unwrap();
        // The project is gone with its last tab, so the only-project
        // workspace is now empty.
        assert!(ws.snapshot().is_empty());
        // Event order: TabClosed → ProjectDeleted → ActiveChanged.
        assert!(matches!(
            rx.try_recv(),
            Ok(WorkspaceEvent::TabClosed { tab_id }) if tab_id == t
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(WorkspaceEvent::ProjectDeleted { project_id }) if project_id == pid
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(WorkspaceEvent::ActiveChanged {
                project_id: 0,
                tab_id: 0
            })
        ));
        // Active selection cleared to (0, 0) since nothing remains.
        assert_eq!(ws.active(), (0, 0));
    }

    #[test]
    fn close_last_tab_of_inactive_project_keeps_active() {
        // Closing a non-active project's last tab deletes that project
        // but must not steal the active selection from elsewhere.
        let ws = Workspace::new();
        let a = ws.create_project("a", "").unwrap().id;
        let a_tab = ws.open_tab(a, "/", "a1").unwrap().id;
        let b = ws.create_project("b", "").unwrap().id;
        let b_tab = ws.open_tab(b, "/", "b1").unwrap().id;
        // Make project A active, then close project B's last tab.
        ws.focus_tab(a_tab).unwrap();
        ws.close_tab(b_tab).unwrap();
        let snap = ws.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, a);
        // Active stays on A; no spurious reassignment.
        assert_eq!(ws.active(), (a, a_tab));
    }

    #[test]
    fn delete_project_cascades_tabs() {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let _t1 = ws.open_tab(pid, "/", "one").unwrap();
        let _t2 = ws.open_tab(pid, "/", "two").unwrap();
        let deleted = ws.delete_project(pid).unwrap();
        assert_eq!(deleted.len(), 2);
        assert!(ws.snapshot().is_empty());
    }

    fn project_ids_in_order(ws: &Workspace) -> Vec<i64> {
        ws.snapshot().iter().map(|p| p.id).collect()
    }

    #[test]
    fn reorder_projects_rewrites_the_snapshot_order() {
        let ws = Workspace::new();
        let a = ws.create_project("a", "").unwrap().id;
        let b = ws.create_project("b", "").unwrap().id;
        let c = ws.create_project("c", "").unwrap().id;
        let mut rx = ws.subscribe();

        ws.reorder_projects(&[c, a, b]).unwrap();

        assert_eq!(project_ids_in_order(&ws), vec![c, a, b]);
        assert!(matches!(
            rx.try_recv(),
            Ok(WorkspaceEvent::ProjectsReordered { project_ids }) if project_ids == vec![c, a, b]
        ));
    }

    #[test]
    fn reorder_projects_appends_unlisted_by_position_then_id() {
        let ws = Workspace::new();
        let a = ws.create_project("a", "").unwrap().id;
        let b = ws.create_project("b", "").unwrap().id;
        let c = ws.create_project("c", "").unwrap().id;
        ws.reorder_projects(&[c, a, b]).unwrap();
        let mut rx = ws.subscribe();

        // Listed ids lead in the given order; the rest keep their
        // relative order (c before a) behind them.
        ws.reorder_projects(&[b]).unwrap();

        assert_eq!(project_ids_in_order(&ws), vec![b, c, a]);
        assert!(matches!(
            rx.try_recv(),
            Ok(WorkspaceEvent::ProjectsReordered { project_ids }) if project_ids == vec![b, c, a]
        ));
    }

    #[test]
    fn reorder_projects_rejects_unknown_ids_before_mutating() {
        let ws = Workspace::new();
        let a = ws.create_project("a", "").unwrap().id;
        let b = ws.create_project("b", "").unwrap().id;
        let mut rx = ws.subscribe();

        assert!(matches!(
            ws.reorder_projects(&[b, 999, a]),
            Err(WorkspaceError::ProjectNotFound(999))
        ));

        assert_eq!(project_ids_in_order(&ws), vec![a, b]);
        assert!(rx.try_recv().is_err());
    }

    /// Duplicate ids are not rejected here: the last occurrence wins the
    /// position and the emitted payload keeps the duplicate. Callers
    /// uphold uniqueness — the UI validates stable ids and
    /// `roost_ui_model::reorder::moved_ids` rejects duplicate drag
    /// orders. Pinned as current behavior, not endorsed as a contract.
    #[test]
    fn reorder_projects_current_duplicate_id_behavior() {
        let ws = Workspace::new();
        let a = ws.create_project("a", "").unwrap().id;
        let b = ws.create_project("b", "").unwrap().id;
        let c = ws.create_project("c", "").unwrap().id;
        let mut rx = ws.subscribe();

        ws.reorder_projects(&[b, b, a]).unwrap();

        // b takes position 1 (its second slot), a takes 2, unlisted c 3.
        assert_eq!(project_ids_in_order(&ws), vec![b, a, c]);
        assert!(matches!(
            rx.try_recv(),
            Ok(WorkspaceEvent::ProjectsReordered { project_ids })
                if project_ids == vec![b, b, a, c]
        ));
    }

    #[test]
    fn ensure_default_project_creates_only_once() {
        let ws = Workspace::new();
        let a = ws.ensure_default_project("/");
        let b = ws.ensure_default_project("/");
        assert_eq!(a, b);
    }

    #[test]
    fn set_tab_title_locks_against_osc() {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let tid = ws.open_tab(pid, "/", "").unwrap().id;
        ws.set_tab_title(tid, "manual").unwrap();
        ws.set_tab_title_from_osc(tid, "shell-says").unwrap();
        let t = ws.tab(tid).unwrap();
        assert_eq!(t.title, "manual");
        assert!(t.user_titled);
    }

    /// A closed tab stops resolving, and `focus_tab` on it is an error.
    ///
    /// This pair is what the UI's stale-jump guard rides on: an agent
    /// row, a notification row, or an OS banner can outlive its tab, and
    /// `App::reveal_and_focus_tab` asks `tab()` before uncollapsing the
    /// sidebar so a jump that won't happen can't persist
    /// `sidebar_collapsed = false`.
    #[test]
    fn a_closed_tab_no_longer_resolves_or_focuses() {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let keep = ws.open_tab(pid, "/tmp", "").unwrap().id;
        let doomed = ws.open_tab(pid, "/tmp", "").unwrap().id;
        assert!(ws.tab(doomed).is_ok());

        ws.close_tab(doomed).unwrap();
        assert!(
            matches!(ws.tab(doomed), Err(WorkspaceError::TabNotFound(id)) if id == doomed),
            "a closed tab must report gone, not resolve stale"
        );
        assert!(ws.focus_tab(doomed).is_err());
        // The surviving sibling is untouched by the guard's probe.
        assert!(ws.tab(keep).is_ok());
    }

    /// Issue #196: `set_tab_cwd` re-derives the tab title from cwd
    /// when `!user_titled`, so the title follows cwd on any shell
    /// (Apple bash 3.2 / `--norc` bash / etc.), not just shells with
    /// the OSC 0 integration loaded. Events fire cwd-then-title.
    #[test]
    fn set_tab_cwd_re_derives_title_when_not_user_titled() {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let tid = ws.open_tab(pid, "/tmp", "").unwrap().id;
        assert_eq!(ws.tab(tid).unwrap().title, "tmp");
        let mut rx = ws.subscribe();
        ws.set_tab_cwd(tid, "/usr").unwrap();
        // Cwd-then-title: cause-then-effect.
        assert!(matches!(
            rx.try_recv(),
            Ok(WorkspaceEvent::TabCwdChanged { tab_id, ref cwd })
                if tab_id == tid && cwd == "/usr"
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(WorkspaceEvent::TabTitleChanged { tab_id, ref title })
                if tab_id == tid && title == "usr"
        ));
        assert_eq!(ws.tab(tid).unwrap().title, "usr");
    }

    /// `set_tab_cwd` does NOT touch the title when the user has
    /// manually renamed (mirrors `set_tab_title_from_osc`'s gate).
    #[test]
    fn set_tab_cwd_preserves_user_titled_title() {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let tid = ws.open_tab(pid, "/tmp", "").unwrap().id;
        ws.set_tab_title(tid, "manual").unwrap();
        let mut rx = ws.subscribe();
        ws.set_tab_cwd(tid, "/usr").unwrap();
        assert!(matches!(
            rx.try_recv(),
            Ok(WorkspaceEvent::TabCwdChanged { tab_id, ref cwd })
                if tab_id == tid && cwd == "/usr"
        ));
        // No TabTitleChanged — user_titled blocks the re-derivation.
        assert!(rx.try_recv().is_err());
        assert_eq!(ws.tab(tid).unwrap().title, "manual");
    }

    /// `cd .` (cwd unchanged in basename) doesn't churn a redundant
    /// TabTitleChanged. Guards against per-prompt event spam from a
    /// shell that re-emits the same OSC 7 every prompt.
    #[test]
    fn set_tab_cwd_skips_title_event_when_basename_unchanged() {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let tid = ws.open_tab(pid, "/tmp", "").unwrap().id;
        let mut rx = ws.subscribe();
        ws.set_tab_cwd(tid, "/tmp").unwrap();
        // Cwd event fires (same string but the model writes through);
        // title event suppressed since basename didn't change.
        assert!(matches!(
            rx.try_recv(),
            Ok(WorkspaceEvent::TabCwdChanged { .. })
        ));
        assert!(rx.try_recv().is_err());
    }

    /// CLI / IPC `tab.open` callers can pass an explicit placeholder
    /// title (`"roostctl"`, `"Tab 1"`, …). open_tab leaves
    /// user_titled=false on those (the supplied title is treated as
    /// a placeholder per the open_tab comment). The model fix
    /// overwrites the placeholder on the first cwd change.
    /// Guards: a future refactor that flips open_tab to
    /// `user_titled = !title.is_empty()` silently inverts the model
    /// invariant — this test catches it.
    #[test]
    fn set_tab_cwd_overwrites_placeholder_title() {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let tid = ws.open_tab(pid, "/tmp", "roostctl").unwrap().id;
        assert_eq!(ws.tab(tid).unwrap().title, "roostctl");
        assert!(!ws.tab(tid).unwrap().user_titled);
        ws.set_tab_cwd(tid, "/usr").unwrap();
        assert_eq!(ws.tab(tid).unwrap().title, "usr");
    }

    /// Cross-platform parity: `derive_title("/")` returns `"/"`,
    /// matching the Swift twin's `(cwd as NSString).lastPathComponent`
    /// for the root case and what shell integration's `__roost_title`
    /// would emit. Without this special-case Path::file_name() returns
    /// None and the fallback `"shell"` diverges from the Mac UI on
    /// `cd /` — the model fix now exercises this routinely.
    #[test]
    fn derive_title_root_returns_slash() {
        assert_eq!(derive_title("/"), "/");
        assert_eq!(derive_title(""), "shell");
        assert_eq!(derive_title("/tmp"), "tmp");
        assert_eq!(derive_title("/usr/local"), "local");
    }

    // ------------------------------------------------------------------
    // Agent state model (plan 002)
    // ------------------------------------------------------------------

    /// A one-tab workspace with the window reported **unfocused**, so
    /// these tests exercise the agent axis without policy §3.5's focus
    /// suppression in the way. `attention_policy_*` covers the matrix.
    fn agent_ws() -> (Workspace, i64) {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let tid = ws.open_tab(pid, "/", "").unwrap().id;
        ws.set_window_focused(false);
        (ws, tid)
    }

    fn report(
        tab_id: i64,
        source: &str,
        session: &str,
        action: OwnershipAction,
        lifecycle: Option<AgentLifecycle>,
    ) -> TabAgentReportParams {
        TabAgentReportParams {
            session_id: session.to_string(),
            ..TabAgentReportParams::sessionless(tab_id, source, action, lifecycle)
        }
    }

    /// A workspace whose one tab is already claimed by `claude`/`s1`.
    fn owned_ws(lifecycle: AgentLifecycle) -> (Workspace, i64) {
        let (ws, tid) = agent_ws();
        ws.agent_report(&report(
            tid,
            "claude",
            "s1",
            OwnershipAction::Claim,
            Some(lifecycle),
        ))
        .unwrap();
        (ws, tid)
    }

    /// Replaces `set_tab_state_from_osc_respects_hook_active`: the gate
    /// it pinned is gone. OSC 133 now writes the shell axis
    /// unconditionally, and derivation — not a suppression rule —
    /// decides which axis the tab shows.
    #[test]
    fn shell_marks_write_the_shell_axis_under_agent_ownership() {
        let (ws, tid) = owned_ws(AgentLifecycle::Waiting);

        // The mark lands on the shell axis even while an agent owns the
        // tab; the agent's lifecycle still wins the derived state.
        ws.apply_shell_mark(tid, "C").unwrap();
        let tab = ws.tab(tid).unwrap();
        assert_eq!(tab.shell_state, ShellState::ForegroundProcess);
        assert_eq!(tab.agent_lifecycle, AgentLifecycle::Waiting);
        assert_eq!(tab.state, TabState::NeedsInput);
    }

    #[test]
    fn shell_marks_drive_the_state_without_an_owner() {
        let (ws, tid) = agent_ws();
        ws.apply_shell_mark(tid, "C").unwrap();
        assert_eq!(ws.tab(tid).unwrap().state, TabState::Running);
        ws.apply_shell_mark(tid, "D;0").unwrap();
        assert_eq!(ws.tab(tid).unwrap().state, TabState::None);
    }

    /// The `D`/`A` failsafe end to end: a killed agent's ownership
    /// survives as a label but stops driving derivation, and raw OSC
    /// re-opens.
    #[test]
    fn prompt_mark_reopens_raw_osc_for_a_dead_agent() {
        let (ws, tid) = owned_ws(AgentLifecycle::Working);
        assert!(!ws
            .raise_attention(tid, "Wrapper", "noise", AttentionSource::RawOsc)
            .unwrap());
        assert!(!ws.tab(tid).unwrap().has_notification);

        ws.apply_shell_mark(tid, "D;0").unwrap();
        let tab = ws.tab(tid).unwrap();
        assert_eq!(tab.agent_lifecycle, AgentLifecycle::Inactive);
        assert!(tab.hook_active, "ownership survives as a label");
        assert_eq!(tab.state, TabState::None, "falls through to shell");
        assert!(ws
            .raise_attention(tid, "Build", "done", AttentionSource::RawOsc)
            .unwrap());
        assert!(ws.tab(tid).unwrap().has_notification);
    }

    /// Session scoping is enforced in the workspace, atomically with
    /// the mutation (plan §3.3). A report from a different session is
    /// dropped whole — lifecycle *and* attention.
    #[test]
    fn report_from_a_foreign_session_is_dropped() {
        let (ws, tid) = owned_ws(AgentLifecycle::Working);

        let mut stale = report(
            tid,
            "claude",
            "s2",
            OwnershipAction::Preserve,
            Some(AgentLifecycle::Finished),
        );
        stale.attention = AttentionOp::Set;
        stale.title = "Claude Code".into();
        stale.body = "Turn complete".into();
        let (accepted, tab) = ws.agent_report(&stale).unwrap();
        assert!(!accepted);
        assert_eq!(tab.agent_lifecycle, AgentLifecycle::Working);
        assert!(!tab.has_notification, "a dropped report fires nothing");

        // Same session id, different source: still a mismatch.
        let (accepted, _) = ws
            .agent_report(&report(
                tid,
                "codex",
                "s1",
                OwnershipAction::Preserve,
                Some(AgentLifecycle::Finished),
            ))
            .unwrap();
        assert!(!accepted);
    }

    #[test]
    fn claim_supersedes_a_live_owner_and_release_is_scoped() {
        let (ws, tid) = owned_ws(AgentLifecycle::Working);

        let (accepted, tab) = ws
            .agent_report(&report(
                tid,
                "codex",
                "s9",
                OwnershipAction::Claim,
                Some(AgentLifecycle::Waiting),
            ))
            .unwrap();
        assert!(accepted, "claim is the supersede path");
        assert_eq!(tab.ownership.unwrap().source, "codex");

        // The displaced owner can no longer release.
        let (accepted, tab) = ws
            .agent_report(&report(tid, "claude", "s1", OwnershipAction::Release, None))
            .unwrap();
        assert!(!accepted);
        assert!(tab.hook_active);

        let (accepted, tab) = ws
            .agent_report(&report(tid, "codex", "s9", OwnershipAction::Release, None))
            .unwrap();
        assert!(accepted);
        assert!(!tab.hook_active);
        assert_eq!(tab.agent_lifecycle, AgentLifecycle::Inactive);
    }

    /// Attention effects are the workspace's to apply: `set` raises the
    /// pending flag and fires, `clear` drops it.
    #[test]
    fn attention_set_and_clear_drive_the_notification_flag() {
        let (ws, tid) = agent_ws();
        let mut claim = report(tid, "claude", "s1", OwnershipAction::Claim, None);
        claim.attention = AttentionOp::Set;
        claim.title = "Claude Code".into();
        claim.body = "Needs your permission".into();
        let (_, tab) = ws.agent_report(&claim).unwrap();
        assert!(tab.has_notification);

        let mut clear = report(tid, "claude", "s1", OwnershipAction::Preserve, None);
        clear.attention = AttentionOp::Clear;
        let (_, tab) = ws.agent_report(&clear).unwrap();
        assert!(!tab.has_notification);
    }

    // ------------------------------------------------------------------
    // Attention policy B (plan §3.5) — one transaction, one predicate
    // ------------------------------------------------------------------

    /// A report that sets attention on the focused, active tab. Returns
    /// the tab plus every event the report emitted.
    fn attention_report(tab_id: i64) -> TabAgentReportParams {
        let mut r = report(tab_id, "claude", "s1", OwnershipAction::Claim, None);
        r.attention = AttentionOp::Set;
        r.title = "Claude Code".into();
        r.body = "Turn complete".into();
        r
    }

    fn drain(rx: &mut broadcast::Receiver<WorkspaceEvent>) -> Vec<WorkspaceEvent> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    fn has_attention_events(events: &[WorkspaceEvent]) -> bool {
        events.iter().any(|e| {
            matches!(
                e,
                WorkspaceEvent::NotificationFired { .. }
                    | WorkspaceEvent::TabNotification {
                        has_pending: true,
                        ..
                    }
            )
        })
    }

    /// The tab you are looking at is the tab you have seen: a structured
    /// notification for it is dropped whole — no pending bit, no events.
    /// Emitting nothing is what makes "switch away afterwards and the
    /// badge does not appear" true by construction.
    #[test]
    fn attention_policy_drops_a_notification_for_the_focused_active_tab() {
        let (ws, tid) = agent_ws();
        ws.set_window_focused(true);
        let mut rx = ws.subscribe();

        let (accepted, tab) = ws.agent_report(&attention_report(tid)).unwrap();
        assert!(accepted, "the report itself still applies");
        assert!(!tab.has_notification);
        assert!(!has_attention_events(&drain(&mut rx)));

        // Switching away must not resurrect it.
        let other = ws
            .open_tab(ws.tab(tid).unwrap().project_id, "/", "")
            .unwrap()
            .id;
        assert_ne!(other, tid);
        assert!(!ws.tab(tid).unwrap().has_notification);
    }

    #[test]
    fn attention_policy_delivers_when_the_window_is_unfocused() {
        let (ws, tid) = agent_ws();
        ws.set_window_focused(false);
        let mut rx = ws.subscribe();

        let (_, tab) = ws.agent_report(&attention_report(tid)).unwrap();
        assert!(tab.has_notification);
        assert!(has_attention_events(&drain(&mut rx)));
    }

    #[test]
    fn attention_policy_delivers_when_another_tab_is_active() {
        let (ws, tid) = agent_ws();
        let pid = ws.tab(tid).unwrap().project_id;
        let other = ws.open_tab(pid, "/", "").unwrap().id;
        ws.focus_tab(other).unwrap();
        ws.set_window_focused(true);
        let mut rx = ws.subscribe();

        let (_, tab) = ws.agent_report(&attention_report(tid)).unwrap();
        assert!(tab.has_notification);
        assert!(has_attention_events(&drain(&mut rx)));
    }

    // ------------------------------------------------------------------
    // Client-reported focus (host sessions, plan 038 C6)
    // ------------------------------------------------------------------

    /// A two-tab workspace standing in for a session's: no window of its
    /// own, so it starts on the headless default (focused, on the tab
    /// its layout selected) — which is exactly the state
    /// `set_client_focus` exists to correct.
    fn session_like_ws() -> (Workspace, i64, i64) {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let first = ws.open_tab(pid, "/", "").unwrap().id;
        let second = ws.open_tab(pid, "/", "").unwrap().id;
        ws.focus_tab(first).unwrap();
        (ws, first, second)
    }

    /// The whole point of the op: the client says what it is looking at,
    /// and the session suppresses that tab and only that tab.
    #[test]
    fn client_focus_moves_the_suppressed_tab_with_the_selection() {
        let (ws, first, second) = session_like_ws();

        ws.set_client_focus(Some(second)).unwrap();
        assert_eq!(ws.active(), (ws.tab(second).unwrap().project_id, second));
        assert!(!ws
            .raise_attention(second, "Roost", "", AttentionSource::Structured)
            .unwrap());
        assert!(ws
            .raise_attention(first, "Roost", "", AttentionSource::Structured)
            .unwrap());
    }

    /// A null focus moves the flag and nothing else: the selection is
    /// where the client left it, so a reconnect restores the same tab.
    #[test]
    fn a_null_client_focus_unmutes_without_moving_the_selection() {
        let (ws, first, _second) = session_like_ws();
        ws.set_client_focus(Some(first)).unwrap();

        ws.set_client_focus(None).unwrap();
        assert_eq!(ws.active().1, first, "the selection must not move");
        assert!(ws
            .raise_attention(first, "Roost", "", AttentionSource::Structured)
            .unwrap());
    }

    /// Atomic: an unknown tab is refused with nothing applied. A partial
    /// apply here would flip the session to "focused" against whatever
    /// tab happened to be active and mute it.
    #[test]
    fn an_unknown_tab_leaves_the_reported_focus_untouched() {
        let (ws, first, _second) = session_like_ws();
        ws.set_client_focus(None).unwrap();

        let refused = ws.set_client_focus(Some(9_999)).unwrap_err();
        assert!(matches!(refused, WorkspaceError::TabNotFound(9_999)));
        assert_eq!(ws.active().1, first, "the selection must not have moved");
        assert!(
            ws.raise_attention(first, "Roost", "", AttentionSource::Structured)
                .unwrap(),
            "the window flag must not have moved either"
        );
    }

    /// Re-stating the same focus is a no-op on the event stream: a
    /// client that re-asserts on reconnect must not make every attached
    /// client re-render, and `ActiveChanged` is what they re-render on.
    #[test]
    fn re_stating_the_same_client_focus_emits_nothing() {
        let (ws, first, second) = session_like_ws();
        ws.set_client_focus(Some(second)).unwrap();
        let mut rx = ws.subscribe();

        ws.set_client_focus(Some(second)).unwrap();
        assert!(drain(&mut rx).is_empty());

        // Including the flag-only edge: unfocus, then re-state the same
        // tab. The selection never moved, so still nothing.
        ws.set_client_focus(None).unwrap();
        ws.set_client_focus(Some(second)).unwrap();
        assert!(drain(&mut rx).is_empty());

        // …but a move still announces itself, exactly like `tab.focus`.
        ws.set_client_focus(Some(first)).unwrap();
        assert!(drain(&mut rx).iter().any(
            |e| matches!(e, WorkspaceEvent::ActiveChanged { tab_id, .. } if *tab_id == first)
        ));
    }

    /// `release_client_focus` is the lease-loss reset: the flag drops,
    /// the selection stays.
    #[test]
    fn releasing_client_focus_unmutes_and_keeps_the_selection() {
        let (ws, _first, second) = session_like_ws();
        ws.set_client_focus(Some(second)).unwrap();

        ws.release_client_focus();
        assert_eq!(ws.active().1, second);
        assert!(ws
            .raise_attention(second, "Roost", "", AttentionSource::Structured)
            .unwrap());
    }

    /// Structured attention is NEVER gated on agent ownership (plan
    /// §3.4) — `notification.create` / `roostctl notify` must get
    /// through even mid-turn, since that is how the agent itself speaks.
    #[test]
    fn structured_attention_is_never_gated_by_a_live_agent() {
        let (ws, tid) = owned_ws(AgentLifecycle::Working);

        assert!(ws
            .raise_attention(tid, "Roost", "explicit", AttentionSource::Structured)
            .unwrap());
        assert!(ws.tab(tid).unwrap().has_notification);

        // …and the same is true through the report path.
        ws.set_tab_has_notification(tid, false).unwrap();
        let mut r = attention_report(tid);
        r.ownership_action = OwnershipAction::Preserve;
        let (accepted, tab) = ws.agent_report(&r).unwrap();
        assert!(accepted);
        assert!(tab.has_notification);
    }

    /// Raw OSC is the one thing the agent gate drops, and the `D` mark
    /// failsafe re-opens it (plan §3.4).
    #[test]
    fn raw_osc_attention_is_gated_by_a_live_agent_until_a_prompt_mark() {
        let (ws, tid) = owned_ws(AgentLifecycle::Working);

        assert!(!ws
            .raise_attention(tid, "Wrapper", "noise", AttentionSource::RawOsc)
            .unwrap());
        assert!(!ws.tab(tid).unwrap().has_notification);

        ws.apply_shell_mark(tid, "D;0").unwrap();
        assert!(ws
            .raise_attention(tid, "Build", "done", AttentionSource::RawOsc)
            .unwrap());
        assert!(ws.tab(tid).unwrap().has_notification);
    }

    #[test]
    fn raise_attention_on_a_missing_tab_is_an_error() {
        let ws = Workspace::new();
        assert!(matches!(
            ws.raise_attention(999, "t", "b", AttentionSource::Structured),
            Err(WorkspaceError::TabNotFound(999))
        ));
    }

    /// Focus defaults to *focused* before any UI reports it, so a
    /// headless / IPC-only workspace routes exactly as a real window
    /// would. The active tab is therefore suppressed, and — the half
    /// that keeps the default safe — an inactive tab still delivers.
    #[test]
    fn window_focus_defaults_to_focused() {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let active = ws.open_tab(pid, "/", "a").unwrap().id;
        let background = ws.open_tab(pid, "/", "b").unwrap().id;
        ws.focus_tab(active).unwrap();

        assert!(!ws
            .raise_attention(active, "t", "b", AttentionSource::Structured)
            .unwrap());
        assert!(ws
            .raise_attention(background, "t", "b", AttentionSource::Structured)
            .unwrap());
        assert!(ws.tab(background).unwrap().has_notification);
    }

    /// Plan §3.7's transition table. `running`/`needs_input`/`idle`
    /// produce their pre-change `tab.state`; `none` is the one genuine
    /// behavior change — it releases, so the shell axis shows through.
    #[test]
    fn legacy_set_state_follows_the_transition_table() {
        let (ws, tid) = agent_ws();
        for (legacy, lifecycle) in [
            (TabState::Running, AgentLifecycle::Working),
            (TabState::NeedsInput, AgentLifecycle::Waiting),
            (TabState::Idle, AgentLifecycle::Finished),
        ] {
            ws.set_tab_state(tid, legacy).unwrap();
            let tab = ws.tab(tid).unwrap();
            assert_eq!(tab.state, legacy, "legacy projection must not move");
            assert_eq!(tab.agent_lifecycle, lifecycle);
            assert_eq!(tab.ownership.as_ref().unwrap().source, SOURCE_MANUAL);
            assert_eq!(tab.ownership.as_ref().unwrap().session_id, "");
        }

        // `none` releases; with a live foreground process the shell axis
        // now shows `running` rather than `none`.
        ws.apply_shell_mark(tid, "C").unwrap();
        ws.set_tab_state(tid, TabState::None).unwrap();
        let tab = ws.tab(tid).unwrap();
        assert!(!tab.hook_active, "set-state none releases ownership");
        assert_eq!(tab.ownership, None);
        assert_eq!(tab.agent_lifecycle, AgentLifecycle::Inactive);
        assert_eq!(tab.state, TabState::Running, "shell-derived");
    }

    /// A manual override takes the tab from a live agent — the user has
    /// the wheel — and `none` releases even though the release itself is
    /// scoped to `manual`.
    #[test]
    fn manual_override_supersedes_a_live_agent() {
        let (ws, tid) = owned_ws(AgentLifecycle::Working);

        ws.set_tab_state(tid, TabState::NeedsInput).unwrap();
        assert_eq!(
            ws.tab(tid).unwrap().ownership.unwrap().source,
            SOURCE_MANUAL
        );

        // Claude's next in-session event is now out of scope.
        let (accepted, _) = ws
            .agent_report(&report(
                tid,
                "claude",
                "s1",
                OwnershipAction::Preserve,
                Some(AgentLifecycle::Finished),
            ))
            .unwrap();
        assert!(!accepted);

        // And `none` still releases, despite `claude` having held it.
        ws.set_tab_state(tid, TabState::None).unwrap();
        assert_eq!(ws.tab(tid).unwrap().ownership, None);
    }

    #[test]
    fn set_hook_active_claims_and_releases_as_legacy() {
        let (ws, tid) = agent_ws();
        ws.set_tab_hook_active(tid, true).unwrap();
        let tab = ws.tab(tid).unwrap();
        assert!(tab.hook_active);
        assert_eq!(tab.ownership.unwrap().source, SOURCE_LEGACY);
        assert_eq!(
            tab.state,
            TabState::None,
            "ownership alone doesn't move the legacy state"
        );

        ws.set_tab_hook_active(tid, false).unwrap();
        assert!(!ws.tab(tid).unwrap().hook_active);
    }

    #[test]
    fn pty_replacement_clears_ownership() {
        let (ws, tid) = owned_ws(AgentLifecycle::Working);
        ws.apply_shell_mark(tid, "C").unwrap();

        ws.pty_replaced(tid).unwrap();
        let tab = ws.tab(tid).unwrap();
        assert_eq!(tab.ownership, None);
        assert_eq!(tab.agent_lifecycle, AgentLifecycle::Inactive);
        assert_eq!(tab.shell_state, ShellState::Unknown);
        assert_eq!(tab.state, TabState::None);
    }

    /// The derived slices ride along with the full record, so the two
    /// UIs and any external subscriber see one consistent story.
    #[test]
    fn accepted_report_emits_state_hook_and_agent_events() {
        let (ws, tid) = agent_ws();
        let mut rx = ws.subscribe();
        ws.agent_report(&report(
            tid,
            "claude",
            "s1",
            OwnershipAction::Claim,
            Some(AgentLifecycle::Waiting),
        ))
        .unwrap();

        assert!(matches!(
            rx.try_recv(),
            Ok(WorkspaceEvent::TabStateChanged {
                state: TabState::NeedsInput,
                ..
            })
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(WorkspaceEvent::HookActiveChanged { active: true, .. })
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(WorkspaceEvent::AgentChanged { .. })
        ));

        // A report that changes nothing emits nothing — repeated prompt
        // marks are the common case and must not churn the UI.
        ws.apply_shell_mark(tid, "Z").unwrap();
        assert!(rx.try_recv().is_err());
    }

    /// The converse of a dropped report (which succeeds with
    /// `accepted: false`): a tab that doesn't exist is an error.
    #[test]
    fn agent_report_on_a_missing_tab_is_an_error() {
        let ws = Workspace::new();
        assert!(matches!(
            ws.agent_report(&report(999, "claude", "s1", OwnershipAction::Claim, None)),
            Err(WorkspaceError::TabNotFound(999))
        ));
    }

    #[test]
    fn position_is_max_plus_one_after_delete() {
        let ws = Workspace::new();
        let a = ws.create_project("a", "").unwrap();
        let b = ws.create_project("b", "").unwrap();
        let c = ws.create_project("c", "").unwrap();
        assert_eq!((a.position, b.position, c.position), (0, 1, 2));

        // Delete the middle project, then create a new one. The old
        // `len()` rule would reuse position 2 (colliding with c); the
        // fix must hand out max(0, 2) + 1 = 3.
        ws.delete_project(b.id).unwrap();
        let d = ws.create_project("d", "").unwrap();
        assert_eq!(d.position, 3, "new project position collided after delete");

        // Same invariant for tabs within a project.
        let t0 = ws.open_tab(a.id, "/", "t0").unwrap();
        let t1 = ws.open_tab(a.id, "/", "t1").unwrap();
        assert_eq!((t0.position, t1.position), (0, 1));
        ws.close_tab(t0.id).unwrap();
        let t2 = ws.open_tab(a.id, "/", "t2").unwrap();
        assert_eq!(t2.position, 2, "new tab position collided after close");
    }

    /// A `state.json` is user-editable, so `i32::MAX` is representable
    /// in a position. `max + 1` there panics in debug; the allocators
    /// clamp to a tie instead so the UI still starts.
    #[test]
    fn next_position_saturates_at_i32_max() {
        let ws = Workspace::new();
        let p = ws.create_project("p", "").unwrap().id;
        let t = ws.open_tab(p, "/", "t").unwrap().id;
        {
            let mut inner = ws.inner.lock().unwrap();
            inner.projects.get_mut(&p).unwrap().position = i32::MAX;
            inner.tabs.get_mut(&t).unwrap().position = i32::MAX;
            assert_eq!(inner.next_project_position(), i32::MAX);
            assert_eq!(inner.next_tab_position(p), i32::MAX);
        }
        assert_eq!(ws.create_project("q", "").unwrap().position, i32::MAX);
        assert_eq!(ws.open_tab(p, "/", "u").unwrap().position, i32::MAX);
    }

    /// States the rule rather than reproducing a bug: this test cannot
    /// fail against the previous implementation (`sort_by_key(|t|
    /// t.position)`, no id in the key). `self.tabs` is a `BTreeMap`, so
    /// `.values()` always yields ascending id; `sort_by_key` is stable;
    /// a stable sort on `position` alone therefore always preserves
    /// ascending-id order among ties — identical to sorting on
    /// `(position, id)` for every input, not just this one. The id key
    /// is here so the ordering guarantee is a stated rule instead of an
    /// accident of the container choice (#262) — kept as an explicit
    /// statement-of-intent guard, not a regression reproduction.
    #[test]
    fn persist_sorts_tabs_by_position_then_id() {
        let ws = Workspace::new();
        let p = ws.create_project("p", "").unwrap().id;
        let a = ws.open_tab(p, "/", "a").unwrap().id;
        let _b = ws.open_tab(p, "/", "b").unwrap().id;
        let c = ws.open_tab(p, "/", "c").unwrap().id;
        let (snapshot, _seq) = {
            let mut inner = ws.inner.lock().unwrap();
            // c (the highest id) ties with a at position 0; b keeps 1.
            inner.tabs.get_mut(&c).unwrap().position = 0;
            inner.snapshot_for_persist()
        };
        assert!(a < c);
        let titles: Vec<&str> = snapshot.projects[0]
            .tabs
            .iter()
            .map(|t| t.title.as_str())
            .collect();
        assert_eq!(titles, vec!["a", "c", "b"]);
    }

    #[test]
    fn reorder_tabs_partial_keeps_unlisted() {
        let ws = Workspace::new();
        let pid = ws.create_project("p", "").unwrap().id;
        let a = ws.open_tab(pid, "/", "a").unwrap().id;
        let b = ws.open_tab(pid, "/", "b").unwrap().id;
        let c = ws.open_tab(pid, "/", "c").unwrap().id;
        // Reorder only [c, a] — b should land last.
        ws.reorder_tabs(pid, &[c, a]).unwrap();
        let projects = ws.snapshot();
        let tabs: Vec<i64> = projects[0].tabs.iter().map(|t| t.id).collect();
        assert_eq!(tabs, vec![c, a, b]);
    }

    #[test]
    fn persist_drops_stale_out_of_order_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let ws = Workspace::open(path.clone());

        // A newer commit (seq 2) lands first, then a slower earlier
        // commit (seq 1) races in. The stale write must be dropped so
        // the newest snapshot stays on disk (#80).
        ws.persist(
            2,
            &SnapshotFile {
                next_id: 99,
                ..Default::default()
            },
            false,
        );
        ws.persist(
            1,
            &SnapshotFile {
                next_id: 5,
                ..Default::default()
            },
            false,
        );
        assert_eq!(
            read_state(&path).unwrap().unwrap().next_id,
            99,
            "stale snapshot overwrote the newer one"
        );

        // A genuinely newer commit (seq 3) still applies.
        ws.persist(
            3,
            &SnapshotFile {
                next_id: 200,
                ..Default::default()
            },
            false,
        );
        assert_eq!(read_state(&path).unwrap().unwrap().next_id, 200);
    }

    #[test]
    fn persist_restore_round_trips_tab_layout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let pid = {
            let ws = Workspace::open(path.clone());
            let pid = ws.create_project("p", "/proj").unwrap().id;
            let _a = ws.open_tab(pid, "/a", "atab").unwrap().id;
            let b = ws.open_tab(pid, "/b", "btab").unwrap().id;
            let _c = ws.open_tab(pid, "/c", "ctab").unwrap().id;
            // Select the middle tab so restore picks it by position.
            ws.focus_tab(b).unwrap();
            pid
        };

        let ws2 = Workspace::open(path);
        // The reloaded workspace exposes the layout as restore
        // descriptors, NOT as live tabs.
        assert!(
            ws2.snapshot().iter().all(|p| p.tabs.is_empty()),
            "restored tabs must be descriptors, not live tabs"
        );
        let restore = ws2.take_restore_layout().expect("layout present");
        assert_eq!(restore.active_project_id, pid);
        assert_eq!(restore.active_tab_position, 1, "tab 'b' is at position 1");
        let rp = restore
            .projects
            .iter()
            .find(|p| p.project_id == pid)
            .expect("project in layout");
        assert_eq!(
            rp.tabs.iter().map(|t| t.cwd.as_str()).collect::<Vec<_>>(),
            vec!["/a", "/b", "/c"]
        );
        assert_eq!(rp.tabs[1].title, "btab");
        // `take_restore_layout` is one-shot.
        assert!(ws2.take_restore_layout().is_none());
    }

    #[test]
    fn sidebar_collapsed_persists_across_reopen() {
        // Rust UI adapter (Iced) parity with the Mac UI's
        // RoostSidebarVisible: the user's hide/show choice survives quit
        // + relaunch. Backs the locally-run
        // e2e `test_sidebar_collapsed_state_survives_relaunch`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        {
            let ws = Workspace::open(path.clone());
            assert!(!ws.sidebar_collapsed(), "defaults to expanded");
            ws.set_sidebar_collapsed(true);
            assert!(ws.sidebar_collapsed());
        }
        // Reopen: the collapsed choice is restored from disk.
        let ws2 = Workspace::open(path.clone());
        assert!(
            ws2.sidebar_collapsed(),
            "collapsed state must survive reopen"
        );
        // And toggling back to expanded persists too.
        ws2.set_sidebar_collapsed(false);
        drop(ws2);
        let ws3 = Workspace::open(path);
        assert!(
            !ws3.sidebar_collapsed(),
            "expanded state must survive reopen"
        );
    }

    #[test]
    fn sidebar_width_persists_across_reopen() {
        // Rust UI adapter (Iced) parity with the Mac UI's
        // RoostSidebarWidth: the user's drag survives quit + relaunch.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        {
            let ws = Workspace::open(path.clone());
            assert_eq!(ws.sidebar_width(), SIDEBAR_DEFAULT_WIDTH);
            ws.set_sidebar_width(300.0);
            assert_eq!(ws.sidebar_width(), 300.0);
        }
        let ws2 = Workspace::open(path.clone());
        assert_eq!(ws2.sidebar_width(), 300.0, "width must survive reopen");
        ws2.set_sidebar_width(180.0);
        drop(ws2);
        let ws3 = Workspace::open(path);
        assert_eq!(ws3.sidebar_width(), 180.0, "a later drag persists too");
    }

    #[test]
    fn sidebar_width_defaults_when_state_file_absent_or_malformed() {
        assert_eq!(Workspace::new().sidebar_width(), SIDEBAR_DEFAULT_WIDTH);

        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("state.json");
        assert_eq!(
            Workspace::open(missing).sidebar_width(),
            SIDEBAR_DEFAULT_WIDTH
        );

        let malformed = dir.path().join("malformed.json");
        std::fs::write(&malformed, b"not json").unwrap();
        assert_eq!(
            Workspace::open(malformed).sidebar_width(),
            SIDEBAR_DEFAULT_WIDTH
        );
    }

    #[test]
    fn sidebar_width_defaults_for_legacy_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, br#"{"next_id":5,"projects":[]}"#).unwrap();
        assert_eq!(
            Workspace::open(path).sidebar_width(),
            SIDEBAR_DEFAULT_WIDTH,
            "a file predating the key loads as the default width"
        );
    }

    #[test]
    fn sidebar_width_clamps_on_open() {
        let dir = tempfile::tempdir().unwrap();
        for (stored, expected) in [(90.0, SIDEBAR_MIN_WIDTH), (1000.0, SIDEBAR_MAX_WIDTH)] {
            let path = dir.path().join(format!("state-{stored}.json"));
            std::fs::write(
                &path,
                format!(r#"{{"next_id":1,"projects":[],"sidebar_width":{stored}}}"#),
            )
            .unwrap();
            assert_eq!(Workspace::open(path).sidebar_width(), expected);
        }
    }

    #[test]
    fn sidebar_width_non_finite_in_state_file_falls_back_to_default() {
        // JSON has no NaN/Infinity literal, but a hand-edited file can
        // still overflow f64 — the restore path must land on the
        // default rather than a clamped infinity.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            br#"{"next_id":1,"projects":[],"sidebar_width":1e400}"#,
        )
        .unwrap();
        assert_eq!(Workspace::open(path).sidebar_width(), SIDEBAR_DEFAULT_WIDTH);
    }

    #[test]
    fn sidebar_width_clamps_in_setter() {
        let ws = Workspace::new();
        ws.set_sidebar_width(90.0);
        assert_eq!(ws.sidebar_width(), SIDEBAR_MIN_WIDTH);
        ws.set_sidebar_width(1000.0);
        assert_eq!(ws.sidebar_width(), SIDEBAR_MAX_WIDTH);
    }

    #[test]
    fn sidebar_width_ignores_non_finite_input() {
        let ws = Workspace::new();
        ws.set_sidebar_width(300.0);
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            ws.set_sidebar_width(bad);
            assert_eq!(ws.sidebar_width(), 300.0, "{bad} must be ignored");
        }
    }

    #[test]
    fn sidebar_width_unchanged_does_not_commit() {
        // The revision only moves on a commit, and a commit is what
        // rewrites state.json — so a re-set of the same (or same-after-
        // clamp) width must leave it alone.
        let ws = Workspace::new();
        ws.set_sidebar_width(300.0);
        let (revision, _) = ws.snapshot_with_revision();
        ws.set_sidebar_width(300.0);
        ws.set_sidebar_width(f64::NAN);
        assert_eq!(ws.snapshot_with_revision().0, revision);
        ws.set_sidebar_width(1000.0);
        ws.set_sidebar_width(500.0);
        assert_eq!(
            ws.snapshot_with_revision().0,
            revision + 1,
            "both clamp to the max, so only the first commits"
        );
    }

    #[test]
    fn active_tab_position_is_dense_index_not_raw_position() {
        // After a mid-project close, positions go sparse (0,1,2 → 1,2).
        // The persisted active_tab_position must be the DENSE index
        // among the surviving tabs (what the UI selects on restore),
        // not the raw `position` field. #95 review.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        {
            let ws = Workspace::open(path.clone());
            let pid = ws.create_project("p", "/").unwrap().id;
            let a = ws.open_tab(pid, "/a", "a").unwrap().id; // position 0
            let _b = ws.open_tab(pid, "/b", "b").unwrap().id; // position 1
            let c = ws.open_tab(pid, "/c", "c").unwrap().id; // position 2
            ws.close_tab(a).unwrap(); // removes position 0 → surviving positions 1,2
            ws.focus_tab(c).unwrap(); // active = c (raw position 2, dense index 1)
        }
        let ws2 = Workspace::open(path);
        let restore = ws2.take_restore_layout().unwrap();
        assert_eq!(
            restore.active_tab_position, 1,
            "active tab is the 2nd surviving tab → dense index 1, not raw position 2"
        );
        // And the surviving tabs are /b, /c in order.
        assert_eq!(
            restore.projects[0]
                .tabs
                .iter()
                .map(|t| t.cwd.as_str())
                .collect::<Vec<_>>(),
            vec!["/b", "/c"]
        );
    }

    /// Issue #196 follow-up: `user_titled` is persisted across
    /// relaunch so a manually-renamed tab keeps its rename — and so
    /// the model's `set_tab_cwd` re-derivation (also #196) doesn't
    /// silently clobber it on the first post-relaunch `cd`.
    #[test]
    fn user_titled_persists_across_relaunch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        {
            let ws = Workspace::open(path.clone());
            let pid = ws.create_project("p", "/").unwrap().id;
            let manual = ws.open_tab(pid, "/tmp", "").unwrap().id;
            let placeholder = ws.open_tab(pid, "/tmp", "roostctl").unwrap().id;
            ws.set_tab_title(manual, "docs").unwrap();
            assert!(ws.tab(manual).unwrap().user_titled);
            assert!(!ws.tab(placeholder).unwrap().user_titled);
        }
        let ws2 = Workspace::open(path);
        let restore = ws2.take_restore_layout().unwrap();
        let tabs = &restore.projects[0].tabs;
        // Two tabs persisted; restore order matches save order.
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].title, "docs");
        assert!(tabs[0].user_titled, "manual rename keeps user_titled");
        assert_eq!(tabs[1].title, "roostctl");
        assert!(!tabs[1].user_titled, "placeholder title is not user_titled");
    }

    #[test]
    fn restore_layout_reflects_persisted_tab_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        {
            let ws = Workspace::open(path.clone());
            let pid = ws.create_project("p", "/").unwrap().id;
            let a = ws.open_tab(pid, "/a", "a").unwrap().id;
            let b = ws.open_tab(pid, "/b", "b").unwrap().id;
            let c = ws.open_tab(pid, "/c", "c").unwrap().id;
            // Reorder to c, a, b — restore must reflect the new order.
            ws.reorder_tabs(pid, &[c, a, b]).unwrap();
        }
        let ws2 = Workspace::open(path);
        let restore = ws2.take_restore_layout().unwrap();
        assert_eq!(
            restore.projects[0]
                .tabs
                .iter()
                .map(|t| t.cwd.as_str())
                .collect::<Vec<_>>(),
            vec!["/c", "/a", "/b"]
        );
    }

    /// Tied persisted tab positions are reachable — the `state.json`
    /// captured for #262 holds `[2, 3, 3]` — and the restore layout is
    /// the order the bootstrap re-opens tabs in, so the tie decides the
    /// restored tab strip. `sort_by_key` is stable, which makes that
    /// order the file's; Swift's `sorted(by:)` is documented as *not*
    /// stable, so its twin
    /// `restoreLayoutBreaksTiedTabPositionsByFileOrder` needs an explicit
    /// index tiebreak to land here. The fixture's file order is neither
    /// ascending nor descending in `position`, so a sort that ignored the
    /// index could not pass by luck.
    #[test]
    fn restore_layout_breaks_tied_tab_positions_by_file_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{ "next_id": 200, "active_project_id": 100,
                 "projects": [
                   { "id": 100, "name": "p", "cwd": "/tmp", "position": 0, "created_at": 100,
                     "tabs": [
                       { "title": "a", "cwd": "/a", "position": 3, "user_titled": false },
                       { "title": "b", "cwd": "/b", "position": 1, "user_titled": false },
                       { "title": "c", "cwd": "/c", "position": 3, "user_titled": false }
                     ] }
                 ] }"#,
        )
        .unwrap();
        let restore = Workspace::open(path).take_restore_layout().unwrap();
        assert_eq!(
            restore.projects[0]
                .tabs
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "a", "c"]
        );
    }

    #[test]
    fn cwd_changes_write_through() {
        // No throttle: every `set_tab_cwd` writes through, so a reopen
        // sees the LATEST cwd (last write wins), not a coalesced
        // earlier one. The two calls below are microseconds apart.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        {
            let ws = Workspace::open(path.clone());
            let pid = ws.create_project("p", "/").unwrap().id;
            let tid = ws.open_tab(pid, "/start", "").unwrap().id;
            ws.set_tab_cwd(tid, "/first").unwrap();
            ws.set_tab_cwd(tid, "/second").unwrap();
        }
        let ws2 = Workspace::open(path);
        let restore = ws2.take_restore_layout().unwrap();
        assert_eq!(
            restore.projects[0].tabs[0].cwd, "/second",
            "the latest cwd must reach disk (write-through, no throttle)"
        );
    }

    #[test]
    fn flush_freezes_further_persistence() {
        // flush() writes the current layout (with fsync) and then
        // freezes: a subsequent mutation must NOT reach disk, so a
        // teardown PTY-exit cascade can't clobber the flushed layout.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        {
            let ws = Workspace::open(path.clone());
            let pid = ws.create_project("p", "/").unwrap().id;
            let tid = ws.open_tab(pid, "/flushed", "").unwrap().id;
            ws.flush();
            // Frozen — this write is a no-op.
            ws.set_tab_cwd(tid, "/after-flush").unwrap();
        }
        let ws2 = Workspace::open(path);
        let restore = ws2.take_restore_layout().unwrap();
        assert_eq!(
            restore.projects[0].tabs[0].cwd, "/flushed",
            "a post-flush mutation must not have reached disk"
        );
    }

    // Load-path repair of colliding project positions (#262).
    //
    // Persisted *tabs* re-open through `open_tab` (the UI bootstrap),
    // so their positions are freshly allocated and a persisted tab
    // collision self-heals. Persisted *projects* load their position
    // verbatim, so a collision written by a pre-#80 GTK build survives
    // every relaunch until `normalize_project_positions` breaks the
    // tie. The Swift `WorkspaceStatePersistenceTests` mirror these.

    /// Load a hand-written `state.json` and return the restored
    /// `(name, position)` pairs in display order.
    fn restored_project_positions(json: &str) -> Vec<(String, i32)> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, json).unwrap();
        Workspace::open(path)
            .snapshot()
            .into_iter()
            .map(|p| (p.name, p.position))
            .collect()
    }

    fn project_json(rows: &[(i64, &str, i32)]) -> String {
        project_json_active(rows, 0)
    }

    fn project_json_active(rows: &[(i64, &str, i32)], active_project_id: i64) -> String {
        let projects: Vec<String> = rows
            .iter()
            .map(|(id, name, position)| {
                format!(
                    r#"{{ "id": {id}, "name": "{name}", "cwd": "/tmp",
                          "position": {position}, "created_at": {id}, "tabs": [] }}"#
                )
            })
            .collect();
        format!(
            r#"{{ "next_id": 200, "active_project_id": {active_project_id},
                  "projects": [{}] }}"#,
            projects.join(",")
        )
    }

    #[test]
    fn colliding_project_positions_are_repaired_on_load() {
        let restored = restored_project_positions(&project_json(&[
            (100, "AAA", 0),
            (101, "BBB", 2),
            (102, "CCC", 2),
        ]));
        assert_eq!(
            restored,
            vec![
                ("AAA".to_string(), 0),
                ("BBB".to_string(), 2),
                ("CCC".to_string(), 3),
            ],
            "the tie breaks by pushing the loser up; relative order is preserved"
        );
    }

    /// Positions are sparse by design — the invariant is uniqueness
    /// within a parent, never density. `[0, 5, 5]` must repair to
    /// `[0, 5, 6]`, not `[0, 1, 2]`: densifying would rewrite the two
    /// rows that were never wrong.
    #[test]
    fn project_position_repair_preserves_gaps() {
        let restored = restored_project_positions(&project_json(&[
            (100, "AAA", 0),
            (101, "BBB", 5),
            (102, "CCC", 5),
        ]));
        assert_eq!(
            restored.iter().map(|(_, p)| *p).collect::<Vec<_>>(),
            vec![0, 5, 6]
        );
    }

    /// Repairing an already-repaired file changes nothing: load the
    /// collision, persist the repair, reload.
    #[test]
    fn project_position_repair_is_idempotent_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            project_json(&[(100, "AAA", 0), (101, "BBB", 2), (102, "CCC", 2)]),
        )
        .unwrap();

        {
            let ws = Workspace::open(path.clone());
            assert_eq!(
                ws.snapshot().iter().map(|p| p.position).collect::<Vec<_>>(),
                vec![0, 2, 3]
            );
            // Any persisting mutator writes the repair back to disk.
            ws.rename_project(100, "AAA").unwrap();
        }

        let ws2 = Workspace::open(path);
        assert_eq!(
            ws2.snapshot()
                .iter()
                .map(|p| p.position)
                .collect::<Vec<_>>(),
            vec![0, 2, 3],
            "normalizing twice equals normalizing once"
        );
    }

    /// The overwhelmingly common case: nobody's positions collide. The
    /// repair must not touch them, gaps included.
    #[test]
    fn unique_project_positions_are_left_alone() {
        let restored = restored_project_positions(&project_json(&[
            (100, "AAA", 0),
            (101, "BBB", 5),
            (102, "CCC", 9),
        ]));
        assert_eq!(
            restored.iter().map(|(_, p)| *p).collect::<Vec<_>>(),
            vec![0, 5, 9]
        );
    }

    /// The reason the walk seeds `previous` with `None` rather than
    /// `-1`. `position` deserializes as a plain `i32` with no
    /// non-negativity check, so a hand-edited file can hold negatives;
    /// a `-1` seed would rewrite a unique, correctly-ordered
    /// `[-5, -3]` to `[0, 1]`, breaking the no-op guarantee on input
    /// that was never wrong.
    #[test]
    fn unique_negative_project_positions_are_left_alone() {
        let restored =
            restored_project_positions(&project_json(&[(100, "AAA", -5), (101, "BBB", -3)]));
        assert_eq!(
            restored.iter().map(|(_, p)| *p).collect::<Vec<_>>(),
            vec![-5, -3]
        );
    }

    /// The repair's boundary case: `previous + 1` at `i32::MAX` would
    /// panic in a debug build. Positions come straight off a
    /// user-editable file, so it saturates instead — the result is
    /// still a tie, which is accepted (corrupt input degrades, it does
    /// not crash the launch).
    #[test]
    fn project_positions_tied_at_i32_max_load_without_panicking() {
        let restored = restored_project_positions(&project_json(&[
            (100, "AAA", i32::MAX),
            (101, "BBB", i32::MAX),
        ]));
        assert_eq!(
            restored,
            vec![("AAA".to_string(), i32::MAX), ("BBB".to_string(), i32::MAX),],
            "both projects survive; the clamp degrades to a tie"
        );
    }

    /// The accepted degradation at the ceiling, end to end: repairing
    /// `[i32::MAX - 1, i32::MAX - 1]` pushes the loser to `i32::MAX`,
    /// and the next `create_project` clamps onto that same value. Two
    /// projects then share `i32::MAX`.
    ///
    /// **This is the pinned behavior, not a defect** (plan 004 §3.1): a
    /// workspace at the integer ceiling is corrupt input, not a
    /// supported state, so the arithmetic degrades to a tie rather than
    /// panicking at launch. The repair is a *uniqueness* pass below the
    /// ceiling and a *survival* pass at it — it is explicitly **not** a
    /// uniqueness guarantee. The Swift
    /// `ceilingRepairDegradesToATieOnTheNextAllocation` mirrors this.
    #[test]
    fn ceiling_repair_degrades_to_a_tie_on_the_next_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            project_json(&[(100, "AAA", i32::MAX - 1), (101, "BBB", i32::MAX - 1)]),
        )
        .unwrap();

        let ws = Workspace::open(path);
        assert_eq!(
            ws.snapshot().iter().map(|p| p.position).collect::<Vec<_>>(),
            vec![i32::MAX - 1, i32::MAX],
            "the repair spends the last position in the range"
        );

        let next = ws.create_project("CCC", "/tmp").unwrap();
        assert_eq!(
            next.position,
            i32::MAX,
            "max + 1 saturates onto the row the repair just placed"
        );
        assert_eq!(
            ws.snapshot().iter().map(|p| p.position).collect::<Vec<_>>(),
            vec![i32::MAX - 1, i32::MAX, i32::MAX],
            "accepted tie at the ceiling"
        );
    }

    /// A file with no projects must load as an empty workspace rather
    /// than tripping the normalizer's walk (`previous` never leaves
    /// `None`) or the restore-layout build.
    #[test]
    fn empty_project_list_loads_clean() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, project_json(&[])).unwrap();

        let ws = Workspace::open(path);
        assert!(ws.snapshot().is_empty());
        let restore = ws.take_restore_layout().unwrap();
        assert!(restore.projects.is_empty());
    }

    /// A file whose row order differs from display order, with a
    /// collision inside it. The repair must be computed over the
    /// *display* walk while everything ordered by file position — the
    /// restore layout, the active-project resolution — is untouched,
    /// and `snapshot()` must still come back by `(position, id)`.
    ///
    /// File order `C@5, A@0, B@5`; display order `A@0, C@5, B@5`. The
    /// tie is between C and B, and only the walk order reveals which
    /// of them moves. The Swift
    /// `fileOrderDifferingFromDisplayOrderRepairsAndRestores` mirrors it.
    #[test]
    fn file_order_differing_from_display_order_repairs_and_restores() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            project_json_active(&[(1, "C", 5), (2, "A", 0), (3, "B", 5)], 3),
        )
        .unwrap();

        let ws = Workspace::open(path);
        assert_eq!(
            ws.snapshot()
                .iter()
                .map(|p| (p.name.as_str(), p.position))
                .collect::<Vec<_>>(),
            vec![("A", 0), ("C", 5), ("B", 6)],
            "snapshot is by (position, id); B loses the tie to the lower id"
        );

        let restore = ws.take_restore_layout().unwrap();
        assert_eq!(
            restore
                .projects
                .iter()
                .map(|p| p.project_id)
                .collect::<Vec<_>>(),
            vec![1, 2, 3],
            "the restore layout keeps the file's row order"
        );
        assert_eq!(restore.active_project_id, 3);
        assert!(
            ws.snapshot().iter().any(|p| p.id == 3),
            "the active project still resolves against the repaired rows"
        );
    }

    /// Two rows sharing an `id` — corrupt input the loader accepts. The
    /// id-keyed map collapses them last-write-wins, so **insertion
    /// order picks the survivor**, and insertion order is the file's.
    /// The collapse itself is pre-existing and out of scope; what this
    /// pins is that the load-path repair does not change it, and that
    /// Swift agrees. `duplicateProjectIDsKeepTheFileOrderSurvivor`
    /// asserts the same literal fixture on the Mac side; if the two
    /// ever disagree, the same `state.json` yields a different project
    /// identity, metadata and tab layout per platform.
    #[test]
    fn duplicate_project_ids_keep_the_file_order_survivor() {
        let restored = restored_project_positions(&project_json(&[(7, "high", 10), (7, "low", 0)]));
        assert_eq!(
            restored,
            vec![("low".to_string(), 0)],
            "the last row in the file wins, not the last in display order"
        );
    }
}
