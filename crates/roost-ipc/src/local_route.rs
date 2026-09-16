//! Where the UI's own "local" tabs live, as a snapshot its IPC handler
//! can read (plan 063 §D1).
//!
//! This lives here rather than beside the `local-backend` config key it
//! mirrors because `roost-engine` — which answers `identify` from it —
//! does not depend on `roost-ui-model`. Both crates depend on this one,
//! so this is the only place a type can be shared by them.

use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::paths::BundleProfile;

/// Which local backend the UI is running its own tabs on.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocalBackendMode {
    /// PTYs in the UI process — the original arrangement (DL-4).
    #[default]
    InProcess,
    /// PTYs in a `roost-session` daemon on this machine, reached
    /// through *the slot*: the first saved host with a localhost
    /// transport.
    Session,
}

impl LocalBackendMode {
    /// The one spelling: the `local-backend` config value, the
    /// `identify.local_backend` wire string, and what `roostctl doctor`
    /// prints are all this.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InProcess => "in-process",
            Self::Session => "session",
        }
    }
}

impl std::fmt::Display for LocalBackendMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where the local host session listens.
///
/// Resolved from the session bundle profile rather than from any live
/// connection: the path is a property of the profile, so it is knowable
/// even when the slot is down or the UI has yet to publish anything.
/// Both the UI (publishing a route) and the engine (answering
/// `identify`) need the same answer, which is why it is here and not
/// beside either of them.
pub fn session_socket_path() -> Option<String> {
    match BundleProfile::session() {
        Ok(profile) => Some(profile.socket_path.to_string_lossy().into_owned()),
        Err(error) => {
            tracing::warn!(%error, "cannot resolve the local session socket path");
            None
        }
    }
}

/// What the UI currently knows about the local backend, as one
/// immutable snapshot.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LocalRoute {
    pub mode: LocalBackendMode,
    /// The slot's socket path. `None` under
    /// [`LocalBackendMode::InProcess`], where there is no slot.
    pub slot_socket: Option<String>,
    /// The slot's live incarnation — the `h<n>` half of the ref a bare
    /// id is rewritten to (plan 063 §D10). `None` under
    /// [`LocalBackendMode::InProcess`], and under `session` whenever the
    /// slot is not connected, which is what makes "not connected" one
    /// question rather than two.
    ///
    /// Never on any wire: an incarnation is a session-local counter, and
    /// this field exists only so the handler and the UI name the same
    /// connection inside one process.
    pub slot_host: Option<u32>,
    /// The slot's UI-selected `(project_id, tab_id)`, which is what a
    /// bare id means on the UI socket under
    /// [`LocalBackendMode::Session`]. `None` when nothing is selected.
    pub slot_active: Option<(i64, i64)>,
    /// Which phase of a local-backend switch the UI is in, or `None`
    /// when it is idle (plan 063 §D8a).
    ///
    /// A string rather than an enum because the phases are the UI's:
    /// the state machine lives in `roost-iced`, and duplicating its
    /// variants here would be a second spelling that could drift. This
    /// crate only carries the name so `identify` can report it — the
    /// one thing outside the UI process that can see a switch is in
    /// flight, and therefore why a mutation was refused.
    pub switch: Option<&'static str>,
}

/// The shared cell the UI writes and the IPC handler reads.
///
/// A `RwLock<Arc<..>>` rather than an `ArcSwap`: the write rate is one
/// per mode/selection change and the read is a cheap `Arc` clone, which
/// does not justify a new dependency.
#[derive(Debug, Default)]
pub struct LocalBackendCell(RwLock<Arc<LocalRoute>>);

impl LocalBackendCell {
    pub fn new(route: LocalRoute) -> Self {
        Self(RwLock::new(Arc::new(route)))
    }

    /// The current snapshot.
    pub fn load(&self) -> Arc<LocalRoute> {
        Arc::clone(&self.read())
    }

