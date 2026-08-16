use super::*;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct NativePointerOutcome {
    pub(super) selection_completed: bool,
    pub(super) paste_selection: bool,
    pub(super) open_url: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct NativePointerDispatch {
    pub(super) action: PointerAction,
    pub(super) button: Option<PointerButton>,
    pub(super) col: u32,
    pub(super) row: u32,
    pub(super) mods: u16,
    pub(super) click_count: u8,
    pub(super) inside: bool,
    pub(super) link_modifier_held: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LocalPointerGesture {
    Selection,
    MultiClick,
    Url,
}

pub(super) fn pointer_origin_tab<V>(tabs: &mut HashMap<i64, V>, tab_id: i64) -> Option<&mut V> {
    tabs.get_mut(&tab_id)
}

/// Republish what a tab renders after something moved its terminal state.
/// Takes the tab rather than the app so the sites that hold a `&mut` into
/// `App::tabs` — the resize and pointer-cancel loops — can call it too.
/// A failure is logged, not propagated: every caller is a UI-side publish
/// with no error channel, and the next refresh retries from scratch.
pub(super) fn refresh_or_warn(tab_id: i64, tab: &mut TerminalTab, reason: &str) {
    if let Err(error) = tab.refresh_snapshot() {
        tracing::warn!(?error, tab_id, reason, "terminal snapshot refresh failed");
    }
}

/// Drop a tab's composition, logging rather than propagating for the same
/// reason [`refresh_or_warn`] does: every cancel site is a UI transition
/// with no error channel. Reports whether a live composition was
/// discarded — that is what arms `App::ime_discard_next_commit`.
pub(super) fn clear_preedit_or_warn(tab_id: i64, tab: &mut TerminalTab) -> bool {
    match tab.clear_preedit() {
        Ok(cleared) => cleared,
        Err(error) => {
            // Only the repaint after the composition was already taken
            // can fail, so it is gone either way.
            tracing::warn!(?error, tab_id, "terminal preedit clear failed");
            true
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct TerminalGeometry {
    pub(super) cols: u16,
    pub(super) rows: u16,
    pub(super) metrics: TerminalMetrics,
    pub(super) metric_generation: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct GeometryChange {
    pub(super) previous: Option<TerminalGeometry>,
    pub(super) current: TerminalGeometry,
    pub(super) grid_changed: bool,
    pub(super) metrics_changed: bool,
    pub(super) deferred_replies: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum GeometryBatchOperation {
    Apply {
        tab_id: i64,
        cols: u16,
        rows: u16,
        metrics: TerminalMetrics,
        metric_generation: u64,
    },
    Rollback {
        tab_id: i64,
        previous: TerminalGeometry,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct GeometryBatchFailure {
    pub(super) tab_id: i64,
    pub(super) apply: String,
    pub(super) rollback: Vec<(i64, String)>,
}

pub(super) fn apply_geometry_batch(
    tab_ids: &[i64],
    cols: u16,
    rows: u16,
    metrics: TerminalMetrics,
    metric_generation: u64,
    mut transition: impl FnMut(
        GeometryBatchOperation,
    ) -> std::result::Result<Option<GeometryChange>, String>,
) -> std::result::Result<Vec<(i64, GeometryChange)>, GeometryBatchFailure> {
    let mut applied = Vec::with_capacity(tab_ids.len());
    for tab_id in tab_ids {
        let operation = GeometryBatchOperation::Apply {
            tab_id: *tab_id,
            cols,
            rows,
            metrics,
            metric_generation,
        };
        match transition(operation) {
            Ok(Some(change)) => applied.push((*tab_id, change)),
            Ok(None) => {}
            Err(error) => {
                let rollback = applied
                    .iter()
                    .rev()
                    .filter_map(|(rollback_id, change): &(i64, GeometryChange)| {
                        let previous = change.previous?;
                        transition(GeometryBatchOperation::Rollback {
                            tab_id: *rollback_id,
                            previous,
                        })
                        .err()
                        .map(|error| (*rollback_id, error))
                    })
                    .collect();
                return Err(GeometryBatchFailure {
                    tab_id: *tab_id,
                    apply: error,
                    rollback,
                });
            }
        }
    }
    Ok(applied)
}

/// A tab attached to a real PTY running `cat`, sized to the default grid
/// with measured metrics installed — the shape `reconcile` produces. The
/// caller owns the feed channel so it can choose whether to observe what
/// the tab's forwarder puts on it; dropping the receiver on the spot is
/// fine and simply ends the forwarder.
#[cfg(test)]
pub(super) fn attach_test_terminal(
    tab_id: i64,
    feed: EngineFeedSender,
) -> (TerminalTab, Arc<PtySupervisor>) {
    let supervisor = Arc::new(PtySupervisor::new());
    let argv = vec!["/bin/sh".into(), "-c".into(), "cat".into()];
    let _early_output = supervisor
        .spawn(
            tab_id,
            "/tmp",
            &argv,
            DEFAULT_COLS,
            DEFAULT_ROWS,
            std::path::Path::new("/tmp/roost-iced-terminal-test.sock"),
        )
        .expect("spawn test PTY");
    let mut tab = TerminalTab::attach(
        Arc::clone(&supervisor),
        tab_id,
        true,
        Theme::roost_dark_fallback(),
        roost_ui_model::word_selection::DEFAULT_EXTRA_WORD_CHARS.to_string(),
        feed,
    )
    .expect("attach test terminal");
    let metrics = TerminalMetrics::measure(13.0).expect("test terminal metrics");
    tab.apply_geometry(DEFAULT_COLS, DEFAULT_ROWS, metrics, 1)
        .expect("install test terminal metrics")
        .expect("new test terminal changes geometry");
    (tab, supervisor)
}

pub(super) fn terminal_grid(
    size: Size,
    sidebar_width: f32,
    metrics: TerminalMetrics,
) -> (u16, u16) {
    let width = (size.width - sidebar_width - 2.0 * TERMINAL_PADDING).max(metrics.cell_width * 2.0);
    let height =
        (size.height - chrome::BAND_HEIGHT - 2.0 * TERMINAL_PADDING).max(metrics.cell_height * 2.0);
    (
        ((width / metrics.cell_width).floor() as u16).max(2),
        ((height / metrics.cell_height).floor() as u16).max(2),
    )
}

pub(super) struct TerminalTab {
    pub(super) terminal: Terminal,
    render_state: RenderState,
    pub(super) encoder: KeyEncoder,
    mouse_encoder: MouseEncoder,
    pub(super) scroll: TerminalScroll,
    motion_emitter: MotionEmitter,
    /// Suppresses the same-cell drag winit's sub-pixel `CursorMoved` events
    /// would otherwise inject into every stationary click.
    drag_gate: DragCellGate,
    pub(super) tracking_pointer: Option<PointerButton>,
    pub(super) local_pointer_gesture: Option<LocalPointerGesture>,
    pub(super) last_pointer_cell: Option<(u16, u16)>,
    pub(super) link_modifier_held: bool,
    pub(super) hover_url: Option<HoverUrl>,
    pub(super) selection: TerminalSelection,
    pub(super) word_break_chars: String,
    input_started_at: Instant,
    pub(super) session: TabSession,
    output_forwarder: tokio::task::AbortHandle,
    reply_buffer: Arc<Mutex<Vec<u8>>>,
    pub(super) input_capture: Option<InputCapture>,
    pub(super) pointer_shape: String,
    pub(super) theme: Theme,
    /// The live platform-IME composition, mirrored into every snapshot
    /// this tab publishes. Never written into `terminal` — see
    /// [`ImePreedit`].
    pub(super) preedit: Option<ImePreedit>,
    pub(super) snapshot: TerminalSnapshot,
    /// The per-row render cache `refresh_snapshot` maintains; the snapshot
    /// gets a clone of it (O(rows) refcount bumps). The three `cached_*`
    /// fields are the keys this cache is valid under — see
    /// `refresh_snapshot`'s caching invariant.
    grid: Vec<Arc<RenderedRow>>,
    cached_grid_size: Option<(u16, u16)>,
    cached_defaults: Option<(ColorRgb, ColorRgb)>,
    cached_theme_generation: Option<u64>,
    /// Bumped whenever a theme lands on this tab. Nothing theme-derived
    /// besides the default fg/bg pair enters `RenderedRow::build` today,
    /// but GTK's twin resolver already pulls the theme's `bold_color`, so
    /// this fails the cache safe toward over-rebuilding if that override
    /// ever lands here.
    theme_generation: u64,
    cols: u16,
    rows: u16,
    pub(super) applied_metrics: Option<TerminalMetrics>,
    pub(super) metric_generation: u64,
    pub(super) render_stats: crate::perf::TabRenderStats,
}

impl Drop for TerminalTab {
    /// The forwarder owns this tab's PTY receiver, so its life must end
    /// with the tab's. A tab that is built and then discarded — the
    /// failed-geometry arm of `reconcile` — would otherwise leave a live
    /// stream behind, and the retry that attaches the same PTY again
    /// cannot reuse the initial receiver (`TabSession::attach` falls back
    /// to a fresh subscription), so two streams would interleave into one
    /// terminal. Aborting drops the receiver, which ends the engine-side
    /// bridge on its next send: the cascade that holding the receiver on
    /// the tab used to give for free.
    fn drop(&mut self) {
        self.output_forwarder.abort();
    }
}

/// The drain-side scanner's color seed for a theme.
///
/// This is what the terminal itself is seeded with at attach
/// (`set_color_foreground` and friends) and re-seeded with on every
/// theme application, so the drain's answers and the terminal's
/// rendering start from the same colors and are moved by the same OSC
/// sequences from there.
fn theme_osc_colors(theme: &Theme) -> OscColorSnapshot {
    let rgb = |color: roost_vt::ColorRgb| (color.r, color.g, color.b);
    OscColorSnapshot::new(
        rgb(theme.foreground),
        rgb(theme.background),
        rgb(theme.cursor),
        theme.palette.map(rgb),
    )
}

impl TerminalTab {
    /// Attach the UI to a spawned PTY. Must be called inside the app
    /// runtime (`Runtime::enter`): both `TabSession::attach_scanned` and
    /// the output forwarder this spawns bind to the ambient runtime.
    pub(super) fn attach(
        supervisor: Arc<PtySupervisor>,
        tab_id: i64,
        test_mode: bool,
        theme: Theme,
        word_break_chars: String,
        feed: EngineFeedSender,
    ) -> Result<Self> {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            max_scrollback: 2_000,
        })?;
        terminal.set_color_foreground(theme.foreground)?;
        terminal.set_color_background(theme.background)?;
        terminal.set_color_cursor(theme.cursor)?;
        terminal.set_color_palette(&theme.palette)?;
        let reply_buffer = Arc::new(Mutex::new(Vec::new()));
        terminal
            .set_write_pty_buffer(Arc::clone(&reply_buffer))
            .context("install libghostty PTY reply buffer")?;
        let render_state = RenderState::new()?;
        let encoder = KeyEncoder::new()?;
        let mouse_encoder = MouseEncoder::new()?;
        let input_capture = test_mode.then(|| Arc::new(Mutex::new(Vec::new())));
        let (output_tx, output_rx) = tokio::sync::mpsc::unbounded_channel();
        // The OSC opt-in: this session's forwarding task owns the sole
        // router for the tab and answers color queries straight off the
        // drain. `TerminalTab` keeps no router of its own — a second
        // one would double every reply.
        let session = TabSession::attach_scanned(
            supervisor,
            tab_id,
            output_tx,
            input_capture.clone(),
            Some(theme_osc_colors(&theme)),
        )?;
        // The per-tab channel stays — only its drain moves. This forwarder
        // is what puts PTY output in the same arrival order as everything
        // else the app reacts to, and what arms the feed's wake for it.
        let output_forwarder =
            tokio::spawn(engine_feed::pump_tab_output(tab_id, output_rx, feed)).abort_handle();
        Ok(Self {
            terminal,
            render_state,
            encoder,
            mouse_encoder,
            scroll: TerminalScroll::new(),
            motion_emitter: MotionEmitter::new(),
            drag_gate: DragCellGate::new(),
            tracking_pointer: None,
            local_pointer_gesture: None,
            last_pointer_cell: None,
            link_modifier_held: false,
            hover_url: None,
            selection: TerminalSelection::new(),
            word_break_chars,
            input_started_at: Instant::now(),
            session,
            output_forwarder,
            reply_buffer,
            input_capture,
            pointer_shape: "default".into(),
            theme,
            preedit: None,
            snapshot: TerminalSnapshot::blank(DEFAULT_COLS, DEFAULT_ROWS),
            // Left empty on purpose: the first `refresh_snapshot` finds no
            // cached grid size, sizes the grid and forces a full rebuild.
            grid: Vec::new(),
            cached_grid_size: None,
            cached_defaults: None,
            cached_theme_generation: None,
            theme_generation: 0,
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            applied_metrics: None,
            metric_generation: 0,
            render_stats: crate::perf::TabRenderStats::default(),
        })
    }

    /// Apply a chunk of terminal output that has ALREADY been scanned
    /// — everything arriving from the PTY, which the session's drain
    /// scanned as it read it.
    pub(super) fn write_vt(&mut self, bytes: &[u8]) {
        self.terminal.vt_write(bytes);
        self.drain_terminal_replies();
    }

    /// Apply a chunk that has NOT been scanned yet: `tab.feed_pty_bytes`
    /// injects bytes on the UI thread, so they never pass the drain.
    ///
    /// Routing them through `scan_osc` puts them through the same
    /// router and the same color state the drain uses — same streaming
    /// scan position, same chunk-start snapshot contract, replies
    /// enqueued on the same serial channel — so the OSC end-to-end
    /// tests still exercise the production pipeline rather than a
    /// UI-side replica of it. The returned actions are the non-reply
    /// ones, exactly as `TabOutput::Scanned` carries them.
    pub(super) fn scan_and_write_vt(&mut self, bytes: &[u8]) -> Vec<OscAction> {
        let actions = self.session.scan_osc(bytes);
        self.write_vt(bytes);
        actions
    }

    fn drain_terminal_replies(&self) {
        self.session.send_input(self.take_terminal_replies());
    }

    fn take_terminal_replies(&self) -> Vec<u8> {
        self.reply_buffer
            .lock()
            .map(|mut buffer| std::mem::take(&mut *buffer))
            .unwrap_or_default()
    }

    pub(super) fn apply_geometry(
        &mut self,
        cols: u16,
        rows: u16,
        metrics: TerminalMetrics,
        metric_generation: u64,
    ) -> Result<Option<GeometryChange>> {
        let grid_changed = self.cols != cols || self.rows != rows;
        let metrics_changed = self.applied_metrics != Some(metrics);
        if !grid_changed && !metrics_changed {
            return Ok(None);
        }
        let previous = self.applied_metrics.map(|metrics| TerminalGeometry {
            cols: self.cols,
            rows: self.rows,
            metrics,
            metric_generation: self.metric_generation,
        });
        let resize = self.terminal.resize(
            cols,
            rows,
            metrics.cell_width.round().max(1.0) as u32,
            metrics.cell_height.round().max(1.0) as u32,
        );
        let deferred_replies = self.take_terminal_replies();
        resize?;
        self.cols = cols;
        self.rows = rows;
        self.applied_metrics = Some(metrics);
        self.metric_generation = metric_generation;
        self.hover_url = None;
        self.last_pointer_cell = self.last_pointer_cell.map(|(col, row)| {
            (
                col.min(cols.saturating_sub(1)),
                row.min(rows.saturating_sub(1)),
            )
        });
        Ok(Some(GeometryChange {
            previous,
            current: TerminalGeometry {
                cols,
                rows,
                metrics,
                metric_generation,
            },
            grid_changed,
            metrics_changed,
            deferred_replies,
        }))
    }

    pub(super) fn rollback_geometry(&mut self, previous: TerminalGeometry) -> Result<()> {
        let resize = self.terminal.resize(
            previous.cols,
            previous.rows,
            previous.metrics.cell_width.round().max(1.0) as u32,
            previous.metrics.cell_height.round().max(1.0) as u32,
        );
        // The candidate report was staged in `GeometryChange`; the rollback
        // report describes an internal transition the PTY never observed.
        // Neither may escape a failed all-tab transaction.
        let _ = self.take_terminal_replies();
        resize?;
        self.cols = previous.cols;
        self.rows = previous.rows;
        self.applied_metrics = Some(previous.metrics);
        self.metric_generation = previous.metric_generation;
        self.hover_url = None;
        self.last_pointer_cell = self.last_pointer_cell.map(|(col, row)| {
            (
                col.min(previous.cols.saturating_sub(1)),
                row.min(previous.rows.saturating_sub(1)),
            )
        });
        Ok(())
    }

    pub(super) fn commit_geometry(&self, change: GeometryChange) {
        self.session.send_input(change.deferred_replies);
        if change.grid_changed {
            self.session
                .send_resize(change.current.cols, change.current.rows);
        }
    }

    pub(super) fn prepare_pointer_cancel(&mut self) -> Result<Vec<u8>> {
        if let Some(button) = self.tracking_pointer {
            let (col, row) = self.last_pointer_cell.unwrap_or_default();
            self.encode_pointer(
                PointerAction::Release,
                Some(button),
                u32::from(col),
                u32::from(row),
                0,
            )
        } else {
            Ok(Vec::new())
        }
    }

    pub(super) fn commit_pointer_cancel(&mut self, release: Vec<u8>) {
        self.session.send_input(release);
        self.drag_gate.reset();
        self.tracking_pointer = None;
        self.local_pointer_gesture = None;
        self.last_pointer_cell = None;
        self.hover_url = None;
    }

    pub(super) fn dispatch_pointer(
        &mut self,
        action: PointerAction,
        button: Option<PointerButton>,
        col: u32,
        row: u32,
        mods: u16,
    ) -> Result<()> {
        let col = col.min(u32::from(self.cols.saturating_sub(1)));
        let row = row.min(u32::from(self.rows.saturating_sub(1)));
        let motion_without_button = action == PointerAction::Motion && button.is_none();
        let now = self.input_started_at.elapsed().as_secs_f64();
        if motion_without_button && !self.motion_emitter.would_emit(col, row, now) {
            return Ok(());
        }
        if !self.drag_gate.would_dispatch(action, button, (col, row)) {
            return Ok(());
        }
        // A release ends the gesture whether or not it encodes, so its
        // memory clear cannot wait behind the byte check below.
        if action == PointerAction::Release {
            self.drag_gate.commit_dispatched(action, button, (col, row));
        }

        let bytes = self.encode_pointer(action, button, col, row, mods)?;
        if bytes.is_empty() {
            return Ok(());
        }
        if motion_without_button {
            self.motion_emitter.commit(col, row, now);
        } else {
            self.drag_gate.commit_dispatched(action, button, (col, row));
        }
        self.session.send_input(bytes);
        Ok(())
    }

    fn encode_pointer(
        &mut self,
        action: PointerAction,
        button: Option<PointerButton>,
        col: u32,
        row: u32,
        mods: u16,
    ) -> Result<Vec<u8>> {
        let Some(metrics) = self.applied_metrics else {
            return Ok(Vec::new());
        };
        let cell_width = metrics.cell_width.round().max(1.0) as u32;
        let cell_height = metrics.cell_height.round().max(1.0) as u32;
        self.mouse_encoder.sync_from_terminal(&self.terminal);
        self.mouse_encoder.set_size(
            u32::from(self.cols) * cell_width,
            u32::from(self.rows) * cell_height,
            cell_width,
            cell_height,
        );
        let mut event = MouseEvent::new().context("allocate terminal mouse event")?;
        event
            .set_action(pointer_action(action))
            .set_mods(mods)
            .set_position(
                (col * cell_width) as f32 + cell_width as f32 / 2.0,
                (row * cell_height) as f32 + cell_height as f32 / 2.0,
            );
        if let Some(button) = button {
            event.set_button(pointer_button(button));
        } else {
            event.clear_button();
        }
        self.mouse_encoder
            .encode(&event)
            .context("encode terminal mouse event")
    }

    pub(super) fn handle_wheel(
        &mut self,
        history_rows: f64,
        col: u32,
        row: u32,
        mods: u16,
    ) -> Result<()> {
        let route = self.scroll.route(&mut self.terminal, history_rows);
        match route {
            Some(ScrollRoute::MouseReport { direction, rows }) => {
                let button = match direction {
                    ScrollDirection::History => PointerButton::Four,
                    ScrollDirection::Bottom => PointerButton::Five,
                };
                for _ in 0..rows {
                    self.dispatch_pointer(PointerAction::Press, Some(button), col, row, mods)?;
                }
            }
            Some(ScrollRoute::AlternateScreenKey { direction, rows }) => {
                let key = match direction {
                    ScrollDirection::History => roost_vt::ffi::GhosttyKey_GHOSTTY_KEY_ARROW_UP,
                    ScrollDirection::Bottom => roost_vt::ffi::GhosttyKey_GHOSTTY_KEY_ARROW_DOWN,
                };
                let mut event = KeyEvent::new().context("allocate terminal wheel key event")?;
                event.set_action(key_action::PRESS);
                event.set_key(key);
                event.set_mods(0);
                self.encoder.sync_from_terminal(&self.terminal);
                let mut bytes = Vec::new();
                for _ in 0..rows {
                    bytes.extend(
                        self.encoder
                            .encode(&event)
                            .context("encode alternate-screen wheel key")?,
                    );
                }
                self.session.send_input(bytes);
            }
            Some(ScrollRoute::LocalViewport { .. }) | None => {}
        }
        Ok(())
    }

    /// Route a Page Up / Page Down press through the shared scroll policy.
    /// A local page repaints here and reports `LocalViewport` so the caller
    /// consumes the key; `Forward` leaves the key to the normal encode path.
    pub(super) fn handle_page(&mut self, direction: PageDirection) -> Result<PageRoute> {
        let route = self
            .scroll
            .route_page(&mut self.terminal, direction, usize::from(self.rows));
        if matches!(route, PageRoute::LocalViewport { .. }) {
            self.refresh_snapshot()?;
        }
        Ok(route)
    }

    pub(super) fn snap_to_bottom_for_input(&mut self) -> Result<bool> {
        let snapped = self.scroll.snap_to_bottom(&mut self.terminal);
        if snapped {
            self.refresh_snapshot()?;
        }
        Ok(snapped)
    }

    /// Store what the platform IME is composing. Empty text cancels the
    /// composition — that is how winit reports both "cleared" and the
    /// clear that precedes every commit. A live composition snaps
    /// scrollback to the bottom exactly as a keypress does, so the caret
    /// the IME anchors on is on screen.
    pub(super) fn set_preedit(&mut self, text: String, cursor: Option<Range<usize>>) -> Result<()> {
        if text.is_empty() {
            self.clear_preedit()?;
            return Ok(());
        }
        self.snap_to_bottom_for_input()?;
        self.preedit = Some(ImePreedit { text, cursor });
        self.refresh_snapshot()
    }

    pub(super) fn clear_preedit(&mut self) -> Result<bool> {
        if self.preedit.take().is_none() {
            return Ok(false);
        }
        self.refresh_snapshot()?;
        Ok(true)
    }

    /// Send text the IME committed. The composition is dropped first —
    /// the committed text is the whole of what reaches the PTY.
    pub(super) fn commit_ime(&mut self, text: &str) -> Result<()> {
        // A failed repaint must not swallow the commit: the composition is
        // already gone on the IME's side, so these bytes are the user's
        // only copy of the text.
        let cleared = self.clear_preedit();
        let snapped = self.snap_to_bottom_for_input();
        let bytes = input::encode_ime_commit(&mut self.encoder, &self.terminal, text);
        self.session.send_input(bytes);
        cleared?;
        snapped?;
        Ok(())
    }

    /// Route a native pointer gesture with terminal mouse reporting taking
    /// precedence over local selection for the lifetime of the press.
    pub(super) fn handle_native_pointer(
        &mut self,
        event: NativePointerDispatch,
    ) -> Result<NativePointerOutcome> {
        let NativePointerDispatch {
            action,
            button,
            col,
            row,
            mods,
            click_count,
            inside,
            link_modifier_held,
        } = event;
        let col = col.min(u32::from(self.cols.saturating_sub(1)));
        let row = row.min(u32::from(self.rows.saturating_sub(1)));
        let cell = (col as u16, row as u16);
        if inside {
            self.last_pointer_cell = Some(cell);
        } else {
            self.last_pointer_cell = None;
        }
        self.set_link_modifier_held(link_modifier_held)?;
        match action {
            PointerAction::Press if button == Some(PointerButton::Left) && link_modifier_held => {
                if let Some(hover) = self.compute_hover_url(cell.0, cell.1)? {
                    let url = hover.url.clone();
                    self.hover_url = Some(hover);
                    self.local_pointer_gesture = Some(LocalPointerGesture::Url);
                    return Ok(NativePointerOutcome {
                        open_url: Some(url),
                        ..NativePointerOutcome::default()
                    });
                }
                self.route_press_without_link(button, col, row, mods, click_count)
            }
            PointerAction::Motion if self.tracking_pointer.is_some() => {
                self.dispatch_pointer(action, self.tracking_pointer, col, row, mods)?;
                Ok(NativePointerOutcome::default())
            }
            PointerAction::Release if self.tracking_pointer.is_some() => {
                let captured = self.tracking_pointer.take();
                self.dispatch_pointer(action, captured, col, row, mods)?;
                Ok(NativePointerOutcome::default())
            }
            PointerAction::Motion => match self.local_pointer_gesture {
                Some(LocalPointerGesture::Selection) => {
                    self.selection.update(&self.terminal, cell.0, cell.1);
                    Ok(NativePointerOutcome::default())
                }
                Some(LocalPointerGesture::MultiClick | LocalPointerGesture::Url) => {
                    Ok(NativePointerOutcome::default())
                }
                None if self.terminal.mouse_tracking() => {
                    self.dispatch_pointer(action, button, col, row, mods)?;
                    Ok(NativePointerOutcome::default())
                }
                None => Ok(NativePointerOutcome::default()),
            },
            PointerAction::Release => match self.local_pointer_gesture.take() {
                Some(LocalPointerGesture::Selection) => {
                    self.selection.update(&self.terminal, cell.0, cell.1);
                    Ok(NativePointerOutcome {
                        selection_completed: true,
                        ..NativePointerOutcome::default()
                    })
                }
                Some(LocalPointerGesture::MultiClick | LocalPointerGesture::Url) | None => {
                    Ok(NativePointerOutcome::default())
                }
            },
            PointerAction::Press => {
                self.route_press_without_link(button, col, row, mods, click_count)
            }
        }
    }

    fn route_press_without_link(
        &mut self,
        button: Option<PointerButton>,
        col: u32,
        row: u32,
        mods: u16,
        click_count: u8,
    ) -> Result<NativePointerOutcome> {
        let cell = (col as u16, row as u16);
        if self.terminal.mouse_tracking() {
            self.local_pointer_gesture = None;
            if matches!(
                button,
                Some(PointerButton::Left | PointerButton::Right | PointerButton::Middle)
            ) {
                self.tracking_pointer = button;
            }
            self.dispatch_pointer(PointerAction::Press, button, col, row, mods)?;
            return Ok(NativePointerOutcome::default());
        }
        if button == Some(PointerButton::Left)
            && click_count >= 2
            && self
                .expand_selection_at(cell.0, cell.1, click_count)?
                .is_some()
        {
            self.local_pointer_gesture = Some(LocalPointerGesture::MultiClick);
            return Ok(NativePointerOutcome {
                selection_completed: true,
                ..NativePointerOutcome::default()
            });
        }
        if button == Some(PointerButton::Left) {
            self.local_pointer_gesture = self
                .selection
                .begin(&self.terminal, cell.0, cell.1)
                .then_some(LocalPointerGesture::Selection);
            return Ok(NativePointerOutcome::default());
        }
        if button == Some(PointerButton::Middle) {
            return Ok(NativePointerOutcome {
                paste_selection: true,
                ..NativePointerOutcome::default()
            });
        }
        Ok(NativePointerOutcome::default())
    }

    pub(super) fn pointer_leave(&mut self) {
        self.last_pointer_cell = None;
        self.hover_url = None;
    }

    pub(super) fn reset_pointer_state(&mut self) -> bool {
        self.drag_gate.reset();
        let gesture = self.local_pointer_gesture.take();
        let tracking = self.tracking_pointer.take();
        let cell = self.last_pointer_cell.take();
        let hover = self.hover_url.take();
        let modifier = std::mem::take(&mut self.link_modifier_held);
        gesture.is_some() || tracking.is_some() || cell.is_some() || hover.is_some() || modifier
    }

    pub(super) fn effective_pointer_shape(&self) -> &str {
        if self.hover_url.is_some() {
            "pointer"
        } else {
            &self.pointer_shape
        }
    }

    pub(super) fn set_link_modifier_held(&mut self, held: bool) -> Result<()> {
        self.link_modifier_held = held;
        self.recompute_hover()
    }

    fn recompute_hover(&mut self) -> Result<()> {
        self.hover_url = match (self.link_modifier_held, self.last_pointer_cell) {
            (true, Some((col, row))) => self.compute_hover_url(col, row)?,
            _ => None,
        };
        Ok(())
    }

    fn compute_hover_url(&mut self, col: u16, row: u16) -> Result<Option<HoverUrl>> {
        if let Some(url) = self.terminal.hyperlink_at(col, u32::from(row)) {
            let (col0, col1) = roost_url::contiguous_hyperlink_span(
                col,
                self.cols.saturating_sub(1),
                &url,
                |candidate| self.terminal.hyperlink_at(candidate, u32::from(row)),
            );
            return Ok(Some(HoverUrl {
                col0,
                col1,
                row,
                url,
            }));
        }
        let projection = TerminalSelection::row_text_projection(
            &self.terminal,
            &mut self.render_state,
            row,
            self.cols,
        )?;
        let Some(char_col) = projection
            .char_index_at_cell(col)
            .and_then(|index| u16::try_from(index).ok())
        else {
            return Ok(None);
        };
        let Some(span) = roost_url::find_url_at(projection.text(), char_col) else {
            return Ok(None);
        };
        let Some((col0, col1)) =
            projection.cell_span_for_chars(usize::from(span.col0), usize::from(span.col1))
        else {
            return Ok(None);
        };
        Ok(Some(HoverUrl {
            col0,
            col1,
            row,
            url: span.url,
        }))
    }

    pub(super) fn selected_text(&mut self) -> Result<Option<String>> {
        Ok(self.selection.selected_text(
            &self.terminal,
            &mut self.render_state,
            self.cols,
            self.rows,
        )?)
    }

    pub(super) fn paste(&self, text: Option<&str>) {
        let bytes = paste_bytes(&self.terminal, text);
        if !bytes.is_empty() {
            self.session.send_input(bytes);
        }
    }

    pub(super) fn selection_dump(&mut self) -> Result<Option<SelectionData>> {
        Ok(self
            .selection
            .snapshot(&self.terminal, &mut self.render_state, self.cols, self.rows)?
            .map(|snapshot| SelectionData {
                text: snapshot.text,
                anchor_visible: snapshot.anchor_visible,
                cursor_visible: snapshot.cursor_visible,
            }))
    }

    pub(super) fn expand_selection_at(
        &mut self,
        col: u16,
        row: u16,
        click_count: u8,
    ) -> Result<Option<ExpandSelectionData>> {
        let row_text = TerminalSelection::row_text(&self.terminal, &mut self.render_state, row)?;
        let span = match click_count {
            2 => {
                roost_ui_model::word_selection::expand_word(&row_text, col, &self.word_break_chars)
            }
            _ => Some(roost_ui_model::word_selection::expand_line(&row_text)),
        };
        let Some(span) = span else {
            return Ok(None);
        };
        if !self
            .selection
            .set(&self.terminal, (span.col0, row), (span.col1, row))
        {
            return Ok(None);
        }
        let text = self.selection.selected_text(
            &self.terminal,
            &mut self.render_state,
            self.cols,
            self.rows,
        )?;
        Ok(Some(ExpandSelectionData {
            col0: span.col0,
            col1: span.col1,
            text,
        }))
    }

    pub(super) fn set_window_focus(&self, focused: bool) {
        let bytes = self.terminal.encode_focus(focused);
        if !bytes.is_empty() {
            self.session.send_input(bytes);
        }
    }

    pub(super) fn set_theme(&mut self, theme: &Theme) -> Result<()> {
        let previous = self.theme.clone();
        if let Err(failure) = apply_with_rollback(&previous, theme, |candidate| {
            self.apply_theme_candidate(candidate)
        }) {
            return Err(match failure.rollback {
                Some(rollback) => anyhow::anyhow!(
                    "theme apply failed: {}; rollback failed: {}",
                    failure.apply,
                    rollback
                ),
                None => anyhow::anyhow!("theme apply failed: {}", failure.apply),
            });
        }
        if self.terminal.mode_get(2031) {
            self.session.send_input(if theme.background.is_light() {
                b"\x1b[?997;2n".to_vec()
            } else {
                b"\x1b[?997;1n".to_vec()
            });
        }
        Ok(())
    }

    fn apply_theme_candidate(&mut self, theme: &Theme) -> Result<()> {
        self.theme = theme.clone();
        // Every theme application — including `set_theme`'s rollback —
        // lands here, so this is the one place the generation must move.
        self.theme_generation = self.theme_generation.wrapping_add(1);
        self.terminal.set_color_foreground(theme.foreground)?;
        self.terminal.set_color_background(theme.background)?;
        self.terminal.set_color_cursor(theme.cursor)?;
        self.terminal.set_color_palette(&theme.palette)?;
        // The drain answers color queries from its own state, so it has
        // to learn about a theme the same moment the terminal does —
        // including on `set_theme`'s rollback, which is why this sits
        // in the one place every application lands.
        self.session.reseed_osc_colors(theme_osc_colors(theme));
        self.refresh_snapshot()
    }

    /// Rebuild the snapshot from the terminal, reusing every cached row
    /// libghostty reports as unchanged.
    ///
    /// **Caching invariant.** A cached `RenderedRow` is valid exactly
    /// while (a) libghostty reports its row undirty and (b) the inputs
    /// `RenderedRow::build` reads besides the row's own vt cells — the
    /// default fg/bg pair and the grid width — are unchanged. Everything
    /// that alters what a row should render must therefore either mark
    /// that row dirty inside libghostty or move one of the cache keys
    /// guarded below. Anyone adding a third input to `RenderedRow::build`
    /// must add a guard for it here.
    ///
    /// The default-color guard is not belt-and-braces: `OSC 10`/`OSC 11`
    /// and DECSCNM (`CSI ?5h`) change the terminal's default fg/bg with
    /// libghostty reporting `Clean` and no row flagged (measured; pinned
    /// by `crates/roost-vt/tests/render_dirty_test.rs`). Since
    /// `resolve_colors` folds those defaults into every cell that does not
    /// set its own, a cached row would otherwise freeze at the old color.
    pub(super) fn refresh_snapshot(&mut self) -> Result<()> {
        let refresh_started_at = Instant::now();
        self.recompute_hover()?;
        self.render_state.update(&self.terminal)?;
        let colors = self.render_state.colors()?;
        let cursor = self.render_state.cursor();

        // Each guard raises the dirty state BEFORE recording its new key,
        // so a failed `mark_full` leaves the key stale and the next
        // refresh retries the invalidation rather than skipping it.
        let size = (self.cols, self.rows);
        if self.cached_grid_size != Some(size) {
            // Both axes: a width-only resize leaves the row count alone
            // while invalidating every cached row's column content.
            self.render_state.mark_full()?;
            // Every slot shares one empty row — rows are replaced
            // wholesale by the walk below, never mutated in place.
            let blank_row = Arc::new(RenderedRow::default());
            self.grid = vec![blank_row; usize::from(self.rows)];
            self.cached_grid_size = Some(size);
        }
        let defaults = (colors.foreground, colors.background);
        if self.cached_defaults != Some(defaults) {
            self.render_state.mark_full()?;
            self.cached_defaults = Some(defaults);
        }
        if self.cached_theme_generation != Some(self.theme_generation) {
            self.render_state.mark_full()?;
            self.cached_theme_generation = Some(self.theme_generation);
        }

        let cols = self.cols;
        let grid = &mut self.grid;
        let mut rows_rebuilt: u64 = 0;
        let mut cells_walked: u64 = 0;
        self.render_state.walk_dirty(&self.terminal, |row, cells| {
            cells_walked += cells.len() as u64;
            // Clamped against the cache's own length, not `self.rows`:
            // the guard above keeps the two equal, and reading the length
            // here means a row index past the end can never index out of
            // bounds even if that ever stopped holding.
            if row as usize >= grid.len() {
                return;
            }
            grid[row as usize] = Arc::new(RenderedRow::build(cells, defaults, cols));
            rows_rebuilt += 1;
        })?;

        self.snapshot = TerminalSnapshot {
            cols: self.cols,
            rows: self.rows,
            foreground: colors.foreground,
            background: colors.background,
            cursor,
            grid: self.grid.clone(),
            selection_background: self.theme.selection_background,
            selection_spans: self
                .selection
                .visible_spans(&self.terminal, self.cols, self.rows),
            link_hover: self
                .hover_url
                .as_ref()
                .map(|hover| roost_vt::SelectionSpan {
                    row: hover.row,
                    col0: hover.col0,
                    col1: hover.col1.saturating_add(1),
                }),
            pointer_shape: self.effective_pointer_shape().into(),
            preedit: self.preedit.clone(),
        };
        let elapsed = refresh_started_at.elapsed();
        self.render_stats
            .record_refresh(elapsed, rows_rebuilt, cells_walked);
        crate::perf::record_refresh(elapsed, rows_rebuilt, cells_walked);
        Ok(())
    }

    pub(super) fn dump(&self) -> DumpData {
        DumpData {
            cols: u32::from(self.snapshot.cols),
            rows: u32::from(self.snapshot.rows),
            cursor: self
                .snapshot
                .cursor
                .filter(|cursor| cursor.visible)
                .map(|cursor| (cursor.row, cursor.col, cursor.visible)),
            rows_text: self
                .snapshot
                .grid
                .iter()
                .map(|row| row.text.clone())
                .collect(),
        }
    }

    pub(super) fn resolved_cells(&self) -> ResolvedCellsData {
        let mut cells = Vec::with_capacity(usize::from(self.cols) * usize::from(self.rows));
        for row in 0..u32::from(self.rows) {
            let mut by_col: HashMap<u16, &DrawCell> = self
                .snapshot
                .grid
                .get(row as usize)
                .map(|rendered| rendered.cells.iter().map(|cell| (cell.col, cell)).collect())
                .unwrap_or_default();
            for col in 0..self.cols {
                let cell = by_col.remove(&col);
                let foreground = cell.map_or(self.snapshot.foreground, |cell| cell.foreground);
                let background = cell.map_or(self.snapshot.background, |cell| cell.background);
                cells.push(ResolvedCellData {
                    row,
                    col,
                    text: cell.map_or_else(|| " ".into(), |cell| cell.text.clone()),
                    fg: (foreground.r, foreground.g, foreground.b),
                    bg: (background.r, background.g, background.b),
                    has_explicit_bg: cell.is_some_and(|cell| cell.explicit_background),
                    bold: cell.is_some_and(|cell| cell.bold),
                    italic: cell.is_some_and(|cell| cell.italic),
                    inverse: cell.is_some_and(|cell| cell.inverse),
                });
            }
        }
        ResolvedCellsData {
            cols: self.cols,
            rows: self.rows,
            cells,
        }
    }
}
