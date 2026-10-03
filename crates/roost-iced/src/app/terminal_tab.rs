use super::*;
use crate::terminal_widget::next_press_seq;

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
    /// See `TerminalPointerEvent::press_seq`.
    pub(super) press_seq: Option<u64>,
    /// See `TerminalPointerEvent::overshoot`.
    pub(super) overshoot: i16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LocalPointerGesture {
    Selection,
    MultiClick,
    Url,
}

/// A selection drag whose last motion was past the grid's top or bottom
/// edge (#342).
#[derive(Clone, Copy)]
struct SelectionAutoscroll {
    /// The press the drag belongs to (`TerminalPointerEvent::press_seq`).
    press_seq: u64,
    /// `TerminalPointerEvent::overshoot`, never 0.
    overshoot: i16,
    /// The column the drag's last motion clamped to.
    col: u16,
}

/// The history rows one auto-scroll tick moves for a pointer `overshoot`
/// rows past the grid: toward older history above it — the sign flips
/// here, where rows-below-positive meets history-positive — and at most
/// five rows a tick however far out the pointer is.
pub(super) fn autoscroll_history_rows(overshoot: i16) -> isize {
    let rows = overshoot.unsigned_abs().clamp(1, 5) as isize;
    -isize::from(overshoot.signum()) * rows
}

/// The auto-scroll tick over every tab (#342). Only the tab on screen, in
/// a focused window, scrolls: anywhere else the pointer has left the drag
/// behind, so it disarms.
pub(super) fn autoscroll_selections(
    tabs: &mut HashMap<TabKey, TerminalTab>,
    active: TabKey,
    window_focused: bool,
) {
    for (key, tab) in tabs.iter_mut() {
        if *key != active || !window_focused {
            tab.autoscroll = None;
            continue;
        }
        if let Err(error) = tab.autoscroll_selection() {
            tab.autoscroll = None;
            tracing::warn!(?error, tab_id = key.tab, "selection auto-scroll failed");
        }
    }
}

pub(super) fn pointer_origin_tab<V>(tabs: &mut HashMap<TabKey, V>, tab: TabKey) -> Option<&mut V> {
    tabs.get_mut(&tab)
}

/// Let go of the pointer on every tab in `tabs` but `except`: a held
/// tracking button gets its release, and the presses the tab has seen are
/// settled, so the widget lets go of them and whatever they still have
/// queued is dropped (#587). A tab whose release fails to encode keeps its
/// gesture whole — its application never saw the button come up, so a
/// press the widget forwarded next would reach it with no release before.
pub(super) fn cancel_tab_pointers(
    tabs: &mut HashMap<TabKey, TerminalTab>,
    except: Option<TabKey>,
    reason: &str,
) {
    for (key, tab) in tabs.iter_mut().filter(|(key, _)| Some(**key) != except) {
        match tab.prepare_pointer_cancel() {
            Ok(release) => {
                // The cancel drops hover, so the link underline and
                // pointer shape the snapshot carries are decorations for
                // a gesture that no longer exists.
                if tab.commit_pointer_cancel(release) {
                    refresh_or_warn(key.tab, tab, reason);
                }
            }
            Err(error) => tracing::warn!(?error, tab_id = key.tab, "{reason}"),
        }
    }
}

/// Let go of the pointer on a host tab whose attach is about to detach.
/// The attach's data connection is what the tab's input rides, and the
/// detach aborts its writer with whatever it has not written yet, so a
/// held button's release goes over the host's control connection
/// (`tab.write`) instead — the route `tab.resize` takes for a tab with no
/// attach. With no connection to carry it, the gesture is kept whole, as
/// for a release that fails to encode. Reports whether the tab held any
/// pointer state to let go of.
pub(super) fn release_host_pointer_before_detach(
    tab: &mut TerminalTab,
    key: TabKey,
    ops: Option<&crate::host_conn::HostOps>,
) -> Result<bool> {
    let release = tab.prepare_pointer_cancel()?;
    if !release.is_empty() {
        let ops = ops.ok_or_else(|| anyhow::anyhow!("no connection to carry {key}'s release"))?;
        let params = serde_json::to_value(roost_ipc::messages::TabWriteParams {
            tab_id: key.tab,
            data: release,
        })?;
        let intent = crate::host_conn::HostIntent::new(roost_ipc::messages::ops::TAB_WRITE, params)
            .fenced_at(key.host);
        ops.send(intent)
            .map_err(|error| anyhow::anyhow!("queue {key}'s release: {error}"))?;
    }
    Ok(tab.commit_pointer_cancel(Vec::new()))
}