    /// Replace the snapshot. Readers already holding one keep it; they
    /// see the new route on their next [`Self::load`].
    pub fn store(&self, route: LocalRoute) {
        *self.write() = Arc::new(route);
    }

    // A poisoned lock is recovered rather than propagated: the cell
    // holds a whole snapshot that is replaced in one assignment, so a
    // panic elsewhere cannot have left it half-written, and refusing to
    // answer `identify` over it would be the worse failure.
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Arc<LocalRoute>> {
        self.0.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Arc<LocalRoute>> {
        self.0.write().unwrap_or_else(|e| e.into_inner())
    }
}

// ── §D10: what a UI socket does with each op under `session` ────────

/// Where a bare id in an op's params has to be rewritten so the UI's
/// own answer lands on the slot instead of on the workspace `session`
/// mode does not draw.
///
/// The variants name *fields*, not ops: two ops sharing a shape share a
/// row, and adding an op means saying which shape it has rather than
/// writing a second rewriter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotIds {
    /// One field holding a [`crate::messages::WireTabRef`] spelling.
    Tab(&'static str),
    /// `tab.reorder`: `project_id` plus every ref in `tab_ids`.
    TabOrder,
    /// `project.reorder`: every ref in `project_ids`.
    ProjectOrder,
    /// The id is a bare `string_int64` that **cannot** spell
    /// `h<n>.<id>`, so there is nothing for this boundary to rewrite:
    /// the UI resolves the bare id against the slot itself when it turns
    /// it into a key. Listed rather than folded into `UiOwned` because
    /// the op *does* act on a tab, and a reader checking §D10's table
    /// has to see that the rewrite happens — just one layer up.
    ResolvedByTheUi,
}

/// What a UI socket does with one op while `local-backend = session`
/// (plan 063 §D10).
///
/// Exhaustive over `crate::messages::ops` by test
/// (`every_op_constant_is_classified`), which is the load-bearing
/// mitigation the plan's §9 names: an op nobody classified would
/// otherwise answer, silently and wrongly, against the empty in-process
/// workspace this mode hides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpClass {
    /// The whole request goes to the slot and its reply comes back. A
    /// bare id therefore *means* the slot's id.
    Forward,
    /// The UI answers, after the bare ids named here are rewritten to
    /// the slot's `h<n>.<id>` form. A ref that is already
    /// host-qualified is left exactly as it arrived, which is what
    /// keeps an explicit `h<n>` op off the slot.
    Rewrite(SlotIds),
    /// The UI answers, and the mode changes nothing: the op carries no
    /// local workspace id at all.
    UiOwned,
    /// Not served on a UI socket, before this mode and after it —
    /// `events.subscribe` (`not-implemented`) and `tab.attach`
    /// (`unknown-op`; a UI socket mints no tickets).
    Unsupported,
    /// A host-session socket's own op. A UI socket answers `unknown-op`,
    /// which is how a client tells the two sockets apart.
    SessionOnly,
    /// An event name, not an op — nothing dispatches it.
    Event,
}

