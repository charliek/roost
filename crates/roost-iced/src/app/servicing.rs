use super::*;

pub(super) struct AgentMetricsResult {
    session: u64,
    claimed: Vec<String>,
    outcomes: Result<Vec<git_metrics::ProbeOutcome>, String>,
}

impl App {
    pub(super) fn reconcile(&mut self) {
        // Full authoritative snapshot on every UI tick is the recovery path
        // for a slow consumer: deltas are an optimization, never UI truth.
        self.projects = self.workspace.snapshot();
        reconcile_confirm_delete(&mut self.confirm_delete, &self.projects);
        self.reconcile_tab_drag_preview();
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
        let active_tab_id = self.workspace.active().1;
        for (tab_id, tab) in &mut self.tabs {
            if *tab_id != active_tab_id && tab.reset_pointer_state() {
                if let Err(error) = tab.refresh_snapshot() {
                    tracing::warn!(
                        ?error,
                        tab_id,
                        "terminal pointer reset failed after active tab changed"
                    );
                }
            }
        }
        for tab_id in live_ids {
            if self.tabs.contains_key(&tab_id) {
                continue;
            }
            let attached = {
                let _guard = self.runtime.enter();
                TerminalTab::attach(
                    Arc::clone(&self.supervisor),
                    tab_id,
                    self.test_mode,
                    Theme::load_bundled(&self.active_theme_name),
                    self.config.word_break_chars.clone(),
                )
            };
            match attached {
                Ok(mut tab) => {
                    let (cols, rows) = terminal_grid(
                        self.window_size,
                        self.workspace.sidebar_collapsed(),
                        self.terminal_metrics,
                    );
                    match tab.apply_geometry(
                        cols,
                        rows,
                        self.terminal_metrics,
                        self.metric_generation,
                    ) {
                        Ok(Some(change)) => {
                            tab.commit_geometry(change);
                            self.tabs.insert(tab_id, tab);
                        }
                        Ok(None) => {
                            tracing::warn!(tab_id, "new terminal did not install renderer metrics")
                        }
                        Err(error) => tracing::warn!(
                            tab_id,
                            ?error,
                            "new terminal renderer geometry installation failed"
                        ),
                    }
                }
                Err(error) => tracing::debug!(tab_id, ?error, "PTY not ready for UI attach"),
            }
        }
    }

    pub(super) fn service_workspace_events(&mut self) {
        loop {
            match self.workspace_events.try_recv() {
                Ok(WorkspaceEvent::NotificationFired {
                    tab_id,
                    title: _,
                    body,
                }) => {
                    if let Some((project_id, title)) = self.notification_title(tab_id) {
                        self.notification_inbox.upsert(
                            notification_inbox::NotificationRecord::new(
                                tab_id, project_id, title, body,
                            ),
                        );
                    }
                }
                Ok(WorkspaceEvent::TabNotification {
                    tab_id,
                    has_pending: false,
                })
                | Ok(WorkspaceEvent::TabClosed { tab_id }) => {
                    self.notification_inbox.remove(tab_id);
                }
                Ok(WorkspaceEvent::ProjectDeleted { project_id }) => {
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
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(dropped)) => {
                    // Dropped events are not replayed. Recovery comes from the
                    // per-tick snapshot reconcile (reconcile_notification_inbox
                    // rebuilds rows from authoritative workspace state); only
                    // event-carried notification bodies are lost. Jump to the
                    // queue head so the next drain starts at live events
                    // instead of chewing through the stale retained backlog.
                    tracing::warn!(
                        dropped,
                        "Iced workspace event consumer lagged; relying on per-tick snapshot reconcile"
                    );
                    self.workspace_events = self.workspace_events.resubscribe();
                    break;
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                | Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            }
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
        let tx = self.metrics_tx.clone();
        let session = self.palette_session;
        let failed_claims = claimed.clone();
        let task = self
            .runtime
            .spawn(git_metrics::probe_batch(probe, claimed, known));
        self.runtime.spawn(async move {
            let outcomes = task.await.map_err(|error| error.to_string());
            let _ = tx.send(AgentMetricsResult {
                session,
                claimed: failed_claims,
                outcomes,
            });
        });
    }

    pub(super) fn service_agent_metrics(&mut self) {
        while let Ok(result) = self.metrics_rx.try_recv() {
            if self.palette.is_none()
                || result.session != self.palette_session
                || self.metrics_cache.session() != result.session
            {
                continue;
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
    }

    pub(super) fn service_ui_requests(&mut self) -> UiTask {
        let mut task = UiTask::None;
        while let Ok(request) = self.ui_rx.try_recv() {
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
                    let mut actions = None;
                    let result = if !self.test_mode {
                        Err("ROOST_TEST_MODE=1 is required".into())
                    } else if let Some(tab) = self.tabs.get_mut(&tab_id) {
                        actions = Some(tab.write_vt(&data));
                        tab.refresh_snapshot().map_err(|error| error.to_string())
                    } else {
                        Err(format!("tab {tab_id} has no live terminal"))
                    };
                    if let Some(actions) = actions {
                        task = task.then(self.apply_osc_actions(tab_id, actions));
                    }
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
                        .ok_or_else(|| {
                            "ROOST_TEST_MODE=1 is required or tab is missing".to_string()
                        })
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
                        f64::from(sidebar_width(collapsed)),
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
                    let _ = reply.send(self.activate_palette(&id));
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
                        Err(
                            "tab.expand_selection_at requires ROOST_TEST_MODE=1 at UI launch"
                                .into(),
                        )
                    } else {
                        self.tabs
                            .get_mut(&tab_id)
                            .ok_or_else(|| format!("tab {tab_id} has no live terminal"))
                            .and_then(|tab| {
                                tab.expand_selection_at(col, row, click_count)
                                    .map_err(|error| error.to_string())?
                                    .ok_or_else(|| {
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
