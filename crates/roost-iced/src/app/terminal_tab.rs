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

pub(super) fn terminal_grid(
    size: Size,
    sidebar_collapsed: bool,
    metrics: TerminalMetrics,
) -> (u16, u16) {
    let width = (size.width - sidebar_width(sidebar_collapsed) - 2.0 * TERMINAL_PADDING)
        .max(metrics.cell_width * 2.0);
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
    pub(super) tracking_pointer: Option<PointerButton>,
    pub(super) local_pointer_gesture: Option<LocalPointerGesture>,
    pub(super) last_pointer_cell: Option<(u16, u16)>,
    pub(super) link_modifier_held: bool,
    pub(super) hover_url: Option<HoverUrl>,
    pub(super) selection: TerminalSelection,
    pub(super) word_break_chars: String,
    input_started_at: Instant,
    pub(super) session: TabSession,
    pub(super) output_rx: tokio::sync::mpsc::UnboundedReceiver<TabOutput>,
    reply_buffer: Arc<Mutex<Vec<u8>>>,
    pub(super) input_capture: Option<InputCapture>,
    osc_router: OscRouter,
    pub(super) pointer_shape: String,
    pub(super) theme: Theme,
    pub(super) snapshot: TerminalSnapshot,
    cols: u16,
    rows: u16,
    pub(super) applied_metrics: Option<TerminalMetrics>,
    pub(super) metric_generation: u64,
}

impl TerminalTab {
    pub(super) fn attach(
        supervisor: Arc<PtySupervisor>,
        tab_id: i64,
        test_mode: bool,
        theme: Theme,
        word_break_chars: String,
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
        let session = TabSession::attach(supervisor, tab_id, output_tx, input_capture.clone())?;
        Ok(Self {
            terminal,
            render_state,
            encoder,
            mouse_encoder,
            scroll: TerminalScroll::new(),
            motion_emitter: MotionEmitter::new(),
            tracking_pointer: None,
            local_pointer_gesture: None,
            last_pointer_cell: None,
            link_modifier_held: false,
            hover_url: None,
            selection: TerminalSelection::new(),
            word_break_chars,
            input_started_at: Instant::now(),
            session,
            output_rx,
            reply_buffer,
            input_capture,
            osc_router: OscRouter::new(),
            pointer_shape: "default".into(),
            theme,
            snapshot: TerminalSnapshot::blank(DEFAULT_COLS, DEFAULT_ROWS),
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            applied_metrics: None,
            metric_generation: 0,
        })
    }

    pub(super) fn write_vt(&mut self, bytes: &[u8]) -> Vec<OscAction> {
        let colors = self.osc_colors();
        let actions = self.osc_router.feed(bytes, &colors);
        self.terminal.vt_write(bytes);
        self.drain_terminal_replies();
        actions
    }

    fn osc_colors(&self) -> OscColorSnapshot {
        let fallback_foreground = self.theme.foreground;
        let fallback_background = self.theme.background;
        let (foreground, background, cursor) = self
            .terminal
            .live_colors()
            .map(|colors| {
                (
                    colors.foreground,
                    colors.background,
                    colors.cursor.unwrap_or(self.theme.cursor),
                )
            })
            .unwrap_or((fallback_foreground, fallback_background, self.theme.cursor));
        let palette = self.terminal.live_palette().unwrap_or(self.theme.palette);
        let rgb = |color: roost_vt::ColorRgb| (color.r, color.g, color.b);
        OscColorSnapshot::new(
            rgb(foreground),
            rgb(background),
            rgb(cursor),
            palette.map(rgb),
        )
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

        let bytes = self.encode_pointer(action, button, col, row, mods)?;
        if bytes.is_empty() {
            return Ok(());
        }
        if motion_without_button {
            self.motion_emitter.commit(col, row, now);
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

    pub(super) fn snap_to_bottom_for_input(&mut self) -> Result<bool> {
        let snapped = self.scroll.snap_to_bottom(&mut self.terminal);
        if snapped {
            self.refresh_snapshot()?;
        }
        Ok(snapped)
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
        self.terminal.set_color_foreground(theme.foreground)?;
        self.terminal.set_color_background(theme.background)?;
        self.terminal.set_color_cursor(theme.cursor)?;
        self.terminal.set_color_palette(&theme.palette)?;
        self.refresh_snapshot()
    }

    pub(super) fn refresh_snapshot(&mut self) -> Result<()> {
        self.recompute_hover()?;
        self.render_state.update(&self.terminal)?;
        let colors = self.render_state.colors()?;
        let cursor = self.render_state.cursor();
        let mut cells = Vec::new();
        let mut rows = vec![vec![String::new(); usize::from(self.cols)]; usize::from(self.rows)];
        self.render_state.walk(&self.terminal, |row, cell| {
            if row >= u32::from(self.rows) || cell.col >= self.cols {
                return;
            }
            let text = if cell.text.is_empty() {
                " ".to_string()
            } else {
                cell.text
            };
            rows[row as usize][usize::from(cell.col)] = text.clone();
            let (foreground, background) = resolve_colors(
                cell.fg,
                cell.bg,
                (colors.foreground, colors.background),
                cell.style.inverse,
            );
            if text != " " || cell.bg.is_some() || cell.style.inverse {
                cells.push(DrawCell {
                    row,
                    col: cell.col,
                    text,
                    foreground,
                    background,
                    explicit_background: cell.bg.is_some() || cell.style.inverse,
                    bold: cell.style.bold,
                    italic: cell.style.italic,
                    inverse: cell.style.inverse,
                });
            }
        })?;
        let rows_text = rows
            .into_iter()
            .map(|row| row.concat().trim_end().to_string())
            .collect();
        self.snapshot = TerminalSnapshot {
            cols: self.cols,
            rows: self.rows,
            foreground: colors.foreground,
            background: colors.background,
            cursor,
            cells,
            rows_text,
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
        };
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
            rows_text: self.snapshot.rows_text.clone(),
        }
    }

    pub(super) fn resolved_cells(&self) -> ResolvedCellsData {
        let mut by_position: HashMap<(u32, u16), &DrawCell> = self
            .snapshot
            .cells
            .iter()
            .map(|cell| ((cell.row, cell.col), cell))
            .collect();
        let mut cells = Vec::with_capacity(usize::from(self.cols) * usize::from(self.rows));
        for row in 0..u32::from(self.rows) {
            for col in 0..self.cols {
                let cell = by_position.remove(&(row, col));
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