/// Plan 063 §D10's table, one row per `ops` constant.
///
/// The op *strings* are written out rather than referenced as
/// `ops::FOO`, so the test that parses `messages.rs` compares two
/// independently-written spellings instead of one constant against
/// itself.
pub const OP_CLASSES: &[(&str, OpClass)] = &[
    // `identify` is answered here and is already slot-aware: under
    // `session` its `active_*` come from `LocalRoute::slot_active`.
    ("identify", OpClass::UiOwned),
    // The bare-id workspace ops. Each one, answered locally, would read
    // or write the hidden in-process workspace — and `tab.open` with
    // `project_id: 0` would *create a project there* to land in
    // (`ensure_default_project`), which is why forwarding the params
    // verbatim is the decision and not an accident: the slot mints its
    // own default project exactly as a client on the session socket
    // would get.
    ("tab.open", OpClass::Forward),
    ("tab.close", OpClass::Forward),
    ("tab.list", OpClass::Forward),
    ("tab.write", OpClass::Forward),
    ("tab.resize", OpClass::Forward),
    ("project.create", OpClass::Forward),
    ("project.rename", OpClass::Forward),
    ("project.delete", OpClass::Forward),
    ("tab.set_title", OpClass::Forward),
    ("tab.set_state", OpClass::Forward),
    ("tab.clear_notification", OpClass::Forward),
    ("tab.set_hook_active", OpClass::Forward),
    ("tab.agent_report", OpClass::Forward),
    // The request op, not the `tab.notification` event: an agent raising
    // attention on a bare id means the slot's tab.
    ("notification.create", OpClass::Forward),
    // Client-side by nature: a terminal this UI attached, a selection it
    // holds, files it reads off *this* filesystem, an order its sidebar
    // paints. Forwarding any of them would ask the session about state
    // it does not have.
    ("tab.dump", OpClass::Rewrite(SlotIds::Tab("tab_id"))),
    (
        "tab.dump_resolved",
        OpClass::Rewrite(SlotIds::Tab("tab_id")),
    ),
    (
        "tab.capture_pty_input",
        OpClass::Rewrite(SlotIds::Tab("tab_id")),
    ),
    ("tab.focus", OpClass::Rewrite(SlotIds::Tab("tab_id"))),
    ("tab.send_file", OpClass::Rewrite(SlotIds::Tab("tab"))),
    ("tab.reorder", OpClass::Rewrite(SlotIds::TabOrder)),
    ("project.reorder", OpClass::Rewrite(SlotIds::ProjectOrder)),
    // Same family, bare-`string_int64` params — see
    // [`SlotIds::ResolvedByTheUi`].
    ("selection.set", OpClass::Rewrite(SlotIds::ResolvedByTheUi)),
    (
        "selection.clear",
        OpClass::Rewrite(SlotIds::ResolvedByTheUi),
    ),
    ("selection.dump", OpClass::Rewrite(SlotIds::ResolvedByTheUi)),
    (
        "tab.feed_pty_bytes",
        OpClass::Rewrite(SlotIds::ResolvedByTheUi),
    ),
    ("tab.feed_ime", OpClass::Rewrite(SlotIds::ResolvedByTheUi)),
    (
        "tab.expand_selection_at",
        OpClass::Rewrite(SlotIds::ResolvedByTheUi),
    ),
    (
        "tab.dispatch_mouse_event",
        OpClass::Rewrite(SlotIds::ResolvedByTheUi),
    ),
    // No local workspace id to mean anything by.
    ("app.activate", OpClass::UiOwned),
    ("app.screenshot", OpClass::UiOwned),
    ("app.window_metrics", OpClass::UiOwned),
    ("app.sidebar_dump", OpClass::UiOwned),
    ("app.render_stats", OpClass::UiOwned),
    ("palette.open", OpClass::UiOwned),
    ("palette.state", OpClass::UiOwned),
    ("palette.query", OpClass::UiOwned),
    ("palette.activate", OpClass::UiOwned),
    ("palette.dismiss", OpClass::UiOwned),
    ("palette.present", OpClass::UiOwned),
    ("clipboard.dump", OpClass::UiOwned),
    ("clipboard.write", OpClass::UiOwned),
    ("window.resize", OpClass::UiOwned),
    // The sidebar's own width, which belongs to this window and not to
    // whatever backend its tabs run on.
    ("sidebar.set_width", OpClass::UiOwned),
    ("app.set_window_focus", OpClass::UiOwned),
    ("app.cursor_shape", OpClass::UiOwned),
    ("app.active_terminal_focused", OpClass::UiOwned),
    ("app.dock_badge", OpClass::UiOwned),
    ("app.selected_tab_id", OpClass::UiOwned),
    ("app.menu_dump", OpClass::UiOwned),
    ("app.menu_activate", OpClass::UiOwned),
    ("app.update_status", OpClass::UiOwned),
    ("app.update_check", OpClass::UiOwned),
    ("app.notification_status", OpClass::UiOwned),
    ("app.dialog_dump", OpClass::UiOwned),
    ("app.dialog_answer", OpClass::UiOwned),
    ("app.keybind_dispatch", OpClass::UiOwned),
    // Sets *this* machine's own `agent-hooks` key and raises every
    // connected host to match — a property of this UI's config and its
    // host registry, neither of which the slot has any view of.
    ("agent.set_hooks", OpClass::UiOwned),
    // Host registry + connections: client state, and the slot is one of
    // the rows. Forwarding would ask the session about a registry it
    // does not keep.
    ("host.add", OpClass::UiOwned),
    ("host.remove", OpClass::UiOwned),
    ("host.list", OpClass::UiOwned),
    ("host.connect", OpClass::UiOwned),
    ("host.disconnect", OpClass::UiOwned),
    ("host.status", OpClass::UiOwned),
    // Deliberately not forwarded even though the slot would serve them.
    // A subscription handed out here would be a stream this socket
    // cannot fence (`tab.list`'s `revision` is stripped for the same
    // reason), and a ticket is authority over a connection this socket
    // does not own. A client that wants either dials
    // `identify.local_session_socket`.
    ("events.subscribe", OpClass::Unsupported),
    ("tab.attach", OpClass::Unsupported),
    ("session.identify", OpClass::SessionOnly),
    ("session.stop", OpClass::SessionOnly),
    ("session.set_theme", OpClass::SessionOnly),
    ("session.set_focus", OpClass::SessionOnly),
    ("session.set_agent_hooks", OpClass::SessionOnly),
    ("session.put_file", OpClass::SessionOnly),
    ("tab.opened", OpClass::Event),
    ("tab.closed", OpClass::Event),
    ("tab.state_changed", OpClass::Event),
    ("tab.title_changed", OpClass::Event),
    ("tab.cwd_changed", OpClass::Event),
    ("tab.notification", OpClass::Event),
    ("project.created", OpClass::Event),
    ("project.renamed", OpClass::Event),
    ("project.deleted", OpClass::Event),
    ("active.changed", OpClass::Event),
    ("hook_active.changed", OpClass::Event),
    ("notification.fired", OpClass::Event),
    ("agent_report.changed", OpClass::Event),
    ("tabs.reordered", OpClass::Event),
    ("projects.reordered", OpClass::Event),
    ("tab.effect", OpClass::Event),
    ("workspace.durability_changed", OpClass::Event),
];