/// What decides whether a fresh terminal press may start a gesture.
#[derive(Clone, Copy, Debug)]
pub(super) struct PressGate {
    /// The tab on screen, for a press the widget stamped. `None` for a
    /// synthetic press, which names its tab on purpose — a background tab
    /// included.
    pub(super) on_screen: Option<TabKey>,
    pub(super) context_menu_open: bool,
    /// The latest press stamped when a context menu last opened or the
    /// terminal widget was last rewrapped (`App::observe_terminal_wrapping`).
    pub(super) press_floor: u64,
}

impl PressGate {
    fn refuses(&self, event: &TerminalPointerEvent) -> bool {
        event.action == PointerAction::Press
            && (self.on_screen.is_some_and(|tab| tab != event.tab)
                || self.context_menu_open
                || event.press_seq.is_some_and(|seq| seq <= self.press_floor))
    }
}

/// Refuse a fresh press that may not start a gesture (#587), and report
/// whether it was refused. The widget stamps a press when iced hands it
/// the native event, and the model acts on it only when `update` drains
/// the message, so what ran in between decides. Refused: a press for a
/// tab no longer on screen — the switch already let that tab's pointer
/// go, and the release will reach whichever tab shows now — a press
/// under a context menu opened in that gap, whose backdrop (or AppKit's
/// menu tracking) takes the release, and a press stamped by a terminal
/// widget a rewrap has since rebuilt, which drops that press's capture.
/// A cancel that leaves the release with the widget, such as a
/// font-size change, refuses nothing.
pub(super) fn refuse_stale_press(
    tabs: &mut HashMap<TabKey, TerminalTab>,
    event: &TerminalPointerEvent,
    gate: PressGate,
) -> bool {
    if !gate.refuses(event) {
        return false;
    }
    if let (Some(tab), Some(seq)) = (tabs.get_mut(&event.tab), event.press_seq) {
        tab.refuse_press(seq);
    }
    true
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
    /// The grid before the change — known even where `previous` is not,
    /// for a tab that never had metrics installed.
    pub(super) previous_grid: (u16, u16),
    pub(super) current: TerminalGeometry,
    pub(super) metrics_changed: bool,
    pub(super) deferred_replies: Vec<u8>,
}

impl GeometryChange {
    pub(super) fn grid(&self) -> (u16, u16) {
        (self.current.cols, self.current.rows)
    }

    pub(super) fn grid_changed(&self) -> bool {
        self.previous_grid != self.grid()
    }
}

