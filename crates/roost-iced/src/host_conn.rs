//! Host sessions, client side: one connection owner per connected host.
//!
//! A "host" is a saved `roost-session` daemon (`HostSnapshot` in
//! `state.json`). Connecting one mints a fresh [`HostId`] and starts a
//! task that owns the control connection, the subscribed event stream,
//! and a mirror of that session's workspace; everything it learns
//! reaches the UI through the engine feed, in the same FIFO as local
//! events.
//!
//! Four pieces, each testable on its own:
//!
//! * [`state`] — the connection state machine, the compatibility gate,
//!   and the jittered capped backoff. Pure.
//! * [`mirror`] — the client-side projection of a host's workspace,
//!   fenced against the event stream. Pure, plus the shared handle the
//!   task writes and the UI reads.
//! * [`queue`] — the per-host op queue (plan 037 §3.9). One worker, one
//!   order, flushed with errors when the connection dies.
//! * [`task`] — the connection owner that wires the three to a socket.
//!
//! # Incarnations
//!
//! [`HostId`] identifies a connection *instance*, not a host
//! (`roost-ui-model`'s `keys.rs` states the contract). Every connect
//! attempt that follows one which published data mints a fresh id, and
//! the `Connecting` state carries the id it replaces — so a consumer
//! purges the dead incarnation and rebuilds from the fresh mirror off a
//! single message. Anything still keyed on the old id (a delayed
//! callback, a queued message) then matches nothing and is dropped by
//! the ordinary lookup, which is the whole staleness mechanism.
//!
//! # What C4 deliberately does not build
//!
//! The attach data plane. A host tab's bytes and snapshot payloads are
//! C5's, because the decoder and the hydrated `Terminal` are
//! main-thread-only; C4 stops at handing C5 a live control client with
//! the lease, an ordered queue to mint `tab.attach` tokens on, and a
//! mirror that already knows which tabs exist.
//!
//! # How C5/C6/C7 consume this
//!
//! * **State** is pulled, not pushed. [`HostConnSet::mirror`] and
//!   [`HostConnSet::connected`] hand out an [`Arc<SharedMirror>`]; a
//!   consumer calls [`SharedMirror::read`] while it draws and gets
//!   whatever the connection task has written by then. There is no
//!   per-commit snapshot to keep, and none to go stale.
//! * **Wakes** are pushed, and they coalesce.
//!   [`HostWorkspaceEvent::Applied`] says the mirror moved; several may
//!   describe commits the mirror has already passed by the time the
//!   drain runs, which is correct — the mirror is the authority.
//! * **Per-commit facts** ride the item, not the mirror.
//!   `Applied::events` is the batch verbatim, and it is the only place
//!   C5's `tab.effect` and C8's `notification.fired` exist. A consumer
//!   that skips items loses them; a consumer that reads the mirror
//!   instead never had them.

// The C5/C6/C7 seam. Everything a connection needs to *run* is live
// from C4 — launch auto-reconnect drives the whole task — but the
// accessors the sidebar, the palette verbs and the attach path will
// call have no caller yet. One module-level expectation rather than
// thirty item-level ones; it fires as unfulfilled the moment the last
// of them is used, which is exactly when it should be deleted.
#![expect(
    dead_code,
    reason = "the host-client API surface C5/C6/C7 consume lands with them"
)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use roost_ipc::messages::{ops, EventEnvelope, OscColorsParams};
use roost_ui_model::keys::HostId;
use roost_ui_model::theme::Theme;

pub(crate) mod mirror;
pub(crate) mod queue;
pub(crate) mod state;
pub(crate) mod task;

pub(crate) use mirror::SharedMirror;
pub(crate) use queue::{HostOpError, HostOps};
pub(crate) use state::HostConnState;
pub(crate) use task::{ConnectMode, Shutdown};