/// The code a UI socket answers with when an op meant for the slot
/// cannot be put to it (plan 063 §D10): the slot is down, the launch has
/// not dialled it yet, or a switch has quiesced it.
///
/// `host-unavailable` rather than a code of its own. The slot *is* a
/// host, this is already that code's meaning, and
/// `docs/reference/ipc.md` tells a client to treat any code outside the
/// UI socket's documented list as fatal for the request — so inventing
/// one here would put a code on the wire that this socket's own contract
/// says cannot appear there.
pub const SLOT_UNAVAILABLE_CODE: &str = "host-unavailable";

/// Its sentence. One spelling, because the engine's handler and the UI's
/// forward arm both answer it and a client matches on it.
pub const SLOT_UNAVAILABLE: &str = "local session is not connected";

/// The row for `op`, or `None` for a name no row covers.
///
/// `None` is the safe answer and the reason the enumeration test is
/// load-bearing: an unclassified op is neither forwarded nor rewritten,
/// so it behaves exactly as it did before this mode existed — which for
/// a workspace op means answering against the hidden in-process
/// workspace. The table is what turns that from a silent wrong answer
/// into a test failure.
pub fn classify(op: &str) -> Option<OpClass> {
    OP_CLASSES
        .iter()
        .find(|(name, _)| *name == op)
        .map(|(_, class)| *class)
}

/// The request named a bare id, and there is no connected slot for it to
/// mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotRequired;

