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
        TabOutput::Bytes(bytes) => {
            let actions = tab.write_vt(&bytes);
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

impl App {
    pub(super) fn reconcile(&mut self) {
        // A full authoritative snapshot on every reconcile is the recovery
        // path for a slow consumer — a lagged broadcast arrives as a
        // `Resync` and this rebuild is what heals it: deltas are an
        // optimization, never UI truth.
        self.projects = self.workspace.snapshot();
        reconcile_confirm_delete(&mut self.confirm_delete, &self.projects);
        self.reconcile_tab_drag_preview();
        self.reconcile_project_drag_preview();
        self.reconcile_rename_editor();
        self.reconcile_notification_inbox();
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
        for (tab_id, tab) in &mut self.tabs {
            if *tab_id != active_tab_id && tab.reset_pointer_state() {
                refresh_or_warn(*tab_id, tab, "pointer reset after active tab changed");
            }
        }
        let now = Instant::now();
        for tab_id in &live_ids {
            if self.tabs.contains_key(tab_id) {
                continue;
            }
            self.attach_tab_tracked(*tab_id, now);
        }
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
                EngineFeed::AgentMetrics(result) => self.apply_agent_metrics(result),
                EngineFeed::Provider(result) => self.apply_provider_result(*result),
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
                title: _,
                body,
            } => {
                if let Some((project_id, title)) = self.notification_title(tab_id) {
                    self.notification_inbox
                        .upsert(notification_inbox::NotificationRecord::new(
                            tab_id, project_id, title, body,
                        ));
                }
            }
            WorkspaceEvent::TabNotification {
                tab_id,
                has_pending: false,
            }
            | WorkspaceEvent::TabClosed { tab_id } => {
                self.notification_inbox.remove(tab_id);
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
                } else if let Some(actions) =
                    self.tabs.get_mut(&tab_id).map(|tab| tab.write_vt(&data))
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
            UiRequest::TabDumpResolved { tab_id, reply } => {
                let result = self
                    .tabs
                    .get(&tab_id)
                    .map(TerminalTab::resolved_cells)
                    .ok_or_else(|| format!("tab {tab_id} has no live terminal"));
                let _ = reply.send(result);
            }
            UiRequest::WindowMetrics { reply } => {
                let collapsed = self.workspace.sidebar_collapsed();
                let resolved_family = self
                    .font_registry
                    .resolve(self.typography.effective_family())
                    .name
                    .to_string();
                let _ = reply.send(Ok((
                    f64::from(self.window_size.width),
                    f64::from(self.window_size.height),
                    f64::from(self.effective_sidebar_width()),
                    collapsed,
                    Some(f64::from(chrome::BAND_HEIGHT)),
                    Some(resolved_family),
                )));
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
        assert_eq!(tab.snapshot.rows_text[0], "hello");
        supervisor.close(70);
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
                if let EngineFeed::Tab(id, TabOutput::Bytes(bytes)) = item {
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
}
