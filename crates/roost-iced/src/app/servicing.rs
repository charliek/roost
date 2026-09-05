use std::collections::BTreeMap;

use roost_ui_model::keys::HostId;

use super::interactions::{host_reorder_call, ReorderTarget};
use super::*;

/// The UI socket's own error codes (`docs/reference/ipc.md`'s "Errors"
/// bullet) **minus `shutting-down`** — the codes that mean the same
/// thing whichever socket answered, so a session's refusal spelt one of
/// these can cross unchanged.
///
/// `shutting-down` is excluded even though it is on that list, because
/// the same doc marks it session-socket-only: it describes a *session's*
/// stop latch, which a UI socket has no notion of, and both reorder ops
/// are in the session's mutating set — so a latched session answers it
/// for exactly the ops this maps.
const CODES_A_UI_SOCKET_ALSO_SPEAKS: [&str; 10] = [
    "unknown-op",
    "unknown-field",
    "missing-param",
    "invalid-param",
    "parse-error",
    "frame-too-large",
    "duplicate-id",
    "not-found",
    "not-implemented",
    "internal",
];

/// What a host-routed op's failure says on the wire (plan 044 §3.1 d6).
///
/// A session's own refusal keeps its code when that code is one a UI
/// socket speaks, so a caller matching `invalid-param` sees the same
/// thing whether the session or this client refused it — and the
/// `message` is the session's own, not `Display` (which prefixes the
/// code and would double it).
///
/// Everything else folds onto `host-unavailable`: a lease or lifecycle
/// code, and any code a newer or older session invents. That matters
/// because `ipc.md` tells a client to treat an unlisted code as fatal
/// for the request, so passing an unbounded set through would put codes
/// on the UI socket that its own contract says cannot appear there.
/// Nothing is lost — the folded branch reports
/// [`crate::host_conn::HostOpError`]'s `Display`, which for a refusal
/// is the session's code *and* its sentence, and for a dead connection
/// is the same sentence the drag gesture's status banner shows.
fn host_op_failure(error: &crate::host_conn::HostOpError) -> roost_engine::ipc::HostOpFailure {
    if let crate::host_conn::HostOpError::Rejected { code, message } = error {
        if CODES_A_UI_SOCKET_ALSO_SPEAKS.contains(&code.as_str()) {
            return roost_engine::ipc::HostOpFailure::new(code.as_str(), message.clone());
        }
    }
    roost_engine::ipc::HostOpFailure::new(HOST_UNAVAILABLE, error.to_string())
}

/// This client could not reach the host session an op named.
///
/// `ServerCode::as_str` is not `const` (its `Other` arm derefs a
/// `String`), so the spelling is written out here and tied to the typed
/// variant by `host_unavailable_is_the_typed_code`.
const HOST_UNAVAILABLE: &str = "host-unavailable";

/// The answer for an incarnation this client is not connected to — the
/// same `not-found` `tab.focus` gives for the same reason.
fn no_connected_host() -> roost_engine::ipc::HostOpFailure {
    roost_engine::ipc::HostOpFailure::new("not-found", "no connected host with that incarnation")
}