/// Whether this request carries a bare local id that [`rewrite_slot_ids`]
/// would re-address.
///
/// Asked **before** the slot is required, and that order is the fix for
/// a straight regression against `in-process`: `tab.dump {"tab_id":
/// "h9.7"}` names host 9 and has never had anything to do with the local
/// backend, so a slot that is down must not refuse it. Only a bare id
/// needs a slot to mean something.
fn wants_the_slot(ids: SlotIds, params: &serde_json::Value) -> bool {
    match ids {
        // The param is a bare `string_int64` by construction — there is
        // no host-qualified spelling of it to except.
        SlotIds::ResolvedByTheUi => true,
        SlotIds::Tab(field) => params.get(field).is_some_and(bare_tab),
        SlotIds::TabOrder => {
            bare_project(params.get("project_id"))
                && list_items(params.get("tab_ids")).is_some_and(|items| items.iter().all(bare_tab))
        }
        SlotIds::ProjectOrder => list_items(params.get("project_ids"))
            .is_some_and(|items| items.iter().all(bare_project_value)),
    }
}

/// Rewrite the bare ids `ids` names to the slot's `h<slot>.<id>` form,
/// in place.
///
/// Total and quiet: anything that is not a bare ref in the shape this
/// row describes is left exactly as it arrived, so a malformed request
/// still fails in `decode` with the error it has always had, and a
/// **host-qualified** request is routed by the op's own parser as if
/// this mode did not exist — including when there is no slot at all,
/// which is what [`wants_the_slot`] is asked first for.
///
/// The multi-ref rows are all-or-nothing on purpose. Their parsers
/// refuse a mix of bare and qualified refs; rewriting only the bare half
/// of a mixed list would turn that refusal into a reorder against
/// whichever host the other half named.
pub fn rewrite_slot_ids(
    ids: SlotIds,
    params: &mut serde_json::Value,
    slot: Option<u32>,
) -> Result<(), SlotRequired> {
    if !wants_the_slot(ids, params) {
        return Ok(());
    }
    let Some(slot) = slot else {
        return Err(SlotRequired);
    };
    match ids {
        SlotIds::ResolvedByTheUi => {}
        SlotIds::Tab(field) => {
            if let Some(value) = params.get_mut(field) {
                rewrite_tab(value, slot);
            }
        }
        SlotIds::TabOrder => {
            if let Some(value) = params.get_mut("project_id") {
                rewrite_project(value, slot);
            }
            for value in list_items_mut(params.get_mut("tab_ids")) {
                rewrite_tab(value, slot);
            }
        }
        SlotIds::ProjectOrder => {
            for value in list_items_mut(params.get_mut("project_ids")) {
                rewrite_project(value, slot);
            }
        }
    }
    Ok(())
}

fn list_items(value: Option<&serde_json::Value>) -> Option<&Vec<serde_json::Value>> {
    value?.as_array()
}

fn list_items_mut(value: Option<&mut serde_json::Value>) -> &mut [serde_json::Value] {
    match value.and_then(serde_json::Value::as_array_mut) {
        Some(items) => items.as_mut_slice(),
        None => &mut [],
    }
}

fn bare_tab(value: &serde_json::Value) -> bool {
    matches!(
        value.as_str().and_then(crate::messages::WireTabRef::parse),
        Some(crate::messages::WireTabRef::Local(_))
    )
}

fn bare_project_value(value: &serde_json::Value) -> bool {
    matches!(
        value
            .as_str()
            .and_then(crate::messages::WireProjectRef::parse),
        Some(crate::messages::WireProjectRef::Local(_))
    )
}

fn bare_project(value: Option<&serde_json::Value>) -> bool {
    value.is_some_and(bare_project_value)
}

fn rewrite_tab(value: &mut serde_json::Value, slot: u32) {
    if let Some(crate::messages::WireTabRef::Local(id)) =
        value.as_str().and_then(crate::messages::WireTabRef::parse)
    {
        *value = serde_json::Value::String(
            crate::messages::WireTabRef::Host {
                host: slot,
                tab: id,
            }
            .to_string(),
        );
    }
}

