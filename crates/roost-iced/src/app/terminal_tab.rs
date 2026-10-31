use super::*;
use crate::terminal_widget::next_press_seq;
use roost_ipc::dataframe::InputTake;

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

/// The rows of a mirror drawn above the view (plan 084 D7): none while its
/// grid fits, else the fewest that keep the cursor's row on screen. The
/// row is counted from the live bottom (`scrolled_back` rows of history
/// below the viewport), so a local scroll moves the drawn rows with the
/// history instead of re-anchoring them, and a viewport the cursor has
/// left keeps `last`.
pub(super) fn grid_offset_rows(
    cursor_row: Option<u32>,
    scrolled_back: u32,
    grid_rows: u16,
    view_rows: u16,
    last: u32,
) -> u32 {
    let max = u32::from(grid_rows.saturating_sub(view_rows));
    match cursor_row {
        Some(row) => (row.saturating_sub(scrolled_back) + 1)
            .saturating_sub(u32::from(view_rows))
            .min(max),
        None => last.min(max),
    }
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
            if tab.autoscroll.take().is_some() {
                tracing::info!(
                    tab_id = key.tab,
                    active = *key == active,
                    window_focused,
                    "selection auto-scroll stopped: its tab is not shown in a focused window"
                );
            }
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
        if tab.autoscroll_armed() {
            tracing::info!(
                tab_id = key.tab,
                reason,
                "selection auto-scroll stopped by a pointer cancel"
            );
        }
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
    /// The grid the window has room for. A host tab's terminal keeps its
    /// session's grid and is drawn in this view (plan 084 D7); an
    /// in-process tab's grid is its view.
    pub(super) view: (u16, u16),
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
            &TabOpener::None,
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

/// The terminal viewport in the window content: right of the sidebar, under
/// the tab band. The grid a tab is sized to and the origin `app.window_metrics`
/// reports both come from it.
pub(super) fn terminal_viewport(size: Size, sidebar_width: f32) -> iced::Rectangle {
    iced::Rectangle {
        x: sidebar_width,
        y: chrome::BAND_HEIGHT,
        width: size.width - sidebar_width,
        height: size.height - chrome::BAND_HEIGHT,
    }
}

pub(super) fn terminal_grid(
    size: Size,
    sidebar_width: f32,
    metrics: TerminalMetrics,
) -> (u16, u16) {
    let viewport = terminal_viewport(size, sidebar_width);
    let width = (viewport.width - 2.0 * TERMINAL_PADDING).max(metrics.cell_width * 2.0);
    let height = (viewport.height - 2.0 * TERMINAL_PADDING).max(metrics.cell_height * 2.0);
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
    /// OSC 7501 reports and OSC 133 marks, in byte order — installed only
    /// where this terminal is the tab's authoritative one (plan 082 D4.2).
    status_sink: Option<Arc<Mutex<Vec<roost_vt::TerminalEvent>>>>,
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
    /// See [`TerminalGeometry::view`].
    view: (u16, u16),
    /// [`grid_offset_rows`] as of the last snapshot.
    grid_offset: u32,
    /// A local scroll toward history reached the oldest row the viewport
    /// can show, so the grid is drawn from its own top: the rows above
    /// the offset would be out of reach otherwise. Any scroll back down or
    /// a keystroke lets the cursor anchor it again; a new grid keeps it,
    /// with the line at the top of the view.
    top_pinned: bool,
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
        // Only a terminal whose replies reach the PTY answers the program:
        // a host tab's server terminal answers the probe and keeps the
        // records, so a sink here would make a second authority.
        let status_sink = match session.reply_policy() {
            ReplyPolicy::DrainToPty => {
                let sink = Arc::new(Mutex::new(Vec::new()));
                terminal
                    .set_status_sink(Arc::clone(&sink))
                    .context("install libghostty program-status sink")?;
                Some(sink)
            }
            ReplyPolicy::Discard => None,
        };
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
            status_sink,
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
            view: (cols, rows),
            grid_offset: 0,
            top_pinned: false,
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
        self.top_pinned = false;
        self.end_selection_drag();
        self.grid = Vec::new();
        self.cached_grid_size = None;
        self.cached_defaults = None;
        self.cached_theme_generation = None;
        Ok(())
    }

    /// Resize a host tab's client terminal directly — the attach
    /// machine applying a `GRID` its session sent. Bypasses the
    /// `apply_geometry` transaction on purpose: there is no PTY to
    /// report to (the server owns it) and replies are discarded under
    /// the host handle's policy.
    ///
    /// A viewport scrolled back into history keeps the line drawn at its
    /// top there (plan 084 D6): the reflow would otherwise move the
    /// reader somewhere else in the history they were reading.
    pub(super) fn resize_for_host(
        &mut self,
        cols: u16,
        rows: u16,
        cell_w: u32,
        cell_h: u32,
    ) -> Result<()> {
        let anchor = if self.scroll.is_scrolled_back() || self.top_pinned {
            self.terminal
                .track(roost_vt::Point::viewport(0, self.grid_offset))
                .ok()
        } else {
            None
        };
        self.terminal
            .resize(cols, rows, cell_w.max(1), cell_h.max(1))?;
        let _ = self.take_terminal_replies();
        self.adopt_grid(cols, rows);
        if let Some(anchor) = anchor {
            self.restore_top_line(&anchor);
        }
        Ok(())
    }

    /// Scroll the viewport so `anchor`'s line is the one drawn at the
    /// view's top again, at the offset the grid now allows.
    fn restore_top_line(&mut self, anchor: &roost_vt::TrackedRef) {
        let Some(line) = anchor.point(roost_vt::PointTag::Screen) else {
            return;
        };
        let Ok(scrollbar) = self.terminal.scrollbar() else {
            return;
        };
        let offset = u64::from(grid_offset_rows(
            None,
            0,
            self.rows,
            self.view.1,
            self.grid_offset,
        ));
        let wanted_top = i64::from(line.y) - offset as i64;
        let moved_down = wanted_top - scrollbar.offset as i64;
        let history_rows = isize::try_from(-moved_down).unwrap_or(0);
        let _ = self.scroll.scroll_local(&mut self.terminal, history_rows);
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
    /// scanned as it read it. Returns the chunk's program-status events;
    /// always empty for a tab without a status sink.
    pub(super) fn write_vt(&mut self, bytes: &[u8]) -> Vec<StatusEvent> {
        self.terminal.vt_write(bytes);
        self.drain_terminal_replies();
        self.take_status()
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
    pub(super) fn scan_and_write_vt(&mut self, bytes: &[u8]) -> (Vec<StatusEvent>, Vec<OscAction>) {
        let actions = self.session.scan_osc(bytes);
        let status = self.write_vt(bytes);
        (status, actions)
    }

    fn take_status(&self) -> Vec<StatusEvent> {
        self.status_sink
            .as_deref()
            .map(roost_engine::program_status::drain)
            .unwrap_or_default()
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

    /// Lay the tab out in a `cols`×`rows` view with `metrics`. A host
    /// tab's grid stays ([`TerminalGeometry::view`]): the window's grid
    /// reaches its session through the attach and comes back as `GRID`.
    pub(super) fn apply_geometry(
        &mut self,
        cols: u16,
        rows: u16,
        metrics: TerminalMetrics,
        metric_generation: u64,
    ) -> Result<Option<GeometryChange>> {
        let previous_grid = self.grid();
        let view = (cols, rows);
        let grid = if self.session.is_host() {
            previous_grid
        } else {
            view
        };
        let metrics_changed = self.applied_metrics != Some(metrics);
        if previous_grid == grid && self.view == view && !metrics_changed {
            return Ok(None);
        }
        let previous = self.applied_metrics.map(|metrics| TerminalGeometry {
            cols: self.cols,
            rows: self.rows,
            view: self.view,
            metrics,
            metric_generation: self.metric_generation,
        });
        let resize = if previous_grid != grid || metrics_changed {
            self.terminal.resize(
                grid.0,
                grid.1,
                metrics.cell_width.round().max(1.0) as u32,
                metrics.cell_height.round().max(1.0) as u32,
            )
        } else {
            Ok(())
        };
        let deferred_replies = self.take_terminal_replies();
        resize?;
        self.adopt_grid(grid.0, grid.1);
        self.view = view;
        self.applied_metrics = Some(metrics);
        self.metric_generation = metric_generation;
        Ok(Some(GeometryChange {
            previous,
            previous_grid,
            current: TerminalGeometry {
                cols: grid.0,
                rows: grid.1,
                view,
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
        self.view = previous.view;
        self.applied_metrics = Some(previous.metrics);
        self.metric_generation = previous.metric_generation;
        Ok(())
    }

    /// The view this tab is drawn in — see [`TerminalGeometry::view`].
    pub(super) fn view(&self) -> (u16, u16) {
        self.view
    }

    /// See [`grid_offset_rows`].
    pub(super) fn grid_offset(&self) -> u32 {
        self.grid_offset
    }

    /// [`crate::terminal_widget::drawn_rows`], never none.
    fn visible_rows(&self) -> u16 {
        crate::terminal_widget::drawn_rows(self.rows, self.grid_offset, self.view.1).max(1)
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
            self.session.send_input(release, InputTake::None);
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
        let take = if motion_without_button {
            self.motion_emitter.commit(col, row, now);
            InputTake::None
        } else {
            self.drag_gate.commit_dispatched(action, button, (col, row));
            InputTake::After
        };
        self.session.send_input(bytes, take);
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

    /// Route one wheel event. Reports whether it moved the local
    /// viewport rather than reaching the program.
    pub(super) fn handle_wheel(
        &mut self,
        history_rows: f64,
        col: u32,
        row: u32,
        mods: u16,
    ) -> Result<bool> {
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
                self.session.send_input(bytes, InputTake::After);
            }
            Some(ScrollRoute::LocalViewport { .. }) => {
                self.scrolled_locally(history_rows > 0.0);
                return Ok(true);
            }
            None => {}
        }
        Ok(false)
    }

    /// The viewport moved locally, toward history or not: see
    /// [`Self::top_pinned`].
    fn scrolled_locally(&mut self, toward_history: bool) {
        self.top_pinned =
            toward_history && self.terminal.scrollbar().is_ok_and(|bar| bar.offset == 0);
    }

    /// Route a Page Up / Page Down press through the shared scroll policy.
    /// A local page repaints here and reports `LocalViewport` so the caller
    /// consumes the key; `Forward` leaves the key to the normal encode path.
    pub(super) fn handle_page(&mut self, direction: PageDirection) -> Result<PageRoute> {
        let rows = usize::from(self.visible_rows());
        let route = self.scroll.route_page(&mut self.terminal, direction, rows);
        if matches!(route, PageRoute::LocalViewport { .. }) {
            self.scrolled_locally(direction == PageDirection::Up);
            self.refresh_snapshot()?;
        }
        Ok(route)
    }

    pub(super) fn snap_to_bottom_for_input(&mut self) -> Result<bool> {
        let unpinned = std::mem::take(&mut self.top_pinned);
        let snapped = self.scroll.snap_to_bottom(&mut self.terminal);
        if snapped || unpinned {
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
        self.session.send_input(bytes, InputTake::Before);
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
                // A motion that belongs to no press is the pointer hovering:
                // it is not the held button's drag, so it neither moves the
                // selection nor stops its auto-scroll (#606).
                Some(LocalPointerGesture::Selection) if press_seq.is_none() => {
                    Ok(NativePointerOutcome::default())
                }
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
                    self.autoscroll = None;
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
        if self.autoscroll_armed() {
            tracing::info!("selection auto-scroll stopped: selection.set replaced its drag");
        }
        self.end_selection_drag();
        Ok(self.selection.set(&self.terminal, anchor, cursor)?)
    }

    /// `selection.clear`, which ends any drag stretching the selection.
    pub(super) fn clear_selection(&mut self) -> bool {
        if self.autoscroll_armed() {
            tracing::info!("selection auto-scroll stopped: selection.clear ended its drag");
        }
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
            if self.autoscroll.take().is_some() {
                tracing::info!(
                    gesture = ?self.local_pointer_gesture,
                    "selection auto-scroll stopped: its drag is over"
                );
            }
            return Ok(());
        };
        let history_rows = autoscroll_history_rows(armed.overshoot);
        let Some(scrolled) = self.scroll.scroll_local(&mut self.terminal, history_rows) else {
            tracing::info!("selection auto-scroll stopped: the viewport no longer scrolls locally");
            self.autoscroll = None;
            return Ok(());
        };
        let was_pinned = self.top_pinned;
        self.scrolled_locally(history_rows > 0);
        if self.top_pinned != was_pinned {
            // The edge row below is read off the offset the pin moves.
            self.refresh_snapshot()?;
        }
        // The edge the view draws, which a mirror taller than the view
        // has below its offset (plan 084 D7).
        let top = u16::try_from(self.grid_offset).unwrap_or(u16::MAX);
        let edge_row = if armed.overshoot < 0 {
            top
        } else {
            top.saturating_add(self.visible_rows() - 1)
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
            tracing::info!(
                "selection auto-scroll stopped: the selection could not follow the edge"
            );
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
            self.session.send_input(bytes, InputTake::Before);
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
            self.session.send_input(bytes, InputTake::None);
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
        self.grid_offset = if self.rows <= self.view.1 || self.top_pinned {
            0
        } else {
            let scrolled_back = self
                .terminal
                .scrollbar()
                .map(|bar| bar.total.saturating_sub(bar.offset).saturating_sub(bar.len))
                .unwrap_or(0);
            grid_offset_rows(
                cursor.map(|cursor| cursor.row),
                u32::try_from(scrolled_back).unwrap_or(u32::MAX),
                self.rows,
                self.view.1,
                self.grid_offset,
            )
        };

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
            cursor_text: self.theme.cursor_text,
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
    use roost_ipc::program_status::{ProgramStatusState, PromptMark};

    const WORKING: &[u8] = b"\x1b]7501;state=working:app=pi\x1b\\";

    /// Plan 082 D4.2/D4.3: an in-process tab's terminal is the tab's
    /// authoritative one, so its status sink hands each chunk's reports and
    /// prompt marks back from `write_vt`, in byte order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_in_process_tab_hands_back_its_chunks_program_status() {
        let (feed_tx, _feed_rx) = engine_feed::channel();
        let (mut tab, supervisor) = attach_test_terminal(41, feed_tx);

        let mut chunk = WORKING.to_vec();
        chunk.extend_from_slice(b"\x1b]133;D;0\x07");
        let events = tab.write_vt(&chunk);

        assert!(
            matches!(
                events.as_slice(),
                [
                    StatusEvent::Report(report),
                    StatusEvent::Prompt(PromptMark::CommandEnd),
                ] if report.state == ProgramStatusState::Working && report.app == "pi"
            ),
            "{events:?}"
        );
        assert!(tab.write_vt(b"plain").is_empty(), "the sink was drained");
        supervisor.close(41);
    }

    /// A host tab's server terminal is the authoritative one; the client's
    /// stand-in and the hydrated terminal that replaces it keep no records.
    #[test]
    fn a_host_tab_keeps_no_program_status() {
        let (input_tx, _input_rx) = tokio::sync::mpsc::unbounded_channel();
        let (mut tab, _capture) = attach_test_host_terminal(80, 24, input_tx);
        assert!(tab.write_vt(WORKING).is_empty());

        let hydrated = tab.hydration_terminal(80, 24).expect("hydration terminal");
        tab.swap_terminal(hydrated, 80, 24).expect("swap");
        assert!(tab.write_vt(WORKING).is_empty());
    }

    /// A hydrated terminal starts at its live bottom, so a pin to the old
    /// terminal's top does not carry over to it.
    #[test]
    fn a_swapped_in_terminal_is_anchored_on_its_cursor_not_a_stale_pin() {
        let metrics = TerminalMetrics::measure(13.0).expect("metrics");
        let mut tab = laid_out_host_terminal(80, 30, metrics);
        let view = tab
            .apply_geometry(80, 10, metrics, 2)
            .expect("view")
            .expect("a new view");
        tab.commit_geometry(view);
        for line in 0..25 {
            tab.write_vt(format!("line {line:03}\r\n").as_bytes());
        }
        tab.refresh_snapshot().expect("refresh");
        assert!(matches!(
            tab.handle_page(PageDirection::Up),
            Ok(PageRoute::LocalViewport { .. })
        ));
        assert_eq!(tab.grid_offset(), 0, "pinned to the grid's top");

        let mut hydrated = tab.hydration_terminal(80, 30).expect("hydration terminal");
        for line in 0..25 {
            hydrated.vt_write(format!("line {line:03}\r\n").as_bytes());
        }
        tab.swap_terminal(hydrated, 80, 30).expect("swap");
        tab.refresh_snapshot().expect("refresh");
        assert_eq!(tab.grid_offset(), 16, "anchored on the new cursor");
    }

    /// Plan 084 D7's offset: none while the grid fits the view, else the
    /// fewest rows that keep the cursor's row on screen, counted from the
    /// live bottom so a scroll back moves the drawn rows with the history,
    /// and the last offset while the cursor is out of the viewport.
    #[test]
    fn the_offset_keeps_the_cursor_row_in_view_with_the_smallest_scroll() {
        assert_eq!(grid_offset_rows(Some(40), 0, 50, 30, 0), 11);
        assert_eq!(
            grid_offset_rows(Some(49), 0, 50, 30, 0),
            20,
            "the bottom row"
        );
        assert_eq!(grid_offset_rows(Some(5), 0, 50, 30, 9), 0, "top-aligned");
        assert_eq!(grid_offset_rows(Some(40), 0, 24, 30, 0), 0, "the grid fits");
        assert_eq!(grid_offset_rows(Some(40), 0, 30, 30, 0), 0, "exactly");
        assert_eq!(
            grid_offset_rows(Some(43), 3, 50, 30, 0),
            11,
            "scrolled back 3 rows: the same rows, three history rows lower"
        );
        assert_eq!(
            grid_offset_rows(None, 9, 50, 30, 11),
            11,
            "kept while scrolled away"
        );
        assert_eq!(
            grid_offset_rows(None, 9, 50, 45, 11),
            5,
            "clamped to what the grid allows"
        );
    }

    /// Plan 084 D6: a host tab's grid is its session's. A window re-grid
    /// installs the cell metrics and moves the view the grid is drawn in,
    /// and reports no grid change, so nothing downstream sends a resize
    /// or forgets a resume point. An in-process tab's grid is its view.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_host_tab_takes_the_windows_cells_and_view_but_keeps_its_grid() {
        let metrics = TerminalMetrics::measure(13.0).expect("metrics");
        let bigger = TerminalMetrics::measure(15.0).expect("bigger metrics");
        let mut host = laid_out_host_terminal(80, 24, metrics);
        let change = host
            .apply_geometry(120, 30, bigger, 2)
            .expect("apply")
            .expect("a change");
        assert!(!change.grid_changed());
        assert_eq!(host.grid(), (80, 24));
        assert_eq!(host.view(), (120, 30));
        assert_eq!(host.applied_metrics, Some(bigger));
        assert!(
            host.apply_geometry(120, 30, bigger, 2)
                .expect("apply")
                .is_none(),
            "nothing moved"
        );
        host.rollback_geometry(change.previous.expect("a previous layout"))
            .expect("roll back");
        assert_eq!((host.grid(), host.view()), ((80, 24), (80, 24)));

        let (feed_tx, _feed_rx) = engine_feed::channel();
        let (mut local, supervisor) = attach_test_terminal(42, feed_tx);
        let change = local
            .apply_geometry(100, 30, metrics, 2)
            .expect("apply")
            .expect("a change");
        assert!(change.grid_changed());
        assert_eq!((local.grid(), local.view()), ((100, 30), (100, 30)));
        supervisor.close(42);
    }

    fn queued_takes(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<super::super::tab_backend::HostDataMsg>,
    ) -> Vec<InputTake> {
        let mut takes = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let super::super::tab_backend::HostDataMsg::Input { take, .. } = msg {
                takes.push(take);
            }
        }
        takes
    }

    /// Plan 084 D6's marking of what a host tab's input asks of its size:
    /// keys, IME and paste take before their bytes; a click and the wheel
    /// after; a pointer with no button held, a focus report and a pointer
    /// cancel never.
    #[test]
    fn a_host_tabs_input_says_what_it_asks_of_the_size() {
        let metrics = TerminalMetrics::measure(13.0).expect("metrics");
        let (input, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (mut tab, _capture) = attach_test_host_terminal(80, 24, input);
        let installed = tab
            .apply_geometry(80, 24, metrics, 1)
            .expect("metrics")
            .expect("installed");
        tab.commit_geometry(installed);

        tab.paste(Some("pasted"));
        tab.commit_ime("日本").expect("ime");
        assert_eq!(queued_takes(&mut rx), vec![InputTake::Before; 2]);

        // Mouse tracking, any-event (1003), so a hover reports too.
        tab.write_vt(b"\x1b[?1003h\x1b[?1006h");
        tab.dispatch_pointer(PointerAction::Press, Some(PointerButton::Left), 3, 2, 0)
            .expect("press");
        tab.dispatch_pointer(PointerAction::Motion, Some(PointerButton::Left), 4, 2, 0)
            .expect("drag");
        tab.dispatch_pointer(PointerAction::Release, Some(PointerButton::Left), 4, 2, 0)
            .expect("release");
        tab.handle_wheel(3.0, 4, 2, 0).expect("wheel");
        assert!(queued_takes(&mut rx)
            .iter()
            .all(|take| *take == InputTake::After));

        tab.dispatch_pointer(PointerAction::Motion, None, 9, 9, 0)
            .expect("hover");
        tab.write_vt(b"\x1b[?1004h");
        tab.set_window_focus(false);
        tab.tracking_pointer = Some(PointerButton::Left);
        let release = tab.prepare_pointer_cancel().expect("release");
        tab.commit_pointer_cancel(release);
        let takes = queued_takes(&mut rx);
        assert_eq!(
            takes.len(),
            3,
            "a hover, a focus report and a cancel: {takes:?}"
        );
        assert!(
            takes.iter().all(|take| *take == InputTake::None),
            "{takes:?}"
        );

        // Off mouse tracking, on the alternate screen, the wheel is arrow
        // keys — still the wheel.
        tab.write_vt(b"\x1b[?1003l\x1b[?1049h");
        tab.handle_wheel(3.0, 4, 2, 0).expect("wheel as keys");
        assert_eq!(queued_takes(&mut rx), vec![InputTake::After]);
    }

    /// Plan 084 D6: when a `GRID` reflows a mirror whose viewport is
    /// scrolled back into history, the line drawn at the top of the view
    /// stays there. The mirror here is taller than the view, so the view
    /// draws it from a row offset; the `GRID` brings it to the view's
    /// grid, where nothing is offset, and libghostty's own viewport pin
    /// alone would put a different line on top.
    #[test]
    fn a_grid_keeps_the_line_at_the_top_of_a_scrolled_back_view() {
        let metrics = TerminalMetrics::measure(13.0).expect("metrics");
        let mut tab = laid_out_host_terminal(80, 30, metrics);
        let view = tab
            .apply_geometry(80, 10, metrics, 2)
            .expect("view")
            .expect("a new view");
        tab.commit_geometry(view);
        for line in 0..120 {
            tab.write_vt(format!("line {line:03}\r\n").as_bytes());
        }
        tab.refresh_snapshot().expect("refresh");
        assert_eq!(tab.grid_offset(), 20, "the cursor's row in view");
        assert_eq!(tab.scroll.scroll_local(&mut tab.terminal, 25), Some(true));
        tab.refresh_snapshot().expect("refresh");
        let top = |tab: &mut TerminalTab| {
            let offset = tab.grid_offset() as usize;
            tab.dump(0).expect("dump").rows_text[offset].clone()
        };
        let before = top(&mut tab);
        assert!(before.starts_with("line"), "{before:?}");

        tab.resize_for_host(80, 10, 9, 18).expect("GRID");
        tab.refresh_snapshot().expect("refresh");
        assert_eq!(tab.grid_offset(), 0);
        assert_eq!(top(&mut tab), before);
    }

    /// Plan 084 D7: the rows above the offset stay reachable. With no
    /// history to scroll, or at the oldest row history keeps, a Page Up
    /// draws the grid from its own top; short of that row the cursor
    /// keeps anchoring it, and a keystroke anchors it on the cursor again.
    #[test]
    fn a_page_up_at_the_oldest_row_draws_the_grid_from_its_top() {
        let metrics = TerminalMetrics::measure(13.0).expect("metrics");
        let mut tab = laid_out_host_terminal(80, 30, metrics);
        let view = tab
            .apply_geometry(80, 10, metrics, 2)
            .expect("view")
            .expect("a new view");
        tab.commit_geometry(view);
        for line in 0..25 {
            tab.write_vt(format!("line {line:03}\r\n").as_bytes());
        }
        tab.refresh_snapshot().expect("refresh");
        assert_eq!(tab.grid_offset(), 16, "the cursor's row in view");
        let top = |tab: &mut TerminalTab| {
            let offset = tab.grid_offset() as usize;
            tab.dump(0).expect("dump").rows_text[offset].clone()
        };
        let page = |tab: &mut TerminalTab, direction| {
            assert!(matches!(
                tab.handle_page(direction),
                Ok(PageRoute::LocalViewport { .. })
            ));
        };

        page(&mut tab, PageDirection::Up);
        assert_eq!(tab.grid_offset(), 0, "no history: the grid's own top");
        assert!(top(&mut tab).starts_with("line 000"));
        tab.snap_to_bottom_for_input().expect("a keystroke");
        assert_eq!(tab.grid_offset(), 16, "anchored on the cursor again");

        for line in 25..100 {
            tab.write_vt(format!("line {line:03}\r\n").as_bytes());
        }
        tab.refresh_snapshot().expect("refresh");
        assert_eq!(tab.grid_offset(), 20);
        page(&mut tab, PageDirection::Up);
        assert_eq!(tab.grid_offset(), 20, "history left above: still anchored");
        for _ in 0..10 {
            page(&mut tab, PageDirection::Up);
        }
        assert_eq!(tab.grid_offset(), 0, "the oldest row: the grid's own top");
        assert!(top(&mut tab).starts_with("line 000"), "{:?}", top(&mut tab));
        tab.snap_to_bottom_for_input().expect("a keystroke");
        assert_eq!(tab.grid_offset(), 20, "anchored on the cursor again");
    }

    /// The grid of the page-up case above: 30 rows in a 10-row view, 25
    /// lines and no history, the cursor anchoring the offset at 16.
    fn pinnable_tab() -> TerminalTab {
        let metrics = TerminalMetrics::measure(13.0).expect("metrics");
        let mut tab = laid_out_host_terminal(80, 30, metrics);
        let view = tab
            .apply_geometry(80, 10, metrics, 2)
            .expect("view")
            .expect("a new view");
        tab.commit_geometry(view);
        for line in 0..25 {
            tab.write_vt(format!("line {line:03}\r\n").as_bytes());
        }
        tab.refresh_snapshot().expect("refresh");
        assert_eq!(tab.grid_offset(), 16);
        tab
    }

    fn drawn_top(tab: &mut TerminalTab) -> String {
        let offset = tab.grid_offset() as usize;
        tab.dump(0).expect("dump").rows_text[offset].clone()
    }

    /// A selection drag held above the view scrolls toward history like a
    /// Page Up, and at the oldest row draws the grid from its own top, so
    /// the drag reaches rows the offset hid.
    #[test]
    fn a_selection_drag_held_above_the_view_reaches_the_grids_top() {
        let mut tab = pinnable_tab();
        let dispatch = |tab: &mut TerminalTab, action, row, overshoot| {
            let button = Some(PointerButton::Left);
            let press_seq = tab.synthetic_press_seq(action, button);
            tab.handle_native_pointer(NativePointerDispatch {
                action,
                button,
                col: 0,
                row,
                mods: 0,
                click_count: 1,
                inside: overshoot == 0,
                link_modifier_held: false,
                press_seq,
                overshoot,
            })
            .expect("pointer");
        };
        dispatch(&mut tab, PointerAction::Press, 20, 0);
        dispatch(&mut tab, PointerAction::Motion, 16, -3);
        assert!(tab.autoscroll_armed());
        tab.autoscroll_selection().expect("tick");
        assert_eq!(tab.grid_offset(), 0, "the oldest row: the grid's own top");
        assert!(drawn_top(&mut tab).starts_with("line 000"));
        assert!(
            tab.selected_text()
                .expect("selection")
                .is_some_and(|text| text.starts_with("line 000")),
            "the drag reached the grid's top row"
        );
    }

    /// Plan 084 D6: a `GRID` keeps the line drawn at the top of a pinned
    /// view there, as it does a scrolled-back one's — the take that a
    /// settled Page Up sends included — and one that moves nothing but
    /// the owner leaves the pin alone.
    #[test]
    fn a_grid_keeps_the_top_line_of_a_view_pinned_to_the_grids_top() {
        let mut tab = pinnable_tab();
        assert!(matches!(
            tab.handle_page(PageDirection::Up),
            Ok(PageRoute::LocalViewport { .. })
        ));
        assert!(drawn_top(&mut tab).starts_with("line 000"));

        tab.resize_for_host(80, 30, 9, 18)
            .expect("an owner-only GRID");
        tab.refresh_snapshot().expect("refresh");
        assert_eq!(tab.grid_offset(), 0, "still pinned");
        assert!(drawn_top(&mut tab).starts_with("line 000"));

        tab.resize_for_host(80, 10, 9, 18).expect("the take's GRID");
        tab.refresh_snapshot().expect("refresh");
        let top = drawn_top(&mut tab);
        assert!(top.starts_with("line 000"), "{top:?}");
    }

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