/// One host section as `app.sidebar_dump` reports it (plan 044 §3.1 d7).
///
/// The rows come from the view's mirror clone, which is the
/// **authoritative** order — a drag preview lives in the App's preview
/// slot and is applied when the sidebar is built, never here. Keys are
/// built through the wire ref types rather than formatted by hand, so
/// the spelling this emits cannot drift from the one the reorder and
/// focus ops parse. A never-connected host has no mirror, so it lists
/// no projects and mints no `h0.…` key.
fn host_dump(view: &HostView) -> SidebarDumpHost {
    SidebarDumpHost {
        id: view.saved_id.clone(),
        label: view.label.clone(),
        state: view.state.wire().to_string(),
        projects: view
            .projects
            .iter()
            .map(|project| SidebarDumpHostProject {
                key: roost_ipc::messages::WireProjectRef::Host {
                    host: view.host.raw(),
                    project: project.id,
                }
                .to_string(),
                name: project.name.clone(),
                tabs: project
                    .tabs
                    .iter()
                    .map(|tab| SidebarDumpHostTab {
                        key: roost_ipc::messages::WireTabRef::Host {
                            host: view.host.raw(),
                            tab: tab.id,
                        }
                        .to_string(),
                        title: tab.title.clone(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

pub(crate) struct AgentMetricsResult {
    session: u64,
    claimed: Vec<String>,
    outcomes: Result<Vec<git_metrics::ProbeOutcome>, String>,
}

/// `Workspace::open_tab` commits and broadcasts `TabOpened` before the
/// caller's `PtySupervisor::spawn` promotes the session, so the attach the
/// event drives can land in that gap and fail. The bounded retry that
/// covers it (#267): forty attempts, 25 ms apart. Attempt one is the reconcile that noticed the
/// tab; the rest are [`Message::AttachRetryTick`].
pub(super) const ATTACH_RETRY_LIMIT: u32 = 40;
pub(crate) const ATTACH_RETRY_INTERVAL: Duration = Duration::from_millis(25);
/// The wall-clock half of the same budget. Reconcile shares the attempt
/// counter with the timer, so a burst of workspace events could otherwise
/// spend forty attempts in milliseconds and give up inside the very race
/// this waits out. Giving up needs both halves.
const ATTACH_RETRY_WINDOW: Duration = ATTACH_RETRY_INTERVAL.saturating_mul(ATTACH_RETRY_LIMIT);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AttachRetryVerdict {
    /// Inside the budget: the tab stays pending and the retry timer stays
    /// armed for it.
    Retry,
    /// The budget just ran out — reported once, at the failure that spent
    /// it.
    Exhausted { attempts: u32, waited: Duration },
    /// The budget was already spent for this tab. Reconcile still makes
    /// its cheap attempt, but nothing re-arms and nothing re-warns.
    GaveUp,
}

#[derive(Debug, Clone, Copy)]
struct PendingAttach {
    attempts: u32,
    first_seen: Instant,
    /// Set when the budget ran out. The entry then OUTLIVES its budget on
    /// purpose: removing it would let the next reconcile insert a fresh
    /// one and start the whole budget over, so giving up would never
    /// stick and the 25 ms timer would re-arm forever.
    exhausted: bool,
}

/// Tabs the workspace lists that have no terminal yet. A retryable entry
/// is exactly what arms the attach-retry subscription, so this set is also
/// the app's "am I still waiting on a PTY" answer. Ordered, so which tab
/// attaches first on a shared tick is repeatable.
#[derive(Debug, Default)]
pub(super) struct PendingAttachments {
    tabs: BTreeMap<TabKey, PendingAttach>,
}

impl PendingAttachments {
    /// Record one failed attach. The first failure starts the budget; the
    /// verdict says whether a retry is still owed.
    pub(super) fn record_failure(&mut self, key: TabKey, now: Instant) -> AttachRetryVerdict {
        let entry = self.tabs.entry(key).or_insert(PendingAttach {
            attempts: 0,
            first_seen: now,
            exhausted: false,
        });
        if entry.exhausted {
            return AttachRetryVerdict::GaveUp;
        }
        entry.attempts += 1;
        let attempts = entry.attempts;
        let waited = now.saturating_duration_since(entry.first_seen);
        if attempts < ATTACH_RETRY_LIMIT || waited < ATTACH_RETRY_WINDOW {
            return AttachRetryVerdict::Retry;
        }
        entry.exhausted = true;
        AttachRetryVerdict::Exhausted { attempts, waited }
    }

    /// Stop tracking a tab entirely — it attached. This is also how an
    /// exhausted mark is lifted: a tab that finally gets a session is a
    /// tab with a fresh budget.
    pub(super) fn clear(&mut self, key: TabKey) {
        self.tabs.remove(&key);
    }

    /// Forget every tab of `host` that its workspace no longer lists — a
    /// spawn that failed and rolled back, or a tab closed while it
    /// waited. Exhausted entries go too: the mark belongs to a tab, not
    /// to an id.
    ///
    /// Scoped to one instance because `live` is one instance's snapshot:
    /// another host's pending attaches are that host's reconcile to
    /// prune, and dropping them here would be inferring their absence
    /// from a listing they were never in.
    pub(super) fn retain_live(&mut self, host: HostId, live: &HashSet<TabKey>) {
        self.tabs
            .retain(|key, _| key.host != host || live.contains(key));
    }

    /// Whether any tab is still owed a retry — the attach-retry
    /// subscription's predicate. An exhausted tab is still tracked but no
    /// longer retryable, so the timer disarms.
    pub(super) fn has_retryable(&self) -> bool {
        self.tabs.values().any(|entry| !entry.exhausted)
    }

    /// The tabs still owed a retry, owned so the walk in
    /// `retry_pending_attachments` can mutate the set as it attaches.
    pub(super) fn retry_keys(&self) -> Vec<TabKey> {
        self.tabs
            .iter()
            .filter(|(_, entry)| !entry.exhausted)
            .map(|(key, _)| *key)
            .collect()
    }

    #[cfg(test)]
    fn tracked_keys(&self) -> Vec<TabKey> {
        self.tabs.keys().copied().collect()
    }
}

/// What one drain batch's PTY items produced. Collected during the drain
/// and applied at its tail, so bytes for the same tab coalesce into a
/// single snapshot rebuild however many items carried them.
#[derive(Debug, Default)]
pub(super) struct TabOutputBatch {
    /// Tabs whose terminal state moved this batch — the only ones whose
    /// snapshot needs rebuilding. Every other tab renders the snapshot it
    /// already has.
    pub(super) touched: HashSet<TabKey>,
    pub(super) osc_actions: Vec<(TabKey, Vec<OscAction>)>,
    pub(super) exited: Vec<TabKey>,
    pub(super) error: Option<String>,
}

pub(super) fn collect_tab_output(
    tabs: &mut HashMap<TabKey, TerminalTab>,
    collected: &mut TabOutputBatch,
    key: TabKey,
    output: TabOutput,
) {
    let tab_id = key.tab;
    let Some(tab) = tabs.get_mut(&key) else {
        // A forwarder outlives its tab by however long its last items sit
        // on the feed: the tab was already dropped by the reconcile that
        // saw the workspace stop listing it.
        tracing::trace!(tab_id, "dropped PTY output for a tab that is gone");
        return;
    };
    match output {
        // The session attaches with the OSC opt-in, so PTY output
        // arrives already scanned: its color-query replies left from the
        // drain (that is D10's whole point) and what reaches here is the
        // remaining actions. `Bytes` is the un-opted-in shape — the
        // (now-removed) GTK UI's — and cannot occur on this path; treat
        // it as a chunk with no actions rather than asserting.
        TabOutput::Bytes(bytes) => {
            tab.write_vt(&bytes);
            collected.touched.insert(key);
        }
        TabOutput::Scanned { data, actions } => {
            tab.write_vt(&data);
            // Most chunks of a flood carry no OSC at all; an empty entry
            // would still cost a push and an `apply_osc_actions` hop.
            if !actions.is_empty() {
                collected.osc_actions.push((key, actions));
            }
            collected.touched.insert(key);
        }
        TabOutput::Exit { status, reason } => {
            tracing::info!(tab_id, status, %reason, "PTY exited");
            collected.exited.push(key);
        }
        TabOutput::Error(error) => {
            // Broadcast lag cannot be reconstructed. Surface it and keep
            // the workspace alive so IPC/UI state still resyncs.
            collected.error = Some(format!("tab {tab_id}: {error}"));
            tracing::error!(tab_id, %error, "PTY output stream lost bytes");
        }
    }
}

/// A click on the OS notification banner, decided off the core alone so it
/// is testable without an `App`: focus the tab the banner named and clear
/// its pending notification, then say what raise the click earned. `None`
/// is a tab that closed between the banner and the click — the
/// (now-removed) GTK UI's `focus_tab_by_id` bailed on the same
/// `focus_tab` error.
///
/// The raise is best-effort: a window that has not opened yet has no id,
/// and the tab focus still landed in the core either way. On Wayland,
/// iced `window::gain_focus` is a no-op (no way to spend the spec
/// `ActivationToken`); see [#351](https://github.com/charliek/roost/issues/351).
fn notification_activation(
    workspace: &Workspace,
    window_id: Option<window::Id>,
    key: TabKey,
) -> Option<UiTask> {
    // The local workspace owns only the local id-space: a banner minted
    // by a connection epoch that has since died carries that instance, and
    // focusing its numeric id here would jump to whatever local tab
    // happens to share the number. `focus_tab_in_core` refuses it, which
    // is the same guard every other engine sink now applies.
    if let Err(error) = focus_tab_in_core(workspace, key) {
        tracing::debug!(?key, %error, "notification click named a tab that is gone");
        return None;
    }
    Some(window_id.map_or(UiTask::None, UiTask::Focus))
}

/// What one envelope from a connected host's event batch asks this
/// client to do.
///
/// Some of the batch's events are per-commit *facts* the mirror
/// deliberately does not model (`mirror.rs`'s "workspace facts only"),
/// and the split is the authority split from architecture §6:
/// `tab.effect` is something that happened to a tab and the attached
/// client applies it; `notification.fired` is the same one surface up —
/// a host agent asking for attention reaches the inbox and the desktop
/// exactly as a local one does (plan 037 §3.1's host-blind agent
/// surfaces). The other three ride along for their *retiring* edges,
/// because an attention row that nothing takes down is worse than one
/// that never appeared: `tab.notification` clears one tab's marker,
/// `tab.closed` retires the row and the desktop banner with the tab, and
/// `project.deleted` sweeps every row under the project — the same three
/// arms `apply_workspace_event` runs for the local workspace.
///
/// They are read off the batch rather than off the mirror because they
/// are exact per-commit: the mirror may already be further ahead, and a
/// fire-then-clear pair folded away would leave a banner nobody asked
/// for. What happened while nothing was attached is **not** replayed —
/// plan 037 §4's non-goal; a connect mirrors current state, never a
/// backlog.
///
/// A session's own workspace has no window, so what it suppresses is
/// decided by what this client tells it: `session.set_focus` (plan 038
/// §C6) pushes the selection + window-focus truth down at every edge
/// that moves it, and the session's `attention_suppressed_by_focus`
/// then reads the same focus the user has. A session too old to serve
/// that op refuses it harmlessly and keeps HS-2's behavior — its
/// attached tab suppresses its own `notification.fired`.
#[derive(Debug)]
enum HostEnvelopeAction {
    Effect(roost_ipc::messages::TabEffectEvent),
    Notify(roost_ipc::messages::NotificationFiredEvent),
    /// A tab's pending flag went false: retire its inbox row.
    ClearNotification(i64),
    /// The tab is gone: retire its row and the banner naming it.
    TabClosed(i64),
    /// The project is gone: retire every row under it.
    ProjectDeleted(i64),
    /// The session's active row moved. Only interesting when it moved
    /// *away* from this client's focus claim — a lease-less third party
    /// (`tab.focus`, `tab.open`) can park the session's selection on a
    /// tab nobody is watching, which the suppression predicate would
    /// then mute at the source until this client's next natural edge.
    /// Re-asserting the claim closes that window.
    ActiveMoved(i64),
    /// A workspace fact the mirror already folded in, or an event from a
    /// newer session this client does not know. Both are silent by
    /// contract (`ipc.md` #versioning: old clients ignore new events).
    Ignore,
    Undecodable(serde_json::Error),
}

fn host_envelope_action(envelope: &roost_ipc::messages::EventEnvelope) -> HostEnvelopeAction {
    use roost_ipc::messages::{
        ops, NotificationFiredEvent, ProjectDeletedEvent, TabClosedEvent, TabNotificationEvent,
    };
    use serde::Deserialize;
    // Read out of the borrowed payload: `serde_json` deserializes from
    // `&Value`, so the batch's tree is never copied to build the small
    // owned struct each arm wants.
    fn decode<'a, T: Deserialize<'a>>(
        envelope: &'a roost_ipc::messages::EventEnvelope,
    ) -> Result<T, serde_json::Error> {
        T::deserialize(&envelope.data)
    }
    match envelope.event.as_str() {
        ops::EVENT_TAB_EFFECT => decode(envelope)
            .map_or_else(HostEnvelopeAction::Undecodable, HostEnvelopeAction::Effect),
        ops::EVENT_NOTIFICATION_FIRED => decode::<NotificationFiredEvent>(envelope)
            .map_or_else(HostEnvelopeAction::Undecodable, HostEnvelopeAction::Notify),
        ops::EVENT_TAB_NOTIFICATION => match decode::<TabNotificationEvent>(envelope) {
            // A *fired* flag is the mirror's — it paints the row's dot,
            // and the body only ever arrives on `notification.fired`.
            // Acting on both would upsert a bodyless duplicate beside
            // the real one.
            Ok(event) if event.has_pending => HostEnvelopeAction::Ignore,
            Ok(event) => HostEnvelopeAction::ClearNotification(event.tab_id),
            Err(error) => HostEnvelopeAction::Undecodable(error),
        },
        ops::EVENT_TAB_CLOSED => decode::<TabClosedEvent>(envelope)
            .map_or_else(HostEnvelopeAction::Undecodable, |event| {
                HostEnvelopeAction::TabClosed(event.tab_id)
            }),
        ops::EVENT_PROJECT_DELETED => decode::<ProjectDeletedEvent>(envelope)
            .map_or_else(HostEnvelopeAction::Undecodable, |event| {
                HostEnvelopeAction::ProjectDeleted(event.project_id)
            }),
        ops::EVENT_ACTIVE_CHANGED => decode::<roost_ipc::messages::ActiveChangedEvent>(envelope)
            .map_or_else(HostEnvelopeAction::Undecodable, |event| {
                HostEnvelopeAction::ActiveMoved(event.tab_id)
            }),
        _ => HostEnvelopeAction::Ignore,
    }
}

/// The inbox rows one instance's project list currently owes, in
/// snapshot order.
///
/// One function over the local workspace's snapshot and over a host
/// mirror's rows, because they are the same shape and the same rule:
/// `has_notification` is the membership edge, and the row's title is
/// composed identically so a host row cannot read as a different kind of
/// row in a list that spans every host (plan 037 §3.1's host-blind agent
/// surfaces).
fn pending_notification_rows(
    host: HostId,
    projects: &[Project],
    rung: &HashSet<TabKey>,
) -> Vec<(TabKey, ProjectKey, String)> {
    projects
        .iter()
        .flat_map(|project| {
            project
                .tabs
                .iter()
                .filter(|tab| {
                    // Two sources, one derivation. `has_notification` is
                    // the server's own attention state; `rung` is a bell
                    // this client heard, which raises attention without
                    // any server flag behind it (plan 037 §3.6 — a bell
                    // is an effect, not workspace state). Folding it in
                    // here rather than exempting the row from the prune
                    // is what keeps the inbox purely derived: a row is
                    // shown exactly while something still says so.
                    tab.has_notification || rung.contains(&TabKey::new(host, tab.id))
                })
                .map(move |tab| {
                    (
                        TabKey::new(host, tab.id),
                        ProjectKey::new(host, project.id),
                        notification_inbox::compose_title(
                            &project.name,
                            &notification_inbox::tab_title(&tab.title, &tab.cwd),
                        ),
                    )
                })
        })
        .collect()
}

/// The main-thread marker an IPC-serviced macOS seam call needs. The
/// IPC drain runs in the iced update loop, so the marker is obtainable;
/// `None` would be an invariant break, and surfacing it as an op error
/// is what makes the e2e fail loudly instead of reading a plausible
/// "badge cleared" / empty dump.
#[cfg(target_os = "macos")]
fn serviced_on_main(op: &str) -> Result<objc2::MainThreadMarker, String> {
    objc2::MainThreadMarker::new()
        .ok_or_else(|| format!("{op} serviced off the main thread (AppKit is main-thread-only)"))
}

/// The same marker for the fire-and-forget seam syncs, which have no
/// reply to fail: log and skip.
#[cfg(target_os = "macos")]
pub(super) fn seam_on_main(what: &str) -> Option<objc2::MainThreadMarker> {
    let mtm = objc2::MainThreadMarker::new();
    if mtm.is_none() {
        tracing::error!("{what} ran off the main thread; skipping (AppKit is main-thread-only)");
    }
    mtm
}

/// The `app.dock_badge` read, on the main thread.
#[cfg(target_os = "macos")]
fn read_dock_badge() -> Result<Option<String>, String> {
    let mtm = serviced_on_main("app.dock_badge")?;
    Ok(crate::macos::dock_badge::read(mtm))
}

/// The iced UI also builds for Linux, where there is no Dock. Same
/// verdict as the (now-removed) GTK UI's arm: reject, so the op can
/// never report a cleared badge on a platform that has none.
#[cfg(not(target_os = "macos"))]
fn read_dock_badge() -> Result<Option<String>, String> {
    Err("app.dock_badge is not supported on this UI (macOS iced only)".into())
}

/// The `app.menu_dump` read, on the main thread.
#[cfg(target_os = "macos")]
fn read_menu_dump() -> Result<AppMenuDumpResult, String> {
    let mtm = serviced_on_main("app.menu_dump")?;
    crate::macos::menu::dump(mtm)
}

/// There is no native menu bar off macOS. Same verdict as
/// [`read_dock_badge`]'s Linux arm.
#[cfg(not(target_os = "macos"))]
fn read_menu_dump() -> Result<AppMenuDumpResult, String> {
    Err("app.menu_dump is not supported on this UI (macOS iced only)".into())
}

/// The `app.menu_activate` dispatch, on the main thread.
#[cfg(target_os = "macos")]
fn activate_menu(path: &[String]) -> Result<(), String> {
    let mtm = serviced_on_main("app.menu_activate")?;
    crate::macos::menu::activate(mtm, path)
}

#[cfg(not(target_os = "macos"))]
fn activate_menu(_path: &[String]) -> Result<(), String> {
    Err("app.menu_activate is not supported on this UI (macOS iced only)".into())
}

/// The `app.update_status` read, on the main thread — the Sparkle seam
/// keeps its state in main-thread `thread_local!`s, so the marker is
/// what makes the read well-defined at all, not just a convention.
#[cfg(target_os = "macos")]
fn read_update_status() -> Result<AppUpdateStatusResult, String> {
    let mtm = serviced_on_main("app.update_status")?;
    Ok(crate::macos::sparkle::status(mtm))
}

/// Sparkle is macOS-only. Same verdict as [`read_dock_badge`]'s Linux
/// arm: reject, so the op can never report a plausible "unavailable"
/// on a platform whose seam was never compiled.
#[cfg(not(target_os = "macos"))]
fn read_update_status() -> Result<AppUpdateStatusResult, String> {
    Err("app.update_status is not supported on this UI (macOS iced only)".into())
}

/// The `app.update_check` dispatch, on the main thread.
#[cfg(target_os = "macos")]
fn start_update_check() -> Result<(), String> {
    let mtm = serviced_on_main("app.update_check")?;
    crate::macos::sparkle::check_for_update_information(mtm)
}

#[cfg(not(target_os = "macos"))]
fn start_update_check() -> Result<(), String> {
    Err("app.update_check is not supported on this UI (macOS iced only)".into())
}

/// The `app.notification_status` read, on the main thread — the
/// notification seam keeps its state in main-thread `thread_local!`s
/// (plus a couple of atomics), same reasoning as [`read_update_status`].
#[cfg(target_os = "macos")]
fn read_notification_status() -> Result<AppNotificationStatusResult, String> {
    let mtm = serviced_on_main("app.notification_status")?;
    Ok(crate::macos::notifications::status(mtm))
}

/// The UN backend is macOS-only. Same verdict as [`read_dock_badge`]'s
/// Linux arm.
#[cfg(not(target_os = "macos"))]
fn read_notification_status() -> Result<AppNotificationStatusResult, String> {
    Err("app.notification_status is not supported on this UI (macOS iced only)".into())
}

/// The shared precedence for the six macOS-iced-only test ops
/// (`app.dock_badge`, `app.menu_dump`, `app.menu_activate`,
/// `app.update_status`, `app.update_check`, `app.notification_status`):
/// platform rejection outranks the test-mode gate, so non-macOS iced
/// answers not-implemented (from `read` itself), same as the
/// (now-removed) GTK UI did, not not-enabled.
fn macos_test_gated<T>(
    test_mode: bool,
    read: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    if cfg!(target_os = "macos") && !test_mode {
        Err("ROOST_TEST_MODE=1 is required".into())
    } else {
        read()
    }
}

/// `app.keybind_dispatch`'s namespace restriction. The op exists solely
/// so `ROOST_TEST_MODE=1` can drive the paste accelerator — nothing else
/// in that table has another IPC seam — so `"paste"` is the only name it
/// may resolve. Every other `KeybindAction::from_name` spelling
/// (`close_tab`, `new_tab`, `close_project`, `copy`, …) would hand an
/// arbitrary IPC client a way to close a live terminal, mutate workspace
/// state, or write the system clipboard, and is refused before
/// `dispatch_keybind_action` ever sees it. `ipc.rs`'s dispatcher already
/// rejects a non-`"paste"` action as `invalid-param` ahead of the UI
/// round trip (mirroring `app.dialog_answer`'s `action` check); this is
/// the backstop that keeps the restriction true even if this function is
/// ever called some other way.
fn paste_only_keybind(action: &str) -> Result<KeybindAction, String> {
    if action != "paste" {
        return Err(format!("action must be \"paste\", got {action:?}"));
    }
    KeybindAction::from_name(action)
        .ok_or_else(|| "\"paste\" did not resolve to a KeybindAction".to_string())
}

impl App {
    pub(super) fn reconcile(&mut self) {
        // A full authoritative snapshot on every reconcile is the recovery
        // path for a slow consumer — a lagged broadcast arrives as a
        // `Resync` and this rebuild is what heals it: deltas are an
        // optimization, never UI truth.
        self.projects = self.workspace.snapshot();
        // The host sections are the other half of that snapshot, and
        // they come first for the same reason: everything below re-
        // resolves a key against the rows of whichever instance owns it,
        // and a host key resolves against these. (They read only the
        // registry and the connection set, so nothing below can affect
        // them.)
        self.refresh_host_views();
        // Every pill-relevant change (title, active, notification) lands
        // here, so this is where the elision memo is refreshed. Gated on a
        // window: the bootstrap reconcile runs before the chrome fonts are
        // registered, and a measurement taken then would be cached wrong
        // forever — `window_opened` does the first populate instead.
        if self.window_id.is_some() {
            self.refresh_pill_labels();
        }
        self.request_exit_if_empty();
        self.reconcile_confirm_delete();
        self.reconcile_tab_drag_preview();
        self.reconcile_project_drag_preview();
        self.reconcile_rename_editor();
        self.reconcile_notification_inbox();
        // Immediately after the inbox reconcile, not on the fire/clear
        // edges: this is the authoritative resync, so hanging the badge
        // off it covers fire, clear, tab close and project delete by
        // construction — the same reason the palette refresh below sits
        // here.
        self.sync_dock_badge();
        // Same reasoning, one surface over: the Window menu's rows are
        // project/tab state, so hanging them off the authoritative resync
        // covers open, close, rename, reorder and select by construction.
        self.sync_window_menu();
        self.refresh_notification_palette();
        // Before the selection check, which decides whether the window
        // is still showing a row that exists: whatever this selects is
        // then validated exactly as any other selection would be.
        self.resolve_pending_host_selection();
        self.reconcile_host_selection();
        self.refresh_sidebar_agents();
        self.refresh_agent_palette();
        // Host verbs are live state too: a host that connected while the
        // palette was open must stop offering Connect and start offering
        // Stop (plan 037 §3.1's "verbs appear only when applicable").
        self.refresh_host_palette();
        // The workspace is one backend's id-space, so its ids qualify at
        // that backend's instance — and the prune below is scoped to it
        // for the same reason: a connected host's tabs are pruned by that
        // host's own reconcile, and a snapshot this workspace never
        // listed them in says nothing about them.
        let host = self.backend.host();
        let live: HashSet<TabKey> = self
            .projects
            .iter()
            .flat_map(|project| project.tabs.iter().map(|tab| TabKey::new(host, tab.id)))
            .collect();
        self.tabs
            .retain(|key, _| key.host != host || live.contains(key));
        self.pending_attachments.retain_live(host, &live);
        let active_key = self.active_tab_key();
        self.request_tab_reveal(active_key);
        for (key, tab) in &mut self.tabs {
            if *key != active_key && tab.reset_pointer_state() {
                refresh_or_warn(key.tab, tab, "pointer reset after active tab changed");
            }
        }
        // Every focus change funnels through `focus_tab_and_clear`, which
        // reconciles — so this is the one place a tab switch cancels a
        // composition the user left behind.
        cancel_preedits(&mut self.tabs, &mut self.ime_discard, Some(active_key));
        let now = Instant::now();
        for key in &live {
            if self.tabs.contains_key(key) {
                continue;
            }
            self.attach_tab_tracked(*key, now);
        }
    }

    /// Mac parity: an empty workspace ends the app (App.swift closes the
    /// window on `.projectDeleted` with no projects left, and the process
    /// terminates behind it). Hooked to the reconciled SNAPSHOT rather
    /// than to the `ProjectDeleted` event: a lagged broadcast collapses
    /// into a `Resync`, which carries no per-project event to react to.
    /// Reading the snapshot instead covers every route by construction —
    /// closing the last tab (the engine cascades tab → project), the
    /// confirm dialog, the palette, and raw `project.delete` over IPC.
    ///
    /// Boot is safe: `hydrate_workspace` seeds a default project before
    /// the first reconcile, so the workspace is never observed empty
    /// except after the user emptied it.
    fn request_exit_if_empty(&mut self) {
        if self.exit_state.observe(self.projects.is_empty()) {
            tracing::info!("last project closed; exiting");
        }
    }

    // ---- host tab attach (plan 037 §3.4) --------------------------------

    /// Focus a host tab: build its rendering state if needed and start
    /// the attach. Attach-on-focus is the policy, so every other host
    /// attach detaches first — client memory and data connections stay
    /// bounded at one per host. C6's sidebar click and C7's creation
    /// routing are the callers.
    pub(super) fn host_focus_tab(&mut self, key: TabKey) {
        debug_assert!(!key.is_local(), "local tabs focus through the workspace");
        let others: Vec<TabKey> = self
            .host_attach
            .keys()
            .copied()
            .filter(|other| *other != key)
            .collect();
        for other in others {
            self.host_detach_tab(other);
        }
        if self.host_attach.contains_key(&key) {
            return;
        }
        let (cols, rows) = super::terminal_grid(
            self.window_size,
            self.effective_sidebar_width(),
            self.terminal_metrics,
        );
        let geometry = self.host_geometry(cols, rows);
        let attach =
            host_tab::HostAttach::new(key, geometry).with_resume(self.host_resume.remove(&key));
        let handle = TabHandle::host(attach.input_tx(), self.test_mode);
        match self.tabs.entry(key) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                // Refocus: the surviving terminal keeps rendering (and is
                // the resume base); only the handle turns over, so input
                // reaches the NEW attempt's queue instead of a dead one.
                entry.get_mut().session = handle;
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                let built = {
                    let _guard = self.runtime.enter();
                    TerminalTab::attach_host(
                        cols,
                        rows,
                        Theme::load_bundled(&self.active_theme_name),
                        self.config.word_break_chars.clone(),
                        handle,
                    )
                };
                match built {
                    Ok(mut tab) => {
                        // Install renderer metrics before the tab is
                        // rendered at all: the view draws only a tab with
                        // `applied_metrics`, and a host tab is created
                        // between window resizes, so nothing else would
                        // ever give it any — the terminal would stay
                        // blank while its snapshot filled up. The local
                        // attach does exactly this for the same reason.
                        // The resize half is a no-op on the wire: this
                        // geometry is what `tab.attach` already asked
                        // for, and a host handle drops `send_resize`
                        // anyway (the attach machine owns that).
                        match tab.apply_geometry(
                            cols,
                            rows,
                            self.terminal_metrics,
                            self.metric_generation,
                        ) {
                            Ok(Some(change)) => tab.commit_geometry(change),
                            Ok(None) => {
                                tracing::warn!(%key, "host terminal did not install metrics");
                                return;
                            }
                            Err(error) => {
                                tracing::warn!(%key, ?error, "host terminal geometry failed");
                                return;
                            }
                        }
                        entry.insert(tab);
                    }
                    Err(error) => {
                        tracing::warn!(%key, ?error, "host tab terminal build failed");
                        return;
                    }
                }
            }
        }
        self.host_attach.insert(key, attach);
        self.host_begin_attach(key);
    }

    /// The grid plus the cell pixel size an attach negotiates, from the
    /// metrics the local tabs are already laid out with.
    pub(super) fn host_geometry(&self, cols: u16, rows: u16) -> host_tab::Geometry {
        host_tab::Geometry {
            cols,
            rows,
            cell_w: self.terminal_metrics.cell_width.round().max(1.0) as u32,
            cell_h: self.terminal_metrics.cell_height.round().max(1.0) as u32,
        }
    }

    /// Detach a host tab (focus moved away, or its stream told us to let
    /// go). The rendering state survives in `tabs` — disconnect is not
    /// stop, and refocus resumes from the point saved here.
    pub(super) fn host_detach_tab(&mut self, key: TabKey) {
        if let Some(attach) = self.host_attach.remove(&key) {
            if let Some(resume) = attach.detach() {
                self.host_resume.insert(key, resume);
            }
        }
    }

    /// Drop every piece of app state keyed by a dead connection
    /// incarnation. The fresh mirror is authoritative; purge-then-
    /// re-derive is what prevents duplicates (plan 037 §3.2).
    pub(super) fn purge_host_incarnation(&mut self, incarnation: HostId) {
        // Every attach and every resume point belongs to a key that also
        // has a `tabs` entry (`host_focus_tab` mints them together), so
        // dropping the incarnation's tabs drops all three. The retain
        // afterwards is belt-and-braces for a resume point whose tab
        // already went.
        let dead: Vec<TabKey> = self
            .tabs
            .keys()
            .copied()
            .filter(|key| key.host == incarnation)
            .collect();
        for key in dead {
            self.host_drop_tab(key);
        }
        self.host_resume.retain(|key, _| key.host != incarnation);
        // Bells belong to the incarnation that heard them: the fresh
        // mirror is authoritative, and a tab that is still ringing will
        // ring again.
        self.host_bells.retain(|key| key.host != incarnation);
        // A creation still waiting for this incarnation's mirror will
        // never be answered by it (plan 037 §3.9's "dropped on
        // disconnect").
        if self
            .pending_host_selection
            .is_some_and(|pending| pending.tab.host == incarnation)
        {
            self.pending_host_selection = None;
        }
    }

    /// The tab is over (EXIT, or its whole connection incarnation went):
    /// drop everything local to it. The mirror's `tab.closed` event
    /// retires its sidebar row.
    fn host_drop_tab(&mut self, key: TabKey) {
        self.host_detach_tab(key);
        self.host_resume.remove(&key);
        self.host_bells.remove(&key);
        self.tabs.remove(&key);
        self.notification_inbox.remove(key);
        self.desktop_notifications.retire(key);
    }

    fn host_begin_attach(&mut self, key: TabKey) {
        let Some(attach) = self.host_attach.get_mut(&key) else {
            return;
        };
        let (Some(ops), Some(socket)) = (
            self.hosts.ops_for(key.host),
            self.hosts.endpoint_for(key.host),
        ) else {
            // The connection this key belongs to is gone (stale
            // incarnation, or the host dropped between frames). Nothing
            // to attach to; the entry goes with it.
            tracing::debug!(%key, "host attach requested for a dead connection incarnation");
            self.host_attach.remove(&key);
            return;
        };
        let _guard = self.runtime.enter();
        attach.begin(ops, socket, &roost_vt::libghostty_build(), &self.feed_tx);
    }

    fn apply_host_tab_frame(
        &mut self,
        key: TabKey,
        frame: host_tab::HostTabFrame,
        pty: &mut TabOutputBatch,
    ) {
        let step = match (self.host_attach.get_mut(&key), self.tabs.get_mut(&key)) {
            (Some(attach), Some(tab)) => {
                // The machine arms its own timers, so it needs the app
                // runtime ambient the same way `begin` and `arm_reattach`
                // below do — `tokio::spawn` panics without it.
                let _guard = self.runtime.enter();
                attach.on_frame(frame, tab, &self.feed_tx)
            }
            // A frame for a key with no attach state: the tab detached,
            // the tab dropped, or the whole incarnation is stale — every
            // case is the same harmless late delivery.
            _ => {
                tracing::debug!(%key, "host tab frame without attach state");
                return;
            }
        };
        match step {
            host_tab::AttachStep::None => {}
            host_tab::AttachStep::Refresh => {
                pty.touched.insert(key);
            }
            host_tab::AttachStep::Reattach { delay } => {
                if delay.is_zero() {
                    self.host_begin_attach(key);
                } else if let Some(attach) = self.host_attach.get_mut(&key) {
                    let _guard = self.runtime.enter();
                    attach.arm_reattach(delay, &self.feed_tx);
                }
            }
            host_tab::AttachStep::Detach => self.host_detach_tab(key),
            host_tab::AttachStep::Closed { code } => {
                tracing::debug!(%key, code, "host tab exited");
                pty.touched.remove(&key);
                self.host_drop_tab(key);
            }
        }
    }

    /// Route one host batch's per-commit envelopes to the surfaces that
    /// own them (plan 037 §3.1). [`host_envelope_action`] says which is
    /// which and why they are read off the batch rather than the mirror.
    fn apply_host_envelopes(
        &mut self,
        host: HostId,
        events: &[roost_ipc::messages::EventEnvelope],
    ) -> UiTask {
        let mut task = UiTask::None;
        for envelope in events {
            match host_envelope_action(envelope) {
                HostEnvelopeAction::Ignore => {}
                HostEnvelopeAction::Effect(effect) => {
                    task = task.then(self.apply_host_effect(host, &effect));
                }
                HostEnvelopeAction::Notify(fired) => {
                    self.fire_notification(TabKey::new(host, fired.tab_id), fired.title, fired.body)
                }
                HostEnvelopeAction::ClearNotification(tab_id) => {
                    // The tab's id is kept, exactly as the local clear
                    // keeps it: a later re-notify then replaces the
                    // banner still on the desktop rather than stacking a
                    // duplicate beside it.
                    self.notification_inbox.remove(TabKey::new(host, tab_id));
                }
                HostEnvelopeAction::TabClosed(tab_id) => {
                    // Both surfaces, exactly as the local `TabClosed`
                    // arm does it: the tab is gone, so the banner naming
                    // it is retired rather than left on the desktop
                    // pointing at nothing.
                    let tab = TabKey::new(host, tab_id);
                    self.notification_inbox.remove(tab);
                    self.desktop_notifications.retire(tab);
                }
                HostEnvelopeAction::ProjectDeleted(project_id) => {
                    self.retire_project_notifications(ProjectKey::new(host, project_id));
                }
                HostEnvelopeAction::ActiveMoved(tab_id) => {
                    // Our own set_focus echoes back as a move that
                    // matches the claim, so this cannot ping-pong.
                    if self.hosts.focus_claim_disagrees(host, tab_id) {
                        self.push_host_focus();
                    }
                }
                HostEnvelopeAction::Undecodable(error) => tracing::debug!(
                    ?host, event = %envelope.event, %error,
                    "a host event envelope did not decode"
                ),
            }
        }
        task
    }

    /// An agent asked for attention: one inbox row and one desktop
    /// banner, wherever the tab lives (plan 037 §3.1's host-blind agent
    /// surfaces).
    ///
    /// The two surfaces are composed here rather than once per event
    /// stream because the ordering is the contract: the banner fires
    /// whether or not the row could be composed. A title lookup that
    /// misses means the rows for that key have not landed yet — the
    /// notification is true either way, and a banner is the half the
    /// user is not looking at the window to see. The row is not lost
    /// with it: the same commit sets the tab's `has_notification`, so
    /// [`Self::reconcile_notification_inbox`] derives the row (bodyless)
    /// as soon as the mirror lists that tab.
    ///
    /// Only the lookup differs by key-space, and each answers `None` for
    /// the other's keys: local rows come from this backend's snapshot,
    /// a host's from that session's mirror.
    fn fire_notification(&mut self, key: TabKey, title: String, body: String) {
        let row = if key.is_local() {
            self.notification_title(key)
        } else {
            self.host_notification_title(key)
        };
        if let Some((project, row_title)) = row {
            self.notification_inbox
                .upsert(notification_inbox::NotificationRecord::new(
                    key,
                    project,
                    row_title,
                    body.clone(),
                ));
        }
        self.desktop_notifications.fire(key, title, body);
    }

    /// Apply one `tab.effect` envelope from a connected host: bell rings
    /// the notification inbox (the app's attention surface — local tabs
    /// have no bell path, so this is the closest existing one), and an
    /// OSC 52 write lands on this client's clipboard under the same
    /// config policy a local tab's write obeys. The caller chains the
    /// returned task — it is the queue pump, exactly as
    /// `apply_osc_actions`'s tail is for a local write.
    fn apply_host_effect(
        &mut self,
        host: HostId,
        effect: &roost_ipc::messages::TabEffectEvent,
    ) -> UiTask {
        let key = TabKey::new(host, effect.tab_id);
        match effect.effect {
            roost_ipc::messages::TabEffect::Bell => {
                // Record that this tab rang, then let the ordinary
                // reconcile derive the row from it. Upserting the row
                // here instead would put an undeclared row in a set the
                // reconcile prunes against the mirror, and the next
                // reconcile would erase it — which is exactly what a
                // bell used to do: arrive, and vanish before anyone saw
                // it. It clears where every attention marker clears, on
                // focus.
                if self.host_bells.insert(key) {
                    self.reconcile_notification_inbox();
                }
                UiTask::None
            }
            roost_ipc::messages::TabEffect::ClipboardWrite => {
                let Some(data) = effect.data.as_deref() else {
                    return UiTask::None;
                };
                let Ok(bytes) = roost_ipc::messages::bytes_base64::decode(data) else {
                    tracing::debug!(%key, "clipboard effect with undecodable payload");
                    return UiTask::None;
                };
                let target = match effect.target.unwrap_or_default() {
                    roost_ipc::messages::ClipboardEffectTarget::System => {
                        roost_engine::osc::ClipboardTarget::System
                    }
                    roost_ipc::messages::ClipboardEffectTarget::Selection => {
                        roost_engine::osc::ClipboardTarget::Selection
                    }
                };
                // Reject rather than repair: the local OSC 52 parser
                // refuses non-UTF-8 payloads, and a peer that sends one
                // must not get replacement-altered text onto the
                // clipboard here either.
                let Ok(text) = String::from_utf8(bytes) else {
                    tracing::debug!(%key, "clipboard effect payload is not UTF-8; dropped");
                    return UiTask::None;
                };
                if !enqueue_osc_clipboard_write(
                    &mut self.clipboard,
                    self.config.clipboard_write,
                    target,
                    text,
                ) {
                    tracing::info!(
                        %key,
                        "host OSC 52 clipboard write dropped — clipboard-write = deny"
                    );
                    return UiTask::None;
                }
                self.clipboard.start_next()
            }
        }
    }

    /// The inbox row identity for a host tab, from the mirror the events
    /// stream keeps current.
    fn host_notification_title(&self, key: TabKey) -> Option<(ProjectKey, String)> {
        let mirror = self.hosts.mirror(key.host)?;
        let mirror = mirror.read();
        let (project, tab) = mirror.projects.iter().find_map(|project| {
            let tab = project.tabs.iter().find(|tab| tab.id == key.tab)?;
            Some((project, tab))
        })?;
        // Composed exactly as a local tab's row is (`notification_title`):
        // the inbox is one list across every host, so a host row that
        // said only its bare title would read as a different kind of row.
        Some((
            ProjectKey::new(key.host, project.id),
            notification_inbox::compose_title(
                &project.name,
                &notification_inbox::tab_title(&tab.title, &tab.cwd),
            ),
        ))
    }

    /// Resolve a wire-form tab reference against the UI's keyed map: a
    /// bare id is one of the local backend's, a host-qualified ref names
    /// an attached host tab's client-side terminal (plan 037 §3.4). A
    /// ref from a dead connection epoch simply misses the map — the
    /// staleness contract `HostId` minting exists for.
    fn wire_tab_key(&self, tab: roost_ipc::messages::WireTabRef) -> TabKey {
        match tab {
            roost_ipc::messages::WireTabRef::Local(tab_id) => self.backend.tab_key(tab_id),
            roost_ipc::messages::WireTabRef::Host { host, tab } => {
                TabKey::new(HostId::new(host), tab)
            }
        }
    }

    /// Queue a strip reveal when the OBSERVED active tab changed. Hooked
    /// to reconcile rather than to a focus helper on purpose: `tab.focus`
    /// over IPC mutates the workspace in its handler and reaches the UI
    /// only as a broadcast, so a UI-side funnel would miss exactly the
    /// path the missing reveal was reported on.
    fn request_tab_reveal(&mut self, active_tab: TabKey) {
        if self.revealed_tab == Some(active_tab) {
            return;
        }
        self.revealed_tab = Some(active_tab);
        self.tab_reveal_request = Some(active_tab);
    }

    /// One attach attempt for a tab the workspace lists but the UI has no
    /// terminal for. Returns whether the tab is attached now — reconcile
    /// and the retry driver share this so a retry cannot drift from the
    /// attempt that preceded it.
    fn attach_tab(&mut self, key: TabKey) -> bool {
        // The backend is asked for one of its OWN tabs. A key minted
        // against another instance names nothing this backend has, and
        // handing its number over would attach whichever local session
        // shares the number.
        if key.host != self.backend.host() {
            tracing::debug!(?key, "attach requested for a tab on another instance");
            return false;
        }
        let tab_id = key.tab;
        // Loading the theme and building the terminal costs a 256-entry
        // palette, a scrollback and the encoders — all discarded when the
        // backend's attach finds no session for the id. The retry driver
        // runs 40 times a second, so ask first.
        if !self.backend.is_live(tab_id) {
            tracing::debug!(tab_id, "PTY not ready for UI attach");
            return false;
        }
        let attached = {
            let _guard = self.runtime.enter();
            TerminalTab::attach(
                &self.backend,
                tab_id,
                Theme::load_bundled(&self.active_theme_name),
                self.config.word_break_chars.clone(),
                self.feed_tx.clone(),
            )
        };
        let mut tab = match attached {
            Ok(tab) => tab,
            Err(error) => {
                tracing::debug!(tab_id, ?error, "PTY not ready for UI attach");
                return false;
            }
        };
        let (cols, rows) = terminal_grid(
            self.window_size,
            self.effective_sidebar_width(),
            self.terminal_metrics,
        );
        match tab.apply_geometry(cols, rows, self.terminal_metrics, self.metric_generation) {
            Ok(Some(change)) => {
                tab.commit_geometry(change);
                // A fresh tab's snapshot is the blank default until
                // something refreshes it; nothing else will until the PTY
                // emits its first bytes.
                refresh_or_warn(tab_id, &mut tab, "newly attached tab");
                self.tabs.insert(key, tab);
                true
            }
            Ok(None) => {
                tracing::warn!(tab_id, "new terminal did not install renderer metrics");
                false
            }
            Err(error) => {
                tracing::warn!(
                    tab_id,
                    ?error,
                    "new terminal renderer geometry installation failed"
                );
                false
            }
        }
    }

    /// [`Self::attach_tab`] plus the retry bookkeeping: success clears the
    /// tab from the pending set, failure spends one of its budgeted
    /// attempts.
    ///
    /// A tab whose budget is already spent still comes through here on
    /// every reconcile, and that is its recovery path: the attempt costs a
    /// supervisor lookup, so if a session ever does appear for the id, the
    /// next reconcile attaches it and the exhausted mark goes with it.
    /// What exhaustion ends is the 25 ms timer and the warning, not the
    /// tab's chance to attach.
    fn attach_tab_tracked(&mut self, key: TabKey, now: Instant) {
        if self.attach_tab(key) {
            self.pending_attachments.clear(key);
            return;
        }
        if let AttachRetryVerdict::Exhausted { attempts, waited } =
            self.pending_attachments.record_failure(key, now)
        {
            tracing::warn!(
                tab_id = key.tab,
                attempts,
                waited_ms = waited.as_millis(),
                "tab never attached a terminal: no live PTY within the attach retry budget"
            );
        }
    }

    /// The bounded attach-retry driver behind `Message::AttachRetryTick`.
    /// The subscription that calls it is armed only while some tab is
    /// still owed a retry, so an idle app never runs it.
    pub fn retry_pending_attachments(&mut self) {
        let now = Instant::now();
        for key in self.pending_attachments.retry_keys() {
            if self.tabs.contains_key(&key) {
                self.pending_attachments.clear(key);
                continue;
            }
            // Ask the workspace, not the last snapshot: a spawn that
            // failed rolls the tab back without the UI having reconciled.
            // The probe is a LOCAL liveness oracle, so it only disproves
            // local keys — a non-local pending entry is its own host's to
            // prune (reconcile is host-scoped the same way).
            if key
                .local_tab()
                .is_some_and(|tab_id| self.workspace.tab(tab_id).is_err())
            {
                tracing::debug!(
                    tab_id = key.tab,
                    "dropped a pending attach the workspace no longer lists"
                );
                self.pending_attachments.clear(key);
                continue;
            }
            self.attach_tab_tracked(key, now);
        }
    }

    /// Drain one batch off the engine feed and apply it. Every
    /// asynchronous source shares the channel, so the batch is applied in
    /// arrival order across sources rather than source by source. What the
    /// batch contained never leaves this function — the economy rules it
    /// decides (reconcile or not, refresh which tabs) are applied at its
    /// tail, plus the one reconcile a request may pull forward into the
    /// middle of the drain so it does not read a stale cache.
    pub(super) fn service_engine(&mut self) -> UiTask {
        let mut task = UiTask::None;
        let mut batch = EngineBatch::default();
        let mut pty = TabOutputBatch::default();
        while let Some(item) = self.feed_rx.try_next(&mut batch) {
            match item {
                EngineFeed::Workspace(event) => self.apply_workspace_event(event),
                EngineFeed::Tab(key, output) => {
                    collect_tab_output(&mut self.tabs, &mut pty, key, output);
                }
                EngineFeed::UiRequest(request) => {
                    // IPC reads (`tab.dump`, `palette.state`,
                    // `sidebar.dump`) answer from `self.projects`, which is
                    // only as fresh as the last reconcile. Replies are
                    // eventually consistent by design — every client in
                    // `tools/roosttest` condition-waits rather than
                    // asserting on the first reply — but when a mutation
                    // event already landed in THIS batch there is no
                    // reason to make a caller wait for it: fold it in
                    // first. At most one extra reconcile per mixed batch,
                    // and none for the pure-request and pure-PTY batches
                    // that dominate.
                    if batch.workspace_dirty() {
                        self.reconcile();
                        batch.mark_reconciled();
                    }
                    task = task.then(self.apply_ui_request(request));
                    // The request may itself have mutated the workspace
                    // (`tab.open`, `palette.activate`), and the reconcile
                    // above — if it ran — predates that.
                    batch.mark_dirty();
                }
                // Same shape as the IPC arm above, for the same reason: a
                // menu command acts on `self.projects`, so a mutation
                // already in this batch is folded in before it runs, and
                // whatever it mutates itself still owes a reconcile.
                #[cfg(target_os = "macos")]
                EngineFeed::Menu(event) => {
                    if batch.workspace_dirty() {
                        self.reconcile();
                        batch.mark_reconciled();
                    }
                    task = task.then(self.menu_event(event));
                    batch.mark_dirty();
                }
                // Host mirrors + lifecycle land in the connection set.
                // C6/C7 render off it; C4 only keeps it current, so with
                // zero hosts these arms never run.
                EngineFeed::HostWorkspace(host, event) => {
                    // Attribution before application, and it has to be
                    // here rather than only inside `apply_workspace`: the
                    // envelopes below reach surfaces no later purge can
                    // take back (a desktop banner, the clipboard), so a
                    // batch queued by a connection this app has since
                    // removed or replaced must fire nothing at all.
                    if !self.hosts.owns(host) {
                        tracing::debug!(
                            ?host,
                            "dropping an event batch from a dead connection incarnation"
                        );
                        continue;
                    }
                    // Effects ride the batch verbatim and are applied
                    // here, before the mirror folds the commit away.
                    // Lease-holder-only is structural: this stream only
                    // flows while our connection holds the lease — a
                    // displaced client's events connection is closed at
                    // takeover before the new holder can generate any.
                    if let crate::host_conn::HostWorkspaceEvent::Applied { events, .. } = &event {
                        task = task.then(self.apply_host_envelopes(host, events));
                    }
                    // The mirror moving leaves the view's copy of it
                    // behind — the tail reconcile is what rebuilds the
                    // host sections, their agent rows and the palette.
                    // `try_next` already marked the batch for it (a host
                    // mirror is workspace state), so a request later in
                    // this same batch reconciles before it reads.
                    self.hosts.apply_workspace(host, event);
                }
                EngineFeed::HostTab(key, frame) => {
                    self.apply_host_tab_frame(key, frame, &mut pty);
                }
                EngineFeed::HostState(host, state) => {
                    // The transition itself — attribution, the drop's
                    // probe cancel and the offer it may raise — is
                    // `host_lifecycle::settle_host_state`'s; what is left
                    // here is the window and the workspace it is spent
                    // on.
                    if let Some(edge) = host_lifecycle::settle_host_state(
                        &mut self.hosts,
                        &mut self.bootstraps,
                        host,
                        state,
                    ) {
                        let host = &edge.host;
                        // Stamp the registry the moment a connection
                        // settles — `last_connected` is what the Add Host
                        // list and a `roostctl host list` read to tell a
                        // host that has ever worked from one that never
                        // has, and nothing else writes it.
                        if edge.connected {
                            if let Err(error) = self.workspace.touch_host_connected(host) {
                                tracing::debug!(%host, %error, "could not stamp last_connected");
                            }
                            // A session that just came up believes it is
                            // focused on its own restored tab, and the
                            // connect task cannot know better — the
                            // selection is the UI's. Told here, on the
                            // edge where the lease exists and the queue
                            // is draining.
                            self.push_host_focus();
                        }
                        // Attributed (not a stale task's publication): the
                        // app-side purge follows the set's — everything
                        // keyed by the dead incarnation is re-derived from
                        // the fresh mirror (plan 037 §3.2).
                        if let Some(previous) = edge.previous {
                            self.purge_host_incarnation(previous);
                        }
                        tracing::debug!(host = %edge.host, "host connection state changed");
                        if let Some(offer) = edge.offer {
                            self.start_bootstrap_probe(&edge.host, offer);
                        }
                    }
                    // The band's dot and rollup are cached with the rows;
                    // a state change moves both, and `try_next` marked
                    // the batch for the reconcile that rebuilds them.
                }
                // Nothing renders off it — it is kept for the outage a
                // later drop opens (plan 040 §3.7).
                EngineFeed::HostLease(host, lease) => self.hosts.apply_lease(host, lease),
                EngineFeed::ReconnectDue { host, request } => {
                    self.host_reconnect_due(&host, request)
                }
                EngineFeed::HostTunnel(ready) => self.host_tunnel_ready(*ready),
                EngineFeed::HostBootstrap(event) => self.host_bootstrap_event(*event),
                // A signal reached the process (plan 039 §3.9). Same
                // latch the macOS menu's Quit item uses — `take_exit_task`
                // (called every `update()`) is what turns this into
                // `UiTask::Exit` on the next message, which is guaranteed
                // to be soon: the send that put this item here also woke
                // the subscription that delivers it.
                EngineFeed::Quit => {
                    tracing::info!("signal requested a graceful quit");
                    self.exit_state.request();
                }
                EngineFeed::AgentHooks(result) => self.agent_hooks_ensured(result),
                EngineFeed::AgentMetrics(result) => self.apply_agent_metrics(result),
                EngineFeed::Provider(result) => self.apply_provider_result(*result),
                EngineFeed::NotificationActivated { tab } => {
                    if !tab.is_local() {
                        // A host tab's jump: select it and attach, the
                        // same pair its sidebar row does. A tab whose
                        // host has since dropped selects nothing — the
                        // banner outlived what it named.
                        match self.focus_host_tab_and_clear(tab, true) {
                            Ok(()) => {
                                batch.mark_reconciled();
                                if let Some(window) = self.window_id {
                                    task = task.then(UiTask::Focus(window));
                                }
                            }
                            Err(error) => tracing::debug!(
                                %tab,
                                %error,
                                "notification click named a host tab that is gone"
                            ),
                        }
                    } else if let Some(raise) =
                        notification_activation(&self.workspace, self.window_id, tab)
                    {
                        // The rest of a notification jump, exactly as the
                        // palette's rows do it: reveal the sidebar so the
                        // user sees which project they landed in, and fold
                        // the two ops above into the cache now rather than
                        // waiting for their broadcast to come back around.
                        self.set_sidebar_collapsed(false);
                        self.reconcile();
                        batch.mark_reconciled();
                        task = task.then(raise);
                    }
                }
            }
        }
        if let Some(error) = pty.error {
            self.set_status(error);
        }
        // OSC actions first: `OscAction::PointerShape` mutates the tab, so
        // a refresh that ran before them would publish the shape the batch
        // just replaced and leave the new one waiting for whatever
        // unrelated event refreshes the tab next.
        //
        // A mid-drain reconcile can have dropped a tab these actions were
        // collected for; every arm already resolves the tab through the
        // map (or hands the id to the engine, which answers `NotFound`),
        // so a vanished tab is a no-op rather than a panic. The
        // touched-tab refresh below and the exit list further down are
        // guarded the same way.
        for (key, actions) in pty.osc_actions {
            task = task.then(self.apply_osc_actions(key, actions));
        }
        for key in &pty.touched {
            if let Some(tab) = self.tabs.get_mut(key) {
                refresh_or_warn(key.tab, tab, "PTY output");
            }
        }
        if batch.should_reconcile() {
            self.reconcile();
        }
        for key in pty.exited {
            // The exit closes the tab in the backend that owns it. The
            // local workspace owns only the local id-space, so a key from
            // any other instance must not reach `close_tab` — its number
            // would close whichever local tab happens to share it. HS-2
            // routes those to their own host's client; until then the
            // drop below is the whole of it.
            match key.local_tab() {
                Some(tab_id) => {
                    let _ = self.workspace.close_tab(tab_id);
                }
                None => tracing::debug!(?key, "PTY exit for a tab on another instance"),
            }
            self.tabs.remove(&key);
        }
        // Dead last, and after every other `set_status` this drain can
        // reach: the agent-hooks toast is the one status line whose
        // being *seen* is then written to disk (`noticed`), so it may
        // not be quietly overwritten by, say, a PTY error that landed in
        // the same batch (plan 046 §3.3).
        self.show_agent_hooks_toast();
        // Idle ticks would otherwise bury every informative record under
        // ~60 empty ones a second.
        if !batch.is_empty() {
            tracing::trace!(
                items = batch.items,
                workspace_events = batch.workspace_events,
                non_tab_bytes = batch.non_tab_bytes,
                capped = batch.capped,
                "engine feed batch"
            );
        }
        task
    }

    fn apply_workspace_event(&mut self, event: WorkspaceEvent) {
        match event {
            WorkspaceEvent::NotificationFired {
                tab_id,
                title,
                body,
            } => {
                // The workspace broadcast is one backend's id-space.
                let tab = self.backend.tab_key(tab_id);
                self.fire_notification(tab, title, body);
            }
            WorkspaceEvent::TabNotification {
                tab_id,
                has_pending: false,
            } => {
                // A clear keeps the tab's server id: the banner may still
                // be on the desktop, and a constant per-tab id (as the
                // now-removed GTK UI used too) makes a later re-notify
                // replace it — forgetting the id here would stack a
                // duplicate beside it instead.
                self.notification_inbox.remove(self.backend.tab_key(tab_id));
            }
            WorkspaceEvent::TabClosed { tab_id } => {
                let tab = self.backend.tab_key(tab_id);
                self.notification_inbox.remove(tab);
                self.desktop_notifications.retire(tab);
            }
            // The local workspace asserted its selection, so the host
            // override is over. Reconcile's `local_active` watch catches
            // the cases where the id moved; this catches the one where it
            // did not — an IPC `tab.focus` of the tab that is already
            // active is still a focus intent, and it must win the window
            // back from the host row (`Workspace::focus_tab` emits this
            // unconditionally, which is what makes it a reliable seam).
            WorkspaceEvent::ActiveChanged { .. } => self.set_host_selection(None),
            WorkspaceEvent::ProjectDeleted { project_id } => {
                self.retire_project_notifications(ProjectKey::new(self.backend.host(), project_id));
            }
            // The bridge turns a lagged broadcast into a full-snapshot
            // resync; the batch's reconcile is the recovery, so there is
            // nothing incremental left to apply here. Event-carried
            // notification bodies are still the only casualty of lag —
            // reconcile_notification_inbox rebuilds the rows themselves.
            WorkspaceEvent::Resync(_) => {}
            _ => {}
        }
    }

    /// A project is gone: sweep the inbox rows that named it.
    ///
    /// Deliberately inbox-only, and shared by the local and host paths so
    /// they cannot drift. The desktop banners are already retired by the
    /// `tab.closed` events that precede a delete on both sides (the
    /// engine commits `TabClosed*` then `ProjectDeleted`,
    /// `workspace.rs:952`); this is the sweep for rows whose tab event
    /// was missed.
    fn retire_project_notifications(&mut self, project: ProjectKey) {
        let stale: Vec<TabKey> = self
            .notification_inbox
            .snapshot()
            .iter()
            .filter(|record| record.project == project)
            .map(|record| record.tab)
            .collect();
        for tab in stale {
            self.notification_inbox.remove(tab);
            self.host_bells.remove(&tab);
        }
    }

    fn notification_title(&self, key: TabKey) -> Option<(ProjectKey, String)> {
        let tab_id = key.local_tab()?;
        let host = self.backend.host();
        self.workspace.snapshot().into_iter().find_map(|project| {
            let project_key = ProjectKey::new(host, project.id);
            let project_name = project.name;
            project.tabs.into_iter().find_map(|tab| {
                (tab.id == tab_id).then(|| {
                    (
                        project_key,
                        notification_inbox::compose_title(
                            &project_name,
                            &notification_inbox::tab_title(&tab.title, &tab.cwd),
                        ),
                    )
                })
            })
        })
    }

    /// Rebuild the inbox from the authoritative rows of **every**
    /// id-space the window can currently see: the local backend's
    /// snapshot, plus each host section's mirror.
    ///
    /// The host half is what makes a reconnect restore that host's
    /// attention rows — the fresh `tab.list` already says which tabs are
    /// pending, and §4 forbids replaying the `notification.fired` that
    /// set them — and it is what makes a `notification.fired` whose tab
    /// the mirror had not listed yet heal itself on the next pass rather
    /// than being lost.
    fn reconcile_notification_inbox(&mut self) {
        let host = self.backend.host();
        let mut pending_rows: Vec<(TabKey, ProjectKey, String)> =
            pending_notification_rows(host, &self.projects, &self.host_bells);
        for view in &self.host_views {
            // A saved host that has never published rows carries
            // `HostId::LOCAL` as its placeholder (`refresh_host_views`);
            // deriving from it would mean minting rows in the LOCAL
            // id-space out of an empty project list.
            if view.host.is_local() {
                continue;
            }
            pending_rows.extend(pending_notification_rows(
                view.host,
                &view.projects,
                &self.host_bells,
            ));
        }
        let pending: HashSet<TabKey> = pending_rows.iter().map(|row| row.0).collect();
        // Unscoped, because the derivation above now covers every
        // id-space that has rows to show: a key absent from it is either
        // a tab that stopped being pending, or one keyed at a connection
        // incarnation no section names any more — a reconnect's dead
        // epoch, which nothing will ever list, clear or render again.
        // Both are gone, and §3.2's purge-then-rebuild is exactly the
        // second case. A *dropped* host is not among them: its section
        // keeps its last mirror (that is what the dimmed rows are drawn
        // from), so its pending tabs are still derived here.
        let stale: Vec<TabKey> = self
            .notification_inbox
            .tab_keys()
            .into_iter()
            .filter(|tab| !pending.contains(tab))
            .collect();
        for tab in stale {
            self.notification_inbox.remove(tab);
        }

        let existing: HashSet<TabKey> = self.notification_inbox.tab_keys().into_iter().collect();
        let slots = notification_inbox::CAP.saturating_sub(self.notification_inbox.count());
        let mut additions = Vec::with_capacity(slots);
        for row in pending_rows
            .into_iter()
            .filter(|row| !existing.contains(&row.0))
            .take(slots)
        {
            additions.push(row);
        }
        // Insert in reverse snapshot order so the first deterministic
        // project/tab fallback remains at the front after repeated prepends.
        while let Some((tab, project, title)) = additions.pop() {
            self.notification_inbox
                .upsert(notification_inbox::NotificationRecord::new(
                    tab, project, title, "",
                ));
        }
    }

    /// Mirror the notification-inbox count onto the macOS Dock tile —
    /// the parity port of `mac/Sources/Roost/App.swift`'s
    /// `refreshDockBadge()`. A no-op on every other host.
    ///
    /// Both callers (this reconcile and `window_opened`) run in the iced
    /// update loop, which is the main thread — the seam's
    /// `MainThreadMarker` acquisition is what enforces that, per
    /// CLAUDE.md's threading table. Nothing off the update loop may call
    /// this.
    pub(super) fn sync_dock_badge(&self) {
        #[cfg(target_os = "macos")]
        {
            // Bootstrap's initial reconcile() runs before iced constructs
            // the winit event loop, and winit documents
            // NSApplication::sharedApplication before EventLoop::new as
            // unsupported — so no AppKit until the window exists. The
            // window_opened initial sync covers boot.
            if self.window_id.is_none() {
                return;
            }
            crate::macos::dock_badge::sync(self.notification_inbox.count());
        }
    }

    /// Install the native menu bar — the parity port of `App.swift`'s
    /// `installMainMenu()`. A no-op on every other host.
    ///
    /// Called from `window_opened` for the same reason the Dock badge is:
    /// AppKit before winit has built the event loop is unsupported, and a
    /// focus-regain re-entry is idempotent (the seam installs once).
    pub(super) fn install_main_menu(&mut self) {
        #[cfg(target_os = "macos")]
        {
            if self.window_id.is_none() {
                return;
            }
            let Some(mtm) = seam_on_main("menu install") else {
                return;
            };
            let built = crate::macos::menu::install(
                mtm,
                self.title_fallback,
                &self.keybindings,
                self.feed_tx.clone(),
            );
            if built {
                // A freshly built menu is all-enabled. Reset the cache to
                // match so `sync_menu_gating` — which runs later in this
                // same update turn — pushes whatever route the app is
                // actually in (an IPC `palette.present` can beat the first
                // window).
                self.menu_gating = crate::macos::menu::MenuGating::default();
                // Ditto for the Window rows: the menu was built with none.
                self.menu_window_rows = crate::macos::menu::WindowRows::default();
            }
        }
    }

    /// Load Sparkle and start its updater — the parity port of
    /// `App.swift`'s `SPUStandardUpdaterController` init. A no-op on
    /// every other host, and (on macOS) a no-op after the first call.
    ///
    /// Called from `window_opened` after the menu install, for the
    /// menu's own reason plus one of Sparkle's: its standard user driver
    /// is an AppKit consumer, so it may not exist before winit has built
    /// the event loop.
    pub(super) fn init_sparkle(&mut self) {
        #[cfg(target_os = "macos")]
        {
            if self.window_id.is_none() {
                return;
            }
            let Some(mtm) = seam_on_main("sparkle init") else {
                return;
            };
            crate::macos::sparkle::init(mtm, self.test_mode);
        }
    }

    /// Install the `UNUserNotificationCenter` delegate and request
    /// authorization — the parity port of `DesktopNotifications.swift`'s
    /// launch-time setup. A no-op on every other host, and (on macOS) a
    /// no-op after the first call.
    ///
    /// Called from `window_opened` rather than at boot: the delegate is
    /// retained in a main-thread `thread_local!`, and the seam's own
    /// convention is that the native surfaces come up once a window
    /// exists.
    pub(super) fn init_notifications(&mut self) {
        #[cfg(target_os = "macos")]
        {
            if self.window_id.is_none() {
                return;
            }
            let Some(mtm) = seam_on_main("notifications init") else {
                return;
            };
            crate::macos::notifications::init(mtm);
        }
    }

    /// Re-read the system's notification authorization.
    ///
    /// Called from `set_window_focus` on the focus-*gain* edge only —
    /// deliberately not from `window_opened`, which also runs on focus
    /// loss and would fire the refresh, and its conditional re-request,
    /// at the instant the system's own prompt steals focus.
    pub(super) fn refresh_notification_authorization(&mut self) {
        #[cfg(target_os = "macos")]
        {
            let Some(mtm) = seam_on_main("notifications refresh") else {
                return;
            };
            crate::macos::notifications::refresh_authorization(mtm);
        }
    }

    /// Push `SPUUpdater.canCheckForUpdates` onto the "Check for
    /// Updates…" item, when it moved.
    ///
    /// Its own axis rather than a field of `MenuGating`: the item is
    /// ungated by the keyboard route (§ 3.8), and what moves it is the
    /// updater — boot, and the start/end of every check. Hence the call
    /// sites: `window_opened` (boot), `sync_menu_gating` (the
    /// route-change funnel, which every update turn passes through, so
    /// it is also where a check that finished in the background lands),
    /// and both of the two calls that START a check.
    pub(super) fn sync_update_menu_item(&mut self) {
        #[cfg(target_os = "macos")]
        {
            if self.window_id.is_none() {
                return;
            }
            let Some(mtm) = seam_on_main("update-item sync") else {
                return;
            };
            let can_check = crate::macos::sparkle::can_check(mtm);
            if self.menu_can_check_updates == Some(can_check) {
                return;
            }
            self.menu_can_check_updates = Some(can_check);
            crate::macos::menu::sync_update_item(mtm, can_check);
        }
    }

    /// Rebuild the Window menu's project/tab rows when the rows moved.
    ///
    /// Reconcile is where this hangs (the Dock badge's reasoning), and
    /// reconcile itself only runs when the engine batch is dirty (a real
    /// workspace/UI-request change, not every PTY byte batch — the 16ms
    /// tick that used to make it per-drain is gone). Even so, `derive`
    /// clones every project name and formats "Tab N" for every tab, so the
    /// allocation-free `WindowRows::matches` check runs FIRST; `derive` (and
    /// the AppKit rebuild behind it) only run on an actual mismatch.
    pub(super) fn sync_window_menu(&mut self) {
        #[cfg(target_os = "macos")]
        {
            if self.window_id.is_none() {
                return;
            }
            let host = self.backend.host();
            let active_project = self.active_project_key();
            let active_tab = self.active_tab_key();
            if self
                .menu_window_rows
                .matches(&self.projects, host, active_project, active_tab)
            {
                return;
            }
            let rows = crate::macos::menu::WindowRows::derive(
                &self.projects,
                host,
                active_project,
                active_tab,
            );
            let Some(mtm) = seam_on_main("window-menu rebuild") else {
                return;
            };
            crate::macos::menu::sync_window_menu(mtm, &rows, &self.keybindings, self.menu_gating());
            self.menu_window_rows = rows;
        }
    }

    /// Push the current keyboard route onto the menu bar, when it moved.
    ///
    /// The one call site is `update()`'s post-dispatch drain, so every
    /// route transition — palette open/close, rename begin/commit, confirm
    /// modal, IME composition start/end — is covered without a call site
    /// per transition (plan 028 § 3.5).
    pub fn sync_menu_gating(&mut self) {
        #[cfg(target_os = "macos")]
        {
            self.push_menu_gating();
            // Not inside `push_menu_gating`'s early returns: a route that
            // did NOT move can still coincide with a check that finished,
            // and this funnel is the one place every update turn passes
            // through.
            self.sync_update_menu_item();
        }
    }

    #[cfg(target_os = "macos")]
    fn push_menu_gating(&mut self) {
        let gating = self.menu_gating();
        if self.menu_gating == gating {
            return;
        }
        let Some(mtm) = seam_on_main("menu gating") else {
            return;
        };
        self.menu_gating = gating;
        crate::macos::menu::sync_gating(gating, mtm);
    }

    /// Rebuild the sidebar's host sections from the connection set.
    ///
    /// Every saved host gets a section whether or not it is connected —
    /// a disconnected one lists the rows its last mirror published, and
    /// one that has never connected lists none. With no saved hosts this
    /// clears to empty and the sidebar keeps exactly today's chrome.
    pub(super) fn refresh_host_views(&mut self) {
        self.host_views = self
            .workspace
            .hosts()
            .into_iter()
            .map(|host| {
                let (state, incarnation, mirror) = match self.hosts.section(&host.id) {
                    Some(section) => (
                        section.state.section_state(),
                        section.incarnation,
                        section.mirror.map(|mirror| mirror.read()),
                    ),
                    // Not being driven at all reads as disconnected —
                    // the section is listed with a ↻, which is the whole
                    // of the "no daemon is spawned silently" rule on
                    // screen — unless an ssh establish is in flight,
                    // which is `connecting` on the band exactly like a
                    // dial would be.
                    None if self.hosts.establishing(&host.id) => {
                        (host_sidebar::SectionState::Connecting, None, None)
                    }
                    None => (host_sidebar::SectionState::Disconnected, None, None),
                };
                // Taken before `host.id` is moved into the view.
                let reason = self.hosts.section_reason(&host.id).map(str::to_string);
                super::HostView {
                    saved_id: host.id,
                    // The registry's label wins over the connection's:
                    // they are the same string, and the registry is the
                    // one that exists before a connection does.
                    label: host.label,
                    // Same reason the label comes from here: the verb
                    // policy has to know whether a host is this
                    // machine's own *before* anything connects to it.
                    // The classifier's own rule rather than a raw `==`:
                    // the sentinel is trimmed before it is matched, so a
                    // saved `" localhost"` is the local session
                    // everywhere or nowhere.
                    localhost: roost_ipc::ssh::target_is_localhost(&host.target),
                    reason,
                    host: incarnation.unwrap_or(HostId::LOCAL),
                    state,
                    projects: mirror
                        .as_ref()
                        .map(|mirror| mirror.projects.clone())
                        .unwrap_or_default(),
                    active_tab_id: mirror.as_ref().map_or(0, |mirror| mirror.active_tab_id),
                    agents: 0,
                }
            })
            .collect();
    }

    fn refresh_sidebar_agents(&mut self) {
        let host = self.backend.host();
        let now = agent_palette::now_unix();
        let mut rows: HashMap<ProjectKey, Vec<agent_palette::SidebarAgentRow>> = self
            .projects
            .iter()
            .map(|project| {
                (
                    ProjectKey::new(host, project.id),
                    agent_palette::sidebar_agents(project, host, now),
                )
            })
            .collect();
        // Host sections carry the same rows under the same keys — the
        // ⌘⇧A toggle and the row widget are host-blind by construction
        // (plan 037 §3.1). A disconnected host's rows are still built:
        // they render dimmed rather than disappearing, and the count they
        // feed is what its band would say if the state left the rollup
        // slot free.
        for view in &mut self.host_views {
            let mut agents = 0;
            for project in &view.projects {
                let project_rows = agent_palette::sidebar_agents(project, view.host, now);
                agents += project_rows.len();
                rows.insert(ProjectKey::new(view.host, project.id), project_rows);
            }
            view.agents = agents;
        }
        self.sidebar_agents = rows;
        // Last, because the rollups read the counts filled just above.
        self.host_sections = host_sidebar::sections(
            &self
                .host_views
                .iter()
                .map(|view| host_sidebar::HostInput {
                    saved_id: view.saved_id.as_str(),
                    label: view.label.as_str(),
                    host: view.host,
                    state: view.state,
                    agents: view.agents,
                    reason: view.reason.as_deref(),
                })
                .collect::<Vec<_>>(),
        );
    }

    fn sidebar_dump(&self) -> SidebarDumpResult {
        let host = self.backend.host();
        let active_tab = self.active_tab_key();
        let projects = self
            .workspace
            .snapshot()
            .into_iter()
            .map(|project| SidebarDumpProject {
                project_id: project.id,
                agents: self
                    .sidebar_agents
                    .get(&ProjectKey::new(host, project.id))
                    .into_iter()
                    .flatten()
                    // The dump is a wire result, so the keys narrow back
                    // to the bare ids a client speaks.
                    .map(|row| SidebarDumpAgentRow {
                        tab_id: row.tab.tab,
                        name: row.name.clone(),
                        lifecycle: row.lifecycle,
                        status_text: row.status_text.clone(),
                        time_text: row.time_text.clone(),
                        is_active: row.tab == active_tab,
                    })
                    .collect(),
            })
            .collect();
        SidebarDumpResult {
            agents_visible: self.config.show_sidebar_agents,
            projects,
            hosts: self.host_views.iter().map(host_dump).collect(),
        }
    }

    pub(super) fn apply_metrics_cache(
        &self,
        cwds: &HashMap<TabKey, String>,
        items: &mut [palette::PaletteItem],
    ) {
        for item in items {
            let Some(tab) = agent_palette::agent_tab_key(&item.id) else {
                continue;
            };
            let (Some(agent), Some(cwd)) = (item.agent.as_mut(), cwds.get(&tab)) else {
                continue;
            };
            agent.metrics_text = self
                .metrics_cache
                .text_for_session(self.palette_session, cwd)
                .map(str::to_string);
        }
    }

    pub(super) fn spawn_agent_metrics(&mut self, cwds: &HashMap<TabKey, String>) {
        self.metrics_cache.begin_session(self.palette_session);
        let claimed = self.metrics_cache.claim_unprobed(cwds.values().cloned());
        if claimed.is_empty() {
            return;
        }
        let known = self.metrics_cache.known_roots();
        let probe = Arc::clone(&self.git_probe);
        let feed = self.feed_tx.clone();
        let session = self.palette_session;
        let failed_claims = claimed.clone();
        let task = self
            .runtime
            .spawn(git_metrics::probe_batch(probe, claimed, known));
        self.runtime.spawn(async move {
            let outcomes = task.await.map_err(|error| error.to_string());
            feed.send(EngineFeed::AgentMetrics(AgentMetricsResult {
                session,
                claimed: failed_claims,
                outcomes,
            }));
        });
    }

    fn apply_agent_metrics(&mut self, result: AgentMetricsResult) {
        if self.palette.is_none()
            || result.session != self.palette_session
            || self.metrics_cache.session() != result.session
        {
            return;
        }
        match result.outcomes {
            Ok(outcomes) => {
                for outcome in outcomes {
                    let Some(root) = outcome.root.clone() else {
                        if let git_metrics::ProbeValue::Measured(Err(error)) = &outcome.value {
                            tracing::debug!(cwd = %outcome.cwd, reason = %error, "no git metrics");
                        }
                        self.metrics_cache.store_unresolved(&outcome.cwd);
                        continue;
                    };
                    let text = match outcome.value {
                        git_metrics::ProbeValue::Reused(text) => text,
                        git_metrics::ProbeValue::Measured(Ok(metrics)) => metrics.text(),
                        git_metrics::ProbeValue::Measured(Err(error)) => {
                            tracing::debug!(cwd = %outcome.cwd, reason = %error, "no git metrics");
                            git_metrics::UNKNOWN.to_string()
                        }
                    };
                    self.metrics_cache.store_root(&outcome.cwd, &root, text);
                }
            }
            Err(error) => {
                tracing::warn!(%error, "git metrics task failed");
                for cwd in result.claimed {
                    self.metrics_cache.store_unresolved(&cwd);
                }
            }
        }
    }

    /// The IPC wire is host-unaware by pin, so every `tab_id` below is
    /// one of the local backend's bare ids — `backend.tab_key` is the
    /// joint that qualifies them, exactly as the workspace's active
    /// selection is qualified.
    fn apply_ui_request(&mut self, request: UiRequest) -> UiTask {
        let mut task = UiTask::None;
        match request {
            UiRequest::Activate => {
                if let Some(id) = self.window_id {
                    task = task.then(UiTask::Focus(id));
                }
            }
            UiRequest::Dump { tab_id, reply } => {
                let result = self
                    .tabs
                    .get(&self.wire_tab_key(tab_id))
                    .map(TerminalTab::dump)
                    .ok_or_else(|| format!("tab {tab_id} has no live terminal"));
                let _ = reply.send(result);
            }
            UiRequest::TabFeedPtyBytes {
                tab_id,
                data,
                reply,
            } => {
                let key = self.backend.tab_key(tab_id);
                // Same ordering as the feed batch's tail: an OSC action can
                // mutate the tab (pointer shape), so it lands before the
                // refresh that publishes it, never after. That second
                // lookup is the price of handing `self` to `apply_osc_actions`.
                let result = if !self.test_mode {
                    Err("ROOST_TEST_MODE=1 is required".to_string())
                } else if let Some(actions) = self
                    .tabs
                    .get_mut(&key)
                    .map(|tab| tab.scan_and_write_vt(&data))
                {
                    task = task.then(self.apply_osc_actions(key, actions));
                    self.tabs
                        .get_mut(&key)
                        .ok_or_else(|| format!("tab {tab_id} has no live terminal"))
                        .and_then(|tab| tab.refresh_snapshot().map_err(|error| error.to_string()))
                } else {
                    Err(format!("tab {tab_id} has no live terminal"))
                };
                let _ = reply.send(result);
            }
            UiRequest::TabCapturePtyInput {
                tab_id,
                drain,
                reply,
            } => {
                let result = self
                    .tabs
                    .get(&self.wire_tab_key(tab_id))
                    .and_then(|tab| tab.session.capture())
                    .ok_or_else(|| "ROOST_TEST_MODE=1 is required or tab is missing".to_string())
                    .and_then(|capture| {
                        capture
                            .lock()
                            .map(|mut bytes| {
                                if drain {
                                    std::mem::take(&mut *bytes)
                                } else {
                                    bytes.clone()
                                }
                            })
                            .map_err(|_| "PTY input capture lock poisoned".to_string())
                    });
                let _ = reply.send(result);
            }
            UiRequest::TabFeedIme {
                tab_id,
                action,
                text,
                cursor,
                reply,
            } => {
                let result = if !self.test_mode {
                    Err("ROOST_TEST_MODE=1 is required".to_string())
                } else {
                    match self.keyboard_route() {
                        // `tab_id` arrives off the wire, which is bare by
                        // pin; the route it is checked against carries the
                        // key, so both halves have to agree.
                        KeyboardRoute::Terminal(active)
                            if active == self.backend.tab_key(tab_id) =>
                        {
                            match action.as_str() {
                                "preedit" => {
                                    self.ime_preedit(text, cursor);
                                    Ok(())
                                }
                                "commit" => {
                                    self.ime_commit(&text);
                                    Ok(())
                                }
                                "clear" => {
                                    self.ime_session_boundary();
                                    Ok(())
                                }
                                other => Err(format!("unknown tab.feed_ime action: {other}")),
                            }
                        }
                        KeyboardRoute::Terminal(active) => Err(format!(
                            "tab {tab_id} is not the active terminal \
                                 (keyboard route owns tab {})",
                            active.tab
                        )),
                        KeyboardRoute::None
                        | KeyboardRoute::Confirm
                        | KeyboardRoute::HostDialog
                        | KeyboardRoute::Editor
                        | KeyboardRoute::Palette => Err(format!(
                            "tab {tab_id} is not the active terminal \
                                 (keyboard route is not a terminal)"
                        )),
                    }
                };
                let _ = reply.send(result);
            }
            UiRequest::TabDumpResolved { tab_id, reply } => {
                let result = self
                    .tabs
                    .get(&self.wire_tab_key(tab_id))
                    .map(TerminalTab::resolved_cells)
                    .ok_or_else(|| format!("tab {tab_id} has no live terminal"));
                let _ = reply.send(result);
            }
            UiRequest::AppRenderStats { reset, reply } => {
                let stats = crate::perf::snapshot();
                if reset {
                    crate::perf::reset();
                }
                let _ = reply.send(Ok(AppRenderStatsResult {
                    refresh_calls: stats.refresh_calls as i64,
                    refresh_nanos: stats.refresh_nanos as i64,
                    rows_rebuilt: stats.rows_rebuilt as i64,
                    cells_walked: stats.cells_walked as i64,
                    draw_calls: stats.draw_calls as i64,
                    draw_nanos: stats.draw_nanos as i64,
                    fill_text_calls: stats.fill_text_calls as i64,
                    view_calls: stats.view_calls as i64,
                    view_nanos: stats.view_nanos as i64,
                    elide_calls: stats.elide_calls as i64,
                    elide_nanos: stats.elide_nanos as i64,
                }));
            }
            UiRequest::WindowMetrics { reply } => {
                let collapsed = self.workspace.sidebar_collapsed();
                let resolved_family = self
                    .font_registry
                    .resolve(self.typography.effective_family())
                    .name
                    .to_string();
                let _ = reply.send(Ok(WindowMetricsResult {
                    window_width: f64::from(self.window_size.width),
                    window_height: f64::from(self.window_size.height),
                    sidebar_width: f64::from(self.effective_sidebar_width()),
                    sidebar_collapsed: collapsed,
                    terminal_top: Some(f64::from(chrome::BAND_HEIGHT)),
                    terminal_font_family: Some(resolved_family),
                }));
            }
            UiRequest::WindowResize {
                width,
                height,
                reply,
            } => {
                let result = if !self.test_mode {
                    Err("ROOST_TEST_MODE=1 is required".into())
                } else {
                    let size = Size::new(width as f32, height as f32);
                    // Some Wayland compositors retain authority over the
                    // toplevel size and may ignore a client request. Apply
                    // the requested logical geometry immediately for the
                    // deterministic test port; a compositor Resized event
                    // remains authoritative if it sends one afterward.
                    self.resize(size);
                    if let Some(id) = self.window_id {
                        task = task.then(UiTask::Resize(id, size));
                    } else {
                        // IPC can become reachable just before Iced emits
                        // WindowOpened. Preserve the native resize until an
                        // ID exists instead of rejecting a ready server.
                        self.pending_window_resize = Some(size);
                    }
                    Ok(())
                };
                let _ = reply.send(result);
            }
            UiRequest::SidebarSetWidth { width, reply } => {
                let result = if !self.test_mode {
                    Err("ROOST_TEST_MODE=1 is required".into())
                } else {
                    // A drag overlay still in flight would shadow the width
                    // the op just set — commit it first so the op's value is
                    // the one the layout and the next relaunch both see.
                    self.commit_sidebar_drag();
                    self.workspace.set_sidebar_width(width);
                    self.resize(self.window_size);
                    Ok(())
                };
                let _ = reply.send(result);
            }
            UiRequest::AppSetWindowFocus { focused, reply } => {
                let result = if self.test_mode {
                    self.set_window_focus(focused);
                    Ok(())
                } else {
                    Err("ROOST_TEST_MODE=1 is required".into())
                };
                let _ = reply.send(result);
            }
            UiRequest::AppCursorShape { reply } => {
                let shape = self
                    .tabs
                    .get(&self.active_tab_key())
                    .map_or("default", TerminalTab::effective_pointer_shape);
                let _ = reply.send(Ok(shape.into()));
            }
            UiRequest::AppActiveTerminalFocused { reply } => {
                let focused = matches!(self.keyboard_route(), KeyboardRoute::Terminal(_));
                let _ = reply.send(Ok(focused));
            }
            UiRequest::AppSelectedTabId { reply } => {
                let _ = reply.send(Ok(self.workspace.active().1));
            }
            UiRequest::AppDockBadge { reply } => {
                // Reads AppKit, deliberately without re-deriving the
                // label from the inbox first: the op exists to prove the
                // badge write reached the Dock, and a resync here would
                // make it prove only the mapping.
                let result = macos_test_gated(self.test_mode, read_dock_badge);
                let _ = reply.send(result);
            }
            UiRequest::AppMenuDump { reply } => {
                let result = macos_test_gated(self.test_mode, read_menu_dump);
                let _ = reply.send(result);
            }
            UiRequest::AppMenuActivate { path, reply } => {
                let result = macos_test_gated(self.test_mode, || activate_menu(&path));
                let _ = reply.send(result);
            }
            UiRequest::AppDialogDump { reply } => {
                let result = if self.test_mode {
                    Ok(self.dialog_dump())
                } else {
                    Err("ROOST_TEST_MODE=1 is required".into())
                };
                let _ = reply.send(result);
            }
            UiRequest::AppDialogAnswer { action, reply } => {
                let result = if self.test_mode {
                    self.dialog_answer(&action)
                } else {
                    Err("ROOST_TEST_MODE=1 is required".into())
                };
                // The button's own task, chained rather than replacing
                // whatever this drain already owed — Add Host's confirm
                // dispatches an engine op, and dropping it would leave
                // the dialog waiting on a dial nobody started.
                let _ = reply.send(match result {
                    Ok(button) => {
                        task = task.then(button);
                        Ok(())
                    }
                    Err(error) => Err(error),
                });
            }
            UiRequest::AppKeybindDispatch { action, reply } => {
                let result = if !self.test_mode {
                    Err("ROOST_TEST_MODE=1 is required".into())
                } else {
                    match paste_only_keybind(&action) {
                        Ok(keybind) => {
                            task = task.then(self.dispatch_keybind_action(keybind, false));
                            Ok(())
                        }
                        Err(error) => Err(error),
                    }
                };
                let _ = reply.send(result);
            }
            UiRequest::AppUpdateStatus { reply } => {
                let result = macos_test_gated(self.test_mode, read_update_status);
                let _ = reply.send(result);
            }
            UiRequest::AppUpdateCheck { reply } => {
                let result = macos_test_gated(self.test_mode, start_update_check);
                // A check that just started can flip
                // `canCheckForUpdates` off; push it so the menu item
                // greys out for the duration rather than at the next
                // unrelated reconcile.
                self.sync_update_menu_item();
                let _ = reply.send(result);
            }
            UiRequest::AppNotificationStatus { reply } => {
                let result = macos_test_gated(self.test_mode, read_notification_status);
                let _ = reply.send(result);
            }
            UiRequest::Screenshot { scale, reply } => {
                self.screenshots.enqueue(scale, reply);
            }
            UiRequest::PaletteOpen { kind, reply } => {
                let result = self
                    .open_palette(&kind)
                    .map(|()| self.palette_state_result());
                if result.is_ok() {
                    task = task.then(self.take_palette_focus_task());
                }
                let _ = reply.send(result);
            }
            UiRequest::PaletteState { reply } => {
                let _ = reply.send(Ok(self.palette_state_result()));
            }
            UiRequest::PaletteQuery { query, reply } => {
                let _ = reply.send(self.query_palette(&query));
            }
            UiRequest::PaletteActivate { id, reply } => {
                // The row is the one a click runs; only the origin says
                // that nobody is sitting in front of this one.
                let activation = self.activate_palette(&id, Self::IPC_ACTIVATION_ORIGIN);
                match activation.reply {
                    PaletteReplyRoute::Ready(result) => {
                        let _ = reply.send(result);
                    }
                    // The client stays blocked until this row's engine op
                    // reports back: `palette.activate` answers with what
                    // its action produced, and for these rows the action
                    // has not produced it yet.
                    PaletteReplyRoute::Deferred(op) => {
                        self.palette_activate_replies.insert(op, reply);
                    }
                }
                // The rename rows open the inline editor from here too,
                // and `Add Host…` opens its dialog — both owe a focus.
                task = task
                    .then(activation.task)
                    .then(self.take_rename_focus_task())
                    .then(self.take_add_host_focus_task());
            }
            UiRequest::PaletteDismiss { reply } => {
                let result = self
                    .try_dismiss_palette()
                    .map(|()| self.palette_state_result());
                let _ = reply.send(result);
            }
            UiRequest::PalettePresent {
                title,
                placeholder,
                items,
                reply,
            } => {
                self.present_palette(title, placeholder, items, reply);
                task = task.then(self.take_palette_focus_task());
            }
            UiRequest::SelectionSet {
                tab_id,
                anchor,
                cursor,
                reply,
            } => {
                let result = self
                    .tabs
                    .get_mut(&self.backend.tab_key(tab_id))
                    .ok_or_else(|| format!("tab {tab_id} has no live terminal"))
                    .and_then(|tab| {
                        let anchored = tab
                            .selection
                            .set(&tab.terminal, anchor, cursor)
                            .map_err(|error| error.to_string())?;
                        if !anchored {
                            return Err(format!(
                                "selection coordinates are outside tab {tab_id}'s viewport"
                            ));
                        }
                        tab.refresh_snapshot().map_err(|error| error.to_string())
                    });
                let _ = reply.send(result);
            }
            UiRequest::SelectionClear { tab_id, reply } => {
                let result = self
                    .tabs
                    .get_mut(&self.backend.tab_key(tab_id))
                    .ok_or_else(|| format!("tab {tab_id} has no live terminal"))
                    .and_then(|tab| {
                        tab.selection.clear();
                        tab.refresh_snapshot().map_err(|error| error.to_string())
                    });
                let _ = reply.send(result);
            }
            UiRequest::SelectionDump { tab_id, reply } => {
                let result = self
                    .tabs
                    .get_mut(&self.backend.tab_key(tab_id))
                    .ok_or_else(|| format!("tab {tab_id} has no live terminal"))
                    .and_then(|tab| tab.selection_dump().map_err(|error| error.to_string()));
                let _ = reply.send(result);
            }
            UiRequest::ClipboardDump { target, reply } => {
                self.clipboard.enqueue_ipc_read(target, reply);
                task = task.then(self.clipboard.start_next());
            }
            UiRequest::ClipboardWrite { target, text } => {
                self.clipboard.enqueue_write(target, text);
                task = task.then(self.clipboard.start_next());
            }
            UiRequest::TabExpandSelectionAt {
                tab_id,
                col,
                row,
                click_count,
                reply,
            } => {
                let result = if !self.test_mode {
                    Err("tab.expand_selection_at requires ROOST_TEST_MODE=1 at UI launch".into())
                } else {
                    self.tabs
                        .get_mut(&self.backend.tab_key(tab_id))
                        .ok_or_else(|| format!("tab {tab_id} has no live terminal"))
                        .and_then(|tab| {
                            let expanded = tab.expand_selection_at(col, row, click_count);
                            // The op commits the selection before it can
                            // fail extracting that selection's text, so the
                            // snapshot is republished either way: on
                            // success the reply must not describe a span
                            // the rendering does not show, and on failure
                            // the committed selection must not stay
                            // invisible.
                            refresh_or_warn(tab_id, tab, "expand selection");
                            expanded.map_err(|error| error.to_string())?.ok_or_else(|| {
                                format!(
                                    "no word/line span at ({col}, {row}) on tab {tab_id} \
                                         (whitespace double-click, or row out of range)"
                                )
                            })
                        })
                };
                let _ = reply.send(result);
            }
            UiRequest::SidebarDump { reply } => {
                // The host half of the dump reads the `host_views`
                // cache, which a reconcile refreshes — so without this
                // the answer can lag the mirror by one event, exactly
                // the staleness `host_status_op` refreshes for.
                //
                // Only half of that precedent is taken, deliberately.
                // (1) No `refresh_sidebar_agents()`: this op's whole
                // contract is that `projects[].agents` is the cache the
                // sidebar paints from, so a refresh a UI forgot to run
                // is wire-visible (plan 007 §3.8) — refreshing it here
                // would make the op self-healing and blind. Safe
                // because the count that pass recomputes
                // (`HostView::agents`) is written and read inside that
                // one function, so leaving it zeroed until the next
                // reconcile changes nothing anyone reads. (2) No
                // band/view pairing guard: `host_status_op` zips
                // `host_sections` with `host_views` and has to refuse
                // when a mid-list removal has mispaired them; this op
                // zips nothing — each section is built from one view —
                // so there is no pairing to be wrong.
                //
                // Honest about what this line is worth: it is a belt
                // against a same-batch race that no test exercises.
                // Deleting it leaves the lane green, because the tail
                // reconcile of the batch that carried the event has
                // already refreshed the cache by the time a follow-up
                // request is served (plan 044 §8 records it as a
                // known-unexercised path).
                self.refresh_host_views();
                let _ = reply.send(Ok(self.sidebar_dump()));
            }
            UiRequest::TabDispatchMouseEvent {
                tab_id,
                kind,
                button,
                cell_x,
                cell_y,
                mods,
                reply,
            } => {
                let result = if !self.test_mode {
                    Err("ROOST_TEST_MODE=1 is required".into())
                } else {
                    u16::try_from(mods)
                        .map_err(|_| format!("modifier mask {mods} exceeds u16"))
                        .and_then(|mods| {
                            self.tabs
                                .get_mut(&self.backend.tab_key(tab_id))
                                .ok_or_else(|| format!("tab {tab_id} has no live terminal"))?
                                .dispatch_pointer(kind, button, cell_x, cell_y, mods)
                                .map_err(|error| error.to_string())
                        })
                };
                let _ = reply.send(result);
            }
            // The host registry + connections, as ops (plan 037 §3.5).
            // Served here rather than in the engine so a `roostctl host`
            // verb reconciles the same surfaces the palette row does —
            // the sidebar section, the connection, the saved list — and
            // so every verb has exactly one implementation.
            UiRequest::HostAdd {
                label,
                target,
                reply,
            } => {
                // Registry-only, per `roostctl host add`'s documented
                // semantics; `host.connect` is the second step, and the
                // Add Host dialog's "Add & Connect" takes both.
                let _ = reply.send(
                    self.host_add_requested(&label, &target, None)
                        .map(Into::into),
                );
            }
            UiRequest::HostRemove { id, reply } => {
                let _ = reply.send(self.host_remove_requested(&id));
            }
            UiRequest::HostTabFocus {
                host,
                tab_id,
                reply,
            } => {
                let key = TabKey::new(HostId::new(host), tab_id);
                // The same two steps the sidebar click takes, in the
                // same order: the selection first (so the view and the
                // keyboard route move together), then the attach.
                let result = self
                    .focus_host_tab_and_clear(key, false)
                    .map_err(|_| roost_engine::WorkspaceError::TabNotFound(tab_id));
                let _ = reply.send(result);
            }
            UiRequest::HostTabReorder {
                host,
                project_id,
                tab_ids,
                reply,
            } => {
                self.host_reorder_op(
                    HostId::new(host),
                    ReorderTarget::Tabs { project_id },
                    &tab_ids,
                    reply,
                );
            }
            UiRequest::HostProjectReorder {
                host,
                project_ids,
                reply,
            } => {
                self.host_reorder_op(
                    HostId::new(host),
                    ReorderTarget::Projects,
                    &project_ids,
                    reply,
                );
            }
            UiRequest::HostConnect {
                id,
                test_user_origin,
                reply,
            } => {
                let _ = reply.send(self.host_connect_op(&id, test_user_origin));
            }
            UiRequest::HostDisconnect { id, reply } => {
                let _ = reply.send(self.host_disconnect_op(&id));
            }
            UiRequest::HostStatus { id, reply } => {
                let _ = reply.send(self.host_status_op(id.as_deref()));
            }
        }
        task
    }

    /// Who a connection arriving over the IPC socket is asked for by.
    ///
    /// Named rather than spelled at the call site so the rule it stands
    /// for — a modal never opens to answer a machine (plan 039 §3.5) —
    /// is something a test can hold on to. `roostctl host connect` dials
    /// exactly as a click does, so nothing downstream could infer this.
    pub(super) const IPC_CONNECT_ORIGIN: crate::host_conn::RequestOrigin =
        crate::host_conn::RequestOrigin::Ipc;

    /// `host.connect`, as the op answers it: start the attempt and
    /// report the state it left the host in.
    ///
    /// `connecting` rather than `connected` is the honest answer — the
    /// dial, the identify and the lease are a round trip this reply does
    /// not wait for, and a client that wants the settled verdict watches
    /// the section (or asks again).
    ///
    /// `test_user_origin` is `HostConnectParams::test_user_origin`,
    /// already gated in `roost-engine` on nothing — the test-mode check
    /// lives here, next to every other `self.test_mode` gate, so a
    /// production build ignores the flag outright rather than trusting
    /// a decode-time gate two crates away. When it is honored, the
    /// request routes through `host_connect_requested` — the same
    /// NeedsRestart-aware entry a click uses — instead of the plain
    /// dial `host.connect` otherwise gives a machine, which is the only
    /// way `tools/roosttest` can reach the bootstrap offer or the
    /// remote-restart prompt at all (plan 039 §3.5).
    fn host_connect_op(
        &mut self,
        saved_id: &str,
        test_user_origin: bool,
    ) -> Result<HostConnectionResult, roost_engine::WorkspaceError> {
        let host = self.saved_host(saved_id)?;
        if test_user_origin && self.test_mode {
            self.host_connect_requested(saved_id, crate::host_conn::RequestOrigin::User);
        } else {
            self.host_reconnect_requested(
                saved_id,
                Self::IPC_CONNECT_ORIGIN,
                crate::host_conn::AttemptCause::Explicit,
            );
        }
        Ok(self.host_connection_result(host))
    }

    /// The UI-socket form of a host reorder (plan 044 §3.1 d6): the same
    /// call the drop gesture dispatches, minus the preview.
    ///
    /// Alone among the `host.*` arms this one cannot answer inside
    /// `update` — the outcome is the session's, a round trip away — so
    /// the reply rides into the spawned future and is answered there.
    /// Dropping it instead would reach the caller as
    /// `internal: UI dropped reply`, which says nothing about a host.
    fn host_reorder_op(
        &self,
        host: HostId,
        target: ReorderTarget,
        ordered_ids: &[i64],
        reply: roost_engine::ipc::HostOpReply<()>,
    ) {
        let Some(call) = host_reorder_call(&self.hosts, host, target, ordered_ids) else {
            let _ = reply.send(Err(no_connected_host()));
            return;
        };
        self.runtime_handle.spawn(async move {
            let _ = reply.send(call.await.map_err(|error| host_op_failure(&error)));
        });
    }

    fn host_disconnect_op(
        &mut self,
        saved_id: &str,
    ) -> Result<HostConnectionResult, roost_engine::WorkspaceError> {
        let host = self.saved_host(saved_id)?;
        self.host_disconnect_requested(saved_id);
        Ok(self.host_connection_result(host))
    }

    /// `host.status`, as the op answers it: every saved host's band, or
    /// just the one named (plan 042 §3.1).
    ///
    /// One main-thread read, and it starts by rebuilding the two caches
    /// it reads. Every *feed* item that moves a connection is classified
    /// as workspace state (`engine_feed.rs`), so a request drained
    /// behind one already reconciled — but a mutating request drained
    /// *ahead* of this one in the same batch (a second client's
    /// `host.connect`) only marks the batch dirty for the tail, and
    /// reading the cache then would pair a fresh `generation` with a
    /// stale `state`. Both refreshes are idempotent recomputations, so
    /// running them here costs a reconcile's worth of work and buys one
    /// freshness for the whole reply.
    fn host_status_op(
        &mut self,
        id: Option<&str>,
    ) -> Result<HostStatusResult, roost_engine::WorkspaceError> {
        if let Some(id) = id {
            self.saved_host(id)?;
        }
        self.refresh_host_views();
        self.refresh_sidebar_agents();

        let saved = self.workspace.hosts();
        // `host_sections` is LOCAL plus one band per view, and empty
        // when there are no saved hosts at all (the zero-host sidebar is
        // byte-identical to the pre-host-sessions one).
        let paired = saved.len() == self.host_views.len()
            && (self.host_views.is_empty()
                || self.host_sections.len() == self.host_views.len() + 1);
        if !paired {
            return Err(roost_engine::WorkspaceError::Inconsistent(format!(
                "the sidebar has {} bands and {} views for {} saved hosts",
                self.host_sections.len(),
                self.host_views.len(),
                saved.len(),
            )));
        }

        let mut hosts = Vec::new();
        for (index, host) in saved.into_iter().enumerate() {
            if id.is_some_and(|id| id != host.id) {
                continue;
            }
            let band = &self.host_sections[index + 1];
            if band.saved_id.as_deref() != Some(host.id.as_str()) {
                return Err(roost_engine::WorkspaceError::Inconsistent(format!(
                    "band {index} is {:?}, not host {}",
                    band.saved_id, host.id
                )));
            }
            hosts.push(HostStatus {
                id: host.id.clone(),
                label: host.label,
                target: host.target,
                last_connected: host.last_connected,
                generation: self.hosts.generation(&host.id),
                state: band.state.wire().to_string(),
                // The band's input, untruncated — the ssh failure
                // families are written as sentences and the rollup
                // beside them is capped at 60 characters.
                reason: self.hosts.section_reason(&host.id).map(str::to_string),
                // Present only where the band had to cut something: a
                // settled localhost launch failure, whose three rungs no
                // 45-character line could hold.
                detail: self.hosts.section_detail(&host.id).map(str::to_string),
                // The band's output, verbatim. Re-deriving it here would
                // be a second formatter to keep in step with the one the
                // sidebar draws.
                rollup: band.rollup.clone(),
                retry: self.hosts.retry_schedule(&host.id),
            });
        }
        Ok(HostStatusResult { hosts })
    }

    /// The saved host with this id, as the registry has it. The one
    /// spelling of "look a host up by id" on the app side.
    pub(super) fn saved_host(
        &self,
        saved_id: &str,
    ) -> Result<roost_engine::persistence::HostSnapshot, roost_engine::WorkspaceError> {
        self.workspace
            .hosts()
            .into_iter()
            .find(|host| host.id == saved_id)
            .ok_or_else(|| roost_engine::WorkspaceError::HostNotFound(saved_id.to_string()))
    }

    /// The `{host, state}` both connection ops answer with, read off the
    /// connection set *after* the verb ran.
    fn host_connection_result(
        &self,
        host: roost_engine::persistence::HostSnapshot,
    ) -> HostConnectionResult {
        // Through the section state the sidebar itself reads, so the
        // reply and the dot drawn beside it can never disagree. A host
        // this app is not driving at all reads as disconnected, which is
        // exactly what its section shows.
        let state = self.hosts.state(&host.id).map_or_else(
            || {
                if self.hosts.establishing(&host.id) {
                    host_sidebar::SectionState::Connecting
                } else {
                    host_sidebar::SectionState::Disconnected
                }
            },
            |state| state.section_state(),
        );
        HostConnectionResult {
            host: host.into(),
            state: state.wire().to_string(),
        }
    }

    pub(super) fn apply_osc_actions(&mut self, key: TabKey, actions: Vec<OscAction>) -> UiTask {
        let tab_id = key.tab;
        for action in actions {
            match action {
                // The only arm that leaves the UI: `LocalClient` drives the
                // local workspace, so a non-local tab's OSC would apply a
                // title/cwd/agent claim to whatever local tab shares its
                // number. HS-2 sends it to that host's client instead.
                OscAction::Workspace { command, payload } => match key.local_tab() {
                    Some(local_tab) => self.client.apply_osc(local_tab, command, &payload),
                    None => tracing::debug!(
                        ?key,
                        %command,
                        "workspace OSC from another instance is not the local client's to apply"
                    ),
                },
                OscAction::ClipboardWrite { target, text } => {
                    if !enqueue_osc_clipboard_write(
                        &mut self.clipboard,
                        self.config.clipboard_write,
                        target,
                        text,
                    ) {
                        tracing::info!(
                            tab_id,
                            "OSC 52 clipboard write dropped — clipboard-write = deny"
                        );
                        continue;
                    }
                }
                OscAction::PointerShape(name) => {
                    if let Some(tab) = self.tabs.get_mut(&key) {
                        tab.pointer_shape = canonical_pointer_shape(&name).into();
                    }
                }
            }
        }
        self.clipboard.start_next()
    }
}

#[cfg(test)]
mod tests {
    use roost_ui_model::keys::HostId;

    use super::*;

    /// Every `HostOpError` a host-routed op can fail with, and the code
    /// plus message it puts on the UI socket (plan 044 §3.1 d6).
    ///
    /// The row that matters is `shutting-down`. Both reorder ops are in
    /// a session's mutating set, so a session with a latched stop
    /// answers that code for exactly these ops — and `ipc.md` says it is
    /// session-socket-only, so letting it cross would put a code on the
    /// UI socket that its own contract says cannot appear there. It
    /// folds, and so does anything a session invents that this build has
    /// never heard of.
    #[test]
    fn a_host_failure_reports_only_codes_a_ui_socket_speaks() {
        use crate::host_conn::HostOpError;
        use roost_ipc::client::ServerCode;

        let rejected = |code: &str| HostOpError::Rejected {
            code: ServerCode::from_wire(code),
            message: "the session said so".into(),
        };

        for code in CODES_A_UI_SOCKET_ALSO_SPEAKS {
            let failure = host_op_failure(&rejected(code));
            assert_eq!(failure.code, code);
            assert_eq!(
                failure.message, "the session said so",
                "a crossing code keeps the session's own sentence, not \
                 `Display`'s code-prefixed one"
            );
        }

        for code in [
            "shutting-down",
            "connect-required",
            "taken-over",
            "already-connected",
            "too-many-tokens",
            "a-code-from-a-newer-session",
        ] {
            let failure = host_op_failure(&rejected(code));
            assert_eq!(failure.code, HOST_UNAVAILABLE, "{code} must not cross");
            assert_eq!(
                failure.message,
                format!("{code}: the session said so"),
                "folding keeps both halves of what the session said"
            );
        }

        // The three that are this connection failing rather than the
        // session answering: one code, and `HostOpError`'s own words —
        // the sentence the drag gesture's status banner shows.
        for error in [
            HostOpError::Disconnected,
            HostOpError::Transport("broken pipe".into()),
            HostOpError::Unavailable,
        ] {
            let expected = error.to_string();
            let failure = host_op_failure(&error);
            assert_eq!(failure.code, HOST_UNAVAILABLE);
            assert_eq!(failure.message, expected);
        }
    }

    /// The minted spelling is the typed variant's, so a client matching
    /// `ServerCode::HostUnavailable` and this handler cannot drift.
    #[test]
    fn host_unavailable_is_the_typed_code() {
        assert_eq!(
            HOST_UNAVAILABLE,
            roost_ipc::client::ServerCode::HostUnavailable.as_str()
        );
        assert_eq!(
            roost_ipc::client::ServerCode::from_wire(HOST_UNAVAILABLE),
            roost_ipc::client::ServerCode::HostUnavailable,
            "an unrecognised code would decode as `Other` and defeat the \
             point of declaring it"
        );
    }

    /// The other arm of `host_reorder_op`: an incarnation this client
    /// holds no op queue for is answered, not dropped — dropping the
    /// reply would reach the caller as `internal: UI dropped reply`.
    #[test]
    fn an_unconnected_incarnation_is_not_found() {
        let failure = no_connected_host();
        assert_eq!(failure.code, "not-found");
        assert!(
            failure.message.contains("incarnation"),
            "the refusal names what was not found: {}",
            failure.message
        );
        assert!(
            CODES_A_UI_SOCKET_ALSO_SPEAKS.contains(&failure.code.as_str()),
            "this arm mints its own code, so it has to be one the UI \
             socket speaks"
        );
    }

    fn envelope(event: &str, data: serde_json::Value) -> roost_ipc::messages::EventEnvelope {
        roost_ipc::messages::EventEnvelope {
            event: event.to_string(),
            data,
        }
    }

    /// `app.keybind_dispatch` is a paste-only seam, not a general
    /// dispatcher: every other spelling `KeybindAction::from_name`
    /// would otherwise resolve — including destructive/state-mutating
    /// ones like `close_tab` and `close_project`, and the
    /// clipboard-writing `copy` — must be refused, and refused with a
    /// message that names the one action this op actually accepts.
    #[test]
    fn keybind_dispatch_test_op_accepts_only_paste() {
        assert_eq!(paste_only_keybind("paste"), Ok(KeybindAction::Paste));

        for other in ["close_tab", "new_tab", "close_project", "copy", "unbind"] {
            let error =
                paste_only_keybind(other).expect_err("a non-paste action name must be refused");
            assert!(
                error.contains("paste"),
                "refusal for {other:?} must name the allowed action: {error}"
            );
        }

        let error =
            paste_only_keybind("not_a_real_action").expect_err("an unknown name is refused too");
        assert!(error.contains("paste"));
    }

    /// A host agent's notification is routed to the notification path,
    /// carrying the title and body verbatim — the inbox row and the
    /// desktop banner are then composed exactly as a local tab's are
    /// (plan 037 §3.1's "notifications from host tabs fire like local
    /// ones").
    ///
    /// The wire spells `tab_id` as a string, so the decode is half the
    /// point: a host tab id is an `i64` in a foreign id-space, and it
    /// only becomes addressable once `apply_host_envelopes` qualifies it
    /// at that host's incarnation.
    #[test]
    fn a_hosts_notification_envelope_routes_to_the_notification_path() {
        let fired = envelope(
            roost_ipc::messages::ops::EVENT_NOTIFICATION_FIRED,
            serde_json::json!({"tab_id": "7", "title": "Claude", "body": "needs input"}),
        );
        let HostEnvelopeAction::Notify(notification) = host_envelope_action(&fired) else {
            panic!("a notification.fired must reach the notification path");
        };
        assert_eq!(notification.tab_id, 7);
        assert_eq!(notification.title, "Claude");
        assert_eq!(notification.body, "needs input");

        // And the key it lands under is the host's, never the local
        // workspace's — the id-collision rule (AC11) at this seam.
        let host = HostId::new(4);
        let key = TabKey::new(host, notification.tab_id);
        assert!(!key.is_local());
        assert_ne!(key, TabKey::local(7));
    }

    /// The one rule the origin field exists for: a connection asked for
    /// over the IPC socket is never a user's, so nothing downstream can
    /// decide to open a modal for it (plan 039 §3.5). `roostctl host
    /// connect` reaches `host_reconnect_requested` with the very same
    /// `ConnectMode` a click does, so this is the only place the
    /// difference is stated.
    #[test]
    fn a_connect_arriving_over_ipc_is_never_a_users() {
        use crate::host_conn::RequestOrigin;
        assert_eq!(App::IPC_CONNECT_ORIGIN, RequestOrigin::Ipc);
        assert_ne!(App::IPC_CONNECT_ORIGIN, RequestOrigin::User);
    }

    /// The clearing edge only. A *pending* flag is the mirror's to paint
    /// and carries no body; acting on it here would upsert a bodyless
    /// duplicate beside the row `notification.fired` already made.
    #[test]
    fn only_the_clearing_edge_of_a_tabs_pending_flag_is_acted_on() {
        let op = roost_ipc::messages::ops::EVENT_TAB_NOTIFICATION;
        assert!(matches!(
            host_envelope_action(&envelope(
                op,
                serde_json::json!({"tab_id": "7", "has_pending": false})
            )),
            HostEnvelopeAction::ClearNotification(7)
        ));
        assert!(matches!(
            host_envelope_action(&envelope(
                op,
                serde_json::json!({"tab_id": "7", "has_pending": true})
            )),
            HostEnvelopeAction::Ignore
        ));
    }

    /// Effects still route where C5 put them, and everything else is
    /// silent: a workspace fact the mirror folds in, and an event from a
    /// newer session this client has never heard of, are both ignored
    /// rather than logged as faults (`ipc.md` #versioning).
    #[test]
    fn effects_route_and_unknown_envelopes_are_silently_ignored() {
        assert!(matches!(
            host_envelope_action(&envelope(
                roost_ipc::messages::ops::EVENT_TAB_EFFECT,
                serde_json::json!({"tab_id": "3", "effect": "bell"})
            )),
            HostEnvelopeAction::Effect(_)
        ));
        for event in ["tab.title_changed", "something.from.the.future"] {
            assert!(
                matches!(
                    host_envelope_action(&envelope(event, serde_json::json!({}))),
                    HostEnvelopeAction::Ignore
                ),
                "{event}"
            );
        }
    }

    /// The retiring edges a host owes, and the reason they are here at
    /// all: an attention row nothing takes down outlives the tab it
    /// names. The local workspace already retires both surfaces on
    /// `TabClosed` and sweeps the project on `ProjectDeleted`; a host
    /// tab closed from anywhere — another window, `roostctl`, the shell
    /// exiting — must do the same.
    #[test]
    fn a_hosts_close_and_delete_retire_what_they_named() {
        assert!(matches!(
            host_envelope_action(&envelope(
                roost_ipc::messages::ops::EVENT_TAB_CLOSED,
                serde_json::json!({"tab_id": "7"})
            )),
            HostEnvelopeAction::TabClosed(7)
        ));
        assert!(matches!(
            host_envelope_action(&envelope(
                roost_ipc::messages::ops::EVENT_PROJECT_DELETED,
                serde_json::json!({"project_id": "4"})
            )),
            HostEnvelopeAction::ProjectDeleted(4)
        ));
    }

    /// The inbox derivation is one rule over both id-spaces, which is
    /// what makes a reconnect restore a host's attention rows: the
    /// mirror already says which tabs are pending, so the reconcile
    /// rebuilds them without a replayed `notification.fired` (plan 037
    /// §4 forbids the replay).
    ///
    /// The keys are the point. The same numeric tab id under a host and
    /// under the local workspace are two rows, and a host row's title
    /// reads exactly like a local one so the single cross-host list does
    /// not look like two kinds of list.
    #[test]
    fn pending_rows_derive_identically_for_a_host_and_for_the_local_workspace() {
        fn tab(id: i64, has_notification: bool) -> roost_ipc::messages::Tab {
            roost_ipc::messages::Tab {
                id,
                project_id: 4,
                title: format!("tab-{id}"),
                cwd: "/w/roost".into(),
                state: roost_ipc::messages::TabState::None,
                has_notification,
                is_active: false,
                user_titled: false,
                position: 0,
                created_at: 0,
                last_active: 0,
                hook_active: false,
                shell_state: roost_ipc::agent::ShellState::default(),
                agent_lifecycle: roost_ipc::agent::AgentLifecycle::default(),
                ownership: None,
            }
        }
        let projects = vec![Project {
            id: 4,
            name: "roost".into(),
            cwd: "/w/roost".into(),
            position: 0,
            created_at: 0,
            tabs: vec![tab(7, true), tab(8, false)],
        }];

        let host = HostId::new(3);
        let none = HashSet::new();
        let remote = pending_notification_rows(host, &projects, &none);
        let local = pending_notification_rows(HostId::LOCAL, &projects, &none);
        assert_eq!(remote.len(), 1, "only the pending tab earns a row");
        assert_eq!(remote[0].0, TabKey::new(host, 7));
        assert_eq!(remote[0].1, ProjectKey::new(host, 4));
        assert_ne!(remote[0].0, local[0].0, "one number, two id-spaces");
        assert_eq!(
            remote[0].2, local[0].2,
            "and one composed title, so the list reads as one list"
        );

        assert!(pending_notification_rows(host, &[], &none).is_empty());
    }

    /// A bell has no server flag behind it, so unless the derivation
    /// itself knows about it the very next reconcile prunes its row —
    /// which is what made a host bell a no-op on screen. It is an input
    /// here, keyed at the host that rang: a bell on one incarnation must
    /// not light the same number on another.
    #[test]
    fn a_bell_earns_a_row_the_reconcile_will_not_prune() {
        fn tab(id: i64) -> roost_ipc::messages::Tab {
            roost_ipc::messages::Tab {
                id,
                project_id: 4,
                title: format!("tab-{id}"),
                cwd: "/w/roost".into(),
                state: roost_ipc::messages::TabState::None,
                has_notification: false,
                is_active: false,
                user_titled: false,
                position: 0,
                created_at: 0,
                last_active: 0,
                hook_active: false,
                shell_state: roost_ipc::agent::ShellState::default(),
                agent_lifecycle: roost_ipc::agent::AgentLifecycle::default(),
                ownership: None,
            }
        }
        let projects = vec![Project {
            id: 4,
            name: "roost".into(),
            cwd: "/w/roost".into(),
            position: 0,
            created_at: 0,
            tabs: vec![tab(7), tab(8)],
        }];
        let host = HostId::new(3);

        assert!(
            pending_notification_rows(host, &projects, &HashSet::new()).is_empty(),
            "no flags, no bells, no rows"
        );

        let rung: HashSet<TabKey> = [TabKey::new(host, 7)].into_iter().collect();
        let rows = pending_notification_rows(host, &projects, &rung);
        assert_eq!(rows.len(), 1, "the tab that rang earns exactly one row");
        assert_eq!(rows[0].0, TabKey::new(host, 7));

        // The same bare number on another incarnation is another tab.
        assert!(
            pending_notification_rows(HostId::new(9), &projects, &rung).is_empty(),
            "a bell is keyed at the host that heard it"
        );
    }

    /// A malformed payload is reported as undecodable rather than
    /// silently dropped — the two are the same on screen and very
    /// different in a log.
    #[test]
    fn a_malformed_payload_is_distinguishable_from_an_unknown_event() {
        assert!(matches!(
            host_envelope_action(&envelope(
                roost_ipc::messages::ops::EVENT_NOTIFICATION_FIRED,
                serde_json::json!({"title": "no tab id"})
            )),
            HostEnvelopeAction::Undecodable(_)
        ));
    }

    /// These tests hand `collect_tab_output` the items a forwarder would
    /// have delivered, so the feed receiver is surplus.
    fn attached(tab_id: i64) -> (HashMap<TabKey, TerminalTab>, Arc<PtySupervisor>) {
        let (feed_tx, _) = engine_feed::channel();
        let (tab, supervisor) = attach_test_terminal(tab_id, feed_tx);
        (HashMap::from([(TabKey::local(tab_id), tab)]), supervisor)
    }

    /// The race the retry exists for, against a real supervisor: the
    /// workspace lists a tab before `PtySupervisor::spawn` promotes its
    /// session, so the first attach fails and a later one succeeds. This
    /// is the attempt/verdict sequence `attach_tab_tracked` drives.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tab_whose_pty_is_not_spawned_yet_attaches_on_a_retry() {
        let supervisor = Arc::new(PtySupervisor::new());
        let (feed_tx, _feed_rx) = engine_feed::channel();
        let backend = TabBackend::in_process(Arc::clone(&supervisor), true);
        let attach = |feed: EngineFeedSender| {
            TerminalTab::attach(
                &backend,
                75,
                Theme::roost_dark_fallback(),
                roost_ui_model::word_selection::DEFAULT_EXTRA_WORD_CHARS.to_string(),
                feed,
            )
        };
        let mut pending = PendingAttachments::default();
        let started = Instant::now();

        assert!(
            attach(feed_tx.clone()).is_err(),
            "no session has been promoted yet"
        );
        assert_eq!(
            pending.record_failure(TabKey::local(75), started),
            AttachRetryVerdict::Retry
        );
        assert!(pending.has_retryable(), "the retry subscription is armed");
        assert_eq!(pending.retry_keys(), vec![TabKey::local(75)]);

        supervisor
            .spawn(
                75,
                "/tmp",
                &["/bin/sh".to_string(), "-c".into(), "cat".into()],
                DEFAULT_COLS,
                DEFAULT_ROWS,
                std::path::Path::new("/tmp/roost-iced-attach-retry-test.sock"),
            )
            .expect("spawn the PTY the retry is waiting for");

        let tab = attach(feed_tx).expect("the retry attaches once the session exists");
        pending.clear(TabKey::local(75));
        assert!(
            !pending.has_retryable(),
            "success disarms the retry subscription"
        );
        assert!(pending.tracked_keys().is_empty());

        drop(tab);
        supervisor.close(75);
    }

    /// The recovery path an exhausted tab keeps: reconcile still makes its
    /// (supervisor-lookup cheap) attempt, so a session that shows up after
    /// the budget ran out still attaches and takes the mark with it. This
    /// engine has no respawn-in-place event to hang recovery off — a tab's
    /// session is spawned once at open — so the attempt itself is the
    /// recovery signal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_late_session_still_attaches_a_tab_whose_budget_ran_out() {
        let supervisor = Arc::new(PtySupervisor::new());
        let (feed_tx, _feed_rx) = engine_feed::channel();
        let mut pending = PendingAttachments::default();
        let started = Instant::now();

        for attempt in 0..=ATTACH_RETRY_LIMIT {
            assert!(!supervisor.has(76), "the guard reconcile attempts first");
            pending.record_failure(TabKey::local(76), started + ATTACH_RETRY_INTERVAL * attempt);
        }
        assert!(!pending.has_retryable(), "the budget is spent");
        assert_eq!(
            pending.tracked_keys(),
            vec![TabKey::local(76)],
            "but the tab is remembered"
        );

        supervisor
            .spawn(
                76,
                "/tmp",
                &["/bin/sh".to_string(), "-c".into(), "cat".into()],
                DEFAULT_COLS,
                DEFAULT_ROWS,
                std::path::Path::new("/tmp/roost-iced-attach-recovery-test.sock"),
            )
            .expect("the session finally arrives");

        assert!(supervisor.has(76));
        let tab = TerminalTab::attach(
            &TabBackend::in_process(Arc::clone(&supervisor), true),
            76,
            Theme::roost_dark_fallback(),
            roost_ui_model::word_selection::DEFAULT_EXTRA_WORD_CHARS.to_string(),
            feed_tx,
        )
        .expect("an exhausted mark never blocks the attach itself");
        pending.clear(TabKey::local(76));
        assert!(
            pending.tracked_keys().is_empty(),
            "attaching lifts the exhausted mark"
        );

        drop(tab);
        supervisor.close(76);
    }

    /// The retry cadence, walked one 25 ms shot at a time: forty attempts
    /// spanning the full window, then the give-up that reports both.
    #[test]
    fn the_attach_budget_is_spent_once_and_reports_how_long_it_waited() {
        let mut pending = PendingAttachments::default();
        let started = Instant::now();
        for attempt in 0..ATTACH_RETRY_LIMIT {
            assert_eq!(
                pending.record_failure(TabKey::local(9), started + ATTACH_RETRY_INTERVAL * attempt),
                AttachRetryVerdict::Retry,
                "attempt {attempt} is inside the budget"
            );
        }
        let last = started + ATTACH_RETRY_INTERVAL * ATTACH_RETRY_LIMIT;
        assert_eq!(
            pending.record_failure(TabKey::local(9), last),
            AttachRetryVerdict::Exhausted {
                attempts: ATTACH_RETRY_LIMIT + 1,
                waited: ATTACH_RETRY_WINDOW,
            },
            "the budget reports the whole wait, not the last gap"
        );
        assert!(
            !pending.has_retryable(),
            "an exhausted tab stops arming the retry subscription"
        );
        assert_eq!(
            pending.tracked_keys(),
            vec![TabKey::local(9)],
            "and stays tracked, so nothing can restart its budget"
        );
    }

    /// The reason an exhausted entry is kept rather than dropped: every
    /// later reconcile re-attempts a live unattached tab, and a dropped
    /// entry would be reinserted with a fresh budget — giving up would
    /// never stick and the 25 ms timer would re-arm forever.
    #[test]
    fn exhaustion_survives_every_later_reconcile_and_warns_once() {
        let mut pending = PendingAttachments::default();
        let started = Instant::now();
        let mut exhaustions = 0;
        for attempt in 0..=ATTACH_RETRY_LIMIT {
            if matches!(
                pending.record_failure(TabKey::local(9), started + ATTACH_RETRY_INTERVAL * attempt),
                AttachRetryVerdict::Exhausted { .. }
            ) {
                exhaustions += 1;
            }
        }
        assert_eq!(exhaustions, 1);

        let later = started + ATTACH_RETRY_WINDOW * 10;
        for step in 0..100 {
            assert_eq!(
                pending.record_failure(TabKey::local(9), later + ATTACH_RETRY_INTERVAL * step),
                AttachRetryVerdict::GaveUp,
                "a later reconcile neither restarts the budget nor re-warns"
            );
        }
        assert!(!pending.has_retryable(), "and never re-arms the timer");
        assert_eq!(pending.tracked_keys(), vec![TabKey::local(9)]);

        pending.retain_live(HostId::LOCAL, &HashSet::new());
        assert!(
            pending.tracked_keys().is_empty(),
            "a closed tab is pruned exhausted mark and all"
        );
        assert_eq!(
            pending.record_failure(TabKey::local(9), later),
            AttachRetryVerdict::Retry,
            "and an id the workspace lists again is a new tab with a new budget"
        );
    }

    /// Reconcile and the timer share one counter, so a burst of workspace
    /// events can spend every attempt in microseconds. Giving up then
    /// would abandon the tab inside the very race the retry exists for.
    #[test]
    fn a_burst_of_reconciles_cannot_end_the_attach_budget_early() {
        let mut pending = PendingAttachments::default();
        let now = Instant::now();
        for _ in 0..4 * ATTACH_RETRY_LIMIT {
            assert_eq!(
                pending.record_failure(TabKey::local(9), now),
                AttachRetryVerdict::Retry,
                "attempts alone cannot end the wall-clock window"
            );
        }
        assert!(
            pending.has_retryable(),
            "the tab is still waiting for its PTY"
        );
        assert!(matches!(
            pending.record_failure(TabKey::local(9), now + ATTACH_RETRY_WINDOW),
            AttachRetryVerdict::Exhausted { .. }
        ));
    }

    #[test]
    fn pending_attachments_follow_the_workspace_and_stay_in_a_stable_order() {
        let mut pending = PendingAttachments::default();
        let now = Instant::now();
        for tab_id in [7, 3, 5] {
            assert_eq!(
                pending.record_failure(TabKey::local(tab_id), now),
                AttachRetryVerdict::Retry
            );
        }
        assert_eq!(
            pending.retry_keys(),
            vec![TabKey::local(3), TabKey::local(5), TabKey::local(7)]
        );

        pending.retain_live(
            HostId::LOCAL,
            &HashSet::from([TabKey::local(3), TabKey::local(7)]),
        );
        assert_eq!(
            pending.retry_keys(),
            vec![TabKey::local(3), TabKey::local(7)],
            "a tab the workspace dropped is never retried"
        );
        pending.clear(TabKey::local(3));
        pending.clear(TabKey::local(7));
        assert!(pending.tracked_keys().is_empty());
        pending.retain_live(HostId::LOCAL, &HashSet::from([TabKey::local(1)]));
        assert!(
            !pending.has_retryable(),
            "a live tab with no failed attach is not pending"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bytes_write_through_and_touch_only_their_own_tab() {
        let (mut tabs, supervisor) = attached(70);
        let mut collected = TabOutputBatch::default();

        collect_tab_output(
            &mut tabs,
            &mut collected,
            TabKey::local(70),
            TabOutput::Bytes(b"\x1b[2J\x1b[Hhello".to_vec()),
        );

        assert_eq!(collected.touched, HashSet::from([TabKey::local(70)]));
        assert!(
            collected.osc_actions.is_empty(),
            "plain output carries no OSC, so the tail has nothing to apply"
        );
        assert!(collected.exited.is_empty());
        assert!(collected.error.is_none());
        let tab = tabs
            .get_mut(&TabKey::local(70))
            .expect("the tab is still attached");
        tab.refresh_snapshot().expect("refresh the touched tab");
        assert_eq!(tab.snapshot.grid[0].text, "hello");
        supervisor.close(70);
    }

    /// What the OSC opt-in actually delivers: the bytes still write
    /// through, and the actions the drain did NOT consume (it keeps the
    /// query replies) ride along for the batch tail to apply.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scanned_output_writes_through_and_carries_its_actions() {
        let (mut tabs, supervisor) = attached(75);
        let mut collected = TabOutputBatch::default();

        collect_tab_output(
            &mut tabs,
            &mut collected,
            TabKey::local(75),
            TabOutput::Scanned {
                data: b"\x1b[2J\x1b[Hhello".to_vec(),
                actions: vec![OscAction::PointerShape("pointer".into())],
            },
        );

        assert_eq!(collected.touched, HashSet::from([TabKey::local(75)]));
        assert_eq!(
            collected.osc_actions,
            vec![(
                TabKey::local(75),
                vec![OscAction::PointerShape("pointer".into())]
            )]
        );
        let tab = tabs
            .get_mut(&TabKey::local(75))
            .expect("the tab is still attached");
        tab.refresh_snapshot().expect("refresh the touched tab");
        assert_eq!(tab.snapshot.grid[0].text, "hello");
        supervisor.close(75);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn output_for_a_tab_that_is_gone_is_dropped() {
        let (mut tabs, supervisor) = attached(71);
        let mut collected = TabOutputBatch::default();

        for output in [
            TabOutput::Bytes(b"late".to_vec()),
            TabOutput::Exit {
                status: 0,
                reason: "shell exited".into(),
            },
            TabOutput::Error("broadcast lagged".into()),
        ] {
            collect_tab_output(&mut tabs, &mut collected, TabKey::local(999), output);
        }

        assert!(collected.touched.is_empty());
        assert!(collected.osc_actions.is_empty());
        assert!(collected.exited.is_empty());
        assert!(collected.error.is_none());
        supervisor.close(71);
    }

    /// The collision the host-qualified keys exist for: a dead connection
    /// epoch's items carry its instance, and the local tab that happens to
    /// share their numeric id must not receive them — not the bytes, not
    /// the exit that would close it, not the error that would surface as a
    /// status.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn output_from_a_stale_instance_never_reaches_the_local_tab_of_that_id() {
        let (mut tabs, supervisor) = attached(77);
        let stale = TabKey::new(HostId::new(9), 77);
        assert_ne!(stale, TabKey::local(77));
        let mut collected = TabOutputBatch::default();

        for output in [
            TabOutput::Bytes(b"\x1b[2J\x1b[Hstale".to_vec()),
            TabOutput::Scanned {
                data: b"stale".to_vec(),
                actions: vec![OscAction::PointerShape("pointer".into())],
            },
            TabOutput::Exit {
                status: 0,
                reason: "shell exited".into(),
            },
            TabOutput::Error("broadcast lagged".into()),
        ] {
            collect_tab_output(&mut tabs, &mut collected, stale, output);
        }

        assert!(collected.touched.is_empty());
        assert!(collected.osc_actions.is_empty());
        assert!(collected.exited.is_empty());
        assert!(collected.error.is_none());
        let tab = tabs
            .get_mut(&TabKey::local(77))
            .expect("the live tab is untouched");
        tab.refresh_snapshot().expect("refresh the live tab");
        assert_eq!(
            tab.snapshot.grid[0].text.trim(),
            "",
            "no stale byte was written through to the live terminal"
        );
        supervisor.close(77);
    }

    /// The same collision walked through the REAL drain, not the helper:
    /// items are sent on a live feed, taken with `try_next`, dispatched by
    /// the arms `service_engine` uses, and finished by its batch tail —
    /// the tail being where `TabOutput::Exit` reaches `workspace.close_tab`
    /// and a banner click reaches `focus_tab_in_core`. Those two are the
    /// engine sinks, so this is the test that pins them: a dead epoch's
    /// items must close nothing, focus nothing and write nothing.
    ///
    /// `App` has no test constructor (bootstrap needs a profile, the
    /// instance lock and the Iced runtime), so the drain is reproduced
    /// here against a real `Workspace` and a real attached tab.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stale_instances_items_survive_the_whole_drain_without_touching_the_local_tab() {
        let workspace = Workspace::new();
        let project = workspace.create_project("p", "/tmp").expect("project");
        let live = workspace.open_tab(project.id, "/tmp", "live").expect("tab");
        let other = workspace
            .open_tab(project.id, "/tmp", "other")
            .expect("tab");
        workspace.focus_tab(other.id).expect("focus elsewhere");
        workspace
            .set_tab_has_notification(live.id, true)
            .expect("mark pending");

        let (feed_tx, _keep) = engine_feed::channel();
        let (mut terminal, supervisor) = attach_test_terminal(live.id, feed_tx);
        terminal.refresh_snapshot().expect("initial snapshot");
        let mut tabs = HashMap::from([(TabKey::local(live.id), terminal)]);

        // The dead epoch reuses the live tab's number — the whole point.
        let stale = TabKey::new(HostId::new(9), live.id);
        let (tx, mut rx) = engine_feed::channel();
        assert!(tx.send(EngineFeed::Tab(
            stale,
            TabOutput::Bytes(b"\x1b[2J\x1b[Hstale".to_vec())
        )));
        assert!(tx.send(EngineFeed::Tab(
            stale,
            TabOutput::Exit {
                status: 0,
                reason: "shell exited".into(),
            }
        )));
        assert!(tx.send(EngineFeed::NotificationActivated { tab: stale }));

        // `service_engine`'s drain loop and batch tail, verbatim in shape.
        let mut batch = EngineBatch::default();
        let mut pty = TabOutputBatch::default();
        let mut raised = 0;
        while let Some(item) = rx.try_next(&mut batch) {
            match item {
                EngineFeed::Tab(key, output) => {
                    collect_tab_output(&mut tabs, &mut pty, key, output);
                }
                EngineFeed::NotificationActivated { tab } => {
                    if notification_activation(&workspace, Some(window::Id::unique()), tab)
                        .is_some()
                    {
                        raised += 1;
                    }
                }
                _ => panic!("only the two host-keyed arms were sent"),
            }
        }
        for key in pty.exited {
            if let Some(tab_id) = key.local_tab() {
                let _ = workspace.close_tab(tab_id);
            }
            tabs.remove(&key);
        }

        assert_eq!(raised, 0, "a dead epoch's banner click earns no raise");
        assert!(pty.error.is_none());
        assert!(
            workspace.tab(live.id).is_ok(),
            "the exit must not have closed the local tab of that number"
        );
        assert_eq!(
            workspace.active().1,
            other.id,
            "and the click must not have moved the focus"
        );
        let terminal = tabs
            .get_mut(&TabKey::local(live.id))
            .expect("the local tab is still attached");
        terminal.refresh_snapshot().expect("refresh the live tab");
        assert_eq!(
            terminal.snapshot.grid[0].text.trim(),
            "",
            "and no stale byte was written through to it"
        );
        supervisor.close(live.id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_exit_is_collected_for_close_and_an_error_becomes_a_status() {
        let (mut tabs, supervisor) = attached(72);
        let mut collected = TabOutputBatch::default();

        collect_tab_output(
            &mut tabs,
            &mut collected,
            TabKey::local(72),
            TabOutput::Exit {
                status: 0,
                reason: "shell exited".into(),
            },
        );
        collect_tab_output(
            &mut tabs,
            &mut collected,
            TabKey::local(72),
            TabOutput::Error("broadcast lagged: dropped 3 message(s)".into()),
        );

        assert_eq!(collected.exited, vec![TabKey::local(72)]);
        assert_eq!(
            collected.error.as_deref(),
            Some("tab 72: broadcast lagged: dropped 3 message(s)")
        );
        assert!(
            collected.touched.is_empty(),
            "neither an exit nor an error changes what the tab renders"
        );
        supervisor.close(72);
    }

    /// The premise the batch tail's OSC-before-refresh order rests on: the
    /// snapshot holds a *copy* of `pointer_shape` taken at refresh time, so
    /// an OSC action that lands after a refresh stays invisible until some
    /// unrelated event refreshes the tab again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pointer_shape_reaches_the_snapshot_only_through_a_later_refresh() {
        let (feed_tx, _) = engine_feed::channel();
        let (mut tab, supervisor) = attach_test_terminal(73, feed_tx);
        tab.refresh_snapshot().expect("initial snapshot");
        assert_eq!(tab.snapshot.pointer_shape, "default");

        // What `App::apply_osc_actions` does for OscAction::PointerShape.
        tab.pointer_shape = canonical_pointer_shape("crosshair").into();
        assert_eq!(
            tab.snapshot.pointer_shape, "default",
            "a refresh ordered before the OSC would have published this"
        );

        tab.refresh_snapshot().expect("post-OSC snapshot");
        assert_eq!(tab.snapshot.pointer_shape, "crosshair");
        supervisor.close(73);
    }

    /// `reconcile`'s failed-geometry arm builds a tab and then discards it,
    /// and a later attach takes the same PTY over. The discarded tab's
    /// forwarder must not outlive it: the second `TabSession::attach`
    /// cannot reuse the initial receiver, so a survivor would put a second
    /// FIFO stream on the feed and interleave it with the real one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_discarded_tab_takes_its_output_forwarder_with_it() {
        let (discarded_feed, mut discarded_rx) = engine_feed::channel();
        let (discarded, supervisor) = attach_test_terminal(74, discarded_feed);
        drop(discarded);

        let (live_feed, mut live_rx) = engine_feed::channel();
        let tab = TerminalTab::attach(
            &TabBackend::in_process(Arc::clone(&supervisor), true),
            74,
            Theme::roost_dark_fallback(),
            roost_ui_model::word_selection::DEFAULT_EXTRA_WORD_CHARS.to_string(),
            live_feed,
        )
        .expect("re-attach the PTY the discarded tab left running");
        tab.session.send_input(b"forwarder-marker\n".to_vec());

        let live = feed_text_until(
            &mut live_rx,
            TabKey::local(74),
            "forwarder-marker",
            Duration::from_secs(5),
        )
        .await;
        assert!(
            live.contains("forwarder-marker"),
            "the re-attached tab is the live stream: {live:?}"
        );

        // The marker has already round-tripped, so a forwarder that
        // outlived its tab has had its chance; this window is slack, not
        // synchronisation.
        let stale = feed_text_until(
            &mut discarded_rx,
            TabKey::local(74),
            "forwarder-marker",
            Duration::from_millis(250),
        )
        .await;
        assert!(
            !stale.contains("forwarder-marker"),
            "the discarded tab's forwarder went with it: {stale:?}"
        );
        supervisor.close(74);
    }

    /// The banner-click path is the one focus route that never has a
    /// `&mut App` behind it, so what it owes the core — focus the tab, clear
    /// its pending notification, and only then earn a raise — is pinned
    /// here. A tab that closed between the banner and the click must move
    /// nothing at all.
    #[test]
    fn a_banner_click_focuses_its_tab_clears_it_and_raises_the_window() {
        let workspace = Workspace::new();
        let project = workspace.create_project("p", "/tmp").expect("project");
        let clicked = workspace.open_tab(project.id, "/tmp", "one").expect("tab");
        let other = workspace.open_tab(project.id, "/tmp", "two").expect("tab");
        workspace
            .set_tab_has_notification(clicked.id, true)
            .expect("mark pending");
        workspace.focus_tab(other.id).expect("focus elsewhere");
        let pending = |tab_id: i64| {
            workspace
                .snapshot()
                .iter()
                .flat_map(|project| project.tabs.iter())
                .find(|tab| tab.id == tab_id)
                .map(|tab| tab.has_notification)
        };

        let window = window::Id::unique();
        let raise = notification_activation(&workspace, Some(window), TabKey::local(clicked.id))
            .expect("the tab the banner named is still there");
        assert!(matches!(raise, UiTask::Focus(id) if id == window));
        assert_eq!(workspace.active().1, clicked.id);
        assert_eq!(
            pending(clicked.id),
            Some(false),
            "the jump clears the badge"
        );

        // No window id yet (or a headless run): the focus still landed in
        // the core, and only the raise is skipped.
        assert!(matches!(
            notification_activation(&workspace, None, TabKey::local(other.id)),
            Some(UiTask::None)
        ));
        assert_eq!(workspace.active().1, other.id);

        workspace.close_tab(clicked.id).expect("close the tab");
        assert!(
            notification_activation(&workspace, Some(window), TabKey::local(clicked.id)).is_none(),
            "a banner outliving its tab is a no-op"
        );
        assert_eq!(workspace.active().1, other.id, "and moves nothing");
    }

    /// The same click from a dead connection epoch: its instance no longer
    /// matches anything, and the local tab that shares its numeric id must
    /// not be jumped to.
    #[test]
    fn a_banner_from_a_stale_instance_never_jumps_the_local_tab_of_that_id() {
        let workspace = Workspace::new();
        let project = workspace.create_project("p", "/tmp").expect("project");
        let one = workspace.open_tab(project.id, "/tmp", "one").expect("tab");
        let two = workspace.open_tab(project.id, "/tmp", "two").expect("tab");
        workspace
            .set_tab_has_notification(one.id, true)
            .expect("mark pending");
        workspace.focus_tab(two.id).expect("focus elsewhere");

        assert!(
            notification_activation(
                &workspace,
                Some(window::Id::unique()),
                TabKey::new(HostId::new(9), one.id),
            )
            .is_none(),
            "another instance's banner earns no raise"
        );
        assert_eq!(
            workspace.active().1,
            two.id,
            "and never moves the local focus"
        );
    }

    /// Pins the per-tab counters `refresh_snapshot` maintains. These are
    /// asserted on the tab's own `TabRenderStats`, not the process-global
    /// aggregate in `perf` — `cargo test -p roost-iced` runs concurrently
    /// with other tests that spawn their own PTY and refresh their own
    /// tab, and a global counter would pick up their activity too.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refresh_snapshot_updates_the_tabs_own_render_stats() {
        let (feed_tx, _) = engine_feed::channel();
        let (mut tab, supervisor) = attach_test_terminal(75, feed_tx);
        assert_eq!(tab.render_stats, crate::perf::TabRenderStats::default());

        tab.refresh_snapshot().expect("refresh");

        assert_eq!(tab.render_stats.refresh_calls, 1);
        assert_eq!(
            tab.render_stats.rows_rebuilt,
            u64::from(DEFAULT_ROWS),
            "the first refresh has no cached grid, so it rebuilds every row"
        );
        assert_eq!(
            tab.render_stats.cells_walked,
            u64::from(DEFAULT_COLS) * u64::from(DEFAULT_ROWS)
        );
        assert!(
            tab.render_stats.refresh_nanos > 0,
            "refresh does real work, so elapsed time should be nonzero"
        );

        tab.refresh_snapshot().expect("second refresh");
        assert_eq!(
            tab.render_stats.refresh_calls, 2,
            "counters accumulate across calls rather than resetting"
        );
        assert_eq!(
            tab.render_stats.rows_rebuilt,
            u64::from(DEFAULT_ROWS),
            "nothing touched the terminal, so the second refresh rebuilds \
             zero rows and the total does not move"
        );
        assert_eq!(
            tab.render_stats.cells_walked,
            u64::from(DEFAULT_COLS) * u64::from(DEFAULT_ROWS),
            "and walks no cells either"
        );

        supervisor.close(75);
    }

    /// The failure a per-row cache can silently produce is "right cells,
    /// wrong row" — content landing one row off, or a stale row surviving
    /// a rebuild. A substring search over the joined dump would not catch
    /// either, so this writes a distinct marker to one row at a time and
    /// checks the WHOLE row vector element-for-element after every write.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn incremental_rebuild_keeps_every_row_at_its_own_index() {
        let (feed_tx, _) = engine_feed::channel();
        let (mut tab, supervisor) = attach_test_terminal(76, feed_tx);
        // Absolute positioning only: a scroll would move every row and
        // turn this into a full-rebuild test by accident.
        tab.write_vt(b"\x1b[2J\x1b[H");
        tab.refresh_snapshot().expect("refresh the cleared grid");

        let rows = usize::from(DEFAULT_ROWS);
        let mut expected = vec![String::new(); rows];
        // Out of order on purpose, and not every row: a cache that keys on
        // walk order rather than the reported row index passes an
        // in-order fill.
        for (step, row) in [4usize, 1, rows - 1, 0, 9, 4].into_iter().enumerate() {
            let marker = format!("marker-{step}-row-{row}");
            tab.write_vt(format!("\x1b[{};1H{marker}", row + 1).as_bytes());
            tab.refresh_snapshot().expect("refresh after the write");
            expected[row] = marker;
            assert_eq!(
                tab.dump().rows_text,
                expected,
                "after step {step} (row {row}) every row must hold exactly its own content"
            );
        }

        supervisor.close(76);
    }

    /// `TerminalSnapshot::blank` fills its rows with an empty string while
    /// `refresh_snapshot` builds `" "`-filled rows and trims them. Both
    /// must land on `""`, because `tab.dump` — and the whole e2e suite
    /// through it — reads one before the first refresh and the other
    /// after.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_blank_snapshot_and_a_refreshed_empty_grid_dump_the_same_rows() {
        let (feed_tx, _) = engine_feed::channel();
        let (mut tab, supervisor) = attach_test_terminal(77, feed_tx);
        let blank = tab.dump().rows_text;
        assert_eq!(blank, vec![String::new(); usize::from(DEFAULT_ROWS)]);

        tab.write_vt(b"\x1b[2J\x1b[H");
        tab.refresh_snapshot().expect("refresh the cleared grid");
        assert_eq!(
            tab.dump().rows_text,
            blank,
            "a refreshed empty grid trims down to the same rows blank starts at"
        );

        supervisor.close(77);
    }

    /// `OSC 11` changes the terminal's default background with libghostty
    /// reporting nothing dirty, so only `refresh_snapshot`'s cached-default
    /// guard keeps cached rows from freezing at the old color. Without it
    /// the untouched row below would keep rendering the old background.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn changing_the_default_background_rebuilds_cached_rows() {
        let (feed_tx, _) = engine_feed::channel();
        let (mut tab, supervisor) = attach_test_terminal(78, feed_tx);
        tab.write_vt(b"\x1b[2J\x1b[1;1Hcolored");
        tab.refresh_snapshot()
            .expect("refresh with the row written");
        let before = tab.snapshot.background;

        let rebuilt_before = tab.render_stats.rows_rebuilt;
        tab.write_vt(b"\x1b]11;rgb:00/00/ff\x07");
        tab.refresh_snapshot().expect("refresh after OSC 11");

        assert_ne!(
            tab.snapshot.background, before,
            "OSC 11 must reach the render state's default background"
        );
        assert_eq!(
            tab.render_stats.rows_rebuilt - rebuilt_before,
            u64::from(DEFAULT_ROWS),
            "a default-color change invalidates every cached row"
        );
        let resolved = tab.resolved_cells();
        let cell = resolved
            .cells
            .iter()
            .find(|cell| cell.row == 0 && cell.col == 0)
            .expect("row 0 col 0 is in the resolved grid");
        assert_eq!(
            cell.bg,
            (
                tab.snapshot.background.r,
                tab.snapshot.background.g,
                tab.snapshot.background.b
            ),
            "the rebuilt row resolves against the new default, not the cached one"
        );

        supervisor.close(78);
    }

    /// `tab.dump_resolved` densifies the sparse per-row cells back into a
    /// full grid. It is the one consumer that has to re-derive a cell's row
    /// from its grid position now that `DrawCell` no longer carries one, so
    /// it gets its own coverage: dense, row-major, and each cell resolved
    /// against the row it actually came from.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolved_cells_densifies_the_grid_row_major_from_the_row_index() {
        let (feed_tx, _) = engine_feed::channel();
        let (mut tab, supervisor) = attach_test_terminal(79, feed_tx);
        tab.write_vt(b"\x1b[2J\x1b[H");
        // Row 3 (1-based) col 2 (1-based) — off both axes' origin, so a
        // transposed or off-by-one index cannot coincide with the truth.
        tab.write_vt(b"\x1b[3;2H\x1b[1;41mX\x1b[0m");
        tab.refresh_snapshot().expect("refresh");

        let resolved = tab.resolved_cells();
        assert_eq!(resolved.cols, DEFAULT_COLS);
        assert_eq!(resolved.rows, DEFAULT_ROWS);
        assert_eq!(
            resolved.cells.len(),
            usize::from(DEFAULT_COLS) * usize::from(DEFAULT_ROWS),
            "the resolved grid is dense"
        );
        for (index, cell) in resolved.cells.iter().enumerate() {
            assert_eq!(cell.row, (index / usize::from(DEFAULT_COLS)) as u32);
            assert_eq!(cell.col, (index % usize::from(DEFAULT_COLS)) as u16);
        }

        let marked = &resolved.cells[2 * usize::from(DEFAULT_COLS) + 1];
        assert_eq!(marked.text, "X");
        assert!(marked.bold);
        assert!(marked.has_explicit_bg);
        let red = tab.theme.palette[1];
        assert_eq!(
            marked.bg,
            (red.r, red.g, red.b),
            "SGR 41 resolves through the theme palette's red"
        );

        let neighbor = &resolved.cells[2 * usize::from(DEFAULT_COLS)];
        assert_eq!(neighbor.text, " ");
        assert!(!neighbor.has_explicit_bg);
        assert!(!neighbor.bold);
        assert_eq!(
            neighbor.bg,
            (
                tab.snapshot.background.r,
                tab.snapshot.background.g,
                tab.snapshot.background.b
            ),
            "an untouched cell falls back to the terminal default"
        );

        supervisor.close(79);
    }

    /// A single-row write with the cursor already parked on that row must
    /// rebuild exactly one row — the headline claim `refresh_snapshot`'s
    /// per-row cache makes. The cursor is parked and settled *before* the
    /// write under test because libghostty dirties both the row the cursor
    /// leaves and the row it lands on (pinned by
    /// `crates/roost-vt/tests/render_dirty_test.rs`'s
    /// `row_flags_are_cleared_alongside_the_global_layer`); moving and
    /// writing in the same step would fold that cursor-motion row into the
    /// count this test is trying to isolate.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_single_row_write_with_the_cursor_already_parked_rebuilds_exactly_that_row() {
        let (feed_tx, _) = engine_feed::channel();
        let (mut tab, supervisor) = attach_test_terminal(80, feed_tx);
        tab.write_vt(b"\x1b[2J\x1b[H");
        tab.refresh_snapshot().expect("settle the cleared grid");

        tab.write_vt(b"\x1b[3;1H");
        tab.refresh_snapshot()
            .expect("settle with the cursor parked on row 2");

        let rebuilt_before = tab.render_stats.rows_rebuilt;
        let cells_before = tab.render_stats.cells_walked;
        tab.write_vt(b"X");
        tab.refresh_snapshot()
            .expect("refresh after the single-row write");

        assert_eq!(
            tab.render_stats.rows_rebuilt - rebuilt_before,
            1,
            "the cursor was already on row 2, so writing to it dirties only that row"
        );
        assert_eq!(
            tab.render_stats.cells_walked - cells_before,
            u64::from(DEFAULT_COLS),
            "walk_dirty hands the whole row's cells to the one rebuilt row"
        );

        supervisor.close(80);
    }

    /// `set_theme` bumps `theme_generation`, and `refresh_snapshot`'s
    /// `cached_theme_generation` guard exists precisely to force a full
    /// rebuild off that bump — today nothing but the default fg/bg pair
    /// (already covered by the default-color guard) is theme-derived, but
    /// the guard is there so a future theme-derived input (e.g. a
    /// `bold_color` override, like the now-removed GTK UI's) fails safe
    /// toward over-rebuilding rather than silently keeping stale rows.
    ///
    /// Measured while writing this test: `apply_theme_candidate`'s color
    /// FFI calls (`set_color_foreground`/`background`/`cursor`/`palette`)
    /// already report `Dirty::Full` on their own at our pinned Ghostty SHA
    /// — pinned separately by `theme_color_changes_report_full` in
    /// `crates/roost-vt/tests/render_dirty_test.rs` — so with a real theme
    /// apply neither `cached_defaults` nor `cached_theme_generation` is
    /// individually load-bearing for this test (confirmed: disabling both
    /// at once still left it passing). What *does* make it fail is the
    /// same class of bug as the resize guard above — the FFI calls
    /// silently not reaching libghostty while `theme_generation` still
    /// bumps: stubbing those calls out with the generation guard in place
    /// still passed (`DEFAULT_ROWS`), and disabling the guard on top of
    /// that stub dropped the rebuild to 0.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn applying_a_theme_rebuilds_every_row() {
        let (feed_tx, _) = engine_feed::channel();
        let (mut tab, supervisor) = attach_test_terminal(81, feed_tx);
        tab.write_vt(b"\x1b[2J\x1b[Hhello");
        tab.refresh_snapshot().expect("settle the written grid");

        let rebuilt_before = tab.render_stats.rows_rebuilt;
        let dracula = Theme::load_bundled("Dracula");
        tab.set_theme(&dracula).expect("theme applies");

        assert_eq!(
            tab.render_stats.rows_rebuilt - rebuilt_before,
            u64::from(DEFAULT_ROWS),
            "set_theme's own refresh must rebuild every row, not just the changed ones"
        );

        supervisor.close(81);
    }

    /// Pins the `(cols, rows)` cache-size guard against a narrower
    /// row-count-only guard: a width-only resize leaves `self.rows`
    /// unchanged, so a guard keyed on row count alone would miss it and
    /// every cached row would keep rendering at the old column width.
    ///
    /// Measured while writing this test: at our pinned Ghostty SHA,
    /// `Terminal::resize` itself always reports `Dirty::Full` regardless of
    /// which axis moved (pinned separately by
    /// `resize_reports_full_over_the_new_row_count` in
    /// `crates/roost-vt/tests/render_dirty_test.rs`), so on the real
    /// `apply_geometry` path this guard's own `mark_full` is currently a
    /// redundant second line of defense, not the sole reason this test
    /// passes. It stops being redundant, and this test starts actually
    /// depending on it, the moment `apply_geometry`'s call into libghostty
    /// silently no-ops while `self.cols`/`self.rows` still move — verified
    /// by temporarily stubbing that call out during review: with the
    /// `(cols, rows)` guard intact the rebuild count held at
    /// `DEFAULT_ROWS`, and narrowing the guard to rows-only on top of that
    /// stub dropped it to 0.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_width_only_resize_rebuilds_every_row() {
        let (feed_tx, _) = engine_feed::channel();
        let (mut tab, supervisor) = attach_test_terminal(82, feed_tx);
        tab.write_vt(b"\x1b[2J\x1b[Hhello");
        tab.refresh_snapshot().expect("settle the written grid");

        let metrics = tab.applied_metrics.expect("installed metrics");
        let rebuilt_before = tab.render_stats.rows_rebuilt;
        let change = tab
            .apply_geometry(
                DEFAULT_COLS + 10,
                DEFAULT_ROWS,
                metrics,
                tab.metric_generation + 1,
            )
            .expect("apply a width-only geometry change")
            .expect("cols moved, so this is a real geometry change");
        assert!(
            change.grid_changed,
            "cols moved, so the grid-changed flag must fire even though rows did not"
        );
        tab.commit_geometry(change);
        tab.refresh_snapshot()
            .expect("refresh after the width-only resize");

        assert_eq!(
            tab.render_stats.rows_rebuilt - rebuilt_before,
            u64::from(DEFAULT_ROWS),
            "a width-only resize invalidates every cached row even though the row count is unchanged"
        );

        supervisor.close(82);
    }

    /// Pins that a scrolled-back viewport is never served from the stale
    /// row cache. None of `refresh_snapshot`'s three cache-key guards fire
    /// here — grid size, defaults, and theme generation are all unchanged
    /// by a page up — so this pins libghostty's own dirty reporting for a
    /// viewport move (it reports every row dirty) rather than one of this
    /// module's guards.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scrolling_back_into_history_rebuilds_every_row_and_changes_the_text() {
        let (feed_tx, _) = engine_feed::channel();
        let (mut tab, supervisor) = attach_test_terminal(83, feed_tx);
        for line in 0..(usize::from(DEFAULT_ROWS) * 3) {
            tab.write_vt(format!("history-{line:04}\r\n").as_bytes());
        }
        tab.refresh_snapshot().expect("settle at the live bottom");
        let before_text = tab.dump().rows_text;

        let rebuilt_before = tab.render_stats.rows_rebuilt;
        let route = tab
            .handle_page(PageDirection::Up)
            .expect("page up into history");
        assert!(
            matches!(
                route,
                PageRoute::LocalViewport {
                    scrolled_back: true
                }
            ),
            "enough history exists that page up must move the local viewport: {route:?}"
        );

        assert_eq!(
            tab.render_stats.rows_rebuilt - rebuilt_before,
            u64::from(DEFAULT_ROWS),
            "a viewport move rebuilds every row rather than reusing the live-bottom cache"
        );
        assert_ne!(
            tab.dump().rows_text,
            before_text,
            "the scrolled-back viewport must show different rows than the live bottom"
        );

        supervisor.close(83);
    }

    /// The headline win of the dirty-tracking change: a hover-only motion
    /// event — no button, no terminal mouse tracking — never writes to the
    /// terminal, so `refresh_snapshot` must rebuild nothing even though it
    /// still republishes the snapshot (pointer shape / hover overlay can
    /// change independently of content).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pointer_motion_refresh_with_no_terminal_change_rebuilds_zero_rows() {
        let (feed_tx, _) = engine_feed::channel();
        let (mut tab, supervisor) = attach_test_terminal(84, feed_tx);
        tab.write_vt(b"\x1b[2J\x1b[Hhello");
        tab.refresh_snapshot().expect("settle the written grid");

        let rebuilt_before = tab.render_stats.rows_rebuilt;
        let cells_before = tab.render_stats.cells_walked;
        // What `App::pointer` does for a hover-only motion: dispatch through
        // `handle_native_pointer`, then refresh.
        tab.handle_native_pointer(NativePointerDispatch {
            action: PointerAction::Motion,
            button: None,
            col: 2,
            row: 0,
            mods: 0,
            click_count: 0,
            inside: true,
            link_modifier_held: false,
        })
        .expect("hover motion dispatch");
        tab.refresh_snapshot()
            .expect("refresh after the motion event");

        assert_eq!(
            tab.render_stats.rows_rebuilt - rebuilt_before,
            0,
            "a motion event with no mouse tracking touches only overlay state, not content"
        );
        assert_eq!(
            tab.render_stats.cells_walked - cells_before,
            0,
            "and walks no cells either"
        );

        supervisor.close(84);
    }
}