fn rewrite_project(value: &mut serde_json::Value, slot: u32) {
    if let Some(crate::messages::WireProjectRef::Local(id)) = value
        .as_str()
        .and_then(crate::messages::WireProjectRef::parse)
    {
        *value = serde_json::Value::String(
            crate::messages::WireProjectRef::Host {
                host: slot,
                project: id,
            }
            .to_string(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spelling is the contract: the config value, the `identify`
    /// field, and the doctor line are the same three strings.
    #[test]
    fn a_mode_spells_itself_the_same_way_everywhere() {
        assert_eq!(LocalBackendMode::InProcess.as_str(), "in-process");
        assert_eq!(LocalBackendMode::Session.as_str(), "session");
        assert_eq!(LocalBackendMode::Session.to_string(), "session");
        assert_eq!(
            serde_json::to_string(&LocalBackendMode::InProcess).unwrap(),
            "\"in-process\""
        );
        assert_eq!(
            serde_json::from_str::<LocalBackendMode>("\"session\"").unwrap(),
            LocalBackendMode::Session
        );
    }

    #[test]
    fn every_handle_on_the_cell_sees_the_last_route_stored() {
        let cell = Arc::new(LocalBackendCell::new(LocalRoute::default()));
        let reader = Arc::clone(&cell);
        assert_eq!(reader.load().mode, LocalBackendMode::InProcess);
        assert_eq!(reader.load().slot_socket, None);

        cell.store(LocalRoute {
            mode: LocalBackendMode::Session,
            slot_socket: Some("/run/roost/session.sock".into()),
            slot_host: Some(4),
            slot_active: Some((3, 7)),
            switch: Some("replaying"),
        });

        let seen = reader.load();
        assert_eq!(seen.mode, LocalBackendMode::Session);
        assert_eq!(seen.slot_socket.as_deref(), Some("/run/roost/session.sock"));
        assert_eq!(seen.slot_host, Some(4));
        assert_eq!(seen.slot_active, Some((3, 7)));
        assert_eq!(seen.switch, Some("replaying"));
    }

    // ── §D10's table ────────────────────────────────────────────────

    /// Every `pub const` in `messages::ops`, read out of the source file
    /// rather than out of this module.
    ///
    /// Parsing the source is the point. A test that walked
    /// [`OP_CLASSES`] would pass for any table, including one missing
    /// the op somebody added this morning — which is exactly the
    /// failure plan 063 §9 asks this test to catch.
    fn ops_declared_in_the_source() -> Vec<(String, String)> {
        let source = include_str!("messages.rs");
        let start = source
            .find("\npub mod ops {\n")
            .expect("messages.rs declares `pub mod ops`");
        let body = &source[start..];
        let end = body.find("\n}\n").expect("the ops module closes");
        let pattern = regex_lite_const_lines(&body[..end]);
        assert!(
            pattern.len() > 50,
            "only {} constants parsed out of the ops module - the parser has \
             drifted from the source and would pass vacuously",
            pattern.len()
        );
        pattern
    }

    /// `pub const NAME: &str = "value";`, without pulling in a regex
    /// crate for four lines of scanning.
    fn regex_lite_const_lines(body: &str) -> Vec<(String, String)> {
        body.lines()
            .filter_map(|line| {
                let rest = line.trim().strip_prefix("pub const ")?;
                let (name, rest) = rest.split_once(": &str = ")?;
                let value = rest.strip_prefix('"')?.strip_suffix("\";")?;
                Some((name.to_string(), value.to_string()))
            })
            .collect()
    }

    #[test]
    fn every_op_constant_is_classified() {
        let missing: Vec<_> = ops_declared_in_the_source()
            .into_iter()
            .filter(|(_, value)| classify(value).is_none())
            .map(|(name, value)| format!("ops::{name} ({value:?})"))
            .collect();
        assert!(
            missing.is_empty(),
            "plan 063 §D10's table has no row for: {}. Add one to \
             `OP_CLASSES` - an op nobody classified is answered against \
             the in-process workspace `session` mode hides.",
            missing.join(", ")
        );
    }

    /// The other direction: a row naming an op that no longer exists is
    /// a row nothing can reach, and would hide the real op's absence.
    #[test]
    fn every_row_names_a_live_op_constant() {
        let declared: Vec<String> = ops_declared_in_the_source()
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        let orphans: Vec<_> = OP_CLASSES
            .iter()
            .map(|(op, _)| *op)
            .filter(|op| !declared.iter().any(|value| value == op))
            .collect();
        assert!(orphans.is_empty(), "no `ops` constant spells: {orphans:?}");
        assert_eq!(
            OP_CLASSES.len(),
            declared.len(),
            "one row per constant, and no duplicates"
        );
    }

    #[test]
    fn the_named_rows_are_the_ones_the_plan_pinned() {
        // §D10 corrects an earlier draft on each of these by name.
        assert_eq!(classify("notification.create"), Some(OpClass::Forward));
        assert_eq!(
            classify("tab.send_file"),
            Some(OpClass::Rewrite(SlotIds::Tab("tab")))
        );
        for op in [
            "selection.set",
            "selection.clear",
            "selection.dump",
            "tab.expand_selection_at",
            "tab.feed_ime",
            "tab.feed_pty_bytes",
            "tab.capture_pty_input",
        ] {
            assert!(
                matches!(classify(op), Some(OpClass::Rewrite(_))),
                "{op} is a rewrite row"
            );
        }
        assert_eq!(classify("tab.attach"), Some(OpClass::Unsupported));
        assert_eq!(classify("events.subscribe"), Some(OpClass::Unsupported));
        for op in [
            "identify",
            "palette.activate",
            "host.status",
            "app.screenshot",
            "app.render_stats",
            "window.resize",
            "app.sidebar_dump",
        ] {
            assert_eq!(classify(op), Some(OpClass::UiOwned), "{op}");
        }
        assert_eq!(classify("tab.notification"), Some(OpClass::Event));
        assert_eq!(classify("nonesuch.op"), None);
    }

    // ── the rewrite ─────────────────────────────────────────────────

    fn rewritten(op: &str, mut params: serde_json::Value) -> serde_json::Value {
        let Some(OpClass::Rewrite(ids)) = classify(op) else {
            panic!("{op} is not a rewrite row");
        };
        rewrite_slot_ids(ids, &mut params, Some(4)).expect("a slot was offered");
        params
    }

    /// Whether a request with no slot available is refused or waved
    /// through — plan 063 §D10, and the regression it closes.
    fn without_a_slot(op: &str, mut params: serde_json::Value) -> Result<(), SlotRequired> {
        let Some(OpClass::Rewrite(ids)) = classify(op) else {
            panic!("{op} is not a rewrite row");
        };
        rewrite_slot_ids(ids, &mut params, None)
    }

    #[test]
    fn a_bare_ref_becomes_the_slots_ref() {
        assert_eq!(
            rewritten("tab.dump", serde_json::json!({"tab_id": "7"})),
            serde_json::json!({"tab_id": "h4.7"})
        );
        assert_eq!(
            rewritten(
                "tab.send_file",
                serde_json::json!({"tab": "7", "paths": []})
            ),
            serde_json::json!({"tab": "h4.7", "paths": []})
        );
        assert_eq!(
            rewritten(
                "tab.reorder",
                serde_json::json!({"project_id": "1", "tab_ids": ["7", "9"]})
            ),
            serde_json::json!({"project_id": "h4.1", "tab_ids": ["h4.7", "h4.9"]})
        );
        assert_eq!(
            rewritten(
                "project.reorder",
                serde_json::json!({"project_ids": ["1", "2"]})
            ),
            serde_json::json!({"project_ids": ["h4.1", "h4.2"]})
        );
    }

    /// The §D10 ordering clause, as a property: a request that already
    /// names a host is routed by the op's own parser, so this boundary
    /// must hand it on untouched. A rewrite here would re-address
    /// another host's reorder to the slot.
    #[test]
    fn a_host_qualified_ref_is_left_alone() {
        for (op, params) in [
            ("tab.dump", serde_json::json!({"tab_id": "h3.7"})),
            ("tab.focus", serde_json::json!({"tab_id": "h3.7"})),
            (
                "tab.reorder",
                serde_json::json!({"project_id": "h3.1", "tab_ids": ["h3.7"]}),
            ),
            (
                "project.reorder",
                serde_json::json!({"project_ids": ["h3.1", "h3.2"]}),
            ),
        ] {
            assert_eq!(rewritten(op, params.clone()), params, "{op}");
        }
    }

    /// A mixed list is the op's own `invalid-param`, and it has to stay
    /// one: rewriting the bare half would make it a valid reorder
    /// against the host the other half named.
    #[test]
    fn a_mixed_list_is_not_half_rewritten() {
        for (op, params) in [
            (
                "tab.reorder",
                serde_json::json!({"project_id": "1", "tab_ids": ["h3.7", "9"]}),
            ),
            (
                "tab.reorder",
                serde_json::json!({"project_id": "h3.1", "tab_ids": ["9"]}),
            ),
            (
                "project.reorder",
                serde_json::json!({"project_ids": ["1", "h3.2"]}),
            ),
        ] {
            assert_eq!(rewritten(op, params.clone()), params, "{op}");
        }
    }

    /// A host-qualified request has never had anything to do with the
    /// local backend, so a slot that is down must not refuse it — that
    /// would be a straight regression against `in-process`, where the
    /// same request works.
    #[test]
    fn only_a_bare_id_needs_a_slot_to_mean_anything() {
        for (op, params) in [
            ("tab.dump", serde_json::json!({"tab_id": "h9.7"})),
            ("tab.focus", serde_json::json!({"tab_id": "h9.7"})),
            (
                "tab.send_file",
                serde_json::json!({"tab": "h9.7", "paths": []}),
            ),
            (
                "tab.reorder",
                serde_json::json!({"project_id": "h9.1", "tab_ids": ["h9.7"]}),
            ),
            (
                "project.reorder",
                serde_json::json!({"project_ids": ["h9.1"]}),
            ),
            // Not a ref this row would rewrite either way, so it keeps
            // its own `decode` error rather than gaining ours.
            ("tab.dump", serde_json::json!({"tab_id": "not-an-id"})),
            ("tab.dump", serde_json::json!({})),
        ] {
            assert_eq!(without_a_slot(op, params), Ok(()), "{op}");
        }

        for (op, params) in [
            ("tab.dump", serde_json::json!({"tab_id": "7"})),
            (
                "tab.reorder",
                serde_json::json!({"project_id": "1", "tab_ids": ["7"]}),
            ),
            ("project.reorder", serde_json::json!({"project_ids": ["1"]})),
            // The bare-`string_int64` family has no qualified spelling,
            // so it always needs one.
            ("selection.dump", serde_json::json!({"tab_id": "7"})),
        ] {
            assert_eq!(without_a_slot(op, params), Err(SlotRequired), "{op}");
        }
    }

    /// Malformed params keep the error they have always had rather than
    /// gaining a rewritten field that decodes to something else.
    #[test]
    fn nothing_that_is_not_a_bare_ref_is_touched() {
        for (op, params) in [
            ("tab.dump", serde_json::json!({})),
            ("tab.dump", serde_json::json!({"tab_id": 7})),
            ("tab.dump", serde_json::json!({"tab_id": "not-an-id"})),
            ("tab.reorder", serde_json::json!({"tab_ids": ["7"]})),
            (
                "project.reorder",
                serde_json::json!({"project_ids": "nope"}),
            ),
            ("selection.set", serde_json::json!({"tab_id": "7"})),
        ] {
            assert_eq!(rewritten(op, params.clone()), params, "{op}");
        }
    }
}