/// A host's mirror moving forward, as it reaches the UI.
///
/// The mirror itself does **not** travel on the feed: the task writes
/// [`SharedMirror`] in place and this is only the notification that it
/// moved, so a chatty host cannot pile full-workspace clones onto an
/// unbounded channel. A consumer reads the current state through
/// [`HostConnSet::mirror`] at drain time.
#[derive(Debug)]
pub(crate) enum HostWorkspaceEvent {
    /// The mirror was built or rebuilt from a fenced `tab.list`.
    /// Everything keyed on the previous contents is stale — the server
    /// is authoritative, so this is purge-then-rebuild, never a merge.
    ///
    /// It carries the handle because this is the one item that
    /// introduces it: a reset is the only point at which the set learns
    /// which [`SharedMirror`] an incarnation reads from.
    Reset(Arc<SharedMirror>),
    /// One commit applied. The mirror already reflects it.
    ///
    /// `events` is the batch verbatim, because the mirror deliberately
    /// models workspace facts only: `tab.effect` (C5's to apply) and
    /// `notification.fired` (C8's) ride here rather than being folded
    /// away. They are exact per-commit even though the mirror behind
    /// them may already be further ahead.
    Applied {
        revision: u64,
        events: Vec<EventEnvelope>,
    },
}

/// Which connection an incarnation was minted by: the saved host, and
/// *which* of that host's connections.
///
/// The generation is what makes the attribution safe across a rapid
/// disconnect + reconnect. Aborting a task is not instantaneous, so the
/// replaced task can still mint and publish after its replacement is
/// live; keyed on the host name alone those publications would land on
/// the replacement — a stale `Connecting` purging the new mirror, a
/// stale `Connected` resurrecting a connection that is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Registration {
    host: String,
    generation: u64,
}

/// Mints connection-instance ids, and remembers which connection each
/// one belongs to.
///
/// The ownership half is not bookkeeping for its own sake: feed items
/// are tagged with the incarnation alone, so without a registry the app
/// would have to *guess* which host a `Connecting` came from — and two
/// hosts reconnecting at once would make that guess wrong. A task
/// registers its new id before it publishes anything under it, so the
/// lookup on the drain side either succeeds or is genuinely stale.
///
/// One per app, shared by every host, so two hosts' incarnations can
/// never collide.
#[derive(Debug, Clone)]
pub(crate) struct HostIdMinter {
    next: Arc<AtomicU32>,
    owners: Arc<Mutex<HashMap<HostId, Registration>>>,
}

