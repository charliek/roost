use std::collections::BTreeMap;

use super::*;

pub(crate) struct AgentMetricsResult {
    session: u64,
    claimed: Vec<String>,
    outcomes: Result<Vec<git_metrics::ProbeOutcome>, String>,
}

/// `Workspace::open_tab` commits and broadcasts `TabOpened` before the
/// caller's `PtySupervisor::spawn` promotes the session, so the attach the
/// event drives can land in that gap and fail. GTK ships the same bounded
/// retry for the same race (`roost-linux/src/app.rs`, #267): forty
/// attempts, 25 ms apart. Attempt one is the reconcile that noticed the
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
    tabs: BTreeMap<i64, PendingAttach>,
}

impl PendingAttachments {
    /// Record one failed attach. The first failure starts the budget; the
    /// verdict says whether a retry is still owed.
    pub(super) fn record_failure(&mut self, tab_id: i64, now: Instant) -> AttachRetryVerdict {
        let entry = self.tabs.entry(tab_id).or_insert(PendingAttach {
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
    pub(super) fn clear(&mut self, tab_id: i64) {
        self.tabs.remove(&tab_id);
    }

    /// Forget every tab the workspace no longer lists — a spawn that
    /// failed and rolled back, or a tab closed while it waited. Exhausted
    /// entries go too: the mark belongs to a tab, not to an id.
    pub(super) fn retain_live(&mut self, live_ids: &HashSet<i64>) {
        self.tabs.retain(|tab_id, _| live_ids.contains(tab_id));
    }

    /// Whether any tab is still owed a retry — the attach-retry
    /// subscription's predicate. An exhausted tab is still tracked but no
    /// longer retryable, so the timer disarms.
    pub(super) fn has_retryable(&self) -> bool {
        self.tabs.values().any(|entry| !entry.exhausted)
    }

    /// The ids still owed a retry, owned so the walk in
    /// `retry_pending_attachments` can mutate the set as it attaches.
    pub(super) fn retry_ids(&self) -> Vec<i64> {
        self.tabs
            .iter()
            .filter(|(_, entry)| !entry.exhausted)
            .map(|(tab_id, _)| *tab_id)
            .collect()
    }

    #[cfg(test)]
    fn tracked_ids(&self) -> Vec<i64> {
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
    pub(super) touched: HashSet<i64>,
    pub(super) osc_actions: Vec<(i64, Vec<OscAction>)>,
    pub(super) exited: Vec<i64>,
    pub(super) error: Option<String>,
}

pub(super) fn collect_tab_output(
    tabs: &mut HashMap<i64, TerminalTab>,
    collected: &mut TabOutputBatch,
    tab_id: i64,
    output: TabOutput,
) {
    let Some(tab) = tabs.get_mut(&tab_id) else {
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
        // remaining actions. `Bytes` is the un-opted-in shape — GTK's —
        // and cannot occur on this path; treat it as a chunk with no
        // actions rather than asserting.
        TabOutput::Bytes(bytes) => {
            tab.write_vt(&bytes);
            collected.touched.insert(tab_id);
        }
        TabOutput::Scanned { data, actions } => {
            tab.write_vt(&data);
            // Most chunks of a flood carry no OSC at all; an empty entry
            // would still cost a push and an `apply_osc_actions` hop.
            if !actions.is_empty() {
                collected.osc_actions.push((tab_id, actions));
            }
            collected.touched.insert(tab_id);
        }
        TabOutput::Exit { status, reason } => {
            tracing::info!(tab_id, status, %reason, "PTY exited");
            collected.exited.push(tab_id);
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
/// is a tab that closed between the banner and the click — GTK's
/// `focus_tab_by_id` bails on the same `focus_tab` error.
///
/// The raise is best-effort: a window that has not opened yet has no id,
/// and the focus still landed in the core either way.
fn notification_activation(
    workspace: &Workspace,
    window_id: Option<window::Id>,
    tab_id: i64,
) -> Option<UiTask> {
    if let Err(error) = focus_tab_in_core(workspace, tab_id) {
        tracing::debug!(tab_id, %error, "notification click named a tab that is gone");
        return None;
    }
    Some(window_id.map_or(UiTask::None, UiTask::Focus))
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
/// verdict as the GTK arm: reject, so the op can never report a cleared
/// badge on a platform that has none.
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
/// answers not-implemented (from `read` itself) like GTK does, not
/// not-enabled.
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

impl App {
    pub(super) fn reconcile(&mut self) {
        // A full authoritative snapshot on every reconcile is the recovery
        // path for a slow consumer — a lagged broadcast arrives as a
        // `Resync` and this rebuild is what heals it: deltas are an
        // optimization, never UI truth.
        self.projects = self.workspace.snapshot();
        // Every pill-relevant change (title, active, notification) lands
        // here, so this is where the elision memo is refreshed. Gated on a
        // window: the bootstrap reconcile runs before the chrome fonts are
        // registered, and a measurement taken then would be cached wrong
        // forever — `window_opened` does the first populate instead.
        if self.window_id.is_some() {
            self.refresh_pill_labels();
        }
        self.request_exit_if_empty();
        reconcile_confirm_delete(&mut self.confirm_delete, &self.projects);
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
        self.refresh_sidebar_agents();
        self.refresh_agent_palette();
        let live_ids: HashSet<i64> = self
            .projects
            .iter()
            .flat_map(|project| project.tabs.iter().map(|tab| tab.id))
            .collect();
        self.tabs.retain(|tab_id, _| live_ids.contains(tab_id));
        self.pending_attachments.retain_live(&live_ids);
        let active_tab_id = self.workspace.active().1;
        self.request_tab_reveal(active_tab_id);
        for (tab_id, tab) in &mut self.tabs {
            if *tab_id != active_tab_id && tab.reset_pointer_state() {
                refresh_or_warn(*tab_id, tab, "pointer reset after active tab changed");
            }
        }
        // Every focus change funnels through `focus_tab_and_clear`, which
        // reconciles — so this is the one place a tab switch cancels a
        // composition the user left behind.
        cancel_preedits(&mut self.tabs, &mut self.ime_discard, Some(active_tab_id));
        let now = Instant::now();
        for tab_id in &live_ids {
            if self.tabs.contains_key(tab_id) {
                continue;
            }
            self.attach_tab_tracked(*tab_id, now);
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

    /// Queue a strip reveal when the OBSERVED active tab changed. Hooked
    /// to reconcile rather than to a focus helper on purpose: `tab.focus`
    /// over IPC mutates the workspace in its handler and reaches the UI
    /// only as a broadcast, so a UI-side funnel would miss exactly the
    /// path the missing reveal was reported on.
    fn request_tab_reveal(&mut self, active_tab_id: i64) {
        if self.revealed_tab_id == Some(active_tab_id) {
            return;
        }
        self.revealed_tab_id = Some(active_tab_id);
        self.tab_reveal_request = Some(active_tab_id);
    }

    /// One attach attempt for a tab the workspace lists but the UI has no
    /// terminal for. Returns whether the tab is attached now — reconcile
    /// and the retry driver share this so a retry cannot drift from the
    /// attempt that preceded it.
    fn attach_tab(&mut self, tab_id: i64) -> bool {
        // Loading the theme and building the terminal costs a 256-entry
        // palette, a scrollback and the encoders — all discarded when
        // `TabSession::attach` consults this very map and finds nothing.
        // The retry driver runs 40 times a second, so ask first.
        if !self.supervisor.has(tab_id) {
            tracing::debug!(tab_id, "PTY not ready for UI attach");
            return false;
        }
        let attached = {
            let _guard = self.runtime.enter();
            TerminalTab::attach(
                Arc::clone(&self.supervisor),
                tab_id,
                self.test_mode,
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
                self.tabs.insert(tab_id, tab);
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
    fn attach_tab_tracked(&mut self, tab_id: i64, now: Instant) {
        if self.attach_tab(tab_id) {
            self.pending_attachments.clear(tab_id);
            return;
        }
        if let AttachRetryVerdict::Exhausted { attempts, waited } =
            self.pending_attachments.record_failure(tab_id, now)
        {
            tracing::warn!(
                tab_id,
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
        for tab_id in self.pending_attachments.retry_ids() {
            if self.tabs.contains_key(&tab_id) {
                self.pending_attachments.clear(tab_id);
                continue;
            }
            // Ask the workspace, not the last snapshot: a spawn that
            // failed rolls the tab back without the UI having reconciled.
            if self.workspace.tab(tab_id).is_err() {
                tracing::debug!(
                    tab_id,
                    "dropped a pending attach the workspace no longer lists"
                );
                self.pending_attachments.clear(tab_id);
                continue;
            }
            self.attach_tab_tracked(tab_id, now);
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
                EngineFeed::Tab(tab_id, output) => {
                    collect_tab_output(&mut self.tabs, &mut pty, tab_id, output);
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
                EngineFeed::AgentMetrics(result) => self.apply_agent_metrics(result),
                EngineFeed::Provider(result) => self.apply_provider_result(*result),
                EngineFeed::NotificationActivated { tab_id } => {
                    if let Some(raise) =
                        notification_activation(&self.workspace, self.window_id, tab_id)
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
        for (tab_id, actions) in pty.osc_actions {
            task = task.then(self.apply_osc_actions(tab_id, actions));
        }
        for tab_id in &pty.touched {
            if let Some(tab) = self.tabs.get_mut(tab_id) {
                refresh_or_warn(*tab_id, tab, "PTY output");
            }
        }
        if batch.should_reconcile() {
            self.reconcile();
        }
        for tab_id in pty.exited {
            let _ = self.workspace.close_tab(tab_id);
            self.tabs.remove(&tab_id);
        }
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
                if let Some((project_id, row_title)) = self.notification_title(tab_id) {
                    self.notification_inbox
                        .upsert(notification_inbox::NotificationRecord::new(
                            tab_id,
                            project_id,
                            row_title,
                            body.clone(),
                        ));
                }
                self.desktop_notifications.fire(tab_id, title, body);
            }
            WorkspaceEvent::TabNotification {
                tab_id,
                has_pending: false,
            } => {
                // A clear keeps the tab's server id: the banner may still
                // be on the desktop, and GTK's constant per-tab id makes a
                // later re-notify replace it — forgetting the id here
                // would stack a duplicate beside it instead.
                self.notification_inbox.remove(tab_id);
            }
            WorkspaceEvent::TabClosed { tab_id } => {
                self.notification_inbox.remove(tab_id);
                self.desktop_notifications.retire(tab_id);
            }
            WorkspaceEvent::ProjectDeleted { project_id } => {
                let stale: Vec<i64> = self
                    .notification_inbox
                    .snapshot()
                    .iter()
                    .filter(|record| record.project_id == project_id)
                    .map(|record| record.tab_id)
                    .collect();
                for tab_id in stale {
                    self.notification_inbox.remove(tab_id);
                }
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

    fn notification_title(&self, tab_id: i64) -> Option<(i64, String)> {
        self.workspace.snapshot().into_iter().find_map(|project| {
            project.tabs.into_iter().find_map(|tab| {
                (tab.id == tab_id).then(|| {
                    let tab_title = if !tab.title.is_empty() {
                        tab.title
                    } else if !tab.cwd.is_empty() {
                        tab.cwd
                    } else {
                        "Tab".to_string()
                    };
                    (
                        project.id,
                        notification_inbox::compose_title(&project.name, &tab_title),
                    )
                })
            })
        })
    }

    fn reconcile_notification_inbox(&mut self) {
        let pending_rows: Vec<(i64, i64, String)> = self
            .projects
            .iter()
            .flat_map(|project| {
                project
                    .tabs
                    .iter()
                    .filter(|tab| tab.has_notification)
                    .map(move |tab| {
                        let tab_title = if !tab.title.is_empty() {
                            tab.title.clone()
                        } else if !tab.cwd.is_empty() {
                            tab.cwd.clone()
                        } else {
                            "Tab".to_string()
                        };
                        (
                            tab.id,
                            project.id,
                            notification_inbox::compose_title(&project.name, &tab_title),
                        )
                    })
            })
            .collect();
        let pending_ids: HashSet<i64> = pending_rows.iter().map(|row| row.0).collect();
        let stale: Vec<i64> = self
            .notification_inbox
            .tab_ids()
            .into_iter()
            .filter(|tab_id| !pending_ids.contains(tab_id))
            .collect();
        for tab_id in stale {
            self.notification_inbox.remove(tab_id);
        }

        let existing: HashSet<i64> = self.notification_inbox.tab_ids().into_iter().collect();
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
        while let Some((tab_id, project_id, title)) = additions.pop() {
            self.notification_inbox
                .upsert(notification_inbox::NotificationRecord::new(
                    tab_id, project_id, title, "",
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
            let (active_project_id, active_tab_id) = self.workspace.active();
            if self
                .menu_window_rows
                .matches(&self.projects, active_project_id, active_tab_id)
            {
                return;
            }
            let rows = crate::macos::menu::WindowRows::derive(
                &self.projects,
                active_project_id,
                active_tab_id,
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

    fn refresh_sidebar_agents(&mut self) {
        let active_tab = self.workspace.active().1;
        let now = agent_palette::now_unix();
        self.sidebar_agents = self
            .projects
            .iter()
            .map(|project| {
                let rows = agent_palette::sidebar_agents(project, now)
                    .into_iter()
                    .map(|row| SidebarDumpAgentRow {
                        tab_id: row.tab_id,
                        name: row.name,
                        lifecycle: row.lifecycle,
                        status_text: row.status_text,
                        time_text: row.time_text,
                        is_active: row.tab_id == active_tab,
                    })
                    .collect();
                (project.id, rows)
            })
            .collect();
    }

    fn sidebar_dump(&self) -> SidebarDumpResult {
        let projects = self
            .workspace
            .snapshot()
            .into_iter()
            .map(|project| SidebarDumpProject {
                project_id: project.id,
                agents: self
                    .sidebar_agents
                    .get(&project.id)
                    .cloned()
                    .unwrap_or_default(),
            })
            .collect();
        SidebarDumpResult {
            agents_visible: self.config.show_sidebar_agents,
            projects,
        }
    }

    pub(super) fn apply_metrics_cache(
        &self,
        cwds: &HashMap<i64, String>,
        items: &mut [palette::PaletteItem],
    ) {
        for item in items {
            let Some(tab_id) = agent_palette::agent_tab_id(&item.id) else {
                continue;
            };
            let (Some(agent), Some(cwd)) = (item.agent.as_mut(), cwds.get(&tab_id)) else {
                continue;
            };
            agent.metrics_text = self
                .metrics_cache
                .text_for_session(self.palette_session, cwd)
                .map(str::to_string);
        }
    }

    pub(super) fn spawn_agent_metrics(&mut self, cwds: &HashMap<i64, String>) {
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
                    .get(&tab_id)
                    .map(TerminalTab::dump)
                    .ok_or_else(|| format!("tab {tab_id} has no live terminal"));
                let _ = reply.send(result);
            }
            UiRequest::TabFeedPtyBytes {
                tab_id,
                data,
                reply,
            } => {
                // Same ordering as the feed batch's tail: an OSC action can
                // mutate the tab (pointer shape), so it lands before the
                // refresh that publishes it, never after. That second
                // lookup is the price of handing `self` to `apply_osc_actions`.
                let result = if !self.test_mode {
                    Err("ROOST_TEST_MODE=1 is required".to_string())
                } else if let Some(actions) = self
                    .tabs
                    .get_mut(&tab_id)
                    .map(|tab| tab.scan_and_write_vt(&data))
                {
                    task = task.then(self.apply_osc_actions(tab_id, actions));
                    self.tabs
                        .get_mut(&tab_id)
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
                    .get(&tab_id)
                    .and_then(|tab| tab.input_capture.as_ref())
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
                        KeyboardRoute::Terminal(active_tab) if active_tab == tab_id => {
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
                        KeyboardRoute::Terminal(active_tab) => Err(format!(
                            "tab {tab_id} is not the active terminal \
                                 (keyboard route owns tab {active_tab})"
                        )),
                        KeyboardRoute::None
                        | KeyboardRoute::Confirm
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
                    .get(&tab_id)
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
                    .get(&self.workspace.active().1)
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
                let activation = self.activate_palette(&id);
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
                // The rename rows open the inline editor from here too.
                task = task
                    .then(activation.task)
                    .then(self.take_rename_focus_task());
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
                    .get_mut(&tab_id)
                    .ok_or_else(|| format!("tab {tab_id} has no live terminal"))
                    .and_then(|tab| {
                        if !tab.selection.set(&tab.terminal, anchor, cursor) {
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
                    .get_mut(&tab_id)
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
                    .get_mut(&tab_id)
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
                        .get_mut(&tab_id)
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
                                .get_mut(&tab_id)
                                .ok_or_else(|| format!("tab {tab_id} has no live terminal"))?
                                .dispatch_pointer(kind, button, cell_x, cell_y, mods)
                                .map_err(|error| error.to_string())
                        })
                };
                let _ = reply.send(result);
            }
        }
        task
    }

    pub(super) fn apply_osc_actions(&mut self, tab_id: i64, actions: Vec<OscAction>) -> UiTask {
        for action in actions {
            match action {
                OscAction::Workspace { command, payload } => {
                    self.client.apply_osc(tab_id, command, &payload);
                }
                OscAction::PtyInput(bytes) => {
                    if let Some(tab) = self.tabs.get(&tab_id) {
                        tab.session.send_input(bytes);
                    }
                }
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
                    if let Some(tab) = self.tabs.get_mut(&tab_id) {
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
    use super::*;

    /// These tests hand `collect_tab_output` the items a forwarder would
    /// have delivered, so the feed receiver is surplus.
    fn attached(tab_id: i64) -> (HashMap<i64, TerminalTab>, Arc<PtySupervisor>) {
        let (feed_tx, _) = engine_feed::channel();
        let (tab, supervisor) = attach_test_terminal(tab_id, feed_tx);
        (HashMap::from([(tab_id, tab)]), supervisor)
    }

    /// The race the retry exists for, against a real supervisor: the
    /// workspace lists a tab before `PtySupervisor::spawn` promotes its
    /// session, so the first attach fails and a later one succeeds. This
    /// is the attempt/verdict sequence `attach_tab_tracked` drives.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tab_whose_pty_is_not_spawned_yet_attaches_on_a_retry() {
        let supervisor = Arc::new(PtySupervisor::new());
        let (feed_tx, _feed_rx) = engine_feed::channel();
        let attach = |feed: EngineFeedSender| {
            TerminalTab::attach(
                Arc::clone(&supervisor),
                75,
                true,
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
            pending.record_failure(75, started),
            AttachRetryVerdict::Retry
        );
        assert!(pending.has_retryable(), "the retry subscription is armed");
        assert_eq!(pending.retry_ids(), vec![75]);

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
        pending.clear(75);
        assert!(
            !pending.has_retryable(),
            "success disarms the retry subscription"
        );
        assert!(pending.tracked_ids().is_empty());

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
            pending.record_failure(76, started + ATTACH_RETRY_INTERVAL * attempt);
        }
        assert!(!pending.has_retryable(), "the budget is spent");
        assert_eq!(pending.tracked_ids(), vec![76], "but the tab is remembered");

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
            Arc::clone(&supervisor),
            76,
            true,
            Theme::roost_dark_fallback(),
            roost_ui_model::word_selection::DEFAULT_EXTRA_WORD_CHARS.to_string(),
            feed_tx,
        )
        .expect("an exhausted mark never blocks the attach itself");
        pending.clear(76);
        assert!(
            pending.tracked_ids().is_empty(),
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
                pending.record_failure(9, started + ATTACH_RETRY_INTERVAL * attempt),
                AttachRetryVerdict::Retry,
                "attempt {attempt} is inside the budget"
            );
        }
        let last = started + ATTACH_RETRY_INTERVAL * ATTACH_RETRY_LIMIT;
        assert_eq!(
            pending.record_failure(9, last),
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
            pending.tracked_ids(),
            vec![9],
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
                pending.record_failure(9, started + ATTACH_RETRY_INTERVAL * attempt),
                AttachRetryVerdict::Exhausted { .. }
            ) {
                exhaustions += 1;
            }
        }
        assert_eq!(exhaustions, 1);

        let later = started + ATTACH_RETRY_WINDOW * 10;
        for step in 0..100 {
            assert_eq!(
                pending.record_failure(9, later + ATTACH_RETRY_INTERVAL * step),
                AttachRetryVerdict::GaveUp,
                "a later reconcile neither restarts the budget nor re-warns"
            );
        }
        assert!(!pending.has_retryable(), "and never re-arms the timer");
        assert_eq!(pending.tracked_ids(), vec![9]);

        pending.retain_live(&HashSet::new());
        assert!(
            pending.tracked_ids().is_empty(),
            "a closed tab is pruned exhausted mark and all"
        );
        assert_eq!(
            pending.record_failure(9, later),
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
                pending.record_failure(9, now),
                AttachRetryVerdict::Retry,
                "attempts alone cannot end the wall-clock window"
            );
        }
        assert!(
            pending.has_retryable(),
            "the tab is still waiting for its PTY"
        );
        assert!(matches!(
            pending.record_failure(9, now + ATTACH_RETRY_WINDOW),
            AttachRetryVerdict::Exhausted { .. }
        ));
    }

    #[test]
    fn pending_attachments_follow_the_workspace_and_stay_in_a_stable_order() {
        let mut pending = PendingAttachments::default();
        let now = Instant::now();
        for tab_id in [7, 3, 5] {
            assert_eq!(
                pending.record_failure(tab_id, now),
                AttachRetryVerdict::Retry
            );
        }
        assert_eq!(pending.retry_ids(), vec![3, 5, 7]);

        pending.retain_live(&HashSet::from([3, 7]));
        assert_eq!(
            pending.retry_ids(),
            vec![3, 7],
            "a tab the workspace dropped is never retried"
        );
        pending.clear(3);
        pending.clear(7);
        assert!(pending.tracked_ids().is_empty());
        pending.retain_live(&HashSet::from([1]));
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
            70,
            TabOutput::Bytes(b"\x1b[2J\x1b[Hhello".to_vec()),
        );

        assert_eq!(collected.touched, HashSet::from([70]));
        assert!(
            collected.osc_actions.is_empty(),
            "plain output carries no OSC, so the tail has nothing to apply"
        );
        assert!(collected.exited.is_empty());
        assert!(collected.error.is_none());
        let tab = tabs.get_mut(&70).expect("the tab is still attached");
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
            75,
            TabOutput::Scanned {
                data: b"\x1b[2J\x1b[Hhello".to_vec(),
                actions: vec![OscAction::PointerShape("pointer".into())],
            },
        );

        assert_eq!(collected.touched, HashSet::from([75]));
        assert_eq!(
            collected.osc_actions,
            vec![(75, vec![OscAction::PointerShape("pointer".into())])]
        );
        let tab = tabs.get_mut(&75).expect("the tab is still attached");
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
            collect_tab_output(&mut tabs, &mut collected, 999, output);
        }

        assert!(collected.touched.is_empty());
        assert!(collected.osc_actions.is_empty());
        assert!(collected.exited.is_empty());
        assert!(collected.error.is_none());
        supervisor.close(71);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_exit_is_collected_for_close_and_an_error_becomes_a_status() {
        let (mut tabs, supervisor) = attached(72);
        let mut collected = TabOutputBatch::default();

        collect_tab_output(
            &mut tabs,
            &mut collected,
            72,
            TabOutput::Exit {
                status: 0,
                reason: "shell exited".into(),
            },
        );
        collect_tab_output(
            &mut tabs,
            &mut collected,
            72,
            TabOutput::Error("broadcast lagged: dropped 3 message(s)".into()),
        );

        assert_eq!(collected.exited, vec![72]);
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

    /// Accumulate one tab's PTY bytes off `rx` until `needle` shows up or
    /// the window elapses. Returns what was seen either way, so the same
    /// helper serves the positive and the negative assertion.
    async fn feed_text_until(
        rx: &mut EngineFeedReceiver,
        tab_id: i64,
        needle: &str,
        window: Duration,
    ) -> String {
        let deadline = Instant::now() + window;
        let mut seen = String::new();
        while Instant::now() < deadline && !seen.contains(needle) {
            let mut batch = EngineBatch::default();
            while let Some(item) = rx.try_next(&mut batch) {
                if let EngineFeed::Tab(
                    id,
                    TabOutput::Bytes(bytes) | TabOutput::Scanned { data: bytes, .. },
                ) = item
                {
                    if id == tab_id {
                        seen.push_str(&String::from_utf8_lossy(&bytes));
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        seen
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
            Arc::clone(&supervisor),
            74,
            true,
            Theme::roost_dark_fallback(),
            roost_ui_model::word_selection::DEFAULT_EXTRA_WORD_CHARS.to_string(),
            live_feed,
        )
        .expect("re-attach the PTY the discarded tab left running");
        tab.session.send_input(b"forwarder-marker\n".to_vec());

        let live =
            feed_text_until(&mut live_rx, 74, "forwarder-marker", Duration::from_secs(5)).await;
        assert!(
            live.contains("forwarder-marker"),
            "the re-attached tab is the live stream: {live:?}"
        );

        // The marker has already round-tripped, so a forwarder that
        // outlived its tab has had its chance; this window is slack, not
        // synchronisation.
        let stale = feed_text_until(
            &mut discarded_rx,
            74,
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
        let raise = notification_activation(&workspace, Some(window), clicked.id)
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
            notification_activation(&workspace, None, other.id),
            Some(UiTask::None)
        ));
        assert_eq!(workspace.active().1, other.id);

        workspace.close_tab(clicked.id).expect("close the tab");
        assert!(
            notification_activation(&workspace, Some(window), clicked.id).is_none(),
            "a banner outliving its tab is a no-op"
        );
        assert_eq!(workspace.active().1, other.id, "and moves nothing");
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
    /// the guard is there so a future theme-derived input (e.g. GTK's
    /// `bold_color` override) fails safe toward over-rebuilding rather than
    /// silently keeping stale rows.
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