/// One step of a geometry batch, addressed by the SAME key the caller's
/// tab map is keyed on. The walk never narrows to a bare id and re-widens
/// it: the tab whose geometry moved is the tab the key names, whichever
/// instance that is.
#[derive(Debug, Clone, Copy)]
pub(super) enum GeometryBatchOperation {
    Apply {
        tab: TabKey,
        cols: u16,
        rows: u16,
        metrics: TerminalMetrics,
        metric_generation: u64,
    },
    Rollback {
        tab: TabKey,
        previous: TerminalGeometry,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct GeometryBatchFailure {
    pub(super) tab: TabKey,
    pub(super) apply: String,
    pub(super) rollback: Vec<(TabKey, String)>,
}

pub(super) fn apply_geometry_batch(
    keys: &[TabKey],
    cols: u16,
    rows: u16,
    metrics: TerminalMetrics,
    metric_generation: u64,
    mut transition: impl FnMut(
        GeometryBatchOperation,
    ) -> std::result::Result<Option<GeometryChange>, String>,
) -> std::result::Result<Vec<(TabKey, GeometryChange)>, GeometryBatchFailure> {
    let mut applied = Vec::with_capacity(keys.len());
    for key in keys {
        let operation = GeometryBatchOperation::Apply {
            tab: *key,
            cols,
            rows,
            metrics,
            metric_generation,
        };
        match transition(operation) {
            Ok(Some(change)) => applied.push((*key, change)),
            Ok(None) => {}
            Err(error) => {
                let rollback = applied
                    .iter()
                    .rev()
                    .filter_map(|(rollback_key, change): &(TabKey, GeometryChange)| {
                        let previous = change.previous?;
                        transition(GeometryBatchOperation::Rollback {
                            tab: *rollback_key,
                            previous,
                        })
                        .err()
                        .map(|error| (*rollback_key, error))
                    })
                    .collect();
                return Err(GeometryBatchFailure {
                    tab: *key,
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
        &TabBackend::in_process(Arc::clone(&supervisor), true),
        tab_id,
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

/// A host tab's terminal plus the test-mode tap on what it queues toward
/// the host — [`attach_test_terminal`]'s remote twin. There is no PTY and
/// no supervisor: a host tab's bytes leave on `input`, and the capture is
/// what a test reads them back from.
#[cfg(test)]
pub(super) fn attach_test_host_terminal(
    cols: u16,
    rows: u16,
    input: tokio::sync::mpsc::UnboundedSender<super::tab_backend::HostDataMsg>,
) -> (TerminalTab, InputCapture) {
    let handle = TabHandle::host(input, true);
    let capture = handle.capture().cloned().expect("test-mode input capture");
    let tab = TerminalTab::attach_host(
        cols,
        rows,
        Theme::roost_dark_fallback(),
        String::new(),
        handle,
    )
    .expect("host tab terminal");
    (tab, capture)
}

/// A host tab laid out at `cols`×`rows` with `metrics` installed, the
/// shape `host_focus_tab` leaves behind — one a re-grid can move.
#[cfg(test)]
pub(super) fn laid_out_host_terminal(
    cols: u16,
    rows: u16,
    metrics: TerminalMetrics,
) -> TerminalTab {
    let (input, _input_rx) = tokio::sync::mpsc::unbounded_channel();
    let (mut tab, _capture) = attach_test_host_terminal(cols, rows, input);
    let installed = tab
        .apply_geometry(cols, rows, metrics, 1)
        .expect("install host terminal metrics")
        .expect("a new host terminal takes its first metrics");
    tab.commit_geometry(installed);
    tab
}

/// Accumulate one tab's PTY bytes off `rx` until `needle` shows up or
/// the window elapses. Returns what was seen either way, so the same
/// helper serves the positive and the negative assertion.
#[cfg(test)]
pub(super) async fn feed_text_until(
    rx: &mut EngineFeedReceiver,
    tab: TabKey,
    needle: &str,
    window: Duration,
) -> String {
    let deadline = Instant::now() + window;
    let mut seen = String::new();
    loop {
        let mut batch = EngineBatch::default();
        while let Some(item) = rx.try_next(&mut batch) {
            if let EngineFeed::Tab(
                key,
                TabOutput::Bytes(bytes) | TabOutput::Scanned { data: bytes, .. },
            ) = item
            {
                if key == tab {
                    seen.push_str(&String::from_utf8_lossy(&bytes));
                }
            }
        }
        if seen.contains(needle) || Instant::now() >= deadline {
            return seen;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
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
    /// The latest press (`TerminalPointerEvent::press_seq`) this tab has
    /// acted on.
    pub(super) last_press_seq: u64,
    /// The latest press a pointer cancel has settled: its release went
    /// out with the cancel. The view hands it to the widget.
    pub(super) cancelled_through: u64,
    /// Fails every pointer encode: libghostty's encoder has no input that
    /// does, and a cancel's failure path is what the tests pin.
    #[cfg(test)]
    pub(super) fail_pointer_encode: bool,
    /// Live only while [`Self::armed_autoscroll`] says so. A resize clears
    /// it, whether or not the cell grid changes: its overshoot was measured
    /// against edges that have moved.
    autoscroll: Option<SelectionAutoscroll>,
    pub(super) last_pointer_cell: Option<(u16, u16)>,
    pub(super) link_modifier_held: bool,
    pub(super) hover_url: Option<HoverUrl>,
    pub(super) selection: TerminalSelection,
    pub(super) word_break_chars: String,
    input_started_at: Instant,
    /// This tab's attachment to whatever runs its process — the one
    /// place UI code reaches the terminal's backend.
    pub(super) session: TabHandle,
    reply_buffer: Arc<Mutex<Vec<u8>>>,
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
    /// Bumped whenever a theme lands on this tab. It is the cache key
    /// for the theme's `bold_color`, the one theme input
    /// `RenderedRow::build` reads besides the default fg/bg pair:
    /// `bold_color` comes only from the theme, so every change to it
    /// moves this.
    theme_generation: u64,
    cols: u16,
    rows: u16,
    pub(super) applied_metrics: Option<TerminalMetrics>,
    pub(super) metric_generation: u64,
    pub(super) render_stats: crate::perf::TabRenderStats,
}

/// The drain-side scanner's color seed for a theme.
///
/// This is what the terminal itself is seeded with at attach
/// (`set_color_foreground` and friends) and re-seeded with on every
/// theme application, so the drain's answers and the terminal's
/// rendering start from the same colors and are moved by the same OSC
/// sequences from there.
pub(super) fn theme_osc_colors(theme: &Theme) -> OscColorSnapshot {
    let rgb = |color: roost_vt::ColorRgb| (color.r, color.g, color.b);
    OscColorSnapshot::new(
        rgb(theme.foreground),
        rgb(theme.background),
        rgb(theme.cursor),
        theme.palette.map(rgb),
    )
}

impl TerminalTab {
    /// Attach the UI to a tab the backend already has a live session
    /// for. Must be called inside the app runtime (`Runtime::enter`):
    /// the backend's attach binds to the ambient runtime.
    pub(super) fn attach(
        backend: &TabBackend,
        tab_id: i64,
        theme: Theme,
        word_break_chars: String,
        feed: EngineFeedSender,
    ) -> Result<Self> {
        let session = backend.attach(tab_id, theme_osc_colors(&theme), feed)?;
        Self::build(DEFAULT_COLS, DEFAULT_ROWS, theme, word_break_chars, session)
    }

    /// The one `TerminalTab` constructor: a themed terminal at `cols` ×
    /// `rows` with the reply buffer installed, driven by `session`.
    /// Where that session comes from is the caller's business — a local
    /// backend attach, or a host tab's queue toward its data connection.
    fn build(
        cols: u16,
        rows: u16,
        theme: Theme,
        word_break_chars: String,
        session: TabHandle,
    ) -> Result<Self> {
        let mut terminal = Self::new_terminal(cols, rows, &theme)?;
        let reply_buffer = Arc::new(Mutex::new(Vec::new()));
        terminal
            .set_write_pty_buffer(Arc::clone(&reply_buffer))
            .context("install libghostty PTY reply buffer")?;
        let snapshot = TerminalSnapshot::blank_themed(cols, rows, &theme);
        Ok(Self {
            terminal,
            render_state: RenderState::new()?,
            encoder: KeyEncoder::new()?,
            mouse_encoder: MouseEncoder::new()?,
            scroll: TerminalScroll::new(),
            motion_emitter: MotionEmitter::new(),
            drag_gate: DragCellGate::new(),
            tracking_pointer: None,
            local_pointer_gesture: None,
            last_press_seq: 0,
            cancelled_through: 0,
            #[cfg(test)]
            fail_pointer_encode: false,
            autoscroll: None,
            last_pointer_cell: None,
            link_modifier_held: false,
            hover_url: None,
            selection: TerminalSelection::new(),
            word_break_chars,
            input_started_at: Instant::now(),
            session,
            reply_buffer,
            pointer_shape: "default".into(),
            theme,
            preedit: None,
            snapshot,
            // Left empty on purpose: the first `refresh_snapshot` finds no
            // cached grid size, sizes the grid and forces a full rebuild.
            grid: Vec::new(),
            cached_grid_size: None,
            cached_defaults: None,
            cached_theme_generation: None,
            theme_generation: 0,
            cols,
            rows,
            applied_metrics: None,
            metric_generation: 0,
            render_stats: crate::perf::TabRenderStats::default(),
        })
    }

    /// A blank terminal wearing `theme` — the one place a tab's terminal
    /// is built, whether it is the tab's own or a replacement being
    /// hydrated beside it.
    fn new_terminal(cols: u16, rows: u16, theme: &Theme) -> Result<Terminal> {
        let mut terminal = Terminal::new(TerminalOptions {
            cols,
            rows,
            max_scrollback: 2_000,
            continuation_max_bytes: 0,
        })?;
        terminal.set_color_foreground(theme.foreground)?;
        terminal.set_color_background(theme.background)?;
        terminal.set_color_cursor(theme.cursor)?;
        terminal.set_color_palette(&theme.palette)?;
        Ok(terminal)
    }

    /// A terminal for a host attach to hydrate into, built exactly as
    /// this tab's own was and wearing the theme it is wearing now.
    ///
    /// The `vt` payload carries only what the *program* changed, so the
    /// colors underneath have to be the client's — and the reply buffer
    /// is deliberately absent: [`Self::swap_terminal`] installs it at
    /// the moment this terminal becomes the one being rendered, and a
    /// hydration that never finishes must never have been able to write
    /// to the wire.
    pub(super) fn hydration_terminal(&self, cols: u16, rows: u16) -> Result<Terminal> {
        Self::new_terminal(cols, rows, &self.theme)
    }

    /// Attach the UI to a tab that lives on a connected host. No local
    /// backend is involved: `handle` queues input toward the host's data
    /// connection, and the terminal built here is the blank stand-in the
    /// hydration swaps out when it completes (`swap_terminal`) — also
    /// why a re-attach never blanks the tab: the old terminal keeps
    /// rendering until the new one is ready (plan 037 §3.4).
    pub(super) fn attach_host(
        cols: u16,
        rows: u16,
        theme: Theme,
        word_break_chars: String,
        handle: TabHandle,
    ) -> Result<Self> {
        Self::build(cols, rows, theme, word_break_chars, handle)
    }

    /// Install a hydrated terminal in place of the one this tab renders.
    /// The attach built `terminal` from the host's payload — a decoded
    /// snapshot, or a `vt` stream replayed into one of
    /// [`Self::hydration_terminal`]'s — and from here on the tab's own
    /// `write_vt` drives it. The reply buffer moves onto the new
    /// terminal (its replies are then discarded under the host handle's
    /// policy), the selection drops (its grid refs pointed into the old
    /// terminal), and the render caches reset so the next
    /// `refresh_snapshot` rebuilds every row.
    pub(super) fn swap_terminal(
        &mut self,
        mut terminal: Terminal,
        cols: u16,
        rows: u16,
    ) -> Result<()> {
        self.reply_buffer
            .lock()
            .map(|mut buffer| buffer.clear())
            .ok();
        terminal
            .set_write_pty_buffer(Arc::clone(&self.reply_buffer))
            .context("install libghostty PTY reply buffer")?;
        self.terminal = terminal;
        self.cols = cols;
        self.rows = rows;
        self.selection = TerminalSelection::new();
        self.scroll = TerminalScroll::new();
        self.end_selection_drag();
        self.grid = Vec::new();
        self.cached_grid_size = None;
        self.cached_defaults = None;
        self.cached_theme_generation = None;
        Ok(())
    }

    /// Resize a host tab's client terminal directly — the attach
    /// machine's mirror of a RESIZE frame it just queued. Bypasses the
    /// `apply_geometry` transaction on purpose: there is no PTY to
    /// report to (the server owns it), replies are discarded under the
    /// host handle's policy, and the machine already owns latest-wins.
    pub(super) fn resize_for_host(
        &mut self,
        cols: u16,
        rows: u16,
        cell_w: u32,
        cell_h: u32,
    ) -> Result<()> {
        self.terminal
            .resize(cols, rows, cell_w.max(1), cell_h.max(1))?;
        let _ = self.take_terminal_replies();
        self.adopt_grid(cols, rows);
        Ok(())
    }

    fn adopt_grid(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        self.hover_url = None;
        self.autoscroll = None;
        self.last_pointer_cell = self.last_pointer_cell.map(|(col, row)| {
            (
                col.min(cols.saturating_sub(1)),
                row.min(rows.saturating_sub(1)),
            )
        });
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
        self.session.send_replies(self.take_terminal_replies());
    }

    fn take_terminal_replies(&self) -> Vec<u8> {
        self.reply_buffer
            .lock()
            .map(|mut buffer| std::mem::take(&mut *buffer))
            .unwrap_or_default()
    }

    pub(super) fn grid(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    pub(super) fn apply_geometry(
        &mut self,
        cols: u16,
        rows: u16,
        metrics: TerminalMetrics,
        metric_generation: u64,
    ) -> Result<Option<GeometryChange>> {
        let previous_grid = self.grid();
        let metrics_changed = self.applied_metrics != Some(metrics);
        if previous_grid == (cols, rows) && !metrics_changed {
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
        self.adopt_grid(cols, rows);
        self.applied_metrics = Some(metrics);
        self.metric_generation = metric_generation;
        Ok(Some(GeometryChange {
            previous,
            previous_grid,
            current: TerminalGeometry {
                cols,
                rows,
                metrics,
                metric_generation,
            },
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
        self.adopt_grid(previous.cols, previous.rows);
        self.applied_metrics = Some(previous.metrics);
        self.metric_generation = previous.metric_generation;
        Ok(())
    }

    pub(super) fn commit_geometry(&self, change: GeometryChange) {
        let grid_changed = change.grid_changed();
        self.session.send_replies(change.deferred_replies);
        if grid_changed {
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

    /// Send the release [`Self::prepare_pointer_cancel`] staged and settle
    /// every press seen so far. Reports whether the tab held any pointer
    /// state to let go of.
    pub(super) fn commit_pointer_cancel(&mut self, release: Vec<u8>) -> bool {
        if !release.is_empty() {
            self.session.send_input(release);
        }
        self.cancelled_through = self.cancelled_through.max(self.last_press_seq);
        self.drag_gate.reset();
        let tracking = self.tracking_pointer.take();
        let gesture = self.local_pointer_gesture.take();
        let cell = self.last_pointer_cell.take();
        let hover = self.hover_url.take();
        tracking.is_some() || gesture.is_some() || cell.is_some() || hover.is_some()
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
        #[cfg(test)]
        if self.fail_pointer_encode {
            anyhow::bail!("injected pointer encode failure");
        }
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
            press_seq,
            overshoot,
        } = event;
        if press_seq.is_some_and(|seq| seq <= self.cancelled_through) {
            // Queued behind the cancel that settled its press, in the
            // same batch: that press's release already went out, so the
            // rest of it must not reach the application.
            return Ok(NativePointerOutcome::default());
        }
        if let (PointerAction::Press, Some(seq)) = (action, press_seq) {
            self.last_press_seq = self.last_press_seq.max(seq);
        }
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
                    let extended = self.selection.update(&self.terminal, cell.0, cell.1)?;
                    self.autoscroll = match press_seq {
                        Some(press_seq)
                            if extended
                                && overshoot != 0
                                && TerminalScroll::scrolls_locally(&self.terminal) =>
                        {
                            Some(SelectionAutoscroll {
                                press_seq,
                                overshoot,
                                col: cell.0,
                            })
                        }
                        _ => None,
                    };
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
                    self.selection.update(&self.terminal, cell.0, cell.1)?;
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

    /// Settle a press refused before it reached the application (see
    /// [`refuse_stale_press`]): no release is owed, the widget lets go of
    /// it, and whatever of it is still queued is dropped.
    pub(super) fn refuse_press(&mut self, seq: u64) {
        self.cancelled_through = self.cancelled_through.max(seq);
    }

    /// The `press_seq` for an event no widget stamped: a fresh one on a
    /// press, none on a hover, and otherwise this tab's latest press's —
    /// so after a cancel, a button's motion or release is dropped as the
    /// widget's would be.
    pub(super) fn synthetic_press_seq(
        &self,
        action: PointerAction,
        button: Option<PointerButton>,
    ) -> Option<u64> {
        match (action, button) {
            (PointerAction::Press, _) => Some(next_press_seq()),
            (PointerAction::Motion, None) => None,
            _ => Some(self.last_press_seq),
        }
    }

    /// The auto-scroll this tab's selection drag holds, while that drag
    /// is still the one in progress: a selection gesture, and the latest
    /// press this tab has seen.
    fn armed_autoscroll(&self) -> Option<SelectionAutoscroll> {
        self.autoscroll.filter(|armed| {
            self.local_pointer_gesture == Some(LocalPointerGesture::Selection)
                && armed.press_seq == self.last_press_seq
        })
    }

    pub(super) fn autoscroll_armed(&self) -> bool {
        self.armed_autoscroll().is_some()
    }

    /// Stop the auto-scroll but not the drag: its next motion past an edge
    /// arms it again.
    pub(super) fn disarm_autoscroll(&mut self) {
        self.autoscroll = None;
    }

    /// The selection a drag was stretching is gone or replaced, so the
    /// drag is over, and its press is settled as a cancel settles one:
    /// the widget lets go of it, and its later motion and release are
    /// dropped — even if the application turns mouse tracking on while
    /// the button is still down. Its auto-scroll stops.
    fn end_selection_drag(&mut self) {
        if self.local_pointer_gesture == Some(LocalPointerGesture::Selection) {
            self.local_pointer_gesture = None;
            self.cancelled_through = self.cancelled_through.max(self.last_press_seq);
        }
        self.autoscroll = None;
    }

    /// `selection.set`: a selection made from outside the pointer ends
    /// any drag that was stretching the one it replaces.
    pub(super) fn set_selection(&mut self, anchor: (u16, u16), cursor: (u16, u16)) -> Result<bool> {
        self.end_selection_drag();
        Ok(self.selection.set(&self.terminal, anchor, cursor)?)
    }

    /// `selection.clear`, which ends any drag stretching the selection.
    pub(super) fn clear_selection(&mut self) -> bool {
        self.end_selection_drag();
        self.selection.clear()
    }

    /// One auto-scroll step: scroll toward the edge the drag is held past
    /// and stretch the selection to that edge's row. At an end of history
    /// nothing scrolls, but the endpoint still follows the edge row, so a
    /// drag held past the live bottom takes in output as it arrives; the
    /// drag stays armed there. A drag that is over, a selection that is
    /// gone, or a viewport the terminal no longer owns (alternate screen,
    /// mouse tracking) disarms.
    pub(super) fn autoscroll_selection(&mut self) -> Result<()> {
        let Some(armed) = self.armed_autoscroll() else {
            self.autoscroll = None;
            return Ok(());
        };
        let history_rows = autoscroll_history_rows(armed.overshoot);
        let Some(scrolled) = self.scroll.scroll_local(&mut self.terminal, history_rows) else {
            self.autoscroll = None;
            return Ok(());
        };
        let edge_row = if armed.overshoot < 0 {
            0
        } else {
            self.rows.saturating_sub(1)
        };
        let unscrolled_spans = (!scrolled).then(|| {
            self.selection
                .visible_spans(&self.terminal, self.cols, self.rows)
        });
        let extended = self.selection.update(&self.terminal, armed.col, edge_row);
        // A scrolled viewport is published even when the update failed.
        if unscrolled_spans.is_none_or(|spans| {
            spans
                != self
                    .selection
                    .visible_spans(&self.terminal, self.cols, self.rows)
        }) {
            self.refresh_snapshot()?;
        }
        if !extended? {
            self.end_selection_drag();
        }
        Ok(())
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
                .begin(&self.terminal, cell.0, cell.1)?
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
        if !self.set_selection((span.col0, row), (span.col1, row))? {
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
            // Synthesized by this client's VT, so it travels the same
            // reply policy the write- and resize-side drains do.
            self.session.send_replies(if theme.background.is_light() {
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
        self.session.reseed_theme(theme_osc_colors(theme));
        self.refresh_snapshot()
    }

    /// Rebuild the snapshot from the terminal, reusing every cached row
    /// libghostty reports as unchanged.
    ///
    /// **Caching invariant.** A cached `RenderedRow` is valid exactly
    /// while (a) libghostty reports its row undirty and (b) the inputs
    /// `RenderedRow::build` reads besides the row's own vt cells — the
    /// default fg/bg pair, the theme's bold color (keyed by
    /// `theme_generation`) and the grid width — are unchanged. Everything
    /// that alters what a row should render must therefore either mark
    /// that row dirty inside libghostty or move one of the cache keys
    /// guarded below. Anyone adding another input to `RenderedRow::build`
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
        let bold = self.theme.bold_color;
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
            grid[row as usize] = Arc::new(RenderedRow::build(cells, defaults, bold, cols));
            rows_rebuilt += 1;
        })?;

        self.snapshot = TerminalSnapshot {
            cols: self.cols,
            rows: self.rows,
            foreground: colors.foreground,
            background: colors.background,
            cursor,
            cursor_color: self.theme.cursor,
            grid: self.grid.clone(),
            selection_background: self.theme.selection_background,
            selection_foreground: self.theme.selection_foreground,
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

    /// The viewport comes from the render snapshot and the history from
    /// the live terminal, so the snapshot is republished first: a stale
    /// one would put the two halves a PTY chunk apart and break the
    /// adjacency `scrollback_text` promises.
    pub(super) fn dump(&mut self, scrollback: u32) -> Result<DumpData> {
        self.refresh_snapshot()?;
        Ok(DumpData {
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
            scrollback_rows: roost_vt::scrollback_rows(&self.terminal)?,
            scrollback_text: roost_vt::scrollback_text(&self.terminal, scrollback)?,
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// D9 (#545): a session tab's first frame — before the host's
    /// hydration replaces it — must read as the active theme, not the
    /// hardcoded dark placeholder `TerminalSnapshot::blank` carries.
    #[test]
    fn host_attach_first_frame_uses_the_theme() {
        let (input_tx, _input_rx) = tokio::sync::mpsc::unbounded_channel();
        let (tab, _capture) = attach_test_host_terminal(80, 24, input_tx);

        assert_eq!(tab.snapshot.background, tab.theme.background);
        assert_eq!(tab.snapshot.foreground, tab.theme.foreground);
        assert_eq!(tab.snapshot.cursor_color, tab.theme.cursor);
        assert_eq!(
            tab.snapshot.selection_background,
            tab.theme.selection_background
        );
        assert!(tab.snapshot.cursor.is_none());
    }

    /// The UI's row build reads the theme's bold color, and a theme that
    /// changes only `bold_color` recolors a bold row built before it.
    /// Applying a theme re-seeds libghostty's colors, which already
    /// dirties every row, so this does not isolate the `theme_generation`
    /// guard.
    #[test]
    fn a_theme_bold_color_recolors_a_row_built_before_it() {
        let (input_tx, _input_rx) = tokio::sync::mpsc::unbounded_channel();
        let (mut tab, _capture) = attach_test_host_terminal(80, 24, input_tx);
        tab.write_vt(b"\x1b[1mB\x1b[0m");
        tab.refresh_snapshot().expect("build the bold row");
        tab.refresh_snapshot().expect("refresh over the clean row");
        let bold_ink = |tab: &TerminalTab| tab.snapshot.grid[0].cells[0].foreground;
        assert_eq!(bold_ink(&tab), tab.theme.foreground);

        let bold_color = ColorRgb {
            r: 0x12,
            g: 0x34,
            b: 0x56,
        };
        let theme = Theme {
            bold_color: Some(bold_color),
            ..tab.theme.clone()
        };
        tab.set_theme(&theme).expect("apply the bold color");
        assert_eq!(bold_ink(&tab), bold_color);
    }
}