impl HostIdMinter {
    pub(crate) fn new() -> Self {
        Self {
            // 1: `HostId::LOCAL` is 0 and reserved.
            next: Arc::new(AtomicU32::new(1)),
            owners: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// A fresh incarnation, registered to one connection of `host`.
    pub(crate) fn mint(&self, host: &str, generation: u64) -> HostId {
        let mut raw = self.next.fetch_add(1, Ordering::Relaxed);
        // 32 bits of instance ids is ~4 billion connects; wrapping onto
        // LOCAL would alias a host's tabs onto the local workspace's, so
        // skip zero rather than let that happen.
        if raw == HostId::LOCAL.raw() {
            raw = self.next.fetch_add(1, Ordering::Relaxed);
        }
        let id = HostId::new(raw);
        self.lock().insert(
            id,
            Registration {
                host: host.to_string(),
                generation,
            },
        );
        id
    }

    fn registration(&self, id: HostId) -> Option<Registration> {
        self.lock().get(&id).cloned()
    }

    fn forget_id(&self, id: HostId) {
        self.lock().remove(&id);
    }

    /// Drop every registration for a host, whatever generation minted
    /// it.
    ///
    /// By name and not by generation on purpose: this runs at
    /// disconnect and at the start of a reconnect, *before* the
    /// replacement task exists, so it is also what reaps the ids a
    /// previous generation minted on its way out. Anything the outgoing
    /// task registers after this point is dropped by the generation
    /// check rather than by being absent, and the next `forget` sweeps
    /// it up.
    fn forget_host(&self, host: &str) {
        self.lock().retain(|_, owner| owner.host != host);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<HostId, Registration>> {
        self.owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The `target` spelling that means "this machine's own session".
///
/// Everything else is a socket path — including, in HS-2, the local end
/// of an `ssh -L` forward, which is what makes a remote machine
/// reachable with no SSH code on this side (HS-3 makes `ssh://` targets
/// first-class).
pub(crate) const LOCALHOST_TARGET: &str = "localhost";

/// Resolve a saved host's `target` to a socket path, and say whether it
/// is this machine's own session.
///
/// The localhost/remote answer is load-bearing twice over: it is the
/// only host this client may spawn, and the only one whose drops
/// auto-retry.
pub(crate) fn resolve_target(target: &str) -> anyhow::Result<(PathBuf, bool)> {
    if target == LOCALHOST_TARGET {
        let profile = roost_ipc::paths::BundleProfile::session()?;
        return Ok((profile.socket_path, true));
    }
    Ok((PathBuf::from(target), false))
}

/// The client's terminal palette in the wire's spelling.
///
/// The same colors [`crate::app::terminal_tab`] seeds a local terminal
/// with — a host's server-side terminals answer queries from this, so
/// the two ends agree on what "color 4" is.
pub(crate) fn theme_colors(theme: &Theme) -> OscColorsParams {
    let hex = |color: roost_vt::ColorRgb| format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b);
    OscColorsParams {
        foreground: hex(theme.foreground),
        background: hex(theme.background),
        cursor: hex(theme.cursor),
        palette: theme.palette.iter().copied().map(hex).collect(),
    }
}

/// One saved host's connection, as the app holds it.
struct HostConn {
    label: String,
    socket: PathBuf,
    localhost: bool,
    /// Which of this host's connections this is. Every id the task
    /// mints is registered under it, and the drain side drops anything
    /// stamped with an older one.
    generation: u64,
    ops: HostOps,
    shutdown: Arc<Shutdown>,
    /// The incarnation currently being served, once its `Connecting`
    /// has been drained off the feed.
    incarnation: Option<HostId>,
    state: HostConnState,
}

impl Drop for HostConn {
    fn drop(&mut self) {
        // Quitting disconnects; it never stops the session (D8).
        //
        // Signal, don't abort. The task's own shutdown closes its queue
        // and answers every intent still on it with `Disconnected`; an
        // abort here would instead drop those reply channels, and a
        // caller awaiting one would see a bare cancellation. The signal
        // is a guarantee rather than a request because the task bounds
        // its own unwind against it (`task::SHUTDOWN_GRACE`), so there
        // is nothing left for a hard abort to protect.
        self.shutdown.request();
    }
}

/// Every connected host, and the mirrors their tasks publish.
///
/// Inert with zero hosts: nothing is spawned, nothing is drained, and
/// the sidebar is exactly today's (roadmap D8's zero-change rule).
pub(crate) struct HostConnSet {
    runtime: tokio::runtime::Handle,
    feed: crate::engine_feed::EngineFeedSender,
    minter: HostIdMinter,
    /// The client's palette, shared with every task so a reconnect
    /// seeds the theme that is current *now*, not the one at spawn.
    theme: Arc<Mutex<OscColorsParams>>,
    client_build: String,
    /// Keyed on the saved host's stable id (`HostSnapshot.id`).
    conns: HashMap<String, HostConn>,
    mirrors: HashMap<HostId, Arc<SharedMirror>>,
    /// Handed out by [`HostConnSet::connect`], never reused. See
    /// [`Registration`].
    next_generation: u64,
}

impl HostConnSet {
    pub(crate) fn new(
        runtime: tokio::runtime::Handle,
        feed: crate::engine_feed::EngineFeedSender,
        theme: &Theme,
    ) -> Self {
        Self {
            runtime,
            feed,
            minter: HostIdMinter::new(),
            theme: Arc::new(Mutex::new(theme_colors(theme))),
            client_build: roost_vt::libghostty_build(),
            conns: HashMap::new(),
            mirrors: HashMap::new(),
            next_generation: 0,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.conns.is_empty()
    }

    /// Start (or restart) a connection to one saved host.
    ///
    /// An existing connection for the same saved host is dropped first —
    /// its task is aborted and its incarnation forgotten — so "Connect"
    /// on an already-connected host is a deliberate reconnect, which on
    /// this wire is a takeover.
    pub(crate) fn connect(
        &mut self,
        host: &str,
        label: &str,
        socket: PathBuf,
        localhost: bool,
        mode: ConnectMode,
    ) {
        // The incarnation this reconnect displaces, threaded into the
        // replacement task so its FIRST `Connecting` carries it — that
        // is the one message consumers purge dead-incarnation state off,
        // and without it an explicit reconnect would leak everything the
        // old connection's tabs left behind (attach state, terminals,
        // inbox rows).
        let supersedes = self.conns.get(host).and_then(|conn| conn.incarnation);
        self.forget(host);
        self.next_generation += 1;
        let generation = self.next_generation;

        let (ops, ops_rx) = HostOps::channel();
        let shutdown = Arc::new(Shutdown::default());
        let config = task::ConnectionConfig {
            host: host.to_string(),
            label: label.to_string(),
            socket: socket.clone(),
            localhost,
            generation,
            supersedes,
            mode,
            client_build: self.client_build.clone(),
            theme: Arc::clone(&self.theme),
        };
        // Detached on purpose: the task owns its own shutdown, bounds it
        // (`task::SHUTDOWN_GRACE`), and answers its queue on the way
        // out, so a handle kept here would only be a way to cut that
        // short. `HostConn::drop` signals instead.
        self.runtime.spawn(task::run(
            config,
            self.minter.clone(),
            ops_rx,
            self.feed.clone(),
            Arc::clone(&shutdown),
        ));

        self.conns.insert(
            host.to_string(),
            HostConn {
                label: label.to_string(),
                socket,
                localhost,
                generation,
                ops,
                shutdown,
                incarnation: None,
                // What the task is actually doing the moment it is
                // spawned. The feed's first `Connecting` replaces it —
                // this is only what a frame drawn in between reads, so
                // it must not be a state the machine cannot produce.
                state: HostConnState::Connecting { previous: None },
            },
        );
    }

    /// Stop driving a host. Disconnect is never Stop: the session keeps
    /// running, and its tabs with it.
    /// Returns the incarnation that was live, so the caller can purge
    /// the app state keyed on it (attach machinery, client terminals,
    /// inbox rows) — a disconnect with no reconnect never publishes a
    /// `Connecting { previous }` for consumers to purge off. C7's
    /// disconnect verb is the caller.
    pub(crate) fn disconnect(&mut self, host: &str) -> Option<HostId> {
        let incarnation = self.conns.get(host).and_then(|conn| conn.incarnation);
        self.forget(host);
        incarnation
    }

    /// Drop the connection and everything keyed on its incarnation.
    fn forget(&mut self, host: &str) {
        if let Some(conn) = self.conns.remove(host) {
            if let Some(incarnation) = conn.incarnation {
                self.mirrors.remove(&incarnation);
            }
            // `Drop` notifies and aborts.
        }
        self.minter.forget_host(host);
    }

    fn purge(&mut self, incarnation: HostId) {
        self.mirrors.remove(&incarnation);
        self.minter.forget_id(incarnation);
    }

    /// The op queue for a host, by saved id.
    pub(crate) fn ops(&self, host: &str) -> Option<&HostOps> {
        self.conns.get(host).map(|conn| &conn.ops)
    }

    /// Enqueue one control-plane op on a host's queue.
    ///
    /// The single dispatch point plan 037 §3.9 calls for on the host
    /// side: C6/C7's mutation call sites route a host-owned intent here
    /// and a local one to `LocalClient` as today. A host that is not
    /// connected answers the intent rather than dropping it, so a caller
    /// awaiting the reply never waits forever.
    pub(crate) fn send(&self, host: &str, intent: queue::HostIntent) -> Result<(), HostOpError> {
        match self.ops(host) {
            Some(ops) => ops.send(intent),
            None => {
                intent.answer(Err(HostOpError::Unavailable));
                Err(HostOpError::Unavailable)
            }
        }
    }

    /// The op queue for whichever host owns this incarnation.
    pub(crate) fn ops_for(&self, incarnation: HostId) -> Option<&HostOps> {
        let host = self.owner_of(incarnation)?;
        self.ops(&host)
    }

    pub(crate) fn state(&self, host: &str) -> Option<&HostConnState> {
        self.conns.get(host).map(|conn| &conn.state)
    }

    /// The incarnation currently serving a saved host, if it is
    /// connected.
    pub(crate) fn incarnation(&self, host: &str) -> Option<HostId> {
        self.conns.get(host).and_then(|conn| conn.incarnation)
    }

    /// The live mirror for an incarnation. C6/C7 read it through
    /// [`SharedMirror::read`] at draw time; there is no per-commit copy
    /// to hold on to.
    pub(crate) fn mirror(&self, incarnation: HostId) -> Option<&Arc<SharedMirror>> {
        self.mirrors.get(&incarnation)
    }

    /// Connected hosts, as `(saved id, label, incarnation, mirror)`.
    /// What C6's sidebar sections iterate.
    pub(crate) fn connected(
        &self,
    ) -> impl Iterator<Item = (&str, &str, HostId, &Arc<SharedMirror>)> {
        self.conns.iter().filter_map(|(host, conn)| {
            let incarnation = conn.incarnation.filter(|_| conn.state.is_connected())?;
            let mirror = self.mirrors.get(&incarnation)?;
            Some((host.as_str(), conn.label.as_str(), incarnation, mirror))
        })
    }

    /// Where a saved host lives, and whether it is this machine's own.
    pub(crate) fn endpoint(&self, host: &str) -> Option<(&std::path::Path, bool)> {
        self.conns
            .get(host)
            .map(|conn| (conn.socket.as_path(), conn.localhost))
    }

    /// The socket a live incarnation's data connections dial — owned, so
    /// an attach task can carry it off the main thread. `None` for a
    /// stale incarnation, same contract as [`Self::ops_for`].
    pub(crate) fn endpoint_for(&self, incarnation: HostId) -> Option<std::path::PathBuf> {
        let host = self.owner_of(incarnation)?;
        self.endpoint(&host).map(|(socket, _)| socket.to_path_buf())
    }

    /// The theme changed. The slot every task re-reads on reconnect is
    /// updated first, then every connected host is told — so a session
    /// that reconnects during the round trip still gets the new colors.
    pub(crate) fn set_theme(&mut self, theme: &Theme) {
        let colors = theme_colors(theme);
        {
            let mut slot = self
                .theme
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *slot = colors.clone();
        }
        for conn in self.conns.values() {
            // Lease-gated, and it rides the same queue as everything
            // else so it cannot interleave with an attach.
            let _ = conn.ops.send(
                queue::HostIntent::new(
                    ops::SESSION_SET_THEME,
                    serde_json::json!({ "osc_colors": colors }),
                )
                .with_lease(),
            );
        }
    }

    /// Drain one `EngineFeed::HostState`. Returns the saved host it
    /// belongs to, or `None` when the incarnation is stale — an item
    /// minted by a connection this set has since dropped.
    pub(crate) fn apply_state(
        &mut self,
        incarnation: HostId,
        next: HostConnState,
    ) -> Option<String> {
        // Attribution first: a `Connecting` from a replaced connection
        // must not purge the live one's mirror on its way to being
        // dropped.
        let host = self.owner_of(incarnation)?;
        // The reconnect contract: purge the dead incarnation the moment
        // the new attempt starts, so nothing keyed on it survives into
        // the rebuild that follows.
        if let HostConnState::Connecting {
            previous: Some(previous),
        } = &next
        {
            self.purge(*previous);
        }

        let conn = self.conns.get_mut(&host)?;
        conn.incarnation = Some(incarnation);
        conn.state = next;
        Some(host)
    }

    /// Drain one `EngineFeed::HostWorkspace`.
    ///
    /// Only a reset touches this set: the task writes the mirror in
    /// place, so an applied batch is a wake plus the envelopes C5 routes
    /// off the feed item itself. Nothing here needs the batch, and
    /// nothing here may hold it — a per-commit copy is exactly the
    /// unbounded growth the shared mirror exists to avoid.
    pub(crate) fn apply_workspace(&mut self, incarnation: HostId, event: HostWorkspaceEvent) {
        if self.owner_of(incarnation).is_none() {
            return;
        }
        if let HostWorkspaceEvent::Reset(mirror) = event {
            self.mirrors.insert(incarnation, mirror);
        }
    }

    /// Which live connection an incarnation belongs to. `None` for one
    /// this set no longer holds — the stale-key drop pattern
    /// (`app.rs:2903`), applied to whole connections.
    ///
    /// "No longer holds" covers two cases, and the second is the subtle
    /// one: the host may be gone, or it may have been *replaced* while
    /// the previous task was still winding down. Both are stale, and
    /// both are dropped here rather than landing on the replacement.
    fn owner_of(&self, incarnation: HostId) -> Option<String> {
        let registration = self.minter.registration(incarnation)?;
        let Some(conn) = self.conns.get(&registration.host) else {
            tracing::debug!(
                incarnation = incarnation.raw(),
                host = %registration.host,
                "dropping an item from a host this app no longer holds"
            );
            return None;
        };
        if conn.generation != registration.generation {
            tracing::debug!(
                incarnation = incarnation.raw(),
                host = %registration.host,
                "dropping an item from a replaced connection"
            );
            return None;
        }
        Some(registration.host)
    }

    /// A fresh incarnation for a host's *current* connection, as its
    /// task would mint one.
    #[cfg(test)]
    fn mint_for(&self, host: &str) -> HostId {
        self.minter.mint(host, self.conns[host].generation)
    }
}

/// A palette of nothing, for tests that only care about the plumbing.
#[cfg(test)]
pub(crate) fn blank_theme() -> OscColorsParams {
    OscColorsParams {
        foreground: "#000000".into(),
        background: "#000000".into(),
        cursor: "#000000".into(),
        palette: vec!["#000000".into(); 256],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_incarnations_are_unique_and_never_local() {
        let minter = HostIdMinter::new();
        let ids: Vec<HostId> = (0..64).map(|_| minter.mint("h1", 1)).collect();
        let mut seen = std::collections::HashSet::new();
        for id in ids {
            assert!(
                !id.is_local(),
                "a minted instance never aliases the local one"
            );
            assert!(seen.insert(id), "{id:?} was minted twice");
        }
    }

    /// The whole point of minting per connect: the same numeric tab id
    /// under two incarnations is two keys.
    #[test]
    fn two_incarnations_of_one_host_are_two_id_spaces() {
        let minter = HostIdMinter::new();
        let first = minter.mint("h1", 1);
        let second = minter.mint("h1", 2);
        assert_ne!(first, second);
        assert_ne!(
            roost_ui_model::keys::TabKey::new(first, 7),
            roost_ui_model::keys::TabKey::new(second, 7)
        );
    }

    /// A set on the test's own runtime. The receiver comes back with it
    /// so the caller keeps the feed alive: a dropped receiver makes
    /// every task's first publish fail, which would end the connections
    /// these cases are driving by hand.
    fn a_set() -> (HostConnSet, crate::engine_feed::EngineFeedReceiver) {
        let (feed, rx) = crate::engine_feed::channel();
        let set = HostConnSet::new(
            tokio::runtime::Handle::current(),
            feed,
            &Theme::roost_dark(),
        );
        (set, rx)
    }

    /// The drain side resolves a feed item's host off the minter's
    /// registration, so two hosts connecting at once cannot have their
    /// states swapped — the failure a "whichever host is waiting"
    /// heuristic would produce.
    #[tokio::test]
    async fn two_hosts_connecting_at_once_keep_their_own_states() {
        let (mut set, _feed) = a_set();
        set.connect(
            "h1",
            "one",
            PathBuf::from("/nonexistent/roost-set-one.sock"),
            false,
            ConnectMode::Dial,
        );
        set.connect(
            "h2",
            "two",
            PathBuf::from("/nonexistent/roost-set-two.sock"),
            false,
            ConnectMode::Dial,
        );

        let first = set.mint_for("h1");
        let second = set.mint_for("h2");
        assert_eq!(
            set.apply_state(second, HostConnState::Connected).as_deref(),
            Some("h2")
        );
        assert_eq!(
            set.apply_state(first, HostConnState::TakenOver).as_deref(),
            Some("h1")
        );
        assert_eq!(set.state("h1"), Some(&HostConnState::TakenOver));
        assert_eq!(set.state("h2"), Some(&HostConnState::Connected));
        assert_eq!(set.incarnation("h2"), Some(second));
    }

    /// The staleness mechanism end to end: `Connecting` purges the
    /// incarnation it replaces, and anything still arriving under that
    /// id afterwards is dropped rather than applied to the new one.
    #[tokio::test]
    async fn a_reconnect_purges_the_old_incarnation_and_drops_its_stragglers() {
        let (mut set, _feed) = a_set();
        set.connect(
            "h1",
            "one",
            PathBuf::from("/nonexistent/roost-set-purge.sock"),
            false,
            ConnectMode::Dial,
        );

        let old = set.mint_for("h1");
        set.apply_state(old, HostConnState::Connected);
        set.apply_workspace(old, HostWorkspaceEvent::Reset(Arc::default()));
        assert!(set.mirror(old).is_some());

        let new = set.mint_for("h1");
        assert_eq!(
            set.apply_state(
                new,
                HostConnState::Connecting {
                    previous: Some(old)
                }
            )
            .as_deref(),
            Some("h1")
        );
        assert!(
            set.mirror(old).is_none(),
            "the dead incarnation's mirror is purged, not merged forward"
        );

        // A straggler from the old epoch — a batch already in flight when
        // the reconnect began.
        set.apply_workspace(old, HostWorkspaceEvent::Reset(Arc::default()));
        assert!(set.mirror(old).is_none());
        assert_eq!(set.apply_state(old, HostConnState::Connected), None);
        assert!(
            matches!(set.state("h1"), Some(HostConnState::Connecting { .. })),
            "a stale state must not resurrect a connection"
        );
    }

    /// The rapid disconnect + reconnect case. Winding a task down is not
    /// instantaneous, so the replaced one can still mint and publish
    /// after its replacement is live — and it must land nowhere. Keyed
    /// on the host name alone these would hit the replacement: a stale
    /// `Connecting` purging the fresh mirror, a stale `Connected`
    /// resurrecting a connection that is gone.
    #[tokio::test]
    async fn a_replaced_connection_cannot_publish_onto_its_replacement() {
        let (mut set, _feed) = a_set();
        let socket = PathBuf::from("/nonexistent/roost-set-replaced.sock");
        set.connect("h1", "one", socket.clone(), false, ConnectMode::Dial);
        let outgoing = set.conns["h1"].generation;

        // The user hits Connect again before the first task has wound
        // down.
        set.connect("h1", "one", socket, false, ConnectMode::Dial);
        let live = set.mint_for("h1");
        set.apply_state(live, HostConnState::Connected);
        set.apply_workspace(live, HostWorkspaceEvent::Reset(Arc::default()));

        // Only now does the outgoing task get around to its next
        // attempt, under the same saved host.
        let stale = set.minter.mint("h1", outgoing);
        assert_eq!(
            set.apply_state(
                stale,
                HostConnState::Connecting {
                    previous: Some(live)
                }
            ),
            None,
            "a replaced connection's state must not be attributed to its \
             replacement"
        );
        assert!(
            set.mirror(live).is_some(),
            "and it must not purge the live incarnation on the way out"
        );
        assert_eq!(set.state("h1"), Some(&HostConnState::Connected));
        assert_eq!(set.incarnation("h1"), Some(live));

        set.apply_workspace(stale, HostWorkspaceEvent::Reset(Arc::default()));
        assert!(set.mirror(stale).is_none());
        assert!(set.ops_for(stale).is_none());
        assert!(set.ops_for(live).is_some());
    }

    /// Disconnecting drops everything keyed on the host, so a late item
    /// from its task lands nowhere.
    #[tokio::test]
    async fn a_disconnected_host_stops_accepting_its_own_items() {
        let (mut set, _feed) = a_set();
        set.connect(
            "h1",
            "one",
            PathBuf::from("/nonexistent/roost-set-drop.sock"),
            false,
            ConnectMode::Dial,
        );
        let incarnation = set.mint_for("h1");
        set.apply_state(incarnation, HostConnState::Connected);
        set.apply_workspace(incarnation, HostWorkspaceEvent::Reset(Arc::default()));

        set.disconnect("h1");
        assert!(set.is_empty());
        assert!(set.mirror(incarnation).is_none());
        assert_eq!(set.apply_state(incarnation, HostConnState::Connected), None);
        assert!(set.ops("h1").is_none());
        assert!(set.ops_for(incarnation).is_none());
    }

    /// Zero hosts is the zero-change baseline: nothing is spawned, and
    /// every accessor answers empty rather than inventing a host.
    #[tokio::test]
    async fn a_set_with_no_hosts_is_inert() {
        let (set, _feed) = a_set();
        assert!(set.is_empty());
        assert_eq!(set.connected().count(), 0);
        assert!(set.state("anything").is_none());
        assert!(set.endpoint("anything").is_none());
        assert!(set.mirror(HostId::new(1)).is_none());
    }

    #[test]
    fn a_theme_renders_as_the_wires_hex_spelling() {
        let theme = Theme::roost_dark();
        let colors = theme_colors(&theme);
        assert_eq!(
            colors.palette.len(),
            256,
            "a short palette is invalid-param"
        );
        for value in std::iter::once(&colors.foreground)
            .chain([&colors.background, &colors.cursor])
            .chain(colors.palette.iter())
        {
            assert_eq!(value.len(), 7, "{value}");
            assert!(value.starts_with('#'), "{value}");
            assert!(value[1..].chars().all(|c| c.is_ascii_hexdigit()), "{value}");
        }
        assert_eq!(
            colors.background,
            format!(
                "#{:02x}{:02x}{:02x}",
                theme.background.r, theme.background.g, theme.background.b
            )
        );
    }
}
